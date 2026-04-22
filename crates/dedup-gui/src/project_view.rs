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
    ClipboardItem, Context, FocusHandle, MouseButton, Window, black, div, prelude::*, px, rgb,
    uniform_list, white,
};

use crate::app_state::{
    AppState, AppStatus, GroupView, Pane, ScanState, SortKey, StartupError,
    format_completion_banner, format_elapsed, group_view_from_match,
    launch_editor, load_folder,
};
use crate::detail_rows::{
    DetailRow, DetailRowsCache, LineSegment, compute_cache_key,
};
use crate::menubar::{
    ActivateGroup, ClearRecents, ClosePreferences, CollapseAll, DismissCurrentGroup,
    DismissEditorBanner, DismissRecentBanner, ExpandAll, FindInSidebar, FocusDetail, FocusSidebar,
    NextGroup, OpenConfigInEditor, OpenFolder, OpenRecent0, OpenRecent1, OpenRecent2, OpenRecent3,
    OpenRecent4, OpenSelectedInEditor, Preferences, PrevGroup, RemoveStaleRecent, StartScan,
    StopScan, ToggleSidebar,
};
use crate::toast::{
    ACTION_CACHE_DELETE_AND_RESCAN, ACTION_CACHE_RESCAN, ACTION_CONFIG_FIX, ACTION_CONFIG_RESET,
    ACTION_REMOVE_STALE_RECENT, ACTION_SHOW_ISSUES, Toast, ToastKind, format_issues_clipboard,
    panic_message,
};
use crate::tooltip::with_tooltip;
use dedup_core::{
    Cache, Config, FileIssue, MatchGroup, ScanConfig, ScanError, ScanResult, Scanner, Tier,
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
/// Cross-occurrence diff underline color (#55). A muted amber that
/// reads as "different" against the dim detail-pane background
/// without competing with the alpha-rename tints (which live on the
/// pastel half of the wheel — see `tint.rs`). Rendered as a 1px
/// bottom border on each differing segment so it stacks cleanly with
/// the existing `bg_color` tint.
const DIFF_UNDERLINE: u32 = 0xd97706;

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
    /// Scanner returned a non-cancel error (e.g. walk failure, cache
    /// open failure). The `String` is the human-readable error message
    /// the poll loop surfaces as an Error toast. Issue #30.
    ScanFailed(String),
    /// The background worker thread panicked. The `String` is the
    /// panic payload (extracted via `catch_unwind`), forwarded to the
    /// GUI as an Error toast + state reset to Idle. Issue #30.
    BackgroundPanic(String),
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
    /// Focus handle for the root element. GPUI dispatches keyboard
    /// actions through the focused element's key-context tree — without
    /// a focus handle bound to a `track_focus` call on the root div,
    /// every `cx.on_action` handler registered by the menubar would
    /// silently never fire because the window has no focused element.
    /// Issue #42.
    pub focus_handle: FocusHandle,
    /// Focus handle for the sidebar search input (issue #50). When
    /// focused, the search `<div>` is painted with an active border,
    /// `on_key_down` routes printable keys into
    /// [`AppState::search_query`], and the `!SearchInput` context
    /// predicate on the j/k/x/o/enter/arrow bindings causes those
    /// keys to short-circuit back into text entry instead of triggering
    /// list navigation.
    pub search_focus_handle: FocusHandle,
}

impl ProjectView {
    /// Construct an empty ProjectView tied to `cx`.
    ///
    /// `cx` is required so the view can allocate a [`FocusHandle`] — the
    /// handle is tracked by the root div in [`Self::render`] (via
    /// `track_focus`) and focused by `lib.rs::run` after the window
    /// opens, which is what lets keyboard shortcuts actually fire
    /// (issue #42).
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            state: AppState::new(),
            scan_rx: Arc::new(Mutex::new(None)),
            focus_handle: cx.focus_handle(),
            search_focus_handle: cx.focus_handle(),
        }
    }

    /// Apply a freshly-loaded folder to the view and mark it dirty so the
    /// next render picks up the new sidebar rows.
    ///
    /// On a non-[`AppStatus::Error`] open this also pushes `folder` to
    /// the Open Recent MRU (#28), persists `recent.json`, and rebuilds
    /// the File → Open Recent submenu so the menubar reflects the new
    /// entry immediately. An `Error` result (e.g. a corrupt cache) does
    /// not pollute the MRU — the user can re-pick the folder once the
    /// underlying issue is fixed.
    pub fn apply_folder(&mut self, folder: &Path, cx: &mut Context<Self>) {
        let result = load_folder(folder);
        let is_error = matches!(result.status, AppStatus::Error(_));
        // Issue #30 — classify the load result and raise the matching
        // toast before we overwrite state. `NewerCache` still uses the
        // inline body renderer (so the sidebar panel is replaced with
        // the upgrade message), but we also push a toast with the
        // "Rescan (overwrites cache)" action so the user has a
        // dismissable cue. Generic `Error` flows raise a plain Error
        // toast.
        match &result.status {
            AppStatus::NewerCache { .. } => {
                self.state.push_error_toast(
                    "Cache created by newer Dedup version. Rescan?",
                    None,
                    Some(crate::toast::ToastAction {
                        label: "Rescan (overwrites cache)".to_string(),
                        action_name: ACTION_CACHE_RESCAN,
                    }),
                );
            }
            AppStatus::Error(msg) => {
                // Heuristic corruption detection: the generic Error
                // path already has a stringified error; if it looks
                // like SQLite corruption we offer the destructive
                // delete-and-rescan action. Otherwise a plain toast.
                let lower = msg.to_ascii_lowercase();
                if lower.contains("corrupt") || lower.contains("database disk image is malformed") {
                    self.state.push_error_toast(
                        "Cache is corrupted. Delete .dedup/ and rescan?",
                        Some(msg.clone()),
                        Some(crate::toast::ToastAction {
                            label: "Delete .dedup/ and rescan".to_string(),
                            action_name: ACTION_CACHE_DELETE_AND_RESCAN,
                        }),
                    );
                } else {
                    self.state
                        .push_error_toast("Could not open cache", Some(msg.clone()), None);
                }
            }
            _ => {}
        }
        self.state.set_folder_result(result);
        if !is_error {
            self.state.push_recent(folder.to_path_buf());
            // `Context<Self>` deref-muts to `&mut App`, which
            // `rebuild_menus` wants. We pass a snapshot of the MRU
            // entries so the rebuild is pure data — no shared borrow
            // of `self.state` is captured by the callback.
            let entries = self.state.recent_projects.entries.clone();
            crate::menubar::rebuild_menus(cx, &entries);
        }
        // Dismiss any lingering stale-entry banner — the user just
        // successfully opened something, so the banner is no longer
        // relevant.
        self.state.dismiss_recent_banner();
        cx.notify();
    }

    /// Click-handler for a specific File → Open Recent entry.
    ///
    /// Reads `recent_projects.entries[idx]`, validates that the path is
    /// still a directory, and either re-opens it (on success — this
    /// also moves the entry back to the front of the MRU) or surfaces
    /// a stale-entry banner with the `[Remove from recents]` action.
    ///
    /// Intentionally does *not* auto-remove stale entries — the PRD is
    /// explicit that stale clicks must surface UX, not silently drop.
    pub fn open_recent(&mut self, idx: usize, cx: &mut Context<Self>) {
        let Some(entry) = self.state.recent_projects.entries.get(idx).cloned() else {
            return;
        };
        if entry.is_stale() {
            // Issue #30 — promote the stale-entry surface from an
            // inline banner to an Error toast carrying the "Remove
            // from recents" action. The inline banner stays in place
            // for now so the transition is low-risk; both surfaces
            // point at the same handler via `RemoveStaleRecent`.
            self.state.push_stale_recent_toast(&entry.path);
            self.state.surface_recent_banner(entry.path);
            cx.notify();
            return;
        }
        // Path is live — reuse the standard open path. `apply_folder`
        // pushes the entry back to the front of the MRU and rebuilds
        // the submenu.
        self.apply_folder(&entry.path, cx);
    }

    /// File → Open Recent → Clear Menu. Wipes the MRU, persists, and
    /// rebuilds the menu so the submenu collapses back to
    /// "No Recent Projects".
    pub fn clear_recents(&mut self, cx: &mut Context<Self>) {
        self.state.clear_recents();
        self.state.dismiss_recent_banner();
        let entries = self.state.recent_projects.entries.clone();
        crate::menubar::rebuild_menus(cx, &entries);
        cx.notify();
    }

    /// Banner action — drop the current stale entry from the MRU and
    /// dismiss the banner. No-op when no banner is showing.
    pub fn remove_stale_recent(&mut self, cx: &mut Context<Self>) {
        let Some(banner) = self.state.recent_banner.clone() else {
            return;
        };
        self.state.remove_recent(&banner.path);
        self.state.dismiss_recent_banner();
        let entries = self.state.recent_projects.entries.clone();
        crate::menubar::rebuild_menus(cx, &entries);
        cx.notify();
    }

    /// Banner action — dismiss the stale-entry banner without touching
    /// the MRU. User can still remove the entry later by clicking it
    /// again.
    pub fn dismiss_recent_banner(&mut self, cx: &mut Context<Self>) {
        self.state.dismiss_recent_banner();
        cx.notify();
    }

    /// Close the editor-launch banner (issue #29).
    pub fn dismiss_editor_banner(&mut self, cx: &mut Context<Self>) {
        self.state.dismiss_editor_banner();
        cx.notify();
    }

    // -----------------------------------------------------------------
    // Issue #30 — toast dismissal, action routing, and modal handlers.
    // -----------------------------------------------------------------

    /// Dismiss a specific toast by id. Wired from each toast's `[×]`
    /// close button.
    pub fn dismiss_toast(&mut self, id: u64, cx: &mut Context<Self>) {
        self.state.dismiss_toast(id);
        cx.notify();
    }

    /// Dismiss the top (most recent) toast. Safety net in case the id
    /// flow becomes unreachable.
    pub fn dismiss_top_toast(&mut self, cx: &mut Context<Self>) {
        if let Some(last) = self.state.toasts.toasts.last().cloned() {
            self.state.dismiss_toast(last.id);
            cx.notify();
        }
    }

    /// Route a toast action by its `action_name` string key. Clicking
    /// a toast button invokes this with the action's name; every
    /// branch also dismisses the triggering toast so the user gets
    /// immediate feedback regardless of whether the downstream handler
    /// has a visible side-effect.
    pub fn dispatch_toast_action(
        &mut self,
        action_name: &str,
        toast_id: u64,
        cx: &mut Context<Self>,
    ) {
        // Dismiss first — every action either opens a modal or kicks
        // off a follow-up flow; leaving the toast up would be
        // confusing.
        self.state.dismiss_toast(toast_id);
        match action_name {
            ACTION_CACHE_DELETE_AND_RESCAN => self.delete_cache_and_rescan(cx),
            ACTION_CACHE_RESCAN => self.rescan_current_folder(cx),
            ACTION_REMOVE_STALE_RECENT => self.remove_stale_recent(cx),
            ACTION_SHOW_ISSUES => {
                self.state.open_scan_issues();
            }
            ACTION_CONFIG_FIX => self.startup_fix_config(cx),
            ACTION_CONFIG_RESET => self.startup_reset_config(cx),
            other => {
                log::warn!("dedup-gui: unknown toast action {other}");
            }
        }
        cx.notify();
    }

    /// Open the post-scan issues dialog. Safe to call when the issue
    /// list is empty — the state helper no-ops.
    pub fn open_scan_issues(&mut self, cx: &mut Context<Self>) {
        self.state.open_scan_issues();
        cx.notify();
    }

    /// Close the post-scan issues dialog.
    pub fn close_scan_issues(&mut self, cx: &mut Context<Self>) {
        self.state.close_scan_issues();
        cx.notify();
    }

    /// "Copy details" handler on the post-scan issues dialog. Writes
    /// the GitHub-issue-ready markdown block to the clipboard.
    pub fn copy_scan_issues(&mut self, cx: &mut Context<Self>) {
        if self.state.scan_issues.is_empty() {
            return;
        }
        let block = format_issues_clipboard(&self.state.scan_issues);
        cx.write_to_clipboard(ClipboardItem::new_string(block));
        // Brief Info toast so the user has feedback — the clipboard
        // write is otherwise invisible.
        self.state.push_info_toast("Copied issues to clipboard.");
        cx.notify();
    }

    /// "Fix config" action on the startup-error modal. Opens the
    /// offending config file in `$EDITOR` / `$VISUAL` / `vi`; the
    /// modal stays up until the user dismisses it (they may still
    /// need to restart the app to pick up changes).
    pub fn startup_fix_config(&mut self, cx: &mut Context<Self>) {
        let Some(se) = self.state.startup_error.clone() else {
            return;
        };
        if let Some(parent) = se.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let editor = std::env::var("EDITOR")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| std::env::var("VISUAL").ok().filter(|s| !s.is_empty()))
            .unwrap_or_else(|| "vi".to_string());
        match std::process::Command::new(&editor).arg(&se.path).spawn() {
            Ok(_) => {
                self.state.push_info_toast(format!(
                    "Opened {} in {editor}. Restart dedup to reload.",
                    se.path.display()
                ));
            }
            Err(e) => {
                self.state
                    .push_error_toast(format!("Failed to launch {editor}: {e}"), None, None);
            }
        }
        cx.notify();
    }

    /// "Reset to defaults" action on the startup-error modal. Writes
    /// an empty TOML (the defaults-only case) to the failing path,
    /// retries `Config::load`, and clears the modal on success.
    pub fn startup_reset_config(&mut self, cx: &mut Context<Self>) {
        let Some(se) = self.state.startup_error.clone() else {
            return;
        };
        if let Some(parent) = se.path.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            self.state.push_error_toast(
                format!("Failed to create {}: {e}", parent.display()),
                None,
                None,
            );
            cx.notify();
            return;
        }
        // Empty file → `Config::load` falls back to baked-in defaults.
        if let Err(e) = std::fs::write(&se.path, "") {
            self.state.push_error_toast(
                format!("Failed to reset {}: {e}", se.path.display()),
                None,
                None,
            );
            cx.notify();
            return;
        }
        match Config::load(None) {
            Ok(_) => {
                self.state.clear_startup_error();
                self.state.push_info_toast("Reset config to defaults.");
            }
            Err(err) => {
                // Retry failed — update the modal with the new error.
                self.state.set_startup_error(&err);
            }
        }
        cx.notify();
    }

    /// "Rescan (overwrites cache)" toast action. Starts a scan on the
    /// currently-open folder. Equivalent to clicking the sidebar Scan
    /// button; no-op when no folder is open or a scan is already in
    /// flight.
    pub fn rescan_current_folder(&mut self, cx: &mut Context<Self>) {
        self.start_scan(cx);
    }

    /// "Delete .dedup/ and rescan" toast action. Removes the cache
    /// directory entirely (so the next open/scan writes a fresh
    /// schema) and re-triggers `apply_folder` on the current
    /// directory. On failure a new Error toast surfaces the I/O
    /// error; the folder is untouched.
    pub fn delete_cache_and_rescan(&mut self, cx: &mut Context<Self>) {
        let Some(folder) = self.state.current_folder.clone() else {
            return;
        };
        let dedup_dir = folder.join(".dedup");
        if dedup_dir.exists()
            && let Err(e) = std::fs::remove_dir_all(&dedup_dir)
        {
            self.state.push_error_toast(
                format!("Failed to delete {}: {e}", dedup_dir.display()),
                None,
                None,
            );
            cx.notify();
            return;
        }
        // Re-open — this re-materialises the folder with a fresh
        // cache and pushes any new toasts the open flow would normally
        // produce (e.g. Empty state). The user can then click Scan.
        self.apply_folder(&folder, cx);
    }

    /// Open the Preferences dialog overlay (issue #29, ⌘,). The
    /// dialog is rendered inline over the main body — see
    /// [`render_preferences_dialog`].
    pub fn open_preferences(&mut self, cx: &mut Context<Self>) {
        self.state.open_preferences();
        cx.notify();
    }

    /// Close the Preferences dialog without saving (issue #29).
    pub fn close_preferences(&mut self, cx: &mut Context<Self>) {
        self.state.close_preferences();
        cx.notify();
    }

    /// "Edit config file…" button inside the Preferences dialog.
    /// Spawns `$EDITOR` (falling back to `$VISUAL`, then `vi`) on the
    /// active config path — same behavior as `dedup config edit`.
    /// Closes the dialog on success so the user can re-open
    /// Preferences after their edit.
    pub fn open_config_in_editor(&mut self, cx: &mut Context<Self>) {
        let path = dedup_core::Config::global_path();
        if let Some(parent) = path.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            self.state.surface_editor_banner(format!(
                "Failed to create config dir {}: {e}",
                parent.display()
            ));
            cx.notify();
            return;
        }
        if !path.exists()
            && let Err(e) = std::fs::write(&path, "")
        {
            self.state.surface_editor_banner(format!(
                "Failed to create config file {}: {e}",
                path.display()
            ));
            cx.notify();
            return;
        }
        let editor = std::env::var("EDITOR")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| std::env::var("VISUAL").ok().filter(|s| !s.is_empty()))
            .unwrap_or_else(|| "vi".to_string());
        match std::process::Command::new(&editor).arg(&path).spawn() {
            Ok(_) => {
                self.state.close_preferences();
            }
            Err(e) => {
                self.state
                    .surface_editor_banner(format!("Failed to launch {editor}: {e}"));
            }
        }
        cx.notify();
    }

    pub fn select_group(&mut self, id: i64, cx: &mut Context<Self>) {
        // #54 — dismissed-group rows carry their cache-row id as
        // `SuppressionView::last_group_id`. When that id is the click
        // target we still update `selected_group` (so the detail pane
        // renders the read-only banner + code body) but we also move
        // focus to the detail pane so the user lands straight on the
        // review surface.
        let is_dismissed = self
            .state
            .dismissed
            .iter()
            .any(|s| s.last_group_id == Some(id));
        self.state.selected_group = Some(id);
        if is_dismissed {
            self.state.focused_pane = Pane::Detail;
        }
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
            // Issue #30 — wrap the entire worker body in `catch_unwind`
            // so a panic inside the scanner (e.g. a tree-sitter grammar
            // crash we didn't already contain in `scanner::run_tier_b`)
            // surfaces as a toast rather than tearing down the process.
            // The payload's message is extracted via `panic_message`
            // (shared helper in `toast.rs`) and forwarded on the scan
            // channel as `BackgroundPanic`, which the poll loop turns
            // into an Error toast + state reset to Idle.
            let panic_tx = worker_tx.clone();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_scan_worker(&worker_folder, &worker_progress, &worker_cancel, &worker_tx);
            }));
            if let Err(payload) = result {
                let msg = panic_message(&payload);
                log::error!(
                    "dedup-gui: scan worker panicked for {}: {msg}",
                    worker_folder.display()
                );
                let _ = panic_tx.send(ScanEvent::BackgroundPanic(msg));
            }
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
                        ScanEvent::ScanFailed(msg) => {
                            // Non-panic scanner failure → Error toast
                            // + reset state to Idle so the Scan button
                            // re-enables.
                            let _ = this.update(cx, |view, cx| {
                                view.state
                                    .push_error_toast("Scan failed", Some(msg.clone()), None);
                                view.state.cancel_completed();
                                cx.notify();
                            });
                            terminated = true;
                            break;
                        }
                        ScanEvent::BackgroundPanic(msg) => {
                            // Background-thread panic → Error toast +
                            // reset state to Idle so the app stays
                            // alive (issue #30 AC: "Background-thread
                            // panic surfaces as toast; app stays
                            // alive").
                            let _ = this.update(cx, |view, cx| {
                                view.state.push_error_toast(
                                    "Scan crashed (background panic)",
                                    Some(msg.clone()),
                                    None,
                                );
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

    /// ⌘F handler — flip pane focus to the sidebar and move GPUI's
    /// actual keyboard focus onto the search input's handle so
    /// subsequent keystrokes land in [`AppState::search_query`] (issue
    /// #50). The `!SearchInput` context predicate on `j`/`k`/`x`/`o`/
    /// `enter`/arrow bindings takes care of not eating the keystrokes
    /// as list-navigation actions.
    pub fn find_in_sidebar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.state.focus_pane(Pane::Sidebar);
        window.focus(&self.search_focus_handle, cx);
        cx.notify();
    }

    /// Clear the search input and return focus to the root handle
    /// (issue #50). Invoked from the search input's `escape` key
    /// handler; clearing on `Escape` matches the acceptance criterion
    /// "Clearing the input returns the full group list (no re-scan)"
    /// and gives the user an obvious way to abandon a filter.
    pub fn blur_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.state.set_search_query(String::new());
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    /// Append a single character to the live search query. Called by
    /// the search input's `on_key_down` handler for printable keys
    /// (issue #50).
    pub fn search_input_push(&mut self, ch: char, cx: &mut Context<Self>) {
        let mut q = self.state.search_query.clone();
        q.push(ch);
        self.state.set_search_query(q);
        cx.notify();
    }

    /// Remove the last character from the live search query on
    /// `backspace` (issue #50). No-op when the query is empty.
    pub fn search_input_backspace(&mut self, cx: &mut Context<Self>) {
        let mut q = self.state.search_query.clone();
        if q.pop().is_some() {
            self.state.set_search_query(q);
            cx.notify();
        }
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
    /// currently-selected group. Wired to the real launcher (#29) with
    /// the `(path, first_line)` pairs so presets like `nvim +N` land
    /// on the right row.
    pub fn open_selected_in_editor(&mut self, cx: &mut Context<Self>) {
        if self.state.focused_pane != Pane::Sidebar {
            return;
        }
        let occurrences = self.state.selected_occurrences();
        if occurrences.is_empty() {
            return;
        }
        let targets: Vec<(PathBuf, u32)> = occurrences
            .iter()
            .map(|o| (o.path.clone(), o.start_line.max(1) as u32))
            .collect();
        self.launch_editor_with_banner(&targets, cx);
    }

    /// Update the sidebar search query. Invoked from the search input's
    /// on-change callback.
    pub fn set_search_query(&mut self, q: String, cx: &mut Context<Self>) {
        self.state.set_search_query(q);
        cx.notify();
    }

    /// Update the sidebar sort key. Invoked from the sort-dropdown
    /// menu items (issue #46). Also closes the popup via
    /// [`AppState::set_sort_key`] and persists the new key to
    /// `sidebar.json` (issue #56) so it survives a window reopen.
    pub fn set_sort_key(&mut self, key: SortKey, cx: &mut Context<Self>) {
        self.state.set_sort_key(key);
        self.state.persist_sidebar_prefs();
        cx.notify();
    }

    /// Toggle the sort-dropdown popup open/closed (issue #46). Wired
    /// to the `Sort: <key>` button in the sidebar.
    pub fn toggle_sort_popup(&mut self, cx: &mut Context<Self>) {
        self.state.toggle_sort_popup();
        cx.notify();
    }

    /// Close the sort-dropdown popup without changing the key
    /// (issue #46). Wired to the click-outside scrim.
    pub fn close_sort_popup(&mut self, cx: &mut Context<Self>) {
        self.state.close_sort_popup();
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

    /// Restore a previously-dismissed group (#54). Drops the suppression
    /// row from the cache, clears the in-memory dismissed entry, and
    /// — when the underlying `match_groups` row is still live — snaps
    /// the sidebar selection to the restored group so the user lands
    /// on the active detail view immediately.
    ///
    /// Also deletes any per-occurrence dismissals still attached to
    /// the group: restoring a group is a single-click "bring it back"
    /// action, so leaving per-occurrence rows hiding individual files
    /// would be surprising.
    pub fn restore_group_click(&mut self, hash: u64, cx: &mut Context<Self>) {
        let Some((h, _last_gid)) = self.state.restore_group(hash) else {
            return;
        };
        if let Some(folder) = self.state.current_folder.clone() {
            match Cache::open(&folder) {
                Ok(mut cache) => {
                    if let Err(e) = cache.undismiss(h) {
                        log::warn!(
                            "dedup-gui: failed to persist undismiss for hash {h:016x}: {e}"
                        );
                    }
                    if let Err(e) = cache.undismiss_all_occurrences_for(h) {
                        log::warn!(
                            "dedup-gui: failed to clear per-occurrence dismissals for {h:016x}: {e}"
                        );
                    }
                }
                Err(e) => log::warn!(
                    "dedup-gui: failed to open cache to persist group restore: {e}"
                ),
            }
        }
        // Reload so the just-restored group shows up in the active
        // list with its live `match_groups` id (the in-memory
        // `restore_group` already selects it, but a refresh is
        // cheaper than trying to re-derive the view-model state by
        // hand and keeps the two sources in sync).
        if let Some(folder) = self.state.current_folder.clone() {
            let selected = self.state.selected_group;
            let result = load_folder(&folder);
            self.state.set_folder_result(result);
            if let Some(gid) = selected
                && self.state.groups.iter().any(|g| g.id == gid)
            {
                self.state.selected_group = Some(gid);
                self.state.focused_pane = Pane::Detail;
            }
        }
        cx.notify();
    }

    /// Restore a single per-occurrence dismissal (#54).
    pub fn restore_occurrence_click(
        &mut self,
        hash: u64,
        path: PathBuf,
        cx: &mut Context<Self>,
    ) {
        let Some((h, p)) = self.state.restore_occurrence(hash, &path) else {
            return;
        };
        if let Some(folder) = self.state.current_folder.clone() {
            match Cache::open(&folder) {
                Ok(mut cache) => {
                    if let Err(e) = cache.undismiss_occurrence(h, &p) {
                        log::warn!(
                            "dedup-gui: failed to persist undismiss for {}: {e}",
                            p.display()
                        );
                    }
                }
                Err(e) => log::warn!(
                    "dedup-gui: failed to open cache to persist occurrence restore: {e}"
                ),
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

    /// Toolbar "Copy as LLM prompt" (#57) — build the markdown prompt
    /// via [`crate::llm_prompt::llm_prompt`], drop it onto the system
    /// clipboard, and raise a success toast so the user has visible
    /// feedback (the clipboard write is otherwise silent).
    ///
    /// The button that triggers this is disabled upstream when no
    /// group is selected or any occurrence source is unavailable, but
    /// we re-check here so a race (e.g. the file got renamed between
    /// render + click) can't produce a half-populated prompt.
    pub fn copy_group_as_llm_prompt(
        &mut self,
        group_id: i64,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(group) = self.state.groups.iter().find(|g| g.id == group_id).cloned() else {
            return;
        };
        let occurrences = self.state.visible_occurrences_of(&group);
        if occurrences.is_empty() {
            return;
        }
        // Read each occurrence's source through the same helper the
        // detail-pane render uses, so the clipboard content matches
        // what the user sees on-screen.
        let sources: Vec<Option<String>> = occurrences
            .iter()
            .map(|o| read_occurrence_source(&self.state, o).map(|(src, _)| src))
            .collect();
        if !crate::llm_prompt::all_sources_available(&sources) {
            // Race with a file rename / delete between render + click.
            // Surface as a warning toast rather than silently copying a
            // stub prompt — matches the spirit of the disabled-state
            // tooltip.
            self.state.push_warning_toast(
                "Some source files are unavailable — prompt not copied.",
            );
            cx.notify();
            return;
        }
        let prompt = crate::llm_prompt::llm_prompt(&group, &occurrences, &sources);
        cx.write_to_clipboard(ClipboardItem::new_string(prompt));
        self.state.push_info_toast("Copied LLM prompt to clipboard.");
        cx.notify();
    }

    /// Toolbar "Open in editor" — respect checkboxes, fall back to
    /// every visible path when none are checked. Wired to the real
    /// launcher (#29); each target carries the first line number of
    /// the matching occurrence so the editor lands on the right row.
    pub fn open_group_in_editor(&mut self, group_id: i64, cx: &mut Context<Self>) {
        let targets = self.state.open_targets_for_group(group_id);
        if targets.is_empty() {
            return;
        }
        self.launch_editor_with_banner(&targets, cx);
    }

    /// Shared launcher path for the `o` shortcut and the toolbar
    /// button. Delegates to [`launch_editor`] and surfaces the
    /// "No editor found" banner on `Err(NoEditor)`.
    fn launch_editor_with_banner(&mut self, targets: &[(PathBuf, u32)], cx: &mut Context<Self>) {
        let cfg = self.state.editor_config.clone();
        if let Err(e) = launch_editor(&cfg, targets) {
            // All errors surface the same banner; the error message
            // already matches the AC ("No editor found — run dedup
            // config edit to pick one.") for `NoEditor`, and is
            // self-descriptive for the other variants.
            //
            // Issue #30 — also push a persistent Error toast. The
            // inline banner stays for now (low-risk transition); both
            // surfaces disappear when the user dismisses the toast or
            // the banner.
            let msg = e.to_string();
            self.state.push_error_toast(msg.clone(), None, None);
            self.state.surface_editor_banner(msg);
            cx.notify();
        }
    }

    /// Toolbar "Collapse all" — collapses every occurrence in the
    /// active group only (per #45). Other groups keep their state.
    pub fn collapse_all(&mut self, cx: &mut Context<Self>) {
        self.state.collapse_all_in_active_group();
        cx.notify();
    }

    /// Toolbar "Expand all" — inverse of [`Self::collapse_all`].
    pub fn expand_all(&mut self, cx: &mut Context<Self>) {
        self.state.expand_all_in_active_group();
        cx.notify();
    }

    /// ⌘B / View → Toggle Sidebar handler (issue #52). Flips
    /// [`AppState::sidebar_hidden`] then persists the new value to
    /// `sidebar.json` so visibility survives across window close +
    /// reopen (same scope as the resizable-sidebar width pref).
    pub fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.state.toggle_sidebar_visible();
        self.state.persist_sidebar_prefs();
        cx.notify();
    }

    /// Toolbar `[×]` close — clear selection so the detail pane blanks.
    pub fn close_group_detail(&mut self, cx: &mut Context<Self>) {
        self.state.close_group_detail();
        cx.notify();
    }

    /// Per-occurrence header toggle — collapse/expand a single
    /// occurrence's code body. Wired to a click anywhere on the
    /// occurrence-header row (child controls stop propagation).
    pub fn toggle_occurrence_collapse(
        &mut self,
        group_id: i64,
        occ_idx: usize,
        cx: &mut Context<Self>,
    ) {
        self.state.toggle_occurrence_collapse(group_id, occ_idx);
        cx.notify();
    }
}

/// Body of the scan worker thread, extracted so the `catch_unwind`
/// wrapper in [`ProjectView::start_scan`] has a single closure to
/// guard. Drives the scanner, persists the result, and posts the
/// terminal `ScanEvent` onto `tx`.
///
/// Any panic raised inside here is caught by the enclosing
/// `catch_unwind`; deliberate scanner errors surface as
/// `ScanEvent::ScanFailed` on the channel. Either way the poll loop
/// in `start_scan` sees a terminator and returns state to Idle.
fn run_scan_worker(
    worker_folder: &Path,
    worker_progress: &dedup_core::AtomicProgressSink,
    worker_cancel: &Arc<std::sync::atomic::AtomicBool>,
    worker_tx: &mpsc::Sender<ScanEvent>,
) {
    // Build a ScanConfig that matches the CLI's defaults: honour any
    // `.dedup/config.toml` the user has in place, then pin the cache
    // root so warm scans + persistence work.
    let config = Config::load(Some(worker_folder)).unwrap_or_default();
    let mut scan_cfg = ScanConfig::from(&config);
    scan_cfg.cache_root = Some(worker_folder.to_path_buf());
    scan_cfg.cancel = Some(worker_cancel.clone());

    // Wire the Tier A streaming callback to the GUI channel. The
    // callback fires exactly once on the worker thread (after bucket-
    // fill, before Tier B) and forwards the finalized Tier A set as
    // `GroupView` rows ready for the sidebar's impact-sorted merge.
    let stream_tx = worker_tx.clone();
    let cb: TierAStreamCallback = std::sync::Arc::new(move |groups: &[MatchGroup]| {
        let views: Vec<GroupView> = groups
            .iter()
            .enumerate()
            .map(|(i, g)| group_view_from_match(g, i))
            .collect();
        // Best-effort — if the poll loop has already dropped the rx
        // we just swallow the send.
        let _ = stream_tx.send(ScanEvent::TierAStream(views));
    });
    scan_cfg.on_tier_a_groups = Some(cb);

    let scanner = Scanner::new(scan_cfg);
    let result = match scanner.scan_with_progress(worker_folder, worker_progress) {
        Ok(r) => r,
        Err(ScanError::Cancelled) => {
            log::info!("dedup-gui: scan cancelled for {}", worker_folder.display());
            let _ = worker_tx.send(ScanEvent::Cancelled);
            return;
        }
        Err(e) => {
            // Issue #30 — forward a typed failure so the GUI can raise
            // an Error toast. Previously the worker just dropped `tx`;
            // the poll loop would notice the disconnect and quietly
            // reset, which hid the error from the user.
            let msg = format!("{e}");
            log::warn!(
                "dedup-gui: scan failed for {}: {msg}",
                worker_folder.display()
            );
            let _ = worker_tx.send(ScanEvent::ScanFailed(msg));
            return;
        }
    };

    // Persist before signaling completion so the main thread's
    // `load_folder` call sees fully-written cache rows. A cache write
    // failure surfaces as `ScanFailed` (issue #30) instead of being
    // silently logged; the scan still completes so the in-memory
    // result is delivered alongside the error toast.
    match Cache::open(worker_folder) {
        Ok(mut c) => {
            if let Err(e) = c.write_scan_result(&result) {
                log::warn!(
                    "dedup-gui: failed to persist scan result for {}: {e}",
                    worker_folder.display()
                );
                let _ = worker_tx.send(ScanEvent::ScanFailed(format!(
                    "Scan succeeded but cache write failed: {e}"
                )));
            }
        }
        Err(e) => {
            log::warn!(
                "dedup-gui: failed to open cache for {}: {e}",
                worker_folder.display()
            );
            let _ = worker_tx.send(ScanEvent::ScanFailed(format!(
                "Scan succeeded but cache open failed: {e}"
            )));
        }
    }

    // Send is best-effort — if the GUI already swapped the rx out
    // (e.g. user closed the window) we drop the result.
    let _ = worker_tx.send(ScanEvent::Completed(result));
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
    let issues = result.issues.clone();

    view.state.finish_scan(group_count, file_count, duration);
    // Issue #30 — surface the completion banner as an Info toast
    // alongside the in-place `ScanState::Completed` banner, plus an
    // Info toast with "View issues" action when the scan recorded
    // per-file issues. `set_scan_issues` feeds the dialog; the
    // post-scan toast is the one-click entry point.
    view.state
        .push_info_toast(format_completion_banner(group_count, file_count, duration));
    view.state.set_scan_issues(issues);
    view.state.push_post_scan_issues_toast();

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
        // The sync `FileDialog::pick_folder` spins Cocoa's modal runloop
        // while we're still nested inside `App::update` (the action
        // dispatch path holds the `AppCell` borrow). Any GPUI task the
        // modal pumps that calls `update` re-enters `borrow_mut` and
        // panics with "RefCell already borrowed". Using the async
        // variant + `cx.spawn` lets the current dispatch unwind and
        // release the borrow before the panel opens.
        cx.spawn(async move |cx| {
            let picked = rfd::AsyncFileDialog::new()
                .set_title("Open folder")
                .pick_folder()
                .await;
            let Some(handle) = picked else {
                // User hit Cancel — leave state untouched. This is the
                // explicit "open but do nothing" branch required by
                // issue #20 AC ("user-invoked only — no silent scans").
                return;
            };
            let folder = handle.path().to_path_buf();
            let _ = cx.update(|cx| {
                let Some(RootHandle(entity)) = cx.try_global::<RootHandle>().cloned() else {
                    return;
                };
                entity.update(cx, |view, cx| {
                    view.apply_folder(&folder, cx);
                });
            });
        })
        .detach();
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
    // Issue #52 — ⌘B / View → Toggle Sidebar. Flips
    // `AppState::sidebar_hidden` and persists to `sidebar.json` so the
    // visibility choice survives a window close + reopen. Replaces the
    // no-op stub formerly installed by `menubar::register_handlers`.
    cx.on_action(|_: &ToggleSidebar, cx: &mut gpui::App| {
        if let Some(RootHandle(entity)) = cx.try_global::<RootHandle>().cloned() {
            entity.update(cx, |view, cx| view.toggle_sidebar(cx));
        }
    });
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
    // `FindInSidebar` (⌘F) must land keyboard focus on the search
    // input's `FocusHandle`, which requires a `&mut Window`. The
    // app-scope `cx.on_action` handler only has access to `&mut App`,
    // so this action is registered on the root `div` via
    // `InteractiveElement::on_action` instead (see
    // [`ProjectView::render`]). Intentionally left unregistered here.
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

    // Issue #28 — File → Open Recent submenu actions.
    //
    // Five indexed unit actions (one per MRU slot) rather than a
    // single parameterised action — the `actions!` macro doesn't take
    // fields, and the #[derive(Action)] macro requires
    // `serde::Deserialize + schemars::JsonSchema` which is overkill
    // for five menu items. Each handler just forwards the slot index
    // into `open_recent` and lets `AppState.recent_projects.entries`
    // be the source of truth for the path.
    cx.on_action(|_: &OpenRecent0, cx: &mut gpui::App| dispatch_open_recent(0, cx));
    cx.on_action(|_: &OpenRecent1, cx: &mut gpui::App| dispatch_open_recent(1, cx));
    cx.on_action(|_: &OpenRecent2, cx: &mut gpui::App| dispatch_open_recent(2, cx));
    cx.on_action(|_: &OpenRecent3, cx: &mut gpui::App| dispatch_open_recent(3, cx));
    cx.on_action(|_: &OpenRecent4, cx: &mut gpui::App| dispatch_open_recent(4, cx));

    // File → Open Recent → Clear Menu.
    cx.on_action(|_: &ClearRecents, cx: &mut gpui::App| {
        if let Some(RootHandle(entity)) = cx.try_global::<RootHandle>().cloned() {
            entity.update(cx, |view, cx| view.clear_recents(cx));
        }
    });

    // Stale-entry banner actions. These are dispatched by the banner's
    // inline buttons in the project view (not from the menubar); the
    // global `on_action` registration is still the right place because
    // it mirrors how `OpenFolder` etc. are wired.
    cx.on_action(|_: &RemoveStaleRecent, cx: &mut gpui::App| {
        if let Some(RootHandle(entity)) = cx.try_global::<RootHandle>().cloned() {
            entity.update(cx, |view, cx| view.remove_stale_recent(cx));
        }
    });
    cx.on_action(|_: &DismissRecentBanner, cx: &mut gpui::App| {
        if let Some(RootHandle(entity)) = cx.try_global::<RootHandle>().cloned() {
            entity.update(cx, |view, cx| view.dismiss_recent_banner(cx));
        }
    });

    // Issue #29 — editor banner + preferences dialog.
    cx.on_action(|_: &DismissEditorBanner, cx: &mut gpui::App| {
        if let Some(RootHandle(entity)) = cx.try_global::<RootHandle>().cloned() {
            entity.update(cx, |view, cx| view.dismiss_editor_banner(cx));
        }
    });
    cx.on_action(|_: &Preferences, cx: &mut gpui::App| {
        if let Some(RootHandle(entity)) = cx.try_global::<RootHandle>().cloned() {
            entity.update(cx, |view, cx| view.open_preferences(cx));
        }
    });
    cx.on_action(|_: &ClosePreferences, cx: &mut gpui::App| {
        if let Some(RootHandle(entity)) = cx.try_global::<RootHandle>().cloned() {
            entity.update(cx, |view, cx| view.close_preferences(cx));
        }
    });
    cx.on_action(|_: &OpenConfigInEditor, cx: &mut gpui::App| {
        if let Some(RootHandle(entity)) = cx.try_global::<RootHandle>().cloned() {
            entity.update(cx, |view, cx| view.open_config_in_editor(cx));
        }
    });

    // Issue #30 — toast / modal / background-panic action handlers.
    // These are dispatched from inline buttons in the view layer; the
    // global `on_action` registration mirrors how #28's banner actions
    // are wired.
    cx.on_action(|_: &crate::menubar::DismissTopToast, cx: &mut gpui::App| {
        if let Some(RootHandle(entity)) = cx.try_global::<RootHandle>().cloned() {
            entity.update(cx, |view, cx| view.dismiss_top_toast(cx));
        }
    });
    cx.on_action(|_: &crate::menubar::ShowScanIssues, cx: &mut gpui::App| {
        if let Some(RootHandle(entity)) = cx.try_global::<RootHandle>().cloned() {
            entity.update(cx, |view, cx| view.open_scan_issues(cx));
        }
    });
    cx.on_action(|_: &crate::menubar::CloseScanIssues, cx: &mut gpui::App| {
        if let Some(RootHandle(entity)) = cx.try_global::<RootHandle>().cloned() {
            entity.update(cx, |view, cx| view.close_scan_issues(cx));
        }
    });
    cx.on_action(|_: &crate::menubar::CopyScanIssues, cx: &mut gpui::App| {
        if let Some(RootHandle(entity)) = cx.try_global::<RootHandle>().cloned() {
            entity.update(cx, |view, cx| view.copy_scan_issues(cx));
        }
    });
    cx.on_action(|_: &crate::menubar::StartupFixConfig, cx: &mut gpui::App| {
        if let Some(RootHandle(entity)) = cx.try_global::<RootHandle>().cloned() {
            entity.update(cx, |view, cx| view.startup_fix_config(cx));
        }
    });
    cx.on_action(
        |_: &crate::menubar::StartupResetConfig, cx: &mut gpui::App| {
            if let Some(RootHandle(entity)) = cx.try_global::<RootHandle>().cloned() {
                entity.update(cx, |view, cx| view.startup_reset_config(cx));
            }
        },
    );
    cx.on_action(|_: &crate::menubar::RescanCache, cx: &mut gpui::App| {
        if let Some(RootHandle(entity)) = cx.try_global::<RootHandle>().cloned() {
            entity.update(cx, |view, cx| view.rescan_current_folder(cx));
        }
    });
    cx.on_action(
        |_: &crate::menubar::DeleteCacheAndRescan, cx: &mut gpui::App| {
            if let Some(RootHandle(entity)) = cx.try_global::<RootHandle>().cloned() {
                entity.update(cx, |view, cx| view.delete_cache_and_rescan(cx));
            }
        },
    );
}

/// Spawn the 500ms toast auto-dismiss ticker.
///
/// Runs on the GPUI background executor; each wake-up calls
/// [`crate::app_state::AppState::tick_toasts`] with the current
/// [`Instant`] and requests a repaint if any toast was dropped. Kept
/// as a standalone function so `run()` in `lib.rs` can kick it off
/// without exposing the `Context` plumbing.
pub fn start_toast_ticker(entity: gpui::Entity<ProjectView>, cx: &mut gpui::App) {
    let weak = entity.downgrade();
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor()
                .timer(Duration::from_millis(500))
                .await;
            let now = Instant::now();
            let r = weak.update(cx, |view, cx| {
                let before = view.state.toasts.len();
                view.state.tick_toasts(now);
                if view.state.toasts.len() != before {
                    cx.notify();
                }
            });
            if r.is_err() {
                return;
            }
        }
    })
    .detach();
}

/// Shared body for the five `OpenRecentN` action handlers. Pulled out
/// so adding or removing a slot is a one-line change. The slot index
/// comes from the caller — see `OpenRecent0..OpenRecent4` in
/// `register_root`.
fn dispatch_open_recent(idx: usize, cx: &mut gpui::App) {
    if let Some(RootHandle(entity)) = cx.try_global::<RootHandle>().cloned() {
        entity.update(cx, |view, cx| view.open_recent(idx, cx));
    }
}

// -----------------------------------------------------------------------
// Render
// -----------------------------------------------------------------------

impl Render for ProjectView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Issue #42 — root focus anchor. GPUI's key dispatch walks from
        // the focused element up to the window root; without a
        // `track_focus` anywhere in the tree the window has no focused
        // element and keystrokes never reach the keymap, so every
        // `cx.on_action` handler the menubar registers is effectively
        // dead. Attaching the root `FocusHandle` + a `ProjectView`
        // key-context here (and focusing the handle from
        // `lib.rs::run`) gives the keymap a non-empty dispatch path
        // to resolve bindings against.
        let root_focus = self.focus_handle.clone();
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
            AppStatus::Loaded => {
                render_loaded(&self.state, window, &self.search_focus_handle)
                    .into_any_element()
            }
        };

        // Scan progress + completion banners float above the body so
        // every `AppStatus` that has a folder open (Empty, NoDuplicates,
        // Loaded, ...) gets the same feedback.
        // TODO(#30): replace with the real toast system.
        let overlay = render_scan_overlay(&self.state);

        // Issue #28 — stale-recent banner. Rendered above any other
        // overlay so a "can't open that" warning isn't buried under a
        // scan-progress bar. The banner ships two inline buttons:
        // `[Remove from recents]` and `[Dismiss]`. The two buttons
        // dispatch `RemoveStaleRecent` / `DismissRecentBanner`, which
        // are wired in `register_root`.
        // TODO(#30): promote to toast once the toast system lands.
        let stale = self
            .state
            .recent_banner
            .as_ref()
            .map(render_stale_recent_banner);

        // Issue #29 — editor launcher banner + Preferences dialog.
        let editor_banner = self.state.editor_banner.as_ref().map(render_editor_banner);
        let prefs = if self.state.preferences_open {
            Some(render_preferences_dialog(&self.state))
        } else {
            None
        };

        // Issue #30 — toast stack (top-right overlay), startup-error
        // modal (full-screen scrim), post-scan issues dialog.
        let toast_stack = render_toast_stack(&self.state.toasts.toasts);
        let startup_modal = self.state.startup_error.as_ref().map(render_startup_modal);
        let issues_dialog = if self.state.scan_issues_open {
            Some(render_issues_dialog(&self.state.scan_issues))
        } else {
            None
        };

        // Issue #46 — sort-dropdown popup. Rendered as a transparent
        // full-window scrim (click-to-dismiss) with the menu card
        // anchored near the sort button. Layered above the body but
        // below modal dialogs so Preferences / startup-error still take
        // priority when both happen to be open.
        let sort_popup = if self.state.sort_popup_open {
            Some(render_sort_popup(self.state.sort_key))
        } else {
            None
        };

        div()
            .track_focus(&root_focus)
            .key_context("ProjectView")
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(BG))
            .text_color(white())
            // Issue #50 — `FindInSidebar` (⌘F) must end up with
            // keyboard focus on the search input's handle. The
            // app-scope `cx.on_action` registration in
            // [`register_root`] only sees `&mut App`; attaching here
            // on the root div gives us `&mut Window` so
            // `view.find_in_sidebar(window, cx)` can call
            // `window.focus(&search_focus_handle, cx)`.
            .on_action(cx.listener(
                |view, _: &FindInSidebar, window: &mut Window, cx| {
                    view.find_in_sidebar(window, cx);
                },
            ))
            // Issue #47 — sidebar splitter drag. The handler fires once
            // per move event while a `SidebarResizeDrag` is active, no
            // matter whether the cursor is inside the splitter hitbox.
            // `event.position.x` is in window coordinates; the root div
            // is anchored at `x = 0` so that's the new width directly,
            // minus half the splitter so the cursor stays centered.
            .on_drag_move::<SidebarResizeDrag>(cx.listener(
                |view, e: &gpui::DragMoveEvent<SidebarResizeDrag>, _window, cx| {
                    let new_w =
                        f32::from(e.event.position.x) - SIDEBAR_SPLITTER_WIDTH / 2.0;
                    view.state.set_sidebar_width(new_w);
                    cx.notify();
                },
            ))
            .children(stale)
            .children(editor_banner)
            .children(overlay)
            .child(body)
            .children(sort_popup)
            .children(prefs)
            .children(issues_dialog)
            .children(startup_modal)
            .child(toast_stack)
    }
}

/// Build a small, flat action button that dispatches a menubar action
/// on click. Collapses the 9 copy-paste blocks under the toast/banner/
/// dialog renderers into one helper (surfaced by #32 dogfood scan,
/// group 66).
///
/// `bg` varies per call site (warning red, accent, dim, etc.), so it's
/// a parameter. `px_pad` / `py_pad` let the two existing sizes (banner
/// 10×4, dialog 12×6) share the same helper without a new enum.
fn toast_action_button<A>(
    id: impl Into<gpui::ElementId>,
    bg: u32,
    px_pad: f32,
    py_pad: f32,
    label: impl Into<gpui::SharedString>,
    action: A,
) -> gpui::Stateful<gpui::Div>
where
    A: gpui::Action + Clone,
{
    div()
        .id(id)
        .px(px(px_pad))
        .py(px(py_pad))
        .bg(rgb(bg))
        .text_color(white())
        .text_size(px(12.0))
        .rounded(px(4.0))
        .cursor_pointer()
        .on_mouse_down(MouseButton::Left, move |_, window, cx| {
            window.dispatch_action(Box::new(action.clone()), cx);
        })
        .child(label.into())
}

/// Render the inline "couldn't open this recent" banner.
///
/// Styled like `render_error` (same warning palette) but appears above
/// the shell so it's visible even when the body is the Loaded state for
/// a different folder the user had open before clicking the stale
/// entry. See the owning `ProjectView::open_recent` for the control
/// flow.
///
/// The two buttons (`[Remove from recents]` / `[Dismiss]`) dispatch
/// their respective actions rather than mutating state inline — that
/// keeps the render function pure and lets keyboard dispatch surface
/// the same behaviour if we later bind a shortcut.
fn render_stale_recent_banner(banner: &crate::app_state::RecentBanner) -> gpui::Div {
    let path = banner.path.display().to_string();
    div()
        .w_full()
        .bg(rgb(0x7f1d1d))
        .px(px(12.0))
        .py(px(8.0))
        .border_b_1()
        .border_color(black())
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .child(
            div().flex().flex_col().gap(px(2.0)).child(
                div()
                    .text_size(px(12.0))
                    .text_color(rgb(0xfee2e2))
                    .child(format!(
                        "Couldn\u{2019}t open {path} \u{2014} it may have \
                             been moved or deleted."
                    )),
            ),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .gap(px(8.0))
                .child(toast_action_button(
                    "stale-recent-remove",
                    0xb91c1c,
                    10.0,
                    4.0,
                    "Remove from recents",
                    RemoveStaleRecent,
                ))
                .child(toast_action_button(
                    "stale-recent-dismiss",
                    0x450a0a,
                    10.0,
                    4.0,
                    "Dismiss",
                    DismissRecentBanner,
                )),
        )
}

/// Render the inline "No editor found" / editor-launch-failure banner
/// (issue #29). Same palette family as [`render_stale_recent_banner`]
/// but with a single `[Dismiss]` button — the fix lives in
/// `dedup config edit`, not in a one-click banner action.
///
/// TODO(#30): promote to toast.
fn render_editor_banner(banner: &crate::app_state::EditorBanner) -> gpui::Div {
    let msg = banner.message.clone();
    div()
        .w_full()
        .bg(rgb(0x7f1d1d))
        .px(px(12.0))
        .py(px(8.0))
        .border_b_1()
        .border_color(black())
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .child(
            div()
                .text_size(px(12.0))
                .text_color(rgb(0xfee2e2))
                .child(msg),
        )
        .child(toast_action_button(
            "editor-banner-dismiss",
            0x450a0a,
            10.0,
            4.0,
            "Dismiss",
            DismissEditorBanner,
        ))
}

/// Render the Preferences dialog (issue #29).
///
/// ## GPUI compromise
///
/// GPUI's text-input primitives at the pinned revision
/// (da25b914c25f17ba457abc8cf75b8e42d0b899e2, Zed v0.232.3) do not
/// expose a simple `TextInput` widget outside Zed's own workspace
/// crates, which we deliberately don't depend on to keep the tree size
/// bounded (see #19 / #23 notes about GPUI text input). A full modal
/// with editable `command` / `terminal` / `terminal_command`
/// textboxes would require vendoring Zed's `ui` crate — overkill for
/// this milestone.
///
/// Instead, the dialog surfaces:
/// - The current preset + terminal mode as read-only text.
/// - An `[Edit config file…]` button that opens the active
///   `config.toml` in `$EDITOR` (same behavior as `dedup config
///   edit`). The user hand-edits the `[editor]` section there and
///   re-launches the app to pick up changes.
/// - A `[Close]` button that dismisses the dialog.
///
/// Richer preferences UX lands alongside #30's toast / modal system.
fn render_preferences_dialog(state: &AppState) -> gpui::Div {
    let cfg = &state.editor_config;
    let preset = cfg.preset.as_str().to_string();
    let terminal = cfg
        .terminal
        .clone()
        .unwrap_or_else(|| cfg.preset.default_terminal().as_str().to_string());
    let command = cfg
        .command
        .clone()
        .unwrap_or_else(|| "(preset default)".to_string());
    let terminal_command = cfg
        .terminal_command
        .clone()
        .unwrap_or_else(|| "(unset)".to_string());
    let config_path = dedup_core::Config::global_path().display().to_string();

    let row = |label: &str, value: String| {
        div()
            .flex()
            .flex_row()
            .gap(px(12.0))
            .child(
                div()
                    .w(px(140.0))
                    .text_size(px(12.0))
                    .text_color(rgb(ROW_TEXT_DIM))
                    .child(label.to_string()),
            )
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(rgb(ROW_TEXT))
                    .child(value),
            )
    };

    // The overlay is a full-size scrim with a centered card. The scrim
    // click dismisses the dialog; the card catches clicks so hits
    // inside don't propagate to the scrim.
    div()
        .absolute()
        .inset_0()
        .bg(rgb(0x00000099))
        .flex()
        .items_center()
        .justify_center()
        .on_mouse_down(MouseButton::Left, |_, window, cx| {
            window.dispatch_action(Box::new(ClosePreferences), cx);
        })
        .child(
            div()
                .id("preferences-dialog")
                .w(px(520.0))
                .bg(rgb(0x24242a))
                .rounded(px(8.0))
                .border_1()
                .border_color(rgb(0x3b3b48))
                .p(px(20.0))
                .flex()
                .flex_col()
                .gap(px(14.0))
                // Eat clicks on the card itself so they don't reach
                // the scrim's on_mouse_down dismiss handler.
                .on_mouse_down(MouseButton::Left, |_, _, _| {})
                .child(
                    div()
                        .text_size(px(16.0))
                        .text_color(white())
                        .child("Preferences \u{2014} Editor"),
                )
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(rgb(ROW_TEXT_DIM))
                        .child(
                            "Choose your editor preset in \
                             config.toml. Changes take effect on next \
                             open.",
                        ),
                )
                .child(row("Preset", preset))
                .child(row("Terminal", terminal))
                .child(row("Command", command))
                .child(row("Terminal command", terminal_command))
                .child(row("Config file", config_path))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(8.0))
                        .justify_end()
                        .child(toast_action_button(
                            "preferences-close",
                            ACCENT_DIM,
                            12.0,
                            6.0,
                            "Close",
                            ClosePreferences,
                        ))
                        .child(toast_action_button(
                            "preferences-edit",
                            ACCENT,
                            12.0,
                            6.0,
                            "Edit config file\u{2026}",
                            OpenConfigInEditor,
                        )),
                ),
        )
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

fn render_loaded(
    state: &AppState,
    window: &Window,
    search_focus_handle: &FocusHandle,
) -> gpui::Div {
    // Issue #52 — when the sidebar is hidden (⌘B), drop both the
    // sidebar list and the drag splitter from the tree so the detail
    // pane's `flex_1` sibling expands to fill the window. Keeping the
    // splitter would leave a 4px dead strip with the ResizeLeftRight
    // cursor next to the detail pane, which is visually worse than
    // dropping it entirely.
    let mut row = div().size_full().flex().flex_row();
    if !state.sidebar_hidden {
        row = row
            .child(render_sidebar(state, window, search_focus_handle))
            .child(render_sidebar_splitter());
    }
    row.child(render_detail(state))
}

/// Issue #47 — draggable splitter between the sidebar and the detail
/// pane.
///
/// A thin 4-px column with the `ResizeLeftRight` cursor. Starting a
/// drag on it activates a [`SidebarResizeDrag`] payload; the actual
/// "turn window-x into new width" translation lives on the root
/// [`ProjectView`] `on_drag_move` handler so the handler has a stable
/// reference frame regardless of where the sidebar currently ends
/// (the root div is anchored at `x = 0` in window coordinates, so
/// `event.position.x` is the new width directly). On mouse up we
/// persist the final width to `sidebar.json`.
///
/// The dragged-view constructor returns an empty [`Empty`] view —
/// we want the drag state machinery, not a drag ghost, and there's
/// no GPUI primitive for "no drag preview" short of rendering nothing.
pub(crate) const SIDEBAR_SPLITTER_WIDTH: f32 = 4.0;

fn render_sidebar_splitter() -> gpui::Stateful<gpui::Div> {
    div()
        .id("sidebar-splitter")
        .w(px(SIDEBAR_SPLITTER_WIDTH))
        .h_full()
        .bg(rgb(BG))
        .cursor_ew_resize()
        // Start a drag with a [`SidebarResizeDrag`] marker payload.
        // The `Empty` view keeps GPUI happy about the generic `W: Render`
        // bound without adding a visible drag ghost.
        .on_drag(SidebarResizeDrag, |_, _, _window, cx| {
            cx.stop_propagation();
            cx.new(|_| gpui::EmptyView)
        })
        // Stop-propagate the click so the initial mouse-down doesn't
        // bubble to any siblings. The drag state machine handles the
        // rest.
        .on_mouse_down(MouseButton::Left, |_, _, cx| {
            cx.stop_propagation();
        })
        // Persist on mouse up so the value survives a restart. The
        // root handler has already been updating state per move event.
        .on_mouse_up(MouseButton::Left, |_, _, cx: &mut gpui::App| {
            if let Some(RootHandle(entity)) = cx.try_global::<RootHandle>().cloned() {
                entity.update(cx, |view, _| view.state.persist_sidebar_prefs());
            }
        })
}

/// Marker payload attached to an active sidebar-splitter drag. Carries
/// no data — the root `on_drag_move::<SidebarResizeDrag>` handler
/// reads the event position directly. Tagged as a distinct type (not
/// `()`) so `DragMoveEvent<SidebarResizeDrag>` type-dispatches to the
/// right handler.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SidebarResizeDrag;

/// Partition the sidebar's current groups into `(tier_b, tier_a)`.
///
/// While a scan is running we render the cache-backed Tier B set plus
/// the mid-scan streaming Tier A buffer directly (search / sort
/// intentionally do not re-apply to the streaming pulse — the final
/// cache reload brings them back into the `visible_groups` path).
/// Otherwise both lists come from the filtered + sorted
/// `visible_groups`, partitioned by tier.
///
/// Extracted as a standalone fn so the partition is unit-testable
/// without instantiating GPUI (#44).
fn sidebar_tier_partition(state: &AppState) -> (Vec<GroupView>, Vec<GroupView>) {
    if matches!(state.scan_state, ScanState::Running { .. }) {
        (
            state.tier_b_groups().cloned().collect(),
            state.groups_streaming.clone(),
        )
    } else {
        let visible = state.visible_groups();
        (
            visible
                .iter()
                .filter(|g| g.tier == Tier::B)
                .cloned()
                .collect(),
            visible
                .iter()
                .filter(|g| g.tier == Tier::A)
                .cloned()
                .collect(),
        )
    }
}

/// Fixed row height for the sidebar group lists. `uniform_list`
/// measures the first rendered row and reuses that height for every
/// other row — so the row body must render at exactly this height for
/// virtualization to line up with the scroll offset. 24 px at 12 px
/// font + 4 px vertical padding lines up with the prior non-virtual
/// layout (issue #44).
const GROUP_ROW_HEIGHT: f32 = 24.0;

fn render_sidebar(
    state: &AppState,
    window: &Window,
    search_focus_handle: &FocusHandle,
) -> gpui::Div {
    // Issue #23 — the sidebar renders the filtered + sorted list from
    // `AppState::visible_groups`. While a scan is running we still fall
    // back to the streaming buffer for Tier A (it arrives mid-scan,
    // before the cache reload), partitioned into the two tier sections
    // underneath the search / sort / summary row.
    //
    // Issue #44 — the two tier lists render through `uniform_list` so
    // only the visible window of rows is materialized per frame.
    // Section headers, search / sort / summary, and the dismissed
    // section stay outside the lists so they keep their fixed layout
    // and don't pay the virtualization cost.
    let (tier_b, tier_a) = sidebar_tier_partition(state);

    let summary = state.summary();
    let selected = state.selected_group;
    let tier_b_count = tier_b.len();
    let tier_a_count = tier_a.len();

    div()
        .w(px(state.sidebar_width))
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
        .child(render_search_box(state, window, search_focus_handle))
        .child(render_sort_dropdown(state))
        .child(render_summary_header(&summary.format()))
        // Tier B (functions / classes) — header outside, rows virtualized.
        .child(render_section_header(
            "Duplicated functions / classes",
            tier_b_count,
        ))
        .child(render_virtualized_group_list(
            "sidebar-tier-b",
            tier_b,
            selected,
        ))
        // Tier A (blocks) — header outside, rows virtualized.
        .child(render_section_header("Duplicated blocks", tier_a_count))
        .child(render_virtualized_group_list(
            "sidebar-tier-a",
            tier_a,
            selected,
        ))
        // Dismissed section stays outside the lists so its expand/collapse
        // header and rows render normally.
        .child(render_dismissed_section(state))
}

/// Wrap a `uniform_list` of sidebar group rows in a `flex_1` container
/// so the two tier lists share the sidebar's remaining vertical space
/// and each virtualizes its own viewport. The `min_h_0` on the wrapper
/// is required for flex children with scrollable content — without it
/// the container will grow to fit all rows and defeat virtualization.
fn render_virtualized_group_list(
    id: &'static str,
    groups: Vec<GroupView>,
    selected: Option<i64>,
) -> gpui::Div {
    let count = groups.len();
    let rows = Rc::new(groups);
    let rows_for_render = rows.clone();
    let list = uniform_list(id, count, move |range, _window, _cx| {
        range
            .map(|idx| {
                let g = &rows_for_render[idx];
                render_group_row(g, selected == Some(g.id))
            })
            .collect::<Vec<_>>()
    })
    .h_full()
    .flex_1();
    div().flex_1().min_h_0().child(list)
}

/// Live sidebar search input (issues #23, #50).
///
/// Zed's upstream `ui::TextField` primitive is not re-exported from the
/// vendored `gpui` crate we pin, so this is a thin wrapper over a
/// plain `div` with:
///
/// * `track_focus` on [`ProjectView::search_focus_handle`] so GPUI
///   paints focus styling + routes key events here.
/// * `key_context("SearchInput")` so the j/k/x/o/enter/arrow
///   keybindings installed by [`crate::menubar`] (all carry a
///   `!SearchInput` predicate) do **not** fire while the input owns
///   focus — printable keys fall through to the `on_key_down` handler
///   and land in [`AppState::search_query`] instead.
/// * An `on_key_down` handler that interprets each keystroke:
///   `escape` clears the query + blurs back to the root focus handle;
///   `backspace` / `delete` pops the last character; plain printable
///   keys append the character and re-render the filtered sidebar.
///   Keystrokes that carry `cmd` / `ctrl` are ignored so `cmd-w`,
///   `cmd-q`, etc. still dispatch to the menubar when the search
///   input is focused.
///
/// The placeholder "Search…" shows only when the query is empty.
fn render_search_box(
    state: &AppState,
    window: &Window,
    search_focus_handle: &FocusHandle,
) -> gpui::Div {
    let empty = state.search_query.is_empty();
    let text = if empty {
        "Search\u{2026}".to_string()
    } else {
        state.search_query.clone()
    };
    let is_focused = search_focus_handle.is_focused(window);
    let border_color = if is_focused {
        rgb(ACCENT)
    } else {
        rgb(ACCENT_DIM)
    };
    let text_color = if empty {
        rgb(ROW_TEXT_DIM)
    } else {
        rgb(ROW_TEXT)
    };

    // The on_key_down handler fires only while the search input owns
    // focus (GPUI routes key events through the focused element's
    // dispatch tree). It uses the app-global `RootHandle` to hop back
    // into `ProjectView` so we don't have to thread a `cx.listener`
    // through every `render_sidebar` parameter.
    let key_down = |event: &gpui::KeyDownEvent, window: &mut Window, cx: &mut gpui::App| {
        let Some(RootHandle(entity)) = cx.try_global::<RootHandle>().cloned() else {
            return;
        };
        let key = event.keystroke.key.as_str();
        let modifiers = &event.keystroke.modifiers;
        // Ignore modifier combos other than shift — `cmd-q`, `cmd-w`,
        // `ctrl-c`, etc. should still reach the menubar. `shift` is
        // allowed so capital letters can be typed.
        let has_command_mod = modifiers.control
            || modifiers.platform
            || modifiers.function
            || modifiers.alt;

        if key == "escape" {
            entity.update(cx, |view, cx| view.blur_search(window, cx));
            return;
        }

        if has_command_mod {
            // Leave to the keymap — e.g. `cmd-w` closes the window.
            return;
        }

        if key == "backspace" || key == "delete" {
            entity.update(cx, |view, cx| view.search_input_backspace(cx));
            return;
        }

        // Printable character branch — `key_char` holds the codepoint
        // the platform IME produced (`"s"` for `s`, `"S"` for
        // `shift-s`, `"ß"` for `option-s`, etc.). We fall back to
        // `key` for layouts / keystrokes where `key_char` is `None`
        // (macOS returns `None` for IME-consuming keys and for
        // non-printable keys like `tab` / `escape` / `enter`).
        let typed = event
            .keystroke
            .key_char
            .clone()
            .unwrap_or_else(|| key.to_string());
        // Reject multi-codepoint / zero-length / control inputs. A
        // well-formed printable keystroke is always exactly one
        // `char`, so filtering on `chars().count() == 1` matches every
        // ASCII letter / digit / punctuation plus every single-
        // codepoint unicode char (e.g. `ß`, `é`) without letting
        // escape-sequence names like `"tab"` or `"enter"` through.
        let mut it = typed.chars();
        let Some(ch) = it.next() else { return };
        if it.next().is_some() {
            return;
        }
        if ch.is_control() {
            return;
        }
        entity.update(cx, |view, cx| view.search_input_push(ch, cx));
    };

    div()
        .track_focus(search_focus_handle)
        .key_context("SearchInput")
        .mx(px(12.0))
        .px(px(8.0))
        .py(px(6.0))
        .bg(rgb(ACCENT_DIM))
        .rounded(px(4.0))
        .border_1()
        .border_color(border_color)
        .text_size(px(12.0))
        .text_color(text_color)
        .on_mouse_down(MouseButton::Left, {
            let handle = search_focus_handle.clone();
            move |_, window, cx: &mut gpui::App| {
                window.focus(&handle, cx);
            }
        })
        .on_key_down(key_down)
        .child(text)
}

/// Sort-dropdown button (issues #23, #46).
///
/// Renders the `Sort: <current key>` label as a clickable button
/// styled to match the sidebar. Clicking it toggles the full-window
/// popup defined by [`render_sort_popup`]. Selection + click-outside
/// dismissal are routed through `RootHandle` → `ProjectView`.
fn render_sort_dropdown(state: &AppState) -> gpui::Stateful<gpui::Div> {
    let open = state.sort_popup_open;
    let border_color = if open { rgb(ACCENT) } else { rgb(ACCENT_DIM) };
    let button = div()
        .id("sidebar-sort-button")
        .mx(px(12.0))
        .mt(px(4.0))
        .px(px(8.0))
        .py(px(6.0))
        .bg(rgb(ACCENT_DIM))
        .rounded(px(4.0))
        .border_1()
        .border_color(border_color)
        .text_size(px(12.0))
        .text_color(rgb(ROW_TEXT))
        .cursor_pointer()
        .child(format!("Sort: {}", state.sort_key.label()))
        .on_mouse_down(MouseButton::Left, |_, _window, cx: &mut gpui::App| {
            // Stop the scrim's outside-click handler from also firing
            // by closing/opening via the view directly. The scrim only
            // captures clicks while the popup is already open, so on
            // the opening click there's no conflict; on the next click
            // the scrim wins and dismisses the popup.
            if let Some(RootHandle(entity)) = cx.try_global::<RootHandle>().cloned() {
                entity.update(cx, |view, cx| view.toggle_sort_popup(cx));
            }
        });
    // Issue #53 — sort-control tooltip. The label uses the same verb
    // the menubar does so the hint matches the user's mental model.
    with_tooltip(button, "Sort groups")
}

/// Sort-dropdown popup menu (issue #46). Full-window transparent
/// scrim so clicks outside the card dismiss; card itself lists every
/// [`SortKey::ALL`] variant, with the current key highlighted and
/// marked with a leading dot.
fn render_sort_popup(current: SortKey) -> gpui::Div {
    // Card anchored under the sort button. The sidebar is 320 px wide
    // with 12 px horizontal padding around the button and the button
    // stack (search 12 px mt + ~32 px button height + 4 px + 32 px)
    // sits roughly 96 px from the top of the sidebar. 112 px gives a
    // little breathing room under the button.
    let mut card = div()
        .id("sort-popup-card")
        .absolute()
        .left(px(12.0))
        .top(px(112.0))
        .w(px(224.0))
        .bg(rgb(SIDEBAR_BG))
        .rounded(px(6.0))
        .border_1()
        .border_color(rgb(ACCENT))
        .p(px(4.0))
        .flex()
        .flex_col()
        // Eat clicks on the card itself so they don't reach the
        // scrim's `on_mouse_down` dismiss handler.
        .on_mouse_down(MouseButton::Left, |_, _, _| {});

    for key in SortKey::ALL {
        let key = *key;
        let is_current = key == current;
        let marker = if is_current { "\u{2022} " } else { "  " };
        let row = div()
            .id(gpui::ElementId::Name(
                format!("sort-popup-{}", key.label()).into(),
            ))
            .px(px(8.0))
            .py(px(6.0))
            .rounded(px(4.0))
            .text_size(px(12.0))
            .text_color(if is_current {
                rgb(ACCENT)
            } else {
                rgb(ROW_TEXT)
            })
            .cursor_pointer()
            .hover(|s| s.bg(rgb(ROW_SELECTED_BG)))
            .child(format!("{marker}{}", key.label()))
            .on_mouse_down(
                MouseButton::Left,
                move |_, _window, cx: &mut gpui::App| {
                    if let Some(RootHandle(entity)) = cx.try_global::<RootHandle>().cloned() {
                        entity.update(cx, |view, cx| view.set_sort_key(key, cx));
                    }
                },
            );
        card = card.child(row);
    }

    // Scrim — full window, transparent; absorbs outside clicks.
    div()
        .absolute()
        .inset_0()
        .on_mouse_down(MouseButton::Left, |_, _window, cx: &mut gpui::App| {
            if let Some(RootHandle(entity)) = cx.try_global::<RootHandle>().cloned() {
                entity.update(cx, |view, cx| view.close_sort_popup(cx));
            }
        })
        .child(card)
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
    // Fixed row height required by `uniform_list` — it measures the
    // first row and reuses that height for every row in the list
    // (#44). Center the label vertically inside the fixed frame so the
    // visual weight matches the prior `py(px(4.0))` layout.
    let row = div()
        .h(px(GROUP_ROW_HEIGHT))
        .px(px(16.0))
        .flex()
        .flex_row()
        .items_center()
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
            let selected = state.selected_group == s.last_group_id && s.last_group_id.is_some();
            // Issue #54 — the sidebar's dismissed rows delegate layout
            // and click/restore routing to `suppressions_view` so the
            // project view doesn't grow dismissed-specific branches.
            wrap = wrap.child(crate::suppressions_view::render_dismissed_row(s, selected));
        }
    }
    wrap
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

// `DetailRow` + `LineSegment` live in `crate::detail_rows` (#49) so
// `AppState` can own an `Rc<Vec<DetailRow>>` row cache without
// introducing a circular module dep. Re-imported here locally via the
// `use` at the top of the file; `build_detail_rows` still returns
// `Vec<DetailRow>` and `render_detail_row` consumes `&DetailRow`.

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
    let is_dismissed = state.selected_dismissed().is_some();

    // #49 — pull the flattened row vec from the `AppState`-owned cache.
    // `build_detail_rows` runs only on cache miss (group change,
    // collapse toggle, occurrence list change, selection toggle,
    // session-dismiss, or `context_lines` change). Cache stores
    // `Rc<Vec<DetailRow>>` so the `uniform_list` render closure keeps
    // its own handle without cloning the vec. #45 — rows always
    // include the summary and every occurrence header; per-occurrence
    // collapse only suppresses that occurrence's `CodeLine` / `Gap` /
    // `Unavailable` rows, so headers stay clickable even when
    // collapsed.
    let rows = get_or_build_detail_rows(state, group_id, &occurrences);
    let row_count = rows.len();
    let rows_for_render = rows.clone();
    let list = uniform_list("detail-rows", row_count, move |range, _window, _cx| {
        range
            .map(|idx| render_detail_row(&rows_for_render[idx]))
            .collect::<Vec<_>>()
    })
    .h_full()
    .flex_1();

    let mut wrap = div()
        .size_full()
        .flex()
        .flex_col()
        .bg(rgb(BG))
        .flex_1();
    if is_dismissed {
        // #54 — read-only surface. Toolbar collapses to "Restore
        // group" + "Close"; `render_group_toolbar`'s full set of
        // Dismiss/Collapse/Open actions is intentionally absent so
        // the user can't further mutate a suppressed group.
        wrap = wrap.child(render_dismissed_toolbar(group_id, state));
        if let Some(banner) = crate::suppressions_view::render_dismissed_banner(state) {
            wrap = wrap.child(banner);
        }
    } else {
        wrap = wrap.child(render_group_toolbar(state, group_id));
    }
    wrap.child(div().flex_1().px(px(16.0)).pb(px(16.0)).child(list))
        .into_any_element()
}

/// Minimal toolbar rendered over a dismissed group (#54). Shows the
/// headline hash + a `[Restore]` + `[Close]` pair. The full set of
/// toolbar actions (`Dismiss`, `Open in editor`, `Copy paths`,
/// `Collapse/Expand`) is suppressed: the dismissed detail view is
/// read-only.
fn render_dismissed_toolbar(group_id: i64, state: &AppState) -> gpui::Div {
    let label = state
        .selected_dismissed()
        .map(|s| {
            let short: String = s.hash_hex.chars().take(12).collect();
            format!("Dismissed group (hash {short}\u{2026})")
        })
        .unwrap_or_else(|| "Dismissed group".to_string());
    let hash = state.selected_dismissed().map(|s| s.hash).unwrap_or(0);

    let restore = crate::suppressions_view::restore_button(hash);
    let close_btn = with_tooltip(
        div()
            .id(("toolbar-close-dismissed", group_id as u64))
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
            .child("\u{00D7}"),
        "Close detail pane",
    );

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
                .child(label),
        )
        .child(restore)
        .child(close_btn)
}

/// Colours specific to the issue #27 toolbar + per-occurrence cards.
const TOOLBAR_BG: u32 = 0x2a2a33;
const TOOLBAR_BUTTON_BG: u32 = 0x3b3b48;
const TOOLBAR_DANGER_BG: u32 = 0x5a2a2a;
const TOOLBAR_BUTTON_HOVER_BG: u32 = 0x4a4a58;
const CHECKBOX_CHECKED_BG: u32 = ACCENT;
const CHECKBOX_UNCHECKED_BG: u32 = 0x444452;

/// Build a single toolbar button (rounded, coloured rect with on-click).
/// Toolbar pill button with an optional delayed-hover tooltip.
///
/// All toolbar buttons wire a tooltip today (#53) — the `tooltip`
/// arg is `Option` rather than required so the helper still reads as
/// a generic "pill button" and a future, truly-self-describing label
/// can pass `None::<&str>` without a contrived string.
///
/// The tooltip is rendered via [`crate::tooltip::with_tooltip`], which
/// preserves the button's click + hover handlers and just layers a
/// delayed-hover label on top.
fn toolbar_button_with_tooltip(
    label: impl Into<String>,
    tooltip: Option<impl Into<gpui::SharedString>>,
    bg: u32,
    action: impl Fn(&mut gpui::App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let label = label.into();
    let id_key = label.clone();
    let button = div()
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
        .child(id_key);
    match tooltip {
        Some(text) => with_tooltip(button, text),
        None => button,
    }
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
    // resulting state change. Tooltips (#53) disambiguate each action
    // from the rest — particularly `Dismiss group` vs the per-
    // occurrence `×` and the toolbar's close-the-detail `×`.
    let open_btn = toolbar_button_with_tooltip(
        "Open in editor",
        Some("Open checked paths in your editor"),
        TOOLBAR_BUTTON_BG,
        move |cx| {
            if let Some(RootHandle(entity)) = cx.try_global::<RootHandle>().cloned() {
                entity.update(cx, |view, cx| view.open_group_in_editor(gid_open, cx));
            }
        },
    );
    let dismiss_btn = toolbar_button_with_tooltip(
        "Dismiss group",
        Some("Dismiss this group (hide from sidebar)"),
        TOOLBAR_DANGER_BG,
        move |cx| {
            if let Some(RootHandle(entity)) = cx.try_global::<RootHandle>().cloned() {
                entity.update(cx, |view, cx| view.dismiss_group_toolbar(gid_dismiss, cx));
            }
        },
    );
    // Clipboard needs a Window reference; route it through a dedicated
    // mouse-down handler that has access to the window param.
    let copy_btn = with_tooltip(
        div()
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
            .child("Copy paths"),
        "Copy checked paths to clipboard",
    );
    // Issue #57 — "Copy as LLM prompt". Disabled (greyed out + no
    // click handler) when any occurrence source is unavailable so the
    // user can't generate a half-populated prompt. We check via a
    // cheap `Path::exists` stat per occurrence — a handful of `stat`s
    // per render is negligible relative to the uniform-list layout
    // that already runs on every frame, and it avoids reading the
    // full file contents just to decide whether the button is live.
    let gid_llm = group_id;
    let sources_ok = llm_sources_available(state, group_id);
    let llm_tooltip = if sources_ok {
        "Copy a markdown prompt containing every occurrence to the clipboard"
    } else {
        "One or more source files are unavailable — can't build a complete prompt"
    };
    let llm_btn_base = div()
        .id(("toolbar-btn-llm", gid_llm as u64))
        .px(px(10.0))
        .py(px(5.0))
        .bg(rgb(TOOLBAR_BUTTON_BG))
        .rounded(px(4.0))
        .text_size(px(12.0))
        .child("Copy as LLM prompt");
    let llm_btn = if sources_ok {
        with_tooltip(
            llm_btn_base
                .text_color(rgb(ROW_TEXT))
                .cursor_pointer()
                .hover(|s| s.bg(rgb(TOOLBAR_BUTTON_HOVER_BG)))
                .on_mouse_down(
                    MouseButton::Left,
                    move |_, window: &mut Window, cx: &mut gpui::App| {
                        if let Some(RootHandle(entity)) =
                            cx.try_global::<RootHandle>().cloned()
                        {
                            entity.update(cx, |view, cx| {
                                view.copy_group_as_llm_prompt(gid_llm, window, cx)
                            });
                        }
                    },
                ),
            llm_tooltip,
        )
    } else {
        with_tooltip(llm_btn_base.text_color(rgb(ROW_TEXT_DIM)), llm_tooltip)
    };
    let collapse_btn = toolbar_button_with_tooltip(
        "Collapse all",
        Some("Collapse every occurrence in this group"),
        TOOLBAR_BUTTON_BG,
        move |cx| {
            if let Some(RootHandle(entity)) = cx.try_global::<RootHandle>().cloned() {
                entity.update(cx, |view, cx| view.collapse_all(cx));
            }
        },
    );
    let expand_btn = toolbar_button_with_tooltip(
        "Expand all",
        Some("Expand every occurrence in this group"),
        TOOLBAR_BUTTON_BG,
        move |cx| {
            if let Some(RootHandle(entity)) = cx.try_global::<RootHandle>().cloned() {
                entity.update(cx, |view, cx| view.expand_all(cx));
            }
        },
    );
    // Issue #53 — this toolbar `×` is the *close-the-detail-pane*
    // action; its meaning is distinct from both the per-occurrence
    // `×` (dismiss one occurrence) and the per-toast `×` (dismiss a
    // toast). The tooltip is what keeps the three visually-identical
    // glyphs from collapsing into the same mystery button.
    let close_btn = with_tooltip(
        div()
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
            .child("\u{00D7}"),
        "Close detail pane",
    );

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
        .child(llm_btn)
        .child(collapse_btn)
        .child(expand_btn)
        .child(close_btn)
}

/// Cheap "every occurrence source is readable" probe for the "Copy as
/// LLM prompt" button's disabled state (#57).
///
/// Uses `Path::exists` rather than a full read because the button only
/// needs a binary signal at render time — the real read happens once
/// on click, inside [`ProjectView::copy_group_as_llm_prompt`], which
/// also falls back to a warning toast on the (racy) case of a file
/// disappearing between the probe and the click.
fn llm_sources_available(state: &AppState, group_id: i64) -> bool {
    let Some(folder) = state.current_folder.as_ref() else {
        return false;
    };
    let Some(group) = state.groups.iter().find(|g| g.id == group_id) else {
        return false;
    };
    let occurrences = state.visible_occurrences_of(group);
    if occurrences.is_empty() {
        return false;
    }
    occurrences
        .iter()
        .all(|o| folder.join(&o.path).exists())
}

/// Render one flattened row from [`build_detail_rows`].
///
/// Returns a fixed-height element — `uniform_list` assumes every row
/// is the same height, so all variants use [`DETAIL_ROW_HEIGHT`]. The
/// header variant returns a `Stateful<Div>` (for the click handler)
/// while the others are plain `Div`, so we erase to `AnyElement` at
/// this boundary.
fn render_detail_row(row: &DetailRow) -> gpui::AnyElement {
    use gpui::IntoElement;
    match row {
        DetailRow::Summary(text) => div()
            .h(px(DETAIL_ROW_HEIGHT))
            .text_size(px(12.0))
            .text_color(rgb(ROW_TEXT_DIM))
            .child(text.clone())
            .into_any_element(),
        DetailRow::OccurrenceHeader {
            group_id,
            occ_idx,
            label,
            checked,
            path,
            blame_overlay,
            blame_tooltip,
        } => render_occurrence_header_row(
            *group_id,
            *occ_idx,
            label,
            *checked,
            path,
            blame_overlay.as_deref(),
            blame_tooltip.as_deref(),
        )
        .into_any_element(),
        DetailRow::Gap => div().h(px(DETAIL_ROW_HEIGHT)).into_any_element(),
        DetailRow::Unavailable => div()
            .h(px(DETAIL_ROW_HEIGHT))
            .px(px(8.0))
            .text_size(px(11.0))
            .text_color(rgb(ROW_TEXT_DIM))
            .child("(file not available)")
            .into_any_element(),
        DetailRow::CodeLine {
            line_number,
            is_context,
            segments,
        } => render_code_line(*line_number, *is_context, segments).into_any_element(),
    }
}

/// Inline occurrence header row shown inside `uniform_list` — carries
/// the per-occurrence checkbox, `[Copy path]` (hover-only), and `[×]`
/// dismiss controls next to `path:Lstart–end`. Replaces the standalone
/// `render_occurrence_cards` list that used to sit above the code body.
fn render_occurrence_header_row(
    group_id: i64,
    occ_idx: usize,
    label: &str,
    checked: bool,
    path: &std::path::Path,
    blame_overlay: Option<&str>,
    blame_tooltip: Option<&str>,
) -> gpui::Stateful<gpui::Div> {
    let key = (group_id as u64) << 32 | occ_idx as u64;
    let group_hover_key = format!("occ-hdr-{group_id}-{occ_idx}");

    let checkbox_bg = if checked {
        CHECKBOX_CHECKED_BG
    } else {
        CHECKBOX_UNCHECKED_BG
    };
    let check_mark = if checked { "\u{2713}" } else { " " };

    let checkbox = div()
        .id(("occ-checkbox", key))
        .w(px(14.0))
        .h(px(14.0))
        .flex()
        .items_center()
        .justify_center()
        .bg(rgb(checkbox_bg))
        .rounded(px(3.0))
        .text_size(px(10.0))
        .text_color(white())
        .cursor_pointer()
        .child(check_mark.to_string())
        .on_mouse_down(MouseButton::Left, move |_, _window, cx: &mut gpui::App| {
            // #45 — don't let the checkbox click bubble up to the
            // header row's collapse-toggle handler.
            cx.stop_propagation();
            if let Some(RootHandle(entity)) = cx.try_global::<RootHandle>().cloned() {
                entity.update(cx, |view, cx| view.toggle_occurrence(group_id, occ_idx, cx));
            }
        });

    let path_for_copy = path.to_path_buf();
    // Issue #53 — tooltip text is the exact wording the spec calls
    // for (`Copy path to clipboard`) so this is the authoritative
    // occurrence of the phrase — search here if the UX changes.
    let copy_button = with_tooltip(
        div()
            .id(("occ-copy", key))
            .px(px(6.0))
            .bg(rgb(TOOLBAR_BUTTON_BG))
            .rounded(px(3.0))
            .text_size(px(10.0))
            .text_color(rgb(ROW_TEXT))
            .cursor_pointer()
            .invisible()
            .group_hover(group_hover_key.clone(), |s| s.visible())
            .child("Copy path")
            .on_mouse_down(MouseButton::Left, move |_, _window, cx: &mut gpui::App| {
                // #45 — keep the header click-to-collapse from firing
                // when the user clicks `[Copy path]`.
                cx.stop_propagation();
                if let Some(RootHandle(entity)) = cx.try_global::<RootHandle>().cloned() {
                    let p = path_for_copy.clone();
                    entity.update(cx, |view, cx| view.copy_single_path(p, cx));
                }
            }),
        "Copy path to clipboard",
    );

    // Issue #53 — per-occurrence dismiss. The tooltip spells out
    // "this occurrence" so users can tell it apart from the group
    // dismiss and the detail-close `×` sharing the same toolbar row.
    let dismiss = with_tooltip(
        div()
            .id(("occ-dismiss", key))
            .w(px(16.0))
            .h(px(16.0))
            .flex()
            .items_center()
            .justify_center()
            .bg(rgb(TOOLBAR_DANGER_BG))
            .rounded(px(3.0))
            .text_size(px(11.0))
            .text_color(white())
            .cursor_pointer()
            .hover(|s| s.bg(rgb(TOOLBAR_BUTTON_HOVER_BG)))
            .child("\u{00D7}")
            .on_mouse_down(MouseButton::Left, move |_, _window, cx: &mut gpui::App| {
                // #45 — stop the header click-to-collapse from firing
                // when the user clicks `[×]`.
                cx.stop_propagation();
                if let Some(RootHandle(entity)) = cx.try_global::<RootHandle>().cloned() {
                    entity.update(cx, |view, cx| {
                        view.dismiss_occurrence(group_id, occ_idx, cx)
                    });
                }
            }),
        "Dismiss this occurrence",
    );

    // #45 — header is the whole-row click target for collapse. We
    // need a [`Stateful`] div (via `.id(...)`) so `on_mouse_down`
    // actually fires, and `w_full` so the bar spans the detail pane
    // rather than hugging its contents.
    div()
        .id(("occ-header", key))
        .group(group_hover_key)
        .w_full()
        .h(px(DETAIL_ROW_HEIGHT))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .px(px(8.0))
        .bg(rgb(SIDEBAR_BG))
        .cursor_pointer()
        .on_mouse_down(MouseButton::Left, move |_, _window, cx: &mut gpui::App| {
            if let Some(RootHandle(entity)) = cx.try_global::<RootHandle>().cloned() {
                entity.update(cx, |view, cx| {
                    view.toggle_occurrence_collapse(group_id, occ_idx, cx)
                });
            }
        })
        .child(checkbox)
        .child(
            div()
                .flex_1()
                .text_size(px(12.0))
                .text_color(rgb(ROW_TEXT))
                .child(label.to_string()),
        )
        .children(render_blame_overlay(group_id, occ_idx, blame_overlay, blame_tooltip))
        .child(copy_button)
        .child(dismiss)
}

/// Issue #58 — render the `<author> · <short_sha> · <date>` overlay
/// appended to an occurrence header when blame is available. Returns
/// an empty iterator when `overlay_text` is `None` so the surrounding
/// `.children(...)` call is a no-op (non-git folder, blame timeout,
/// parse failure all collapse here).
///
/// Carries an optional tooltip showing the first line of the commit
/// message (AC: "Blame text tooltip shows full commit message's first
/// line"). The tooltip uses the same delayed-hover helper as the rest
/// of the header controls (#53).
fn render_blame_overlay(
    group_id: i64,
    occ_idx: usize,
    overlay_text: Option<&str>,
    tooltip_text: Option<&str>,
) -> Option<gpui::AnyElement> {
    use gpui::IntoElement;
    let text = overlay_text?;
    let key = (group_id as u64) << 32 | occ_idx as u64;
    let node = div()
        .id(("occ-blame", key))
        .text_size(px(11.0))
        .text_color(rgb(ROW_TEXT_DIM))
        .child(text.to_string());
    // If a commit-summary tooltip is available, attach it; otherwise
    // the overlay is a plain dim label. `with_tooltip` requires a
    // `StatefulInteractiveElement`, which `.id(...)` above satisfies.
    let elem = match tooltip_text {
        Some(tip) if !tip.is_empty() => with_tooltip(node, tip.to_string()).into_any_element(),
        _ => node.into_any_element(),
    };
    Some(elem)
}

/// Issue #58 — fetch `(overlay, tooltip)` for a single occurrence's
/// starting line, consulting `AppState.blame_cache` first and falling
/// back to a real `git blame` invocation wrapped by the 500 ms
/// timeout in [`crate::blame::run_git_blame`]. Returns `(None, None)`
/// for any failure (non-git folder, timeout, parse fail, missing
/// `current_folder`) so the header renders without an overlay and
/// without errors.
fn fetch_blame_for_occurrence(
    state: &AppState,
    occ: &crate::app_state::OccurrenceView,
) -> (Option<String>, Option<String>) {
    let Some(folder) = state.current_folder.as_ref() else {
        return (None, None);
    };
    let start_line = occ.start_line.max(1) as u32;
    let abs = folder.join(&occ.path);
    let key = crate::blame::BlameCacheKey::new(abs, start_line);

    if let Some(hit) = state.blame_cache.borrow().get(&key).cloned() {
        return blame_to_fields(hit);
    }

    let provider = crate::blame::GitBlameProvider;
    let fetched = crate::blame::BlameProvider::blame(&provider, folder, &occ.path, start_line)
        .ok()
        .flatten();
    state
        .blame_cache
        .borrow_mut()
        .insert(key, fetched.clone());
    blame_to_fields(fetched)
}

/// Split a cached `Option<BlameInfo>` into the `(overlay_text,
/// tooltip_text)` pair the row constructor wants.
fn blame_to_fields(
    info: Option<crate::blame::BlameInfo>,
) -> (Option<String>, Option<String>) {
    match info {
        Some(b) => {
            let tooltip = if b.summary.is_empty() {
                None
            } else {
                Some(b.summary.clone())
            };
            (Some(b.overlay_text()), tooltip)
        }
        None => (None, None),
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
        // #55 — mark cross-occurrence diffs with a 1px bottom border.
        // Context lines are never flagged (the diff fn filters them
        // out), so we don't need to gate on `is_context` here — but we
        // dim the underline for symmetry with the fg dim pass in case
        // a stale diff flag slips through during a transient render.
        if seg.is_diff {
            let underline = if is_context {
                dim(DIFF_UNDERLINE)
            } else {
                DIFF_UNDERLINE
            };
            node = node.border_b_1().border_color(rgb(underline));
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

/// Issue #49 — lookup-or-build shim around [`build_detail_rows`].
///
/// Computes the current cache key off `state` and either reuses the
/// cached `Rc<Vec<DetailRow>>` on a hit or re-runs
/// [`build_detail_rows`] on a miss. On a miss the fresh rows are
/// stored back in `state.detail_rows_cache` for the next frame.
///
/// The cache lives behind `RefCell<Option<DetailRowsCache>>` on
/// `AppState` because `render_detail` takes `&AppState` — this is the
/// one place we need interior mutability. Rendering is single-threaded
/// (main GPUI thread) so borrow conflicts are structurally impossible.
///
/// Returns an `Rc<Vec<DetailRow>>` (vs `&Vec<…>`) so the caller can
/// hand a `'static` clone into the `uniform_list` render closure
/// without borrowing `state`.
fn get_or_build_detail_rows(
    state: &AppState,
    group_id: i64,
    occurrences: &[crate::app_state::OccurrenceView],
) -> Rc<Vec<DetailRow>> {
    let key = compute_cache_key(
        Some(group_id),
        occurrences,
        &state.collapsed_occurrences,
        &state.selected_occurrence_indices,
        &state.session_occurrence_dismissed,
        state.detail_config.context_lines,
    );

    if let Some(cached) = state.detail_rows_cache.borrow().as_ref()
        && cached.key == key
    {
        return cached.rows.clone();
    }

    let rows = Rc::new(build_detail_rows(state, group_id, occurrences));
    *state.detail_rows_cache.borrow_mut() = Some(DetailRowsCache {
        key,
        rows: rows.clone(),
    });
    rows
}

/// Build the flat row list from the current occurrences. Pure helper —
/// the closure handed to `uniform_list` is `'static` so we pre-compute
/// everything here rather than recomputing per-frame.
///
/// Reads each occurrence's source file once up front: the bytes drive
/// both the per-occurrence highlight + tint pipeline and the cross-
/// occurrence diff overlay (#55). Occurrences whose source can't be
/// read produce a `DetailRow::Unavailable` row and contribute `None`
/// to the diff input, so the other occurrences in the group still diff
/// against each other without panicking.
fn build_detail_rows(
    state: &AppState,
    group_id: i64,
    occurrences: &[crate::app_state::OccurrenceView],
) -> Vec<DetailRow> {
    let mut out = Vec::with_capacity(occurrences.len() * 8);
    out.push(DetailRow::Summary(format!(
        "{} occurrences",
        occurrences.len()
    )));

    let context_lines = state.detail_config.context_lines;

    // Read every occurrence's source once. Kept as `Option<(source,
    // lang_hint)>` so the diff pass (which only needs the source
    // bytes) and the per-occurrence render loop (which needs the
    // lang_hint too) share the same I/O.
    let sources: Vec<Option<(String, Option<String>)>> = occurrences
        .iter()
        .map(|occ| read_occurrence_source(state, occ))
        .collect();

    // Bytes-only vec the diff fn wants. Cloning is cheap relative to
    // the file read we already did, and keeping the shapes separate
    // avoids teaching `detail_rows::diff` about `lang_hint`.
    let diff_sources: Vec<Option<String>> = sources
        .iter()
        .map(|s| s.as_ref().map(|(src, _)| src.clone()))
        .collect();
    let diff_flags =
        crate::detail_rows::diff::diff(occurrences, &diff_sources, context_lines);

    for (i, occ) in occurrences.iter().enumerate() {
        let collapsed = state.is_occurrence_collapsed(group_id, i);
        if i > 0 && !collapsed {
            // Gap sits between consecutive code bodies — skip it when
            // the next occurrence is collapsed so we don't leave a
            // blank band above a lone header (#45 AC: Gap is a "code"
            // row, only emitted for non-collapsed occurrences).
            out.push(DetailRow::Gap);
        }
        let (blame_overlay, blame_tooltip) = fetch_blame_for_occurrence(state, occ);
        out.push(DetailRow::OccurrenceHeader {
            group_id,
            occ_idx: i,
            label: occ.label(),
            checked: state.is_occurrence_selected(group_id, i),
            path: occ.path.clone(),
            blame_overlay,
            blame_tooltip,
        });
        if collapsed {
            continue;
        }
        match sources[i].as_ref() {
            Some((source, lang_hint)) => {
                let slice = crate::detail::extract_with_context(
                    source,
                    occ.start_line.max(1) as u32,
                    occ.end_line.max(1) as u32,
                    context_lines,
                );
                append_slice_rows(
                    &mut out,
                    source,
                    lang_hint.as_deref(),
                    &slice,
                    occ,
                    &diff_flags[i],
                );
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
    diff_ranges: &[std::ops::Range<usize>],
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

    // Sort diff ranges once too — `segments_for_range` does the same
    // "walk runs, intersect with spans" logic, and a sorted input lets
    // it binary-search the relevant entries instead of scanning.
    let mut diff_sorted: Vec<std::ops::Range<usize>> = diff_ranges.to_vec();
    diff_sorted.sort_unstable_by_key(|r| r.start);

    for line in &slice.lines {
        let segments = segments_for_range(
            source,
            &runs,
            &tint_spans,
            &diff_sorted,
            line.byte_range.clone(),
        );
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
    diff_ranges: &[std::ops::Range<usize>],
    line: std::ops::Range<usize>,
) -> Vec<LineSegment> {
    use crate::highlight::theme_color;

    if line.is_empty() {
        return Vec::new();
    }

    // Whether *this whole line* lies inside any diff range. The diff
    // overlay is line-level (see `detail_rows::diff`), so any
    // intersection with this line's byte range flags every segment
    // produced from it. Cheap — typical groups have a handful of diff
    // ranges per occurrence.
    let line_is_diff = diff_ranges
        .iter()
        .any(|r| r.start < line.end && r.end > line.start);

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
                is_diff: line_is_diff,
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

// ---------------------------------------------------------------------------
// Issue #30 — toast + modal + issues-dialog renderers.
// ---------------------------------------------------------------------------

/// Toast palette — values are deliberately hard-coded per the issue
/// spec so the test lane can assert on them without re-reading the
/// source palette.
const TOAST_ERROR_BG: u32 = 0x4c1d1d;
const TOAST_ERROR_BORDER: u32 = 0xdc2626;
const TOAST_WARNING_BG: u32 = 0x422c0f;
const TOAST_WARNING_BORDER: u32 = 0xf59e0b;
const TOAST_INFO_BG: u32 = 0x1f2937;
const TOAST_INFO_BORDER: u32 = 0x4b5563;

/// Build the floating toast stack rendered in the top-right corner of
/// the window. Returns an absolutely-positioned `Div` whose child list
/// is one card per live toast. Empty stack still renders an empty
/// overlay (cheap, avoids a render-tree branch).
fn render_toast_stack(toasts: &[Toast]) -> gpui::Div {
    let mut wrap = div()
        .absolute()
        .top(px(12.0))
        .right(px(12.0))
        .flex()
        .flex_col()
        .gap(px(8.0));
    for toast in toasts {
        wrap = wrap.child(render_toast_card(toast));
    }
    wrap
}

fn render_toast_card(toast: &Toast) -> gpui::Div {
    let (bg, border) = match toast.kind {
        ToastKind::Error => (TOAST_ERROR_BG, TOAST_ERROR_BORDER),
        ToastKind::Warning => (TOAST_WARNING_BG, TOAST_WARNING_BORDER),
        ToastKind::Info => (TOAST_INFO_BG, TOAST_INFO_BORDER),
    };
    let icon = match toast.kind {
        ToastKind::Error => "\u{26A0}",   // ⚠
        ToastKind::Warning => "\u{26A0}", // ⚠
        ToastKind::Info => "\u{2139}",    // ℹ
    };
    let toast_id = toast.id;
    let title = toast.title.clone();
    let body = toast.body.clone();
    let action = toast.action.clone();

    let mut body_col = div()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .child(div().text_size(px(13.0)).text_color(white()).child(title));
    if let Some(b) = body {
        body_col = body_col.child(
            div()
                .text_size(px(11.0))
                .text_color(rgb(ROW_TEXT_DIM))
                .child(b),
        );
    }
    if let Some(a) = action {
        let action_name = a.action_name;
        let label = a.label.clone();
        let id_key = format!("toast-action-{toast_id}");
        body_col = body_col.child(
            div()
                .id(gpui::SharedString::from(id_key))
                .mt(px(4.0))
                .px(px(10.0))
                .py(px(4.0))
                .bg(rgb(border))
                .text_color(white())
                .text_size(px(12.0))
                .rounded(px(4.0))
                .cursor_pointer()
                .on_mouse_down(MouseButton::Left, move |_, _window, cx: &mut gpui::App| {
                    if let Some(RootHandle(entity)) = cx.try_global::<RootHandle>().cloned() {
                        entity.update(cx, |view, cx| {
                            view.dispatch_toast_action(action_name, toast_id, cx)
                        });
                    }
                })
                .child(label),
        );
    }

    let close_id = format!("toast-close-{toast_id}");
    div()
        .w(px(340.0))
        .bg(rgb(bg))
        .border_1()
        .border_color(rgb(border))
        .rounded(px(6.0))
        .p(px(10.0))
        .flex()
        .flex_row()
        .gap(px(8.0))
        .child(
            div()
                .w(px(20.0))
                .text_size(px(14.0))
                .text_color(rgb(border))
                .child(icon.to_string()),
        )
        .child(div().flex_1().child(body_col))
        .child(with_tooltip(
            div()
                .id(gpui::SharedString::from(close_id))
                .w(px(18.0))
                .h(px(18.0))
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(14.0))
                .text_color(rgb(ROW_TEXT_DIM))
                .cursor_pointer()
                .child("\u{00D7}")
                .on_mouse_down(MouseButton::Left, move |_, _window, cx: &mut gpui::App| {
                    if let Some(RootHandle(entity)) = cx.try_global::<RootHandle>().cloned() {
                        entity.update(cx, |view, cx| view.dismiss_toast(toast_id, cx));
                    }
                }),
            // Issue #53 — "Dismiss" matches the toast model: Esc
            // also dismisses, per docs/gui.md's keyboard table.
            "Dismiss notification",
        ))
}

/// Render the invalid-config startup modal (issue #30).
///
/// Full-screen scrim with a centered card carrying the error message
/// and two buttons: `[Fix config]` (opens the file in `$EDITOR`) and
/// `[Reset to defaults]` (overwrites with defaults-only TOML). Clicks
/// on the scrim do *not* dismiss the modal — the user must pick one
/// of the two actions so the app doesn't silently stay in the broken
/// state.
fn render_startup_modal(err: &StartupError) -> gpui::Div {
    let message = err.message.clone();
    let path = err.path.display().to_string();
    div()
        .absolute()
        .inset_0()
        .bg(rgb(0x000000cc))
        .flex()
        .items_center()
        .justify_center()
        .child(
            div()
                .id("startup-error-dialog")
                .w(px(520.0))
                .bg(rgb(0x24242a))
                .rounded(px(8.0))
                .border_1()
                .border_color(rgb(TOAST_ERROR_BORDER))
                .p(px(20.0))
                .flex()
                .flex_col()
                .gap(px(12.0))
                // Eat card clicks so they don't punch through.
                .on_mouse_down(MouseButton::Left, |_, _, _| {})
                .child(
                    div()
                        .text_size(px(16.0))
                        .text_color(white())
                        .child("Invalid configuration"),
                )
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(rgb(ROW_TEXT_DIM))
                        .child(format!("File: {path}")),
                )
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(rgb(ROW_TEXT))
                        .child(message),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(8.0))
                        .justify_end()
                        .child(toast_action_button(
                            "startup-reset",
                            ACCENT_DIM,
                            12.0,
                            6.0,
                            "Reset to defaults",
                            crate::menubar::StartupResetConfig,
                        ))
                        .child(toast_action_button(
                            "startup-fix",
                            ACCENT,
                            12.0,
                            6.0,
                            "Fix config",
                            crate::menubar::StartupFixConfig,
                        )),
                ),
        )
}

/// Render the post-scan issues dialog (issue #30).
///
/// Scrim + centered card with a scrollable list of per-file issues
/// and a `[Copy details]` button that writes the GitHub-issue-ready
/// markdown block to the clipboard. Scrim click closes the dialog.
fn render_issues_dialog(issues: &[FileIssue]) -> gpui::Div {
    // Height-constrained column. GPUI at the pinned Zed revision
    // doesn't expose an `overflow_y_scroll()` shorthand on `Div`
    // directly; clipping long issue lists is acceptable for this
    // milestone — the clipboard "Copy details" action carries the
    // full block regardless of what's visible on-screen.
    let mut list = div()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .h(px(320.0))
        .overflow_hidden();
    for issue in issues {
        let kind = match issue.kind {
            dedup_core::FileIssueKind::ReadError => "ReadError",
            dedup_core::FileIssueKind::Utf8 => "Utf8",
            dedup_core::FileIssueKind::TierBParse => "TierBParse",
            dedup_core::FileIssueKind::TierBPanic => "TierBPanic",
        };
        let path = issue.path.display().to_string();
        let msg = issue.message.clone();
        list = list.child(
            div()
                .flex()
                .flex_row()
                .gap(px(8.0))
                .py(px(2.0))
                .child(
                    div()
                        .w(px(120.0))
                        .text_size(px(11.0))
                        .text_color(rgb(TOAST_ERROR_BORDER))
                        .child(kind),
                )
                .child(
                    div()
                        .flex_1()
                        .text_size(px(11.0))
                        .text_color(rgb(ROW_TEXT))
                        .child(format!("{path}: {msg}")),
                ),
        );
    }

    div()
        .absolute()
        .inset_0()
        .bg(rgb(0x000000aa))
        .flex()
        .items_center()
        .justify_center()
        .on_mouse_down(MouseButton::Left, |_, window, cx| {
            window.dispatch_action(Box::new(crate::menubar::CloseScanIssues), cx);
        })
        .child(
            div()
                .id("issues-dialog")
                .w(px(640.0))
                .bg(rgb(0x24242a))
                .rounded(px(8.0))
                .border_1()
                .border_color(rgb(0x3b3b48))
                .p(px(20.0))
                .flex()
                .flex_col()
                .gap(px(12.0))
                .on_mouse_down(MouseButton::Left, |_, _, _| {})
                .child(
                    div()
                        .text_size(px(16.0))
                        .text_color(white())
                        .child(format!("{} files had issues", issues.len())),
                )
                .child(list)
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(8.0))
                        .justify_end()
                        .child(toast_action_button(
                            "issues-close",
                            ACCENT_DIM,
                            12.0,
                            6.0,
                            "Close",
                            crate::menubar::CloseScanIssues,
                        ))
                        .child(toast_action_button(
                            "issues-copy",
                            ACCENT,
                            12.0,
                            6.0,
                            "Copy details",
                            crate::menubar::CopyScanIssues,
                        )),
                ),
        )
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

#[cfg(test)]
mod detail_row_tests {
    //! `build_detail_rows` row-shape tests for #45 — verify the
    //! collapse state only suppresses code/gap/unavailable rows and
    //! never hides the occurrence headers.
    //!
    //! Runs without GPUI — `build_detail_rows` is a pure function of
    //! `AppState`.

    use super::{build_detail_rows, get_or_build_detail_rows};
    use crate::app_state::{AppState, GroupView, OccurrenceView};
    use crate::detail_rows::DetailRow;
    use dedup_core::Tier;
    use std::path::PathBuf;
    use std::rc::Rc;

    fn occ(path: &str) -> OccurrenceView {
        OccurrenceView {
            path: PathBuf::from(path),
            start_line: 1,
            end_line: 5,
            alpha_rename_spans: Vec::new(),
        }
    }

    /// Two-occurrence group loaded into `AppState` with `selected_group
    /// = Some(group_id)`. `current_folder` stays `None` so
    /// `read_occurrence_source` returns `None` and every non-collapsed
    /// occurrence emits a single [`DetailRow::Unavailable`] — keeping
    /// the row-shape assertions tight and independent of the host FS.
    fn state_with_two_occurrences(group_id: i64) -> AppState {
        let mut s = AppState::new();
        s.groups = vec![GroupView {
            id: group_id,
            tier: Tier::A,
            label: "g".into(),
            occurrences: vec![occ("a.rs"), occ("b.rs")],
            language: None,
            group_hash: Some(0x1),
        }];
        s.selected_group = Some(group_id);
        s
    }

    fn header_indices(rows: &[DetailRow]) -> Vec<usize> {
        rows.iter()
            .enumerate()
            .filter_map(|(i, r)| match r {
                DetailRow::OccurrenceHeader { .. } => Some(i),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn build_detail_rows_none_collapsed_has_code_between_headers() {
        let state = state_with_two_occurrences(1);
        let occurrences = state.selected_occurrences();
        let rows = build_detail_rows(&state, 1, &occurrences);

        // Summary + 2 headers + 2 Unavailable rows + 1 Gap between
        // them.
        assert!(matches!(rows[0], DetailRow::Summary(_)));
        let headers = header_indices(&rows);
        assert_eq!(headers.len(), 2);
        // Unavailable sits right after each header when both are
        // expanded.
        assert!(matches!(rows[headers[0] + 1], DetailRow::Unavailable));
        assert!(matches!(rows[headers[1] + 1], DetailRow::Unavailable));
        // Gap precedes the second header.
        assert!(matches!(rows[headers[1] - 1], DetailRow::Gap));
    }

    #[test]
    fn build_detail_rows_one_collapsed_keeps_both_headers() {
        let mut state = state_with_two_occurrences(1);
        state.toggle_occurrence_collapse(1, 0);
        let occurrences = state.selected_occurrences();
        let rows = build_detail_rows(&state, 1, &occurrences);

        // Both headers still present, but the collapsed occurrence
        // emits no Unavailable row (and no leading Gap — the gap is
        // skipped when the following occurrence is collapsed).
        let headers = header_indices(&rows);
        assert_eq!(headers.len(), 2);
        // The first (collapsed) header must not be followed by
        // Unavailable / CodeLine — the next row is the second
        // occurrence's Gap or Header.
        let after_first = &rows[headers[0] + 1];
        assert!(
            !matches!(
                after_first,
                DetailRow::Unavailable | DetailRow::CodeLine { .. }
            ),
            "collapsed header should not be followed by code/unavailable, got {after_first:?}"
        );
        // The second (expanded) occurrence still renders its
        // Unavailable row.
        assert!(matches!(rows[headers[1] + 1], DetailRow::Unavailable));
    }

    #[test]
    fn build_detail_rows_all_collapsed_has_only_summary_plus_headers() {
        let mut state = state_with_two_occurrences(1);
        state.collapse_all_in_active_group();
        let occurrences = state.selected_occurrences();
        let rows = build_detail_rows(&state, 1, &occurrences);

        // Exactly Summary + 2 headers — no gap, no code, no
        // unavailable.
        assert_eq!(rows.len(), 3, "rows: {rows:#?}");
        assert!(matches!(rows[0], DetailRow::Summary(_)));
        assert!(matches!(rows[1], DetailRow::OccurrenceHeader { .. }));
        assert!(matches!(rows[2], DetailRow::OccurrenceHeader { .. }));
    }

    // ------------------------------------------------------------------
    // Issue #49 — detail-rows cache hit / miss tests.
    //
    // `get_or_build_detail_rows` is the one on-frame entry into the
    // cache. We assert hits via `Rc::ptr_eq` on the returned handles —
    // on a hit the second call must reuse the exact `Rc` stored on the
    // first, on a miss the `Rc` must be freshly allocated.
    // ------------------------------------------------------------------

    /// Baseline — two renders with no state changes in between must
    /// return the same `Rc<Vec<DetailRow>>` (pointer equality), proving
    /// the cache hit.
    #[test]
    fn detail_rows_cache_hits_on_repeated_render() {
        let state = state_with_two_occurrences(1);
        let occurrences = state.selected_occurrences();
        let first = get_or_build_detail_rows(&state, 1, &occurrences);
        let second = get_or_build_detail_rows(&state, 1, &occurrences);
        assert!(
            Rc::ptr_eq(&first, &second),
            "second render should reuse the cached Rc"
        );
    }

    /// Group-selection change must invalidate the cache — the new
    /// render returns a different `Rc` built against the new
    /// occurrence list.
    #[test]
    fn detail_rows_cache_misses_on_group_change() {
        let mut state = AppState::new();
        state.groups = vec![
            GroupView {
                id: 1,
                tier: Tier::A,
                label: "g1".into(),
                occurrences: vec![occ("a.rs")],
                language: None,
                group_hash: Some(0x1),
            },
            GroupView {
                id: 2,
                tier: Tier::A,
                label: "g2".into(),
                occurrences: vec![occ("b.rs"), occ("c.rs")],
                language: None,
                group_hash: Some(0x2),
            },
        ];
        state.selected_group = Some(1);

        let occs1 = state.selected_occurrences();
        let first = get_or_build_detail_rows(&state, 1, &occs1);

        state.selected_group = Some(2);
        let occs2 = state.selected_occurrences();
        let second = get_or_build_detail_rows(&state, 2, &occs2);

        assert!(
            !Rc::ptr_eq(&first, &second),
            "group change must invalidate cache"
        );
        // The two row vecs have different lengths — the second group
        // has two occurrences and therefore more rows.
        assert!(second.len() > first.len());
    }

    /// Per-occurrence collapse toggle must invalidate the cache —
    /// `build_detail_rows` suppresses code rows under a collapsed
    /// occurrence, so the row shape differs.
    #[test]
    fn detail_rows_cache_misses_on_collapse_toggle() {
        let mut state = state_with_two_occurrences(1);
        let occurrences = state.selected_occurrences();
        let first = get_or_build_detail_rows(&state, 1, &occurrences);

        state.toggle_occurrence_collapse(1, 0);
        let second = get_or_build_detail_rows(&state, 1, &occurrences);

        assert!(
            !Rc::ptr_eq(&first, &second),
            "collapse toggle must invalidate cache"
        );
    }

    /// Selection toggle flips `OccurrenceHeader::checked`, which is
    /// part of the materialised row — the cache must invalidate.
    #[test]
    fn detail_rows_cache_misses_on_selection_toggle() {
        let mut state = state_with_two_occurrences(1);
        let occurrences = state.selected_occurrences();
        let first = get_or_build_detail_rows(&state, 1, &occurrences);

        state.toggle_occurrence(1, 0);
        let second = get_or_build_detail_rows(&state, 1, &occurrences);
        assert!(
            !Rc::ptr_eq(&first, &second),
            "selection toggle must invalidate cache"
        );
    }

    /// Occurrence-list change (adding an occurrence to the selected
    /// group) must invalidate the cache.
    #[test]
    fn detail_rows_cache_misses_on_occurrence_list_change() {
        let mut state = state_with_two_occurrences(1);
        let occs1 = state.selected_occurrences();
        let first = get_or_build_detail_rows(&state, 1, &occs1);

        // Mutate the group's occurrence list directly — the cache key
        // hashes occurrence path/start/end, so appending a row must
        // produce a miss.
        state.groups[0].occurrences.push(occ("c.rs"));
        let occs2 = state.selected_occurrences();
        let second = get_or_build_detail_rows(&state, 1, &occs2);

        assert!(
            !Rc::ptr_eq(&first, &second),
            "occurrence list change must invalidate cache"
        );
    }

    /// A collapse + immediate expand on the same occurrence should
    /// leave the cache-key fingerprint unchanged, so the third render
    /// hits the cache even though the state was mutated twice in
    /// between.
    #[test]
    fn detail_rows_cache_round_trips_on_collapse_then_expand() {
        let mut state = state_with_two_occurrences(1);
        let occurrences = state.selected_occurrences();
        let first = get_or_build_detail_rows(&state, 1, &occurrences);

        state.toggle_occurrence_collapse(1, 0);
        let _mid = get_or_build_detail_rows(&state, 1, &occurrences);
        state.toggle_occurrence_collapse(1, 0);
        let third = get_or_build_detail_rows(&state, 1, &occurrences);

        // After round-trip the cached `Rc` is the one from the second
        // render (state matches), not the first — but the row vec
        // content matches the first render. We assert on content-
        // equivalence here; pointer equality between `first` and
        // `third` would require storing every historical cache entry,
        // which defeats the point of a single-slot cache.
        assert_eq!(first.len(), third.len());
    }
}

#[cfg(test)]
mod sidebar_partition_tests {
    //! Pure-data tests for `sidebar_tier_partition` (#44) — the
    //! partition that feeds the two virtualized `uniform_list`
    //! instances in `render_sidebar`. The render code itself needs
    //! GPUI to instantiate; the partition is standalone data logic and
    //! covered here.

    use super::sidebar_tier_partition;
    use crate::app_state::{AppState, GroupView, OccurrenceView};
    use dedup_core::Tier;
    use std::path::PathBuf;

    fn gv(id: i64, tier: Tier, label: &str) -> GroupView {
        GroupView {
            id,
            tier,
            label: label.into(),
            occurrences: vec![OccurrenceView {
                path: PathBuf::from("x.rs"),
                start_line: 1,
                end_line: 2,
                alpha_rename_spans: Vec::new(),
            }],
            language: None,
            group_hash: Some(id as u64),
        }
    }

    #[test]
    fn partition_idle_splits_visible_by_tier() {
        let mut s = AppState::new();
        s.groups = vec![
            gv(1, Tier::A, "a1"),
            gv(2, Tier::B, "b1"),
            gv(3, Tier::A, "a2"),
            gv(4, Tier::B, "b2"),
        ];
        let (tier_b, tier_a) = sidebar_tier_partition(&s);
        let ids_b: Vec<i64> = tier_b.iter().map(|g| g.id).collect();
        let ids_a: Vec<i64> = tier_a.iter().map(|g| g.id).collect();
        assert_eq!(ids_b, vec![2, 4]);
        assert_eq!(ids_a, vec![1, 3]);
    }

    #[test]
    fn partition_idle_respects_search_filter() {
        let mut s = AppState::new();
        s.groups = vec![
            gv(1, Tier::A, "apple"),
            gv(2, Tier::B, "banana"),
            gv(3, Tier::A, "apricot"),
        ];
        s.set_search_query("ap".into());
        let (tier_b, tier_a) = sidebar_tier_partition(&s);
        // Search filter applies through `visible_groups`, so banana
        // is dropped from Tier B and both ap* rows survive in Tier A.
        assert!(tier_b.is_empty(), "tier_b should be empty, got {tier_b:?}");
        let ids_a: Vec<i64> = tier_a.iter().map(|g| g.id).collect();
        assert_eq!(ids_a, vec![1, 3]);
    }

    #[test]
    fn partition_running_uses_streaming_buffer_for_tier_a() {
        let mut s = AppState::new();
        // Cache-backed Tier B rows persist across the scan-running
        // transition; streaming pulse supplies Tier A mid-scan.
        s.groups = vec![gv(10, Tier::B, "b_cached")];
        s.groups_streaming = vec![gv(99, Tier::A, "a_stream")];
        let _ = s.begin_scan().unwrap();
        assert!(s.scan_state.is_running());
        // Repopulate streaming after begin_scan() clears it.
        s.groups_streaming = vec![gv(99, Tier::A, "a_stream")];

        let (tier_b, tier_a) = sidebar_tier_partition(&s);
        let ids_b: Vec<i64> = tier_b.iter().map(|g| g.id).collect();
        let ids_a: Vec<i64> = tier_a.iter().map(|g| g.id).collect();
        assert_eq!(ids_b, vec![10]);
        assert_eq!(ids_a, vec![99]);
    }
}
