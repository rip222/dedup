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
use std::rc::Rc;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use gpui::{
    ClipboardItem, Context, MouseButton, Window, black, div, prelude::*, px, rgb, uniform_list,
    white,
};

use crate::app_state::{
    AppState, AppStatus, GroupView, OccurrenceView, Pane, ScanState, SortKey, SuppressionView,
    format_completion_banner, format_elapsed, group_view_from_match, load_folder, open_in_editor,
};
use crate::menubar::{
    ActivateGroup, CollapseAll, DismissCurrentGroup, ExpandAll, FindInSidebar, FocusDetail,
    FocusSidebar, NextGroup, OpenFolder, OpenSelectedInEditor, PrevGroup, StartScan, StopScan,
};
use dedup_core::{
    Cache, Config, MatchGroup, ScanConfig, ScanError, ScanResult, Scanner, Tier,
    TierAStreamCallback,
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

    // -----------------------------------------------------------------
    // Issue #23 — sidebar sort / filter / search / keyboard nav.
    //
    // These thin wrappers forward into the pure `AppState` methods
    // (defined in `crate::app_state`) so the action handlers installed
    // in `register_root` stay small.
    // -----------------------------------------------------------------

    pub fn focus_sidebar(&mut self, cx: &mut Context<Self>) {
        self.state.focus_pane(Pane::Sidebar);
        cx.notify();
    }

    pub fn focus_detail(&mut self, cx: &mut Context<Self>) {
        self.state.focus_pane(Pane::Detail);
        cx.notify();
    }

    /// ⌘F handler — flip focus to the sidebar + mark the search box as
    /// the intended input target. The actual text-entry hookup is out
    /// of scope for the issue (search_query is updated via the input
    /// element's on-change callback), but flipping focus is enough to
    /// satisfy the acceptance criterion "⌘F focuses the search box".
    pub fn find_in_sidebar(&mut self, cx: &mut Context<Self>) {
        self.state.focus_pane(Pane::Sidebar);
        cx.notify();
    }

    pub fn next_group(&mut self, cx: &mut Context<Self>) {
        // j/k are global key bindings — only act when the sidebar is
        // logically focused so key events in e.g. a detail-pane text
        // input don't get swallowed. ⌘1/⌘2 control the flag.
        if self.state.focused_pane != Pane::Sidebar {
            return;
        }
        self.state.next_group();
        cx.notify();
    }

    pub fn prev_group(&mut self, cx: &mut Context<Self>) {
        if self.state.focused_pane != Pane::Sidebar {
            return;
        }
        self.state.prev_group();
        cx.notify();
    }

    pub fn activate_group(&mut self, cx: &mut Context<Self>) {
        if self.state.focused_pane != Pane::Sidebar {
            return;
        }
        self.state.activate_group();
        cx.notify();
    }

    /// `x` handler — dismiss the currently-selected group. Writes to
    /// the cache's `suppressions` table and updates local state so the
    /// row disappears immediately.
    pub fn dismiss_current_group(&mut self, cx: &mut Context<Self>) {
        if self.state.focused_pane != Pane::Sidebar {
            return;
        }
        let Some((hash, group_id)) = self.state.dismiss_current_group() else {
            return;
        };
        // Best-effort persist — a cache failure leaves the session
        // dismissal in place (better UX than silently doing nothing)
        // and logs the error. Richer error toast is #30.
        if let Some(folder) = self.state.current_folder.clone() {
            match Cache::open(&folder) {
                Ok(mut cache) => {
                    if let Err(e) = cache.dismiss_hash(hash, Some(group_id)) {
                        log::warn!(
                            "dedup-gui: failed to persist dismissal for group {group_id}: {e}"
                        );
                    }
                }
                Err(e) => log::warn!("dedup-gui: failed to open cache to persist dismissal: {e}"),
            }
        }
        cx.notify();
    }

    /// `o` handler — launch the editor for every occurrence in the
    /// currently-selected group. Calls the no-op placeholder today;
    /// issue #29 wires the real launcher.
    pub fn open_selected_in_editor(&mut self, _cx: &mut Context<Self>) {
        if self.state.focused_pane != Pane::Sidebar {
            return;
        }
        let occurrences = self.state.selected_occurrences();
        if occurrences.is_empty() {
            return;
        }
        let paths: Vec<&Path> = occurrences.iter().map(|o| o.path.as_path()).collect();
        // TODO(#29): wire editor launcher.
        open_in_editor(&paths);
    }

    /// Update the sidebar search query. Invoked from the search input's
    /// on-change callback.
    pub fn set_search_query(&mut self, q: String, cx: &mut Context<Self>) {
        self.state.set_search_query(q);
        cx.notify();
    }

    /// Update the sidebar sort key. Invoked from the sort-dropdown
    /// menu items.
    pub fn set_sort_key(&mut self, key: SortKey, cx: &mut Context<Self>) {
        self.state.set_sort_key(key);
        cx.notify();
    }

    // -----------------------------------------------------------------
    // Issue #27 — group toolbar + per-occurrence action handlers.
    //
    // Wrappers around `AppState` methods that also (a) persist to the
    // cache when the action has durable semantics (dismiss group / occ)
    // and (b) write to the system clipboard for copy actions. All are
    // best-effort on the I/O side: a cache/clipboard failure logs and
    // leaves the in-memory state applied so the user sees immediate
    // feedback regardless.
    // -----------------------------------------------------------------

    /// Toggle a single occurrence's checkbox.
    pub fn toggle_occurrence(&mut self, group_id: i64, occ_idx: usize, cx: &mut Context<Self>) {
        self.state.toggle_occurrence(group_id, occ_idx);
        cx.notify();
    }

    /// Dismiss the whole group via the toolbar's `[Dismiss group]`
    /// button. Ignores checkbox state per issue #27.
    pub fn dismiss_group_toolbar(&mut self, group_id: i64, cx: &mut Context<Self>) {
        let Some((hash, gid)) = self.state.dismiss_group(group_id) else {
            return;
        };
        if let Some(folder) = self.state.current_folder.clone() {
            match Cache::open(&folder) {
                Ok(mut cache) => {
                    if let Err(e) = cache.dismiss_hash(hash, Some(gid)) {
                        log::warn!("dedup-gui: failed to persist dismissal for group {gid}: {e}");
                    }
                }
                Err(e) => {
                    log::warn!("dedup-gui: failed to open cache to persist group dismissal: {e}")
                }
            }
        }
        cx.notify();
    }

    /// Dismiss a single occurrence via the per-row `[×]` button.
    pub fn dismiss_occurrence(&mut self, group_id: i64, occ_idx: usize, cx: &mut Context<Self>) {
        let Some((hash, path)) = self.state.dismiss_occurrence(group_id, occ_idx) else {
            return;
        };
        if let Some(folder) = self.state.current_folder.clone() {
            match Cache::open(&folder) {
                Ok(mut cache) => {
                    if let Err(e) = cache.dismiss_occurrence(hash, &path) {
                        log::warn!(
                            "dedup-gui: failed to persist occurrence dismissal for {}: {e}",
                            path.display()
                        );
                    }
                }
                Err(e) => log::warn!(
                    "dedup-gui: failed to open cache to persist occurrence dismissal: {e}"
                ),
            }
        }
        cx.notify();
    }

    /// Toolbar "Copy paths" — writes the checked paths (or all visible
    /// paths when nothing is checked) as a newline-separated string to
    /// the system clipboard.
    pub fn copy_paths_for_group(
        &mut self,
        group_id: i64,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let paths = self.state.copy_paths_for_group(group_id);
        if paths.is_empty() {
            return;
        }
        let text = paths
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join("\n");
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    /// Per-row "Copy path" — single path → clipboard.
    pub fn copy_single_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(path.display().to_string()));
    }

    /// Toolbar "Open in editor" — respect checkboxes, fall back to
    /// every visible path when none are checked. Real launcher lands
    /// in #29; today we call the `open_in_editor` placeholder.
    pub fn open_group_in_editor(&mut self, group_id: i64, _cx: &mut Context<Self>) {
        let paths = self.state.copy_paths_for_group(group_id);
        if paths.is_empty() {
            return;
        }
        let refs: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
        // TODO(#29): wire editor launcher.
        open_in_editor(&refs);
    }

    /// Toolbar "Collapse all".
    pub fn collapse_all(&mut self, cx: &mut Context<Self>) {
        self.state.collapse_all();
        cx.notify();
    }

    /// Toolbar "Expand all".
    pub fn expand_all(&mut self, cx: &mut Context<Self>) {
        self.state.expand_all();
        cx.notify();
    }

    /// Toolbar `[×]` close — clear selection so the detail pane blanks.
    pub fn close_group_detail(&mut self, cx: &mut Context<Self>) {
        self.state.close_group_detail();
        cx.notify();
    }

    /// Per-group header toggle — collapse/expand a single group's
    /// detail body.
    pub fn toggle_group_collapse(&mut self, group_id: i64, cx: &mut Context<Self>) {
        self.state.toggle_collapse(group_id);
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

    // Issue #23 — sidebar focus, search, and keyboard-nav actions.
    // Every handler forwards into the matching `ProjectView::*`
    // wrapper, which in turn delegates to pure-data `AppState` methods.
    cx.on_action(|_: &FocusSidebar, cx: &mut gpui::App| {
        if let Some(RootHandle(entity)) = cx.try_global::<RootHandle>().cloned() {
            entity.update(cx, |view, cx| view.focus_sidebar(cx));
        }
    });
    cx.on_action(|_: &FocusDetail, cx: &mut gpui::App| {
        if let Some(RootHandle(entity)) = cx.try_global::<RootHandle>().cloned() {
            entity.update(cx, |view, cx| view.focus_detail(cx));
        }
    });
    cx.on_action(|_: &FindInSidebar, cx: &mut gpui::App| {
        if let Some(RootHandle(entity)) = cx.try_global::<RootHandle>().cloned() {
            entity.update(cx, |view, cx| view.find_in_sidebar(cx));
        }
    });
    cx.on_action(|_: &NextGroup, cx: &mut gpui::App| {
        if let Some(RootHandle(entity)) = cx.try_global::<RootHandle>().cloned() {
            entity.update(cx, |view, cx| view.next_group(cx));
        }
    });
    cx.on_action(|_: &PrevGroup, cx: &mut gpui::App| {
        if let Some(RootHandle(entity)) = cx.try_global::<RootHandle>().cloned() {
            entity.update(cx, |view, cx| view.prev_group(cx));
        }
    });
    cx.on_action(|_: &ActivateGroup, cx: &mut gpui::App| {
        if let Some(RootHandle(entity)) = cx.try_global::<RootHandle>().cloned() {
            entity.update(cx, |view, cx| view.activate_group(cx));
        }
    });
    cx.on_action(|_: &DismissCurrentGroup, cx: &mut gpui::App| {
        if let Some(RootHandle(entity)) = cx.try_global::<RootHandle>().cloned() {
            entity.update(cx, |view, cx| view.dismiss_current_group(cx));
        }
    });
    cx.on_action(|_: &OpenSelectedInEditor, cx: &mut gpui::App| {
        if let Some(RootHandle(entity)) = cx.try_global::<RootHandle>().cloned() {
            entity.update(cx, |view, cx| view.open_selected_in_editor(cx));
        }
    });

    // Issue #27 — toolbar "Collapse all" / "Expand all" actions. These
    // also have keyboard-shortcut potential but the issue only asks
    // for button wiring; the keybinding table stays unchanged.
    cx.on_action(|_: &CollapseAll, cx: &mut gpui::App| {
        if let Some(RootHandle(entity)) = cx.try_global::<RootHandle>().cloned() {
            entity.update(cx, |view, cx| view.collapse_all(cx));
        }
    });
    cx.on_action(|_: &ExpandAll, cx: &mut gpui::App| {
        if let Some(RootHandle(entity)) = cx.try_global::<RootHandle>().cloned() {
            entity.update(cx, |view, cx| view.expand_all(cx));
        }
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
    // Issue #23 — the sidebar renders the filtered + sorted list from
    // `AppState::visible_groups`. While a scan is running we still fall
    // back to the streaming buffer for Tier A (it arrives mid-scan,
    // before the cache reload), partitioned into the two tier sections
    // underneath the search / sort / summary row.
    let visible = state.visible_groups();
    let (tier_b, tier_a): (Vec<&GroupView>, Vec<&GroupView>) =
        if matches!(state.scan_state, ScanState::Running { .. }) {
            // Scan in flight — show cache-backed Tier B + streaming
            // Tier A so the user still sees Impact-sorted rows during
            // the scan. Search / sort do not re-apply to the streaming
            // buffer (that's a #22 concern); the final cache reload
            // brings them back into the `visible` path.
            (
                state.tier_b_groups().collect(),
                state.groups_streaming.iter().collect(),
            )
        } else {
            (
                visible.iter().filter(|g| g.tier == Tier::B).collect(),
                visible.iter().filter(|g| g.tier == Tier::A).collect(),
            )
        };

    let summary = state.summary();

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
        // Issue #23 — search / sort / summary row.
        .child(render_search_box(state))
        .child(render_sort_dropdown(state))
        .child(render_summary_header(&summary.format()))
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

/// Search input slot (issue #23).
///
/// We render a read-only label showing the current query plus a
/// placeholder "Search…" when empty. The real text-entry binding —
/// GPUI's input widget plumbing — is follow-up work; the slot exists
/// today so ⌘F has something visible to focus + the state field is
/// reachable. Unit tests exercise `set_search_query` directly.
fn render_search_box(state: &AppState) -> gpui::Div {
    let (text, dim) = if state.search_query.is_empty() {
        ("Search\u{2026}".to_string(), true)
    } else {
        (state.search_query.clone(), false)
    };
    let focused = state.focused_pane == Pane::Sidebar;
    let border_color = if focused {
        rgb(ACCENT)
    } else {
        rgb(ACCENT_DIM)
    };
    div()
        .mx(px(12.0))
        .px(px(8.0))
        .py(px(6.0))
        .bg(rgb(ACCENT_DIM))
        .rounded(px(4.0))
        .border_1()
        .border_color(border_color)
        .text_size(px(12.0))
        .text_color(if dim {
            rgb(ROW_TEXT_DIM)
        } else {
            rgb(ROW_TEXT)
        })
        .child(text)
}

/// Sort-dropdown slot (issue #23).
///
/// Shows "Sort: <current key>". Full menu UI lives behind a GPUI popup
/// we don't have a helper for yet; the state field is exposed so the
/// dropdown can be wired in when the popup primitive lands.
fn render_sort_dropdown(state: &AppState) -> gpui::Div {
    div()
        .mx(px(12.0))
        .mt(px(4.0))
        .px(px(8.0))
        .py(px(6.0))
        .bg(rgb(ACCENT_DIM))
        .rounded(px(4.0))
        .text_size(px(12.0))
        .text_color(rgb(ROW_TEXT))
        .child(format!("Sort: {}", state.sort_key.label()))
}

/// Summary header (issue #23). Renders
/// `"N groups · N functions · N blocks · N files · N duplicated lines"`
/// against the currently-filtered list.
fn render_summary_header(text: &str) -> gpui::Div {
    div()
        .px(px(12.0))
        .py(px(6.0))
        .mt(px(4.0))
        .text_size(px(11.0))
        .text_color(rgb(SECTION_HEADER))
        .child(text.to_string())
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

/// One flattened row of the detail pane's virtualized list (issue #26).
///
/// A group with `N` occurrences and `M` rendered lines per occurrence
/// flattens to `N * (M + spacing_rows) + header_rows` rows, all of
/// which get shoveled into [`gpui::uniform_list`]. Rows are a uniform
/// pixel height (see [`DETAIL_ROW_HEIGHT`]); the list lazy-renders only
/// the visible window, so a group with 100+ occurrences scrolls
/// smoothly even though the underlying vec may be tens of thousands of
/// rows long.
#[derive(Debug, Clone)]
enum DetailRow {
    /// The `{occurrences.len()} occurrences` preamble at the top of
    /// the pane.
    Summary(String),
    /// One per occurrence — `path:Lstart–end`.
    OccurrenceHeader(String),
    /// Blank row between consecutive occurrence cards, for visual
    /// separation in the flattened list.
    Gap,
    /// One rendered source line. Pre-tokenised into text segments
    /// (already split by highlight kind + tint overlay) so the
    /// per-frame render closure is a straight map instead of a
    /// tokeniser.
    CodeLine {
        /// Absolute 1-based file line number (shown in the gutter).
        line_number: u32,
        /// Whether this line is dimmed context or focus.
        is_context: bool,
        /// Pre-coloured segments for this line, in source order.
        segments: Vec<LineSegment>,
    },
    /// Placeholder when reading the source file failed.
    Unavailable,
}

/// One styled sub-string of a rendered code line.
///
/// `fg_color` is the highlight palette colour (from [`highlight`]);
/// `bg_color` is `Some(rgb)` when the byte range lies inside an
/// alpha-rename tint span (#25).
#[derive(Debug, Clone)]
struct LineSegment {
    text: String,
    fg_color: u32,
    bg_color: Option<u32>,
}

/// Fixed row height for the detail-pane `uniform_list`. `uniform_list`
/// measures the first item and reuses that height for every other item,
/// so headers, gaps, and code lines all render at the same height. 20
/// pixels at 12px font size gives ~8 px of breathing room without
/// making the list feel sparse.
const DETAIL_ROW_HEIGHT: f32 = 20.0;

/// Width of the gutter column. Six digits at 12 px Menlo monospace is
/// ~7 px per glyph; 48 px comfortably fits line numbers up to 999,999.
const DETAIL_GUTTER_WIDTH: f32 = 48.0;

/// Dimming applied to context lines — rendered as an alpha composite
/// of the palette colour with the background. The palette colours are
/// already middling-brightness so 60 % opacity is enough to clearly
/// distinguish context from focus without washing out to unreadable.
const CONTEXT_ALPHA: f32 = 0.55;

fn render_detail(state: &AppState) -> gpui::AnyElement {
    let occurrences = state.selected_occurrences();
    if occurrences.is_empty() {
        return div()
            .size_full()
            .p(px(16.0))
            .bg(rgb(BG))
            .flex_1()
            .child(
                div()
                    .text_size(px(13.0))
                    .text_color(rgb(ROW_TEXT_DIM))
                    .child("Select a duplicate group to see its occurrences."),
            )
            .into_any_element();
    }

    // Safe: `selected_occurrences()` is only non-empty when
    // `selected_group` is Some.
    let group_id = state.selected_group.unwrap_or(-1);
    let collapsed = state.is_group_collapsed(group_id);

    let rows: Vec<DetailRow> = if collapsed {
        // Collapsed — render only the toolbar + occurrence headers
        // (for the checkbox row shape). The `uniform_list` below still
        // receives a trivial rows list so the detail-pane layout is
        // unchanged; users re-expand to see the code.
        Vec::new()
    } else {
        build_detail_rows(state, &occurrences)
    };
    let rows = Rc::new(rows);
    let row_count = rows.len();
    let rows_for_render = rows.clone();
    let list = uniform_list("detail-rows", row_count, move |range, _window, _cx| {
        range
            .map(|idx| render_detail_row(&rows_for_render[idx]))
            .collect::<Vec<_>>()
    })
    .h_full()
    .flex_1();

    div()
        .size_full()
        .flex()
        .flex_col()
        .bg(rgb(BG))
        .flex_1()
        .child(render_group_toolbar(state, group_id))
        .child(render_occurrence_cards(state, group_id, &occurrences))
        .child(div().flex_1().px(px(16.0)).pb(px(16.0)).child(list))
        .into_any_element()
}

/// Colours specific to the issue #27 toolbar + per-occurrence cards.
const TOOLBAR_BG: u32 = 0x2a2a33;
const TOOLBAR_BUTTON_BG: u32 = 0x3b3b48;
const TOOLBAR_DANGER_BG: u32 = 0x5a2a2a;
const TOOLBAR_BUTTON_HOVER_BG: u32 = 0x4a4a58;
const CHECKBOX_CHECKED_BG: u32 = ACCENT;
const CHECKBOX_UNCHECKED_BG: u32 = 0x444452;

/// Build a single toolbar button (rounded, coloured rect with on-click).
fn toolbar_button(
    label: impl Into<String>,
    bg: u32,
    action: impl Fn(&mut gpui::App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let label = label.into();
    let id_key = label.clone();
    div()
        .id(("toolbar-btn", id_key.len() as u64))
        .px(px(10.0))
        .py(px(5.0))
        .bg(rgb(bg))
        .rounded(px(4.0))
        .text_size(px(12.0))
        .text_color(rgb(ROW_TEXT))
        .cursor_pointer()
        .hover(|s| s.bg(rgb(TOOLBAR_BUTTON_HOVER_BG)))
        .on_mouse_down(MouseButton::Left, move |_, _window, cx: &mut gpui::App| {
            action(cx)
        })
        .child(id_key)
}

/// Group-level toolbar (#27): info label + action buttons.
///
/// The buttons invoke `ProjectView` methods through the global
/// `RootHandle`. `[Dismiss group]` ignores checkboxes; `[Copy paths]`
/// and `[Open in editor]` respect them, falling back to every visible
/// path when the checkbox set is empty.
fn render_group_toolbar(state: &AppState, group_id: i64) -> gpui::Div {
    let (files, dup_lines) = state.group_toolbar_counts(group_id);

    let info_text = format!(
        "{files} file{fplural} \u{00B7} {lines} duplicated line{lplural}",
        fplural = if files == 1 { "" } else { "s" },
        lines = dup_lines,
        lplural = if dup_lines == 1 { "" } else { "s" },
    );

    let gid_open = group_id;
    let gid_dismiss = group_id;
    let gid_copy = group_id;

    // Buttons dispatch through RootHandle so any pane can observe the
    // resulting state change.
    let open_btn = toolbar_button("Open in editor", TOOLBAR_BUTTON_BG, move |cx| {
        if let Some(RootHandle(entity)) = cx.try_global::<RootHandle>().cloned() {
            entity.update(cx, |view, cx| view.open_group_in_editor(gid_open, cx));
        }
    });
    let dismiss_btn = toolbar_button("Dismiss group", TOOLBAR_DANGER_BG, move |cx| {
        if let Some(RootHandle(entity)) = cx.try_global::<RootHandle>().cloned() {
            entity.update(cx, |view, cx| view.dismiss_group_toolbar(gid_dismiss, cx));
        }
    });
    // Clipboard needs a Window reference; route it through a dedicated
    // mouse-down handler that has access to the window param.
    let copy_btn = div()
        .id(("toolbar-btn-copy", gid_copy as u64))
        .px(px(10.0))
        .py(px(5.0))
        .bg(rgb(TOOLBAR_BUTTON_BG))
        .rounded(px(4.0))
        .text_size(px(12.0))
        .text_color(rgb(ROW_TEXT))
        .cursor_pointer()
        .hover(|s| s.bg(rgb(TOOLBAR_BUTTON_HOVER_BG)))
        .on_mouse_down(
            MouseButton::Left,
            move |_, window: &mut Window, cx: &mut gpui::App| {
                if let Some(RootHandle(entity)) = cx.try_global::<RootHandle>().cloned() {
                    entity.update(cx, |view, cx| {
                        view.copy_paths_for_group(gid_copy, window, cx)
                    });
                }
            },
        )
        .child("Copy paths");
    let collapse_btn = toolbar_button("Collapse all", TOOLBAR_BUTTON_BG, move |cx| {
        if let Some(RootHandle(entity)) = cx.try_global::<RootHandle>().cloned() {
            entity.update(cx, |view, cx| view.collapse_all(cx));
        }
    });
    let expand_btn = toolbar_button("Expand all", TOOLBAR_BUTTON_BG, move |cx| {
        if let Some(RootHandle(entity)) = cx.try_global::<RootHandle>().cloned() {
            entity.update(cx, |view, cx| view.expand_all(cx));
        }
    });
    let close_btn = div()
        .id(("toolbar-close", group_id as u64))
        .w(px(22.0))
        .h(px(22.0))
        .flex()
        .items_center()
        .justify_center()
        .bg(rgb(TOOLBAR_BUTTON_BG))
        .rounded(px(4.0))
        .text_size(px(14.0))
        .text_color(rgb(ROW_TEXT))
        .cursor_pointer()
        .hover(|s| s.bg(rgb(TOOLBAR_BUTTON_HOVER_BG)))
        .on_mouse_down(MouseButton::Left, move |_, _window, cx: &mut gpui::App| {
            if let Some(RootHandle(entity)) = cx.try_global::<RootHandle>().cloned() {
                entity.update(cx, |view, cx| view.close_group_detail(cx));
            }
        })
        .child("\u{00D7}");

    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .px(px(16.0))
        .py(px(10.0))
        .bg(rgb(TOOLBAR_BG))
        .border_b_1()
        .border_color(black())
        .child(
            div()
                .flex_1()
                .text_size(px(12.0))
                .text_color(rgb(ROW_TEXT_DIM))
                .child(info_text),
        )
        .child(open_btn)
        .child(dismiss_btn)
        .child(copy_btn)
        .child(collapse_btn)
        .child(expand_btn)
        .child(close_btn)
}

/// Per-occurrence checkbox + path + hover-only `[Copy path]` + `[×]`
/// card rendered between the toolbar and the scrolling code body.
fn render_occurrence_cards(
    state: &AppState,
    group_id: i64,
    occurrences: &[OccurrenceView],
) -> gpui::Div {
    let mut wrap = div()
        .flex()
        .flex_col()
        .px(px(16.0))
        .py(px(8.0))
        .gap(px(4.0));
    for (idx, occ) in occurrences.iter().enumerate() {
        wrap = wrap.child(render_occurrence_card(group_id, idx, occ, state));
    }
    wrap
}

fn render_occurrence_card(
    group_id: i64,
    occ_idx: usize,
    occ: &OccurrenceView,
    state: &AppState,
) -> gpui::Div {
    let checked = state.is_occurrence_selected(group_id, occ_idx);
    let label = occ.label();
    let path_for_copy = occ.path.clone();
    let group_hover_key = format!("occ-card-{group_id}-{occ_idx}");

    let checkbox_bg = if checked {
        CHECKBOX_CHECKED_BG
    } else {
        CHECKBOX_UNCHECKED_BG
    };
    let check_mark = if checked { "\u{2713}" } else { " " };

    let checkbox = div()
        .id(("occ-checkbox", (group_id as u64) << 32 | occ_idx as u64))
        .w(px(16.0))
        .h(px(16.0))
        .flex()
        .items_center()
        .justify_center()
        .bg(rgb(checkbox_bg))
        .rounded(px(3.0))
        .text_size(px(11.0))
        .text_color(white())
        .cursor_pointer()
        .child(check_mark.to_string())
        .on_mouse_down(MouseButton::Left, move |_, _window, cx: &mut gpui::App| {
            if let Some(RootHandle(entity)) = cx.try_global::<RootHandle>().cloned() {
                entity.update(cx, |view, cx| view.toggle_occurrence(group_id, occ_idx, cx));
            }
        });

    // Per-row [×] — dismisses THIS occurrence without dismissing the
    // whole group (#27 AC).
    let dismiss = div()
        .id(("occ-dismiss", (group_id as u64) << 32 | occ_idx as u64))
        .w(px(18.0))
        .h(px(18.0))
        .flex()
        .items_center()
        .justify_center()
        .bg(rgb(TOOLBAR_DANGER_BG))
        .rounded(px(3.0))
        .text_size(px(12.0))
        .text_color(white())
        .cursor_pointer()
        .hover(|s| s.bg(rgb(TOOLBAR_BUTTON_HOVER_BG)))
        .child("\u{00D7}")
        .on_mouse_down(MouseButton::Left, move |_, _window, cx: &mut gpui::App| {
            if let Some(RootHandle(entity)) = cx.try_global::<RootHandle>().cloned() {
                entity.update(cx, |view, cx| {
                    view.dismiss_occurrence(group_id, occ_idx, cx)
                });
            }
        });

    // Per-row `[Copy path]` — hidden by default, revealed on row hover
    // via GPUI's `group_hover` mechanism. The wrapper div declares a
    // named hover group; the copy button is invisible until the group
    // is hovered, at which point it fades into full opacity.
    let copy_button = div()
        .id(("occ-copy", (group_id as u64) << 32 | occ_idx as u64))
        .px(px(8.0))
        .py(px(3.0))
        .bg(rgb(TOOLBAR_BUTTON_BG))
        .rounded(px(3.0))
        .text_size(px(11.0))
        .text_color(rgb(ROW_TEXT))
        .cursor_pointer()
        .invisible()
        .group_hover(group_hover_key.clone(), |s| s.visible())
        .child("Copy path")
        .on_mouse_down(MouseButton::Left, move |_, _window, cx: &mut gpui::App| {
            if let Some(RootHandle(entity)) = cx.try_global::<RootHandle>().cloned() {
                let p = path_for_copy.clone();
                entity.update(cx, |view, cx| view.copy_single_path(p, cx));
            }
        });

    div()
        .group(group_hover_key)
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .px(px(8.0))
        .py(px(5.0))
        .bg(rgb(SIDEBAR_BG))
        .rounded(px(4.0))
        .child(checkbox)
        .child(
            div()
                .flex_1()
                .text_size(px(12.0))
                .text_color(rgb(ROW_TEXT))
                .child(label),
        )
        .child(copy_button)
        .child(dismiss)
}

/// Render one flattened row from [`build_detail_rows`].
///
/// Returns a fixed-height `Div` — `uniform_list` assumes every row is
/// the same height, so all four variants use [`DETAIL_ROW_HEIGHT`].
fn render_detail_row(row: &DetailRow) -> gpui::Div {
    match row {
        DetailRow::Summary(text) => div()
            .h(px(DETAIL_ROW_HEIGHT))
            .text_size(px(12.0))
            .text_color(rgb(ROW_TEXT_DIM))
            .child(text.clone()),
        DetailRow::OccurrenceHeader(text) => div()
            .h(px(DETAIL_ROW_HEIGHT))
            .px(px(8.0))
            .bg(rgb(SIDEBAR_BG))
            .text_size(px(12.0))
            .text_color(rgb(ROW_TEXT))
            .child(text.clone()),
        DetailRow::Gap => div().h(px(DETAIL_ROW_HEIGHT)),
        DetailRow::Unavailable => div()
            .h(px(DETAIL_ROW_HEIGHT))
            .px(px(8.0))
            .text_size(px(11.0))
            .text_color(rgb(ROW_TEXT_DIM))
            .child("(file not available)"),
        DetailRow::CodeLine {
            line_number,
            is_context,
            segments,
        } => render_code_line(*line_number, *is_context, segments),
    }
}

/// Render one `[gutter][code]` row with horizontal overflow scrolling
/// on the code cell and no wrapping — AC: "Long lines scroll
/// horizontally; no wrap".
fn render_code_line(line_number: u32, is_context: bool, segments: &[LineSegment]) -> gpui::Div {
    let gutter = div()
        .w(px(DETAIL_GUTTER_WIDTH))
        .flex_none()
        .text_color(rgb(if is_context {
            dim(ROW_TEXT_DIM)
        } else {
            ROW_TEXT_DIM
        }))
        .child(format!("{line_number}"));

    let mut code = div()
        .id(("detail-code-line", line_number as u64))
        .flex_1()
        .overflow_x_scroll()
        .whitespace_nowrap();

    for seg in segments {
        let fg = if is_context {
            dim(seg.fg_color)
        } else {
            seg.fg_color
        };
        let mut node = div().text_color(rgb(fg)).child(seg.text.clone());
        if let Some(bg) = seg.bg_color {
            node = node.bg(rgb(bg));
        }
        code = code.child(node);
    }

    div()
        .h(px(DETAIL_ROW_HEIGHT))
        .flex()
        .flex_row()
        .gap(px(8.0))
        .text_size(px(12.0))
        .font_family("Menlo")
        .child(gutter)
        .child(code)
}

/// Multiply every RGB channel of `color` by [`CONTEXT_ALPHA`] against a
/// notional black background. Keeps the dimming purely compositional —
/// we don't need a real alpha channel, just a perceptibly quieter fg.
fn dim(color: u32) -> u32 {
    let r = ((color >> 16) & 0xff) as f32 * CONTEXT_ALPHA;
    let g = ((color >> 8) & 0xff) as f32 * CONTEXT_ALPHA;
    let b = (color & 0xff) as f32 * CONTEXT_ALPHA;
    ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)
}

/// Build the flat row list from the current occurrences. Pure helper —
/// the closure handed to `uniform_list` is `'static` so we pre-compute
/// everything here rather than recomputing per-frame.
fn build_detail_rows(
    state: &AppState,
    occurrences: &[crate::app_state::OccurrenceView],
) -> Vec<DetailRow> {
    let mut out = Vec::with_capacity(occurrences.len() * 8);
    out.push(DetailRow::Summary(format!(
        "{} occurrences",
        occurrences.len()
    )));

    let context_lines = state.detail_config.context_lines;
    for (i, occ) in occurrences.iter().enumerate() {
        if i > 0 {
            out.push(DetailRow::Gap);
        }
        out.push(DetailRow::OccurrenceHeader(occ.label()));
        match read_occurrence_source(state, occ) {
            Some((source, lang_hint)) => {
                let slice = crate::detail::extract_with_context(
                    &source,
                    occ.start_line.max(1) as u32,
                    occ.end_line.max(1) as u32,
                    context_lines,
                );
                append_slice_rows(&mut out, &source, lang_hint.as_deref(), &slice, occ);
            }
            None => out.push(DetailRow::Unavailable),
        }
    }

    out
}

/// Read the source file for an occurrence; returns `(source, lang_hint)`
/// or `None` on any I/O failure.
fn read_occurrence_source(
    state: &AppState,
    occ: &crate::app_state::OccurrenceView,
) -> Option<(String, Option<String>)> {
    let folder = state.current_folder.as_ref()?;
    let abs = folder.join(&occ.path);
    let source = std::fs::read_to_string(&abs).ok()?;
    let lang_hint = crate::highlight::lang_hint_for_path(&occ.path);
    Some((source, lang_hint))
}

/// Append one [`DetailRow::CodeLine`] per line in `slice`, pre-
/// tokenised with the same highlight + tint pipeline the non-
/// virtualised render used.
fn append_slice_rows(
    out: &mut Vec<DetailRow>,
    source: &str,
    lang_hint: Option<&str>,
    slice: &crate::detail::ContextualSlice,
    occ: &crate::app_state::OccurrenceView,
) {
    use crate::detail::LineKind;
    use crate::highlight::highlight;

    // Highlight the whole file once — tree-sitter parses more accurately
    // with full context than with a windowed slice, and we amortise the
    // cost across the occurrence's lines.
    let runs = highlight(source, lang_hint);

    // Sort tint spans once. Tier A occurrences hand us an empty vec;
    // the overlay pass is then a no-op.
    let mut tint_spans: Vec<(usize, usize, u32)> = occ.alpha_rename_spans.clone();
    tint_spans.sort_unstable_by_key(|(s, _, _)| *s);

    for line in &slice.lines {
        let segments = segments_for_range(source, &runs, &tint_spans, line.byte_range.clone());
        out.push(DetailRow::CodeLine {
            line_number: line.line_number,
            is_context: line.kind == LineKind::Context,
            segments,
        });
    }
}

/// Clip the highlighted runs + tint spans to a single line's byte
/// range and produce the `LineSegment` list. Runs never straddle `\n`
/// by construction (well, except multi-line strings — but the line's
/// `byte_range` has already been trimmed to exclude `\n` by
/// `extract_with_context`, so any straddle is silently clipped here).
fn segments_for_range(
    source: &str,
    runs: &[crate::highlight::HighlightedRun],
    tint_spans: &[(usize, usize, u32)],
    line: std::ops::Range<usize>,
) -> Vec<LineSegment> {
    use crate::highlight::theme_color;

    if line.is_empty() {
        return Vec::new();
    }

    let mut segments = Vec::new();
    for run in runs.iter() {
        if run.end <= line.start {
            continue;
        }
        if run.start >= line.end {
            break;
        }
        let run_start = run.start.max(line.start);
        let run_end = run.end.min(line.end);

        for piece in split_by_tints(run_start..run_end, tint_spans) {
            let text = source[piece.range.clone()].to_string();
            if text.is_empty() {
                continue;
            }
            segments.push(LineSegment {
                text,
                fg_color: theme_color(run.kind),
                bg_color: piece.tint,
            });
        }
    }

    segments
}

/// One sub-piece of a syntax-highlighted run after overlaying the
/// alpha-rename tint spans. `tint` is `Some(rgb)` if the sub-range
/// lies inside a tint span, `None` otherwise. Pure data so the split
/// logic can be unit-tested off the GPUI main thread.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RunPiece {
    range: std::ops::Range<usize>,
    tint: Option<u32>,
}

/// Intersect the highlight run `run` with the sorted, possibly-
/// overlapping alpha-rename tint spans. Emits contiguous non-empty
/// pieces covering `run` exactly once, each tagged with the tint
/// color the GUI should paint as the background (or `None` for gaps
/// outside any span).
///
/// Assumes `spans` is sorted by start byte. Callers upstream sort once
/// and pass the slice down; the function does not mutate it.
///
/// In the current data model spans are non-overlapping (every alpha-
/// renamed leaf is a distinct source byte range), so the algorithm
/// treats overlaps by picking the span whose start comes first —
/// consistent with how the normaliser emits them.
fn split_by_tints(run: std::ops::Range<usize>, spans: &[(usize, usize, u32)]) -> Vec<RunPiece> {
    let mut out: Vec<RunPiece> = Vec::new();
    if run.start >= run.end {
        return out;
    }
    let mut cursor = run.start;
    for (s, e, idx) in spans {
        let span_start = *s;
        let span_end = *e;
        // Skip spans entirely before the cursor (already consumed or
        // wholly to the left of the run).
        if span_end <= cursor {
            continue;
        }
        // Stop once we pass the run — remaining spans can't intersect.
        if span_start >= run.end {
            break;
        }
        // Gap before the tint: emit untinted.
        let tint_lo = span_start.max(cursor);
        if tint_lo > cursor {
            out.push(RunPiece {
                range: cursor..tint_lo,
                tint: None,
            });
        }
        let tint_hi = span_end.min(run.end);
        if tint_hi > tint_lo {
            out.push(RunPiece {
                range: tint_lo..tint_hi,
                tint: Some(crate::tint::tint_for_placeholder(*idx)),
            });
            cursor = tint_hi;
        }
        if cursor >= run.end {
            break;
        }
    }
    if cursor < run.end {
        out.push(RunPiece {
            range: cursor..run.end,
            tint: None,
        });
    }
    out
}

#[cfg(test)]
mod tint_overlay_tests {
    //! Pure-data tests for the tint-overlay piece splitter (#25).
    //!
    //! The renderer integration runs behind GPUI, which has to be
    //! constructed on the main thread — so we cover the logic here with
    //! plain unit tests that don't touch any GPUI types.

    use super::{RunPiece, split_by_tints};
    use crate::app_state::OccurrenceView;
    use std::path::PathBuf;

    fn occ_with_spans(spans: Vec<(usize, usize, u32)>) -> OccurrenceView {
        OccurrenceView {
            path: PathBuf::from("x.rs"),
            start_line: 1,
            end_line: 1,
            alpha_rename_spans: spans,
        }
    }

    #[test]
    fn tier_a_occurrence_has_empty_spans() {
        // Data-path acceptance: Tier A `OccurrenceView`s must carry no
        // alpha-rename spans. The renderer uses exactly this vector to
        // decide whether to apply the tint overlay — so asserting it
        // here is the inverse of "Tier A gets no tinting".
        let occ = occ_with_spans(Vec::new());
        assert!(occ.alpha_rename_spans.is_empty());
        let pieces = split_by_tints(0..40, &occ.alpha_rename_spans);
        assert_eq!(pieces.len(), 1);
        assert!(pieces[0].tint.is_none());
        assert_eq!(pieces[0].range, 0..40);
    }

    #[test]
    fn tier_b_pieces_split_around_spans() {
        // Three identifiers inside a 40-byte run; expect the splitter
        // to produce alternating tinted / untinted pieces that cover
        // the run exactly once.
        let spans = vec![(4, 8, 1), (16, 20, 2), (30, 34, 1)];
        let pieces = split_by_tints(0..40, &spans);

        let mut covered = 0usize;
        let mut last_end = 0usize;
        for p in &pieces {
            assert_eq!(p.range.start, last_end, "pieces must be contiguous");
            assert!(p.range.end > p.range.start);
            last_end = p.range.end;
            covered += p.range.end - p.range.start;
        }
        assert_eq!(last_end, 40);
        assert_eq!(covered, 40);

        // Same placeholder idx must produce the same tint color across
        // both occurrences inside this run (correspondence contract).
        let first = pieces.iter().find(|p| p.range == (4..8)).unwrap();
        let third = pieces.iter().find(|p| p.range == (30..34)).unwrap();
        assert_eq!(first.tint, third.tint, "idx=1 twice must match color");
        let middle = pieces.iter().find(|p| p.range == (16..20)).unwrap();
        assert_ne!(middle.tint, first.tint, "idx=2 must differ from idx=1");
    }

    #[test]
    fn spans_outside_run_are_ignored() {
        let spans = vec![(0, 2, 1), (100, 120, 2)];
        let pieces = split_by_tints(10..30, &spans);
        // Both spans fall entirely outside 10..30 — result is a single
        // untinted piece covering the whole run.
        assert_eq!(pieces.len(), 1);
        assert_eq!(
            pieces[0],
            RunPiece {
                range: 10..30,
                tint: None
            }
        );
    }

    #[test]
    fn spans_partially_overlap_run() {
        // A span that straddles the run's left edge clips; same for
        // the right edge. Coverage remains contiguous and complete.
        let spans = vec![(5, 15, 1), (25, 35, 2)];
        let pieces = split_by_tints(10..30, &spans);
        assert!(!pieces.is_empty());
        let start = pieces.first().unwrap().range.start;
        let end = pieces.last().unwrap().range.end;
        assert_eq!(start, 10);
        assert_eq!(end, 30);
    }
}
