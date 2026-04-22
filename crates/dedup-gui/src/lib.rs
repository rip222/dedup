//! GPUI-based macOS frontend for dedup.
//!
//! The crate compiles as empty on non-macOS targets via the `cfg` gate
//! below; on macOS it wires up the native NSMenu menubar and the
//! "No project open" empty-state window.
//!
//! Issue #19 sets up the skeleton only — menu item actions and the
//! empty-state button are stubs that log a line and return. The real
//! features (folder open, scan, cancel, recent projects, etc.) land in
//! later issues (#20–#30) per PRD.
//!
//! Issue #16 adds the logging infrastructure the GUI wires up at
//! startup. [`init_logging`] configures a layered [`tracing`]
//! subscriber that writes JSON-formatted events to a daily-rolling file
//! under `~/.config/dedup/logs/`. A companion pruning helper
//! ([`prune_old_logs`]) keeps at most 7 files — `tracing-appender`
//! rotates but does not garbage-collect, so the app calls the helper at
//! startup.
#![cfg(target_os = "macos")]

mod logging;

pub mod app_state;
pub mod detail;
pub mod empty_state;
pub mod highlight;
pub mod menubar;
pub mod project_view;
pub mod recent;
pub mod tint;
pub mod toast;

pub use app_state::{
    AppState, AppStatus, EditorBanner, FolderLoadResult, GroupView, OccurrenceView, Pane,
    RecentBanner, ScanHandles, ScanState, SortKey, StartupError, SummaryCounts, SuppressionView,
    filter_groups, format_completion_banner, format_elapsed, group_label, group_view_from_match,
    impact_key, language_from_path, launch_editor, load_folder, open_in_editor, sort_groups,
    summary,
};
pub use logging::{LogGuard, MAX_LOG_FILES, init_logging, log_dir, prune_old_logs};
pub use project_view::{ProjectView, RootHandle, register_root};
pub use recent::{MAX_RECENTS, RecentProject, RecentProjects, config_dir, recent_file_path};
pub use toast::{
    CacheErrorClass, Toast, ToastAction, ToastKind, ToastStack, classify_cache_error,
    format_issues_clipboard, panic_message,
};

use gpui::{App, AppContext, Bounds, WindowBounds, WindowOptions, px, size};
use gpui_platform::application;

/// Launch the macOS GUI.
///
/// Blocks until the app terminates (e.g. user picks Dedup → Quit).
pub fn run() {
    // Issue #30 — attempt to load the global config before we enter
    // the runloop. A malformed `config.toml` must not crash the GUI;
    // instead we carry the error into the window as a [`StartupError`]
    // and render an inline "Fix config / Reset to defaults" modal. The
    // folder-level layer is loaded per-open, so the check here only
    // exercises the global path. We convert to [`StartupError`] eagerly
    // because `ConfigError` is not `Clone` — the struct form is plain
    // data that the move-closure below can capture trivially.
    let startup_error = dedup_core::Config::load(None)
        .err()
        .as_ref()
        .map(StartupError::from_config_error);

    application().run(move |cx: &mut App| {
        // Hydrate the Open Recent MRU from disk once at startup — the
        // menubar renders off this initial snapshot, and subsequent
        // mutations call `menubar::rebuild_menus` (see `project_view`).
        let initial_recents = recent::RecentProjects::load_from_disk();

        // Install menubar + global action handlers first so shortcuts
        // work even before a window is focused.
        menubar::install(cx, &initial_recents.entries);

        let bounds = Bounds::centered(None, size(px(960.0), px(600.0)), cx);
        let startup_err = startup_error.clone();
        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(gpui::TitlebarOptions {
                        title: Some("Dedup".into()),
                        appears_transparent: false,
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                |window, cx| {
                    let entity = cx.new(|cx| {
                        let mut view = ProjectView::new(cx);
                        view.state.recent_projects = initial_recents.clone();
                        if let Some(err) = startup_err.clone() {
                            view.state.startup_error = Some(err);
                        }
                        view
                    });
                    // Issue #42 — focus the root handle so GPUI has a
                    // non-empty dispatch path for key events; without
                    // this, `cx.on_action` handlers never fire because
                    // the keymap resolves bindings against the focused
                    // element's context tree.
                    let handle = entity.read(cx).focus_handle.clone();
                    window.focus(&handle, cx);
                    entity
                },
            )
            .expect("dedup-gui: failed to open project window");

        // Install the `OpenFolder` handler now that the root entity
        // exists — the handler needs to reach back into this view when
        // the `NSOpenPanel` returns. See `register_root`.
        if let Ok(entity) = window.entity(cx) {
            register_root(entity.clone(), cx);
            // Kick off the toast auto-dismiss ticker — a 500ms timer
            // that drops expired warning/info toasts. Launched here
            // (after `register_root`) so the root handle is already
            // installed for the first tick.
            project_view::start_toast_ticker(entity, cx);
        }

        // Bring the app (and its menubar) to the foreground.
        cx.activate(true);
    });
}

/// Smoke-test entry point — constructs the `Application` value and
/// returns without opening a window or entering the GPUI runloop.
///
/// Invoked by the `dedup-gui --smoke-test` CI step (see
/// `.github/workflows/ci.yml`), which runs on the **main** thread
/// because GPUI requires `App` to be constructed on the main thread
/// (the `cargo test` harness does not — hence this can't be a plain
/// `#[test]`).
pub fn smoke_test() {
    // Constructing `Application` exercises the GPUI platform init path —
    // font stack, scheduler, Cocoa hookup — without actually running the
    // loop or opening a window. This is the cheapest "did it link"
    // assertion we can make in CI.
    let _app = application();
}

#[cfg(test)]
mod tests {
    use crate::menubar;

    /// Sanity-check the menubar tree structure matches issue #19's
    /// acceptance criteria. Pure-data test — no GPUI runtime touched,
    /// so it runs happily off the main thread inside `cargo test`.
    #[test]
    fn menubar_top_level_order_matches_prd() {
        let menus = menubar::build_menus(&[]);
        let names: Vec<&str> = menus.iter().map(|m| m.name.as_ref()).collect();
        assert_eq!(names, ["Dedup", "File", "Scan", "View", "Window", "Help"]);
    }

    /// Every shortcut listed in issue #19's acceptance criteria must be
    /// present in the shortcut table. If a shortcut gets dropped, this
    /// test fails loudly instead of shipping a silently-broken menubar.
    #[test]
    fn menubar_shortcuts_cover_acceptance_criteria() {
        let present: std::collections::HashSet<&str> =
            menubar::SHORTCUTS.iter().map(|&(k, _)| k).collect();
        for required in [
            // Issue #19 acceptance criteria.
            "cmd-,", "cmd-o", "cmd-w", "cmd-r", "cmd-.", "cmd-b", "cmd-1", "cmd-2",
            // Issue #23 acceptance criteria — search + keyboard nav.
            "cmd-f", "j", "k", "up", "down", "enter", "x", "o",
        ] {
            assert!(
                present.contains(required),
                "missing required shortcut {required} — check SHORTCUTS in menubar.rs"
            );
        }
    }

    /// Issue #42 — regression guard for keyboard-shortcut dispatch
    /// wiring. GPUI routes actions via the focused element's
    /// key-context tree; if `ProjectView::render` ever stops calling
    /// both `track_focus` and `key_context` on the root div, or
    /// `lib.rs::run` stops focusing the root handle at window open,
    /// every `cx.on_action` handler silently goes dead again. The
    /// three required call sites live in plain-text source so we can
    /// assert them from a pure-data test without standing up a GPUI
    /// runtime on a worker thread.
    #[test]
    fn project_view_root_establishes_key_context_tree() {
        let view_src = include_str!("project_view.rs");
        assert!(
            view_src.contains(".track_focus(&root_focus)"),
            "project_view.rs render must call track_focus on the root \
             div — otherwise GPUI has no focused element and key \
             bindings never dispatch (issue #42)"
        );
        assert!(
            view_src.contains(".key_context(\"ProjectView\")"),
            "project_view.rs render must set a key_context on the \
             root div so the keymap has a non-empty context stack to \
             match bindings against (issue #42)"
        );
        assert!(
            view_src.contains("focus_handle: FocusHandle"),
            "ProjectView must own a FocusHandle field so the root \
             div can track focus and lib.rs::run can focus it on \
             startup (issue #42)"
        );

        let lib_src = include_str!("lib.rs");
        assert!(
            lib_src.contains("window.focus(&handle, cx)"),
            "lib.rs::run must focus the ProjectView's root handle \
             after opening the window — without an initial focus \
             GPUI's dispatch path is empty and no shortcut fires \
             (issue #42)"
        );
    }
}
