//! Top-level GPUI view that hosts the sidebar + detail pane after a
//! folder has been opened (issue #20).
//!
//! The view owns an [`AppState`] (the pure-data view-model from
//! [`crate::app_state`]) and re-renders whenever the state changes. State
//! transitions happen through two entry points:
//!
//! 1. `File → Open…` (or the empty-state button) dispatches the
//!    `OpenFolder` action. A handler installed on this view opens an
//!    `NSOpenPanel` via [`rfd::FileDialog`] and, on success, replaces the
//!    state with the cache-backed result. No scan runs.
//! 2. Clicking a sidebar row updates `selected_group`.
//!
//! All layout is plain GPUI (`div` + flex + click handlers). The three
//! sidebar sections follow the acceptance criteria: Tier B first, Tier A
//! second, Dismissed collapsed last.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use gpui::{Context, MouseButton, Window, black, div, prelude::*, px, rgb, white};

use crate::app_state::{
    AppState, AppStatus, GroupView, ScanState, SuppressionView, format_completion_banner,
    format_elapsed, group_view_from_match, load_folder,
};
use crate::menubar::{OpenFolder, StartScan, StopScan};
use dedup_core::{
    Cache, Config, MatchGroup, ScanConfig, ScanError, ScanResult, Scanner, TierAStreamCallback,
};

// Colors pulled out so the whole view uses one palette — the empty-state
// view already picked `0x1e1e22` as the background, so we match.
const BG: u32 = 0x1e1e22;
const SIDEBAR_BG: u32 = 0x24242a;
const SECTION_HEADER: u32 = 0x9a9aa2;
const ROW_TEXT: u32 = 0xe0e0e4;
const ROW_TEXT_DIM: u32 = 0xa0a0a8;
const ROW_SELECTED_BG: u32 = 0x3b3b48;
const ACCENT: u32 = 0x3b82f6;
const ACCENT_DIM: u32 = 0x2a2a34;
const BANNER_BG: u32 = 0x14532d;
const BANNER_TEXT: u32 = 0xd1fae5;
const PROGRESS_BAR_BG: u32 = 0x2d2d35;
const PROGRESS_BAR_FG: u32 = 0x3b82f6;

/// Events the scan worker thread sends back to the GUI poll loop.
///
/// `TierAStream` may arrive once per scan (the single final-membership
/// pulse the scanner emits before Tier B promotion). `Completed` or
/// `Cancelled` arrive exactly once and terminate the stream. A
/// disconnected channel without a terminator — e.g. the worker panicked
/// — is treated as `Cancelled` by the poll loop.
#[derive(Debug)]
enum ScanEvent {
    /// Tier A groups at final membership (issue #22 streaming).
    TierAStream(Vec<GroupView>),
    /// Scan finished normally. Wraps the full [`ScanResult`] so the
    /// sidebar can be refreshed from the cache rows written afterward.
    Completed(ScanResult),
    /// Scanner returned [`ScanError::Cancelled`] in response to the
    /// shared cancel flag.
    Cancelled,
}

/// Scan-thread → GUI-thread handoff channel.
///
/// The worker thread may send one [`ScanEvent::TierAStream`] (optional)
/// followed by exactly one terminator ([`ScanEvent::Completed`] or
/// [`ScanEvent::Cancelled`]).
type ScanEventRx = Arc<Mutex<Option<mpsc::Receiver<ScanEvent>>>>;

/// How often the GUI polls the shared progress counters while a scan is
/// running. The PRD / issue #21 calls for "~250 ms update cadence" — we
/// pick exactly 250 ms so `cargo run` and manual QA see the same number.
const PROGRESS_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// How long the post-scan completion banner stays on screen before the
/// auto-dismiss timer returns state to [`ScanState::Idle`].
const BANNER_VISIBLE_FOR: Duration = Duration::from_secs(4);

/// GPUI view for the main window after a folder is opened.
///
/// Holds the application state directly — the root entity is registered
/// as a global (see [`register_root`]) so the menubar's `OpenFolder`
/// handler can reach back in and update it when the file picker returns.
pub struct ProjectView {
    pub state: AppState,
    /// Receiver half of the scan-thread → GUI-thread channel. `None` when
    /// no scan is in flight. Stored behind an `Arc<Mutex<_>>` so the
    /// 250 ms polling task can poll it without taking `&mut self`.
    scan_rx: ScanEventRx,
}

impl ProjectView {
    pub fn new() -> Self {
        Self {
            state: AppState::new(),
            scan_rx: Arc::new(Mutex::new(None)),
        }
    }

    /// Apply a freshly-loaded folder to the view and mark it dirty so the
    /// next render picks up the new sidebar rows.
    pub fn apply_folder(&mut self, folder: &Path, cx: &mut Context<Self>) {
        let result = load_folder(folder);
        self.state.set_folder_result(result);
        cx.notify();
    }

    fn select_group(&mut self, id: i64, cx: &mut Context<Self>) {
        self.state.selected_group = Some(id);
        cx.notify();
    }

    fn toggle_dismissed(&mut self, cx: &mut Context<Self>) {
        self.state.dismissed_expanded = !self.state.dismissed_expanded;
        cx.notify();
    }

    /// Kick off a full-pipeline scan of `self.state.current_folder` on a
    /// background thread, then start a 250 ms polling task that pushes
    /// fresh progress into the sidebar and, on completion, refreshes it
    /// from the newly-written cache.
    ///
    /// No-op when there's no current folder, or when a scan is already
    /// running (defensive — the UI disables the button in that case).
    ///
    /// Issue #21 wiring; cancel + Tier A streaming added in #22.
    pub fn start_scan(&mut self, cx: &mut Context<Self>) {
        let Some(folder) = self.state.current_folder.clone() else {
            return;
        };
        let Some(handles) = self.state.begin_scan() else {
            // Already running — ignore the re-entry.
            return;
        };

        // Spawn the worker thread. Clone the `Arc`s so it owns them for
        // the life of the scan; the `ProgressSink` impl on
        // `AtomicProgressSink` is `Send + Sync`.
        let (tx, rx) = mpsc::channel::<ScanEvent>();
        *self.scan_rx.lock().unwrap() = Some(rx);

        let worker_folder = folder.clone();
        let worker_progress = handles.progress.clone();
        let worker_cancel = handles.cancel.clone();
        let worker_tx = tx.clone();
        thread::spawn(move || {
            // Build a ScanConfig that matches the CLI's defaults: honour
            // any `.dedup/config.toml` the user has in place, then pin the
            // cache root so warm scans + persistence work.
            let config = Config::load(Some(&worker_folder)).unwrap_or_default();
            let mut scan_cfg = ScanConfig::from(&config);
            scan_cfg.cache_root = Some(worker_folder.clone());
            scan_cfg.cancel = Some(worker_cancel.clone());

            // Wire the Tier A streaming callback to the GUI channel.
            // The callback fires exactly once on the worker thread
            // (after bucket-fill, before Tier B) and forwards the
            // finalized Tier A set as `GroupView` rows ready for the
            // sidebar's impact-sorted merge.
            let stream_tx = worker_tx.clone();
            let cb: TierAStreamCallback = std::sync::Arc::new(move |groups: &[MatchGroup]| {
                let views: Vec<GroupView> = groups
                    .iter()
                    .enumerate()
                    .map(|(i, g)| group_view_from_match(g, i))
                    .collect();
                // Best-effort — if the poll loop has already dropped
                // the rx we just swallow the send.
                let _ = stream_tx.send(ScanEvent::TierAStream(views));
            });
            scan_cfg.on_tier_a_groups = Some(cb);

            let scanner = Scanner::new(scan_cfg);
            let result = match scanner.scan_with_progress(&worker_folder, &worker_progress) {
                Ok(r) => r,
                Err(ScanError::Cancelled) => {
                    log::info!("dedup-gui: scan cancelled for {}", worker_folder.display());
                    let _ = worker_tx.send(ScanEvent::Cancelled);
                    return;
                }
                Err(e) => {
                    log::warn!(
                        "dedup-gui: scan failed for {}: {e}",
                        worker_folder.display()
                    );
                    // Drop `tx`; the polling task sees a disconnected
                    // channel and transitions state back to Idle. Richer
                    // error UX lands in #30.
                    return;
                }
            };

            // Persist before signaling completion so the main thread's
            // `load_folder` call sees fully-written cache rows. Best-
            // effort: a cache write failure does not abort the scan —
            // the user still gets an in-memory result but the sidebar
            // will show the old contents. TODO(#30): surface this.
            match Cache::open(&worker_folder) {
                Ok(mut c) => {
                    if let Err(e) = c.write_scan_result(&result) {
                        log::warn!(
                            "dedup-gui: failed to persist scan result for {}: {e}",
                            worker_folder.display()
                        );
                    }
                }
                Err(e) => {
                    log::warn!(
                        "dedup-gui: failed to open cache for {}: {e}",
                        worker_folder.display()
                    );
                }
            }

            // Send is best-effort — if the GUI already swapped the rx
            // out (e.g. user closed the window) we drop the result.
            let _ = worker_tx.send(ScanEvent::Completed(result));
        });
        drop(tx);

        // Polling loop on the foreground (per-entity) executor. `cx.spawn`
        // gives us an `AsyncApp` we can use to `update` the entity; the
        // `BackgroundExecutor::timer` keeps the cadence.
        let rx_handle = self.scan_rx.clone();
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(PROGRESS_POLL_INTERVAL).await;

                // Drain everything currently on the channel in one go.
                // The 250 ms cadence means we expect at most a handful
                // of events per tick (1 streaming pulse + 1 terminator
                // in the common case).
                let events = drain_channel(&rx_handle);

                let mut terminated = false;
                for event in events {
                    match event {
                        ScanEvent::TierAStream(groups) => {
                            let update = this.update(cx, |view, cx| {
                                view.state.merge_streaming_groups(groups);
                                cx.notify();
                            });
                            if update.is_err() {
                                return;
                            }
                        }
                        ScanEvent::Completed(scan_result) => {
                            let update = this.update(cx, |view, cx| {
                                apply_scan_result(view, scan_result, cx);
                            });
                            if update.is_err() {
                                return;
                            }
                            spawn_banner_dismiss(this.clone(), cx).await;
                            terminated = true;
                            break;
                        }
                        ScanEvent::Cancelled => {
                            let _ = this.update(cx, |view, cx| {
                                view.state.cancel_completed();
                                cx.notify();
                            });
                            terminated = true;
                            break;
                        }
                    }
                }

                if terminated {
                    return;
                }

                // Channel dropped without a terminator → worker
                // panicked. Clear the active state so buttons re-enable.
                if rx_handle.lock().unwrap().is_none() {
                    let _ = this.update(cx, |view, cx| {
                        if view.state.scan_state.is_active() {
                            view.state.cancel_completed();
                            cx.notify();
                        }
                    });
                    return;
                }

                // Nothing to apply beyond a repaint of the progress bar.
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    return;
                }
            }
        })
        .detach();
    }

    /// Request cooperative cancel of the in-flight scan. Flips the
    /// shared cancel flag (the scanner checks it between files) and
    /// transitions state to [`ScanState::Cancelling`]. The poll loop
    /// turns the subsequent `ScanEvent::Cancelled` into the
    /// Idle transition + sidebar restore.
    pub fn request_cancel(&mut self, cx: &mut Context<Self>) {
        if !self.state.scan_state.is_running() {
            return;
        }
        self.state.request_cancel();
        cx.notify();
    }
}

/// Drain every currently-buffered [`ScanEvent`] off the channel. On
/// disconnect the receiver is dropped (set to `None`) so the poll loop
/// can notice and tear down cleanly. Factored out of the poll loop so
/// the lock scope is tight and doesn't straddle `.await` points.
fn drain_channel(rx: &ScanEventRx) -> Vec<ScanEvent> {
    let mut out = Vec::new();
    let mut guard = rx.lock().unwrap();
    let disconnected = loop {
        let Some(receiver) = guard.as_ref() else {
            break true;
        };
        match receiver.try_recv() {
            Ok(e) => out.push(e),
            Err(mpsc::TryRecvError::Empty) => break false,
            Err(mpsc::TryRecvError::Disconnected) => break true,
        }
    };
    if disconnected {
        *guard = None;
    }
    out
}

/// Fold a completed [`ScanResult`] into the view: transition state to
/// `Completed`, re-load the sidebar from the freshly-written cache, and
/// notify the render path.
///
/// Extracted from [`ProjectView::start_scan`] to keep the async closure
/// small — GPUI's `cx.spawn` requires the future to be `'static` so
/// inlining closures that capture `&mut Self` gets awkward.
fn apply_scan_result(view: &mut ProjectView, result: ScanResult, cx: &mut Context<ProjectView>) {
    let duration = match &view.state.scan_state {
        ScanState::Running { started_at, .. } => started_at.elapsed(),
        // If the state changed out from under us (shouldn't happen, but
        // belt-and-braces) use zero rather than panicking.
        _ => Duration::from_secs(0),
    };
    let file_count = result.files_scanned;
    let group_count = result.groups.len();

    view.state.finish_scan(group_count, file_count, duration);

    // Reload the sidebar from the freshly-written cache so the GUI and
    // CLI show identical data. The in-memory `ScanResult` is still
    // available via the channel, but re-reading keeps one code path for
    // "render a folder".
    if let Some(folder) = view.state.current_folder.clone() {
        let loaded = load_folder(&folder);
        view.state.set_folder_result(loaded);
    }
    cx.notify();
}

/// Schedule the post-scan banner's auto-dismiss.
///
/// Separated so the polling loop's future stays readable. Awaits the
/// 4-second timer then flips the completion state to [`ScanState::Idle`]
/// if it hasn't already been replaced (e.g. a new scan started in the
/// meantime — in which case `dismiss_completion` is a no-op).
async fn spawn_banner_dismiss(this: gpui::WeakEntity<ProjectView>, cx: &mut gpui::AsyncApp) {
    cx.background_executor().timer(BANNER_VISIBLE_FOR).await;
    let _ = this.update(cx, |view, cx| {
        view.state.dismiss_completion();
        cx.notify();
    });
}

impl Default for ProjectView {
    fn default() -> Self {
        Self::new()
    }
}

// -----------------------------------------------------------------------
// Global — shared handle so action handlers can reach the root view.
// -----------------------------------------------------------------------
// `cx.on_action` registers an app-global handler; to actually *update*
// the main view from inside that handler we need a handle to the view's
// `Entity`. Stashing it in a GPUI `Global` is the idiomatic way — see
// gpui/src/global.rs. We wrap `Entity<ProjectView>` in a newtype so it's
// impossible to accidentally pull out by `cx.global::<Entity<_>>()`.

#[derive(Clone)]
pub struct RootHandle(pub gpui::Entity<ProjectView>);

impl gpui::Global for RootHandle {}

/// Store `entity` as the app-level root-view global and install the
/// `OpenFolder` handler that routes the file picker back into it.
///
/// Called once on app startup, after the first window is opened.
pub fn register_root(entity: gpui::Entity<ProjectView>, cx: &mut gpui::App) {
    cx.set_global(RootHandle(entity));
    cx.on_action(|_: &OpenFolder, cx: &mut gpui::App| {
        // Synchronous `NSOpenPanel`. GPUI guarantees action handlers run
        // on the main thread, which is where Cocoa requires modal file
        // dialogs to live, so this is safe.
        let picked: Option<PathBuf> = rfd::FileDialog::new()
            .set_title("Open folder")
            .pick_folder();
        let Some(folder) = picked else {
            // User hit Cancel — leave state untouched. This is the
            // explicit "open but do nothing" branch required by issue
            // #20 AC ("user-invoked only — no silent scans").
            return;
        };
        let Some(RootHandle(entity)) = cx.try_global::<RootHandle>().cloned() else {
            return;
        };
        entity.update(cx, |view, cx| {
            view.apply_folder(&folder, cx);
        });
    });

    // `StartScan` (⌘R / Scan → Start Scan) — routed through the root
    // view so the menubar shortcut + the sidebar button do the same
    // thing. `start_scan` is a no-op when no folder is open or a scan
    // is already running, so repeated fires from both surfaces are
    // safe.
    cx.on_action(|_: &StartScan, cx: &mut gpui::App| {
        let Some(RootHandle(entity)) = cx.try_global::<RootHandle>().cloned() else {
            return;
        };
        entity.update(cx, |view, cx| {
            view.start_scan(cx);
        });
    });

    // `StopScan` (⌘. / Scan → Stop Scan) — cooperative cancel for an
    // in-flight scan (issue #22). No-op when no scan is running so
    // repeated fires are safe.
    cx.on_action(|_: &StopScan, cx: &mut gpui::App| {
        let Some(RootHandle(entity)) = cx.try_global::<RootHandle>().cloned() else {
            return;
        };
        entity.update(cx, |view, cx| {
            view.request_cancel(cx);
        });
    });
}

// -----------------------------------------------------------------------
// Render
// -----------------------------------------------------------------------

impl Render for ProjectView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        // Shell: horizontal split, sidebar on the left + detail on the
        // right. We render the no-folder / empty / no-duplicates / newer-
        // cache / error banners as full-window overlays over the shell so
        // the sidebar is only painted when a project is actually loaded.
        let body: gpui::AnyElement = match &self.state.status {
            AppStatus::NoFolderOpen => render_no_folder().into_any_element(),
            AppStatus::Empty => render_empty(&self.state).into_any_element(),
            AppStatus::NoDuplicates => render_no_duplicates(&self.state).into_any_element(),
            AppStatus::NewerCache { found, supported } => {
                render_newer_cache(*found, *supported).into_any_element()
            }
            AppStatus::Error(msg) => render_error(msg).into_any_element(),
            AppStatus::Loaded => render_loaded(&self.state).into_any_element(),
        };

        // Scan progress + completion banners float above the body so
        // every `AppStatus` that has a folder open (Empty, NoDuplicates,
        // Loaded, ...) gets the same feedback.
        // TODO(#30): replace with the real toast system.
        let overlay = render_scan_overlay(&self.state);

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(BG))
            .text_color(white())
            .children(overlay)
            .child(body)
    }
}

// ---------------------------------------------------------------------------
// Status-state renderers
// ---------------------------------------------------------------------------

fn centered_panel() -> gpui::Div {
    div()
        .size_full()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
}

fn render_no_folder() -> gpui::Div {
    // The pre-open empty-state previously owned this message — we
    // duplicate it here so ProjectView can render stand-alone without
    // stacking two views. The button dispatches the same OpenFolder
    // action the menubar's File → Open… item fires.
    centered_panel().child(
        div()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(16.0))
            .p(px(32.0))
            .child(div().text_size(px(22.0)).child("No project open"))
            .child(
                div()
                    .text_size(px(13.0))
                    .text_color(rgb(ROW_TEXT_DIM))
                    .child("Open a folder to scan for duplicate code."),
            )
            .child(
                div()
                    .mt(px(12.0))
                    .px(px(20.0))
                    .py(px(10.0))
                    .bg(rgb(ACCENT))
                    .text_color(white())
                    .text_size(px(14.0))
                    .rounded(px(6.0))
                    .border_1()
                    .border_color(black())
                    .cursor_pointer()
                    .on_mouse_down(MouseButton::Left, |_, window, cx| {
                        window.dispatch_action(Box::new(OpenFolder), cx);
                    })
                    .child("Open Folder…"),
            ),
    )
}

fn render_empty(state: &AppState) -> gpui::Div {
    // AC #5: "Empty-directory state: 'No source files found — check your
    // .dedupignore and filters.'". We show this when the user opens a
    // folder that has no `.dedup/cache.sqlite` at all — i.e. the folder
    // has never been scanned (or the scan produced nothing worth caching).
    centered_panel().child(
        div()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(12.0))
            .p(px(32.0))
            .child(div().text_size(px(20.0)).child("No source files found"))
            .child(
                div()
                    .text_size(px(13.0))
                    .text_color(rgb(ROW_TEXT_DIM))
                    .child("Check your .dedupignore and filters."),
            )
            .child(render_folder_footer(state)),
    )
}

fn render_no_duplicates(state: &AppState) -> gpui::Div {
    // AC #6: "Single-file / no-match state: 'No duplicates found'."
    centered_panel().child(
        div()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(12.0))
            .p(px(32.0))
            .child(div().text_size(px(20.0)).child("No duplicates found"))
            .child(render_folder_footer(state)),
    )
}

fn render_newer_cache(found: u32, supported: u32) -> gpui::Div {
    // Lightweight banner — the full toast lands with issue #30. We keep
    // the message shape close to the CLI's `check_newer_schema` output so
    // both surfaces feel like one app.
    centered_panel().child(
        div()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(10.0))
            .p(px(32.0))
            .child(div().text_size(px(20.0)).child("Cache is from a newer Dedup"))
            .child(
                div()
                    .text_size(px(13.0))
                    .text_color(rgb(ROW_TEXT_DIM))
                    .child(format!(
                        "Cache schema v{found} > supported v{supported}. Rescan with this build to continue."
                    )),
            ),
    )
}

fn render_error(msg: &str) -> gpui::Div {
    centered_panel().child(
        div()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(10.0))
            .p(px(32.0))
            .child(div().text_size(px(20.0)).child("Could not open cache"))
            .child(
                div()
                    .text_size(px(13.0))
                    .text_color(rgb(ROW_TEXT_DIM))
                    .child(msg.to_string()),
            ),
    )
}

fn render_folder_footer(state: &AppState) -> gpui::Div {
    match &state.current_folder {
        Some(p) => div()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(10.0))
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(rgb(ROW_TEXT_DIM))
                    .child(format!("Folder: {}", p.display())),
            )
            .child(render_scan_button(state)),
        None => div(),
    }
}

/// Big pill-style "Scan" button. Rendered in the sidebar header (loaded
/// state) and on the Empty / NoDuplicates empty-state panels so the user
/// can kick off a scan regardless of cache state.
///
/// While a scan is in flight the button flips to a "Cancel" button that
/// dispatches [`StopScan`] (issue #22). In Cancelling state the button
/// dims to indicate the cancel has been acknowledged — further clicks
/// are no-ops.
fn render_scan_button(state: &AppState) -> gpui::Div {
    let has_folder = state.current_folder.is_some();
    let base = div()
        .px(px(20.0))
        .py(px(8.0))
        .rounded(px(6.0))
        .text_size(px(13.0))
        .text_color(white());

    match &state.scan_state {
        ScanState::Running { .. } => {
            // Active scan — button doubles as Cancel.
            base.child("Cancel")
                .bg(rgb(ACCENT))
                .cursor_pointer()
                .on_mouse_down(MouseButton::Left, |_, window, cx| {
                    window.dispatch_action(Box::new(StopScan), cx);
                })
        }
        ScanState::Cancelling { .. } => base
            .child("Cancelling\u{2026}")
            .bg(rgb(ACCENT_DIM))
            .text_color(rgb(ROW_TEXT_DIM)),
        _ if !has_folder => base
            .child("Scan")
            .bg(rgb(ACCENT_DIM))
            .text_color(rgb(ROW_TEXT_DIM)),
        _ => base
            .child("Scan")
            .bg(rgb(ACCENT))
            .cursor_pointer()
            .on_mouse_down(MouseButton::Left, |_, window, cx| {
                window.dispatch_action(Box::new(StartScan), cx);
            }),
    }
}

// ---------------------------------------------------------------------------
// Loaded view — sidebar + detail
// ---------------------------------------------------------------------------

fn render_loaded(state: &AppState) -> gpui::Div {
    div()
        .size_full()
        .flex()
        .flex_row()
        .child(render_sidebar(state))
        .child(render_detail(state))
}

fn render_sidebar(state: &AppState) -> gpui::Div {
    // While a scan is running, render Tier A groups from the streaming
    // buffer (final-membership pulses, sorted by Impact). Tier B rows
    // are only known after Tier B completes, so we keep the existing
    // cache-backed Tier B section visible — it will be replaced by
    // `apply_scan_result` when the scan finishes. Cancelling + Idle use
    // the cache-backed view.
    let (tier_a, tier_b): (Vec<&GroupView>, Vec<&GroupView>) =
        if matches!(state.scan_state, ScanState::Running { .. }) {
            (
                state.groups_streaming.iter().collect(),
                state.tier_b_groups().collect(),
            )
        } else {
            (
                state.tier_a_groups().collect(),
                state.tier_b_groups().collect(),
            )
        };

    div()
        .w(px(320.0))
        .h_full()
        .bg(rgb(SIDEBAR_BG))
        .border_r_1()
        .border_color(black())
        .flex()
        .flex_col()
        .overflow_hidden()
        // Header — folder name + Scan button
        .child(
            div()
                .p(px(12.0))
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap(px(8.0))
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(rgb(ROW_TEXT_DIM))
                        .child(match &state.current_folder {
                            Some(p) => p
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_else(|| p.display().to_string()),
                            None => String::new(),
                        }),
                )
                .child(render_scan_button(state)),
        )
        // Section 1: Tier B — "Duplicated functions / classes"
        .child(render_section_header(
            "Duplicated functions / classes",
            tier_b.len(),
        ))
        .children(
            tier_b
                .iter()
                .map(|g| render_group_row(g, state.selected_group == Some(g.id))),
        )
        // Section 2: Tier A — "Duplicated blocks"
        .child(render_section_header("Duplicated blocks", tier_a.len()))
        .children(
            tier_a
                .iter()
                .map(|g| render_group_row(g, state.selected_group == Some(g.id))),
        )
        // Section 3: Dismissed (collapsed by default).
        .child(render_dismissed_section(state))
}

fn render_section_header(title: &'static str, count: usize) -> gpui::Div {
    div()
        .px(px(12.0))
        .py(px(6.0))
        .mt(px(4.0))
        .text_size(px(11.0))
        .text_color(rgb(SECTION_HEADER))
        .child(format!("{title} ({count})"))
}

fn render_group_row(group: &GroupView, selected: bool) -> gpui::Div {
    let id = group.id;
    let label = group.label.clone();
    let row = div()
        .px(px(16.0))
        .py(px(4.0))
        .text_size(px(12.0))
        .text_color(rgb(ROW_TEXT))
        .cursor_pointer()
        .child(label)
        .on_mouse_down(MouseButton::Left, move |_, _window, cx: &mut gpui::App| {
            // Route through the root handle so any other listeners
            // (future: toolbar, keyboard focus) see the same update.
            if let Some(RootHandle(entity)) = cx.try_global::<RootHandle>().cloned() {
                entity.update(cx, |view, cx| view.select_group(id, cx));
            }
        });
    if selected {
        row.bg(rgb(ROW_SELECTED_BG))
    } else {
        row
    }
}

fn render_dismissed_section(state: &AppState) -> gpui::Div {
    let count = state.dismissed.len();
    let expanded = state.dismissed_expanded;
    let arrow = if expanded { "\u{25BC}" } else { "\u{25B6}" };

    let header = div()
        .px(px(12.0))
        .py(px(6.0))
        .mt(px(4.0))
        .text_size(px(11.0))
        .text_color(rgb(SECTION_HEADER))
        .cursor_pointer()
        .child(format!("{arrow} Dismissed ({count})"))
        .on_mouse_down(MouseButton::Left, move |_, _window, cx: &mut gpui::App| {
            if let Some(RootHandle(entity)) = cx.try_global::<RootHandle>().cloned() {
                entity.update(cx, |view, cx| view.toggle_dismissed(cx));
            }
        });

    let mut wrap = div().flex().flex_col().child(header);
    if expanded {
        for s in &state.dismissed {
            wrap = wrap.child(render_dismissed_row(s));
        }
    }
    wrap
}

fn render_dismissed_row(s: &SuppressionView) -> gpui::Div {
    div()
        .px(px(16.0))
        .py(px(4.0))
        .text_size(px(11.0))
        .text_color(rgb(ROW_TEXT_DIM))
        .child(s.label())
}

/// Scan-progress + completion banner overlay.
///
/// Returns an `Option<Div>` so the render tree stays zero-allocation when
/// the scan is idle and no folder is open. For everything else we emit a
/// thin banner strip at the top of the window:
///
/// - `ScanState::Running` — a progress bar with "Scanning… N files · M
///   matches · 1.2s".
/// - `ScanState::Completed` — the acceptance-criterion completion
///   string ("Scan complete — … in Ns.").
///
/// The banner is intentionally simple inline copy rather than a real
/// toast; the proper toast system lands with the error-UX pass in #30.
// TODO(#30): replace with the real toast system.
fn render_scan_overlay(state: &AppState) -> Option<gpui::Div> {
    match &state.scan_state {
        ScanState::Idle => None,
        ScanState::Running {
            started_at,
            progress,
            ..
        } => Some(render_progress_banner(*started_at, progress)),
        ScanState::Cancelling { started_at } => Some(render_cancelling_banner(*started_at)),
        ScanState::Completed {
            group_count,
            file_count,
            duration,
        } => Some(render_completion_banner(
            *group_count,
            *file_count,
            *duration,
        )),
    }
}

fn render_cancelling_banner(started_at: Instant) -> gpui::Div {
    // Mirror of `render_progress_banner` styling so the banner shape
    // doesn't jump — only the copy changes. The elapsed counter shows
    // "time since the user clicked Cancel" so a slow cancel is visible.
    let elapsed = format_elapsed(started_at.elapsed());
    div()
        .w_full()
        .bg(rgb(ACCENT_DIM))
        .px(px(12.0))
        .py(px(8.0))
        .flex()
        .flex_col()
        .gap(px(4.0))
        .border_b_1()
        .border_color(black())
        .child(
            div()
                .text_size(px(12.0))
                .text_color(rgb(ROW_TEXT))
                .child(format!("Cancelling\u{2026} ({elapsed})")),
        )
}

fn render_progress_banner(
    started_at: Instant,
    progress: &dedup_core::AtomicProgressSink,
) -> gpui::Div {
    let files = progress.files_scanned();
    let matches = progress.matches();
    let elapsed = format_elapsed(started_at.elapsed());

    // Indeterminate progress: we don't know the total file count yet
    // (streaming / totals come with #22), so show a solid bar that
    // represents "work underway" rather than a fake fraction.
    div()
        .w_full()
        .bg(rgb(ACCENT_DIM))
        .px(px(12.0))
        .py(px(8.0))
        .flex()
        .flex_col()
        .gap(px(4.0))
        .border_b_1()
        .border_color(black())
        .child(
            div()
                .text_size(px(12.0))
                .text_color(rgb(ROW_TEXT))
                .child(format!(
                    "Scanning\u{2026} {files} files \u{00B7} {matches} matches \u{00B7} {elapsed}"
                )),
        )
        .child(
            // Tiny indeterminate bar — fixed 40 % filled to convey
            // "in-progress" without implying a known total.
            div()
                .w_full()
                .h(px(3.0))
                .bg(rgb(PROGRESS_BAR_BG))
                .rounded(px(2.0))
                .child(
                    div()
                        .w(px(120.0))
                        .h(px(3.0))
                        .bg(rgb(PROGRESS_BAR_FG))
                        .rounded(px(2.0)),
                ),
        )
}

fn render_completion_banner(
    group_count: usize,
    file_count: usize,
    duration: Duration,
) -> gpui::Div {
    div()
        .w_full()
        .bg(rgb(BANNER_BG))
        .px(px(12.0))
        .py(px(8.0))
        .border_b_1()
        .border_color(black())
        .child(
            div()
                .text_size(px(12.0))
                .text_color(rgb(BANNER_TEXT))
                .child(format_completion_banner(group_count, file_count, duration)),
        )
}

fn render_detail(state: &AppState) -> gpui::Div {
    let occurrences = state.selected_occurrences();
    let mut body = div().flex().flex_col().size_full().p(px(16.0)).gap(px(8.0));
    if occurrences.is_empty() {
        body = body.child(
            div()
                .text_size(px(13.0))
                .text_color(rgb(ROW_TEXT_DIM))
                .child("Select a duplicate group to see its occurrences."),
        );
    } else {
        body = body.child(
            div()
                .text_size(px(12.0))
                .text_color(rgb(ROW_TEXT_DIM))
                .child(format!("{} occurrences", occurrences.len())),
        );
        for occ in occurrences {
            body = body.child(
                div()
                    .px(px(12.0))
                    .py(px(8.0))
                    .bg(rgb(SIDEBAR_BG))
                    .rounded(px(4.0))
                    .text_size(px(12.0))
                    .text_color(rgb(ROW_TEXT))
                    .child(occ.label()),
            );
        }
        // NOTE: no syntax highlighting / source preview — that lands in
        // issue #24. The acceptance criteria for #20 explicitly say
        // "path + line range", nothing more.
    }
    body.bg(rgb(BG)).flex_1()
}
