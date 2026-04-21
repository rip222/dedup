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
pub mod empty_state;
pub mod highlight;
pub mod menubar;
pub mod project_view;

pub use app_state::{
    AppState, AppStatus, FolderLoadResult, GroupView, OccurrenceView, Pane, ScanHandles, ScanState,
    SortKey, SummaryCounts, SuppressionView, filter_groups, format_completion_banner,
    format_elapsed, group_label, group_view_from_match, impact_key, language_from_path,
    load_folder, open_in_editor, sort_groups, summary,
};
pub use logging::{LogGuard, MAX_LOG_FILES, init_logging, log_dir, prune_old_logs};
pub use project_view::{ProjectView, RootHandle, register_root};

use gpui::{App, AppContext, Bounds, WindowBounds, WindowOptions, px, size};
use gpui_platform::application;

/// Launch the macOS GUI.
///
/// Blocks until the app terminates (e.g. user picks Dedup → Quit).
pub fn run() {
    application().run(|cx: &mut App| {
        // Install menubar + global action handlers first so shortcuts
        // work even before a window is focused.
        menubar::install(cx);

        let bounds = Bounds::centered(None, size(px(960.0), px(600.0)), cx);
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
                |_, cx| cx.new(|_| ProjectView::new()),
            )
            .expect("dedup-gui: failed to open project window");

        // Install the `OpenFolder` handler now that the root entity
        // exists — the handler needs to reach back into this view when
        // the `NSOpenPanel` returns. See `register_root`.
        if let Ok(entity) = window.entity(cx) {
            register_root(entity, cx);
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
        let menus = menubar::build_menus();
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
}
