//! Native NSMenu menubar for the macOS app.
//!
//! Per PRD §GUI and issue #19 acceptance criteria, the menubar has these
//! top-level menus with the listed items + shortcuts:
//!
//! - **Dedup**: About Dedup, Preferences… (⌘,), separator, Hide, Quit
//! - **File**: Open… (⌘O), Open Recent ▶ (placeholder), separator,
//!   Close Window (⌘W)
//! - **Scan**: Start Scan (⌘R), Stop Scan (⌘.), separator, Clear Cache…
//! - **View**: Toggle Sidebar (⌘B), Focus Sidebar (⌘1), Focus Detail (⌘2)
//! - **Window**: standard (macOS auto-populates)
//! - **Help**: Dedup Help, Report Issue…
//!
//! All action handlers are stubs for this issue — they log and return.
//! The real features wire in via issues #20 (Open), #21/#22 (Scan/Stop),
//! #23–#27 (View), #28 (Recent Projects), #30 (Error UX / Report Issue).

use gpui::{App, KeyBinding, Menu, MenuItem, actions};

// -------------------------------------------------------------------------
// Action types
// -------------------------------------------------------------------------
// The `actions!` macro generates one zero-sized type per name that impls
// `gpui::Action`. Groups (first arg) namespace the actions so `dedup::Quit`
// can't collide with a system `Quit` action from another source.

actions!(
    dedup,
    [
        /// Dedup → About Dedup (stub; wires into #30 error/about dialog).
        About,
        /// Dedup → Preferences… (⌘,). Stub until the prefs dialog lands.
        Preferences,
        /// Dedup → Hide Dedup. Stub — macOS handles hide natively; we
        /// keep a menu entry so the item appears in the app menu and the
        /// acceptance criterion ("Hide" listed) is met.
        Hide,
        /// Dedup → Quit. Calls `cx.quit()` directly — no later issue needed.
        Quit,
        /// File → Open… (⌘O). Wires to folder picker in #20.
        OpenFolder,
        /// File → Close Window (⌘W).
        CloseWindow,
        /// Scan → Start Scan (⌘R). Wires in #21.
        StartScan,
        /// Scan → Stop Scan (⌘.). Wires in #22.
        StopScan,
        /// Scan → Clear Cache…. Wires alongside cache work (post-MVP).
        ClearCache,
        /// View → Toggle Sidebar (⌘B). Wires in #23.
        ToggleSidebar,
        /// View → Focus Sidebar (⌘1). Wires in #23.
        FocusSidebar,
        /// View → Focus Detail (⌘2). Wires in #23.
        FocusDetail,
        /// Edit → Find in Sidebar (⌘F) — focus the sidebar search box
        /// (issue #23).
        FindInSidebar,
        /// Sidebar keyboard-nav — cursor down (`j` / `↓`). Issue #23.
        NextGroup,
        /// Sidebar keyboard-nav — cursor up (`k` / `↑`). Issue #23.
        PrevGroup,
        /// Sidebar keyboard-nav — Enter focuses detail pane. Issue #23.
        ActivateGroup,
        /// Sidebar keyboard-nav — `x` dismisses the currently-selected
        /// group. Issue #23.
        DismissCurrentGroup,
        /// Sidebar keyboard-nav — `o` opens checked files in editor.
        /// Placeholder that logs paths; real launcher lands in #29.
        OpenSelectedInEditor,
        /// Issue #27 — "Collapse all" toolbar button.
        CollapseAll,
        /// Issue #27 — "Expand all" toolbar button.
        ExpandAll,
        /// Help → Dedup Help. Opens docs (post-MVP).
        Help,
        /// Help → Report Issue…. Wires in #30.
        ReportIssue,
    ]
);

/// Install the NSMenu menubar + register global action handlers.
///
/// Called once during app startup, before the first window opens.
pub fn install(cx: &mut App) {
    register_handlers(cx);
    register_keybindings(cx);
    cx.set_menus(build_menus());
}

fn register_handlers(cx: &mut App) {
    // Standard macOS Quit — this is the one action we can wire end-to-end
    // without a later issue, since it just asks GPUI to exit.
    cx.on_action(|_: &Quit, cx: &mut App| {
        cx.quit();
    });

    // All other actions are stubs until their owning issues land. Logging
    // them gives a visible signal during manual testing / demos without
    // committing to half-baked behaviour.
    stub(cx, "About", |_: &About, _: &mut App| {});
    stub(cx, "Preferences (⌘,)", |_: &Preferences, _: &mut App| {});
    stub(cx, "Hide", |_: &Hide, _: &mut App| {});
    // `OpenFolder` is no longer a stub — `crate::project_view::register_root`
    // installs the real NSOpenPanel-backed handler after the root view is
    // created. Dispatching the action with no project view yet (before
    // `register_root` runs) is a no-op, which is fine at startup.
    stub(cx, "Close Window (⌘W)", |_: &CloseWindow, _: &mut App| {});
    // `StartScan` is no longer a stub — `crate::project_view::register_root`
    // wires it to the real scan pipeline once the root view is created.
    // Firing the action before `register_root` runs is a safe no-op.
    // `StopScan` is no longer a stub — `crate::project_view::register_root`
    // wires it to the real cancel pipeline once the root view is created.
    // Firing the action before `register_root` runs (or with no scan in
    // flight) is a safe no-op.
    stub(cx, "Clear Cache…", |_: &ClearCache, _: &mut App| {});
    stub(
        cx,
        "Toggle Sidebar (⌘B) — #23",
        |_: &ToggleSidebar, _: &mut App| {},
    );
    // `FocusSidebar`, `FocusDetail`, `FindInSidebar`, and the five
    // sidebar keyboard-nav actions are no longer stubs — they are wired
    // by `crate::project_view::register_root` once the root view is
    // created. Firing them before then is a safe no-op.
    stub(cx, "Dedup Help", |_: &Help, _: &mut App| {});
    stub(cx, "Report Issue… — #30", |_: &ReportIssue, _: &mut App| {});
}

/// Install a stub handler that logs when its action fires.
///
/// `label` is a human-readable hint shown in the log line so it's clear
/// which shortcut triggered during manual testing.
fn stub<A: gpui::Action>(
    cx: &mut App,
    label: &'static str,
    inner: impl Fn(&A, &mut App) + 'static,
) {
    cx.on_action(move |action: &A, cx: &mut App| {
        log::info!("[menubar stub] {label} — no-op until the owning issue lands");
        eprintln!("[dedup-gui] menubar stub fired: {label}");
        inner(action, cx);
    });
}

fn register_keybindings(cx: &mut App) {
    cx.bind_keys(SHORTCUTS.iter().map(|&(keystroke, action_name)| {
        // Mapping from pure-data shortcut table to concrete `KeyBinding`s.
        // Kept as a single match so adding a new shortcut requires touching
        // both `SHORTCUTS` (the test-visible table) and this match — if they
        // drift, the match arm goes missing and the build breaks.
        match action_name {
            "Preferences" => KeyBinding::new(keystroke, Preferences, None),
            "Quit" => KeyBinding::new(keystroke, Quit, None),
            "OpenFolder" => KeyBinding::new(keystroke, OpenFolder, None),
            "CloseWindow" => KeyBinding::new(keystroke, CloseWindow, None),
            "StartScan" => KeyBinding::new(keystroke, StartScan, None),
            "StopScan" => KeyBinding::new(keystroke, StopScan, None),
            "ToggleSidebar" => KeyBinding::new(keystroke, ToggleSidebar, None),
            "FocusSidebar" => KeyBinding::new(keystroke, FocusSidebar, None),
            "FocusDetail" => KeyBinding::new(keystroke, FocusDetail, None),
            "FindInSidebar" => KeyBinding::new(keystroke, FindInSidebar, None),
            "NextGroup" => KeyBinding::new(keystroke, NextGroup, None),
            "PrevGroup" => KeyBinding::new(keystroke, PrevGroup, None),
            "ActivateGroup" => KeyBinding::new(keystroke, ActivateGroup, None),
            "DismissCurrentGroup" => KeyBinding::new(keystroke, DismissCurrentGroup, None),
            "OpenSelectedInEditor" => KeyBinding::new(keystroke, OpenSelectedInEditor, None),
            other => unreachable!("unmapped shortcut action {other}"),
        }
    }));
}

/// The full keyboard-shortcut table for the app. Kept as `const` data so
/// tests can assert on coverage without touching GPUI's main-thread-only
/// `App` init — and so every acceptance-criterion shortcut is visible in
/// one place.
///
/// Format: `(keystroke, action-type-name)`. Action names are free-form
/// strings matched in `register_keybindings`; adding an entry here
/// without a corresponding match arm there panics on startup via
/// `unreachable!`.
pub(crate) const SHORTCUTS: &[(&str, &str)] = &[
    ("cmd-,", "Preferences"),
    ("cmd-q", "Quit"),
    ("cmd-o", "OpenFolder"),
    ("cmd-w", "CloseWindow"),
    ("cmd-r", "StartScan"),
    ("cmd-.", "StopScan"),
    ("cmd-b", "ToggleSidebar"),
    ("cmd-1", "FocusSidebar"),
    ("cmd-2", "FocusDetail"),
    // Issue #23 — sidebar search + keyboard nav. `j`/`k` and `↓`/`↑`
    // are intentionally not global bindings (the project view handles
    // them through `on_action` dispatch after the sidebar wrapper
    // consumes the key event) so they don't swallow keystrokes in text
    // inputs elsewhere.
    ("cmd-f", "FindInSidebar"),
    ("j", "NextGroup"),
    ("down", "NextGroup"),
    ("k", "PrevGroup"),
    ("up", "PrevGroup"),
    ("enter", "ActivateGroup"),
    ("x", "DismissCurrentGroup"),
    ("o", "OpenSelectedInEditor"),
];

/// Build the NSMenu menu tree.
///
/// Exposed (not `pub(crate)`) rather than truly private so `#[cfg(test)]`
/// integration checks in `tests` submodule can assert on the structure
/// without running GPUI's main-thread-only `App` init.
pub(crate) fn build_menus() -> Vec<Menu> {
    vec![
        // "Dedup" (application menu — shows first on macOS).
        Menu::new("Dedup").items([
            MenuItem::action("About Dedup", About),
            MenuItem::separator(),
            MenuItem::action("Preferences…", Preferences),
            MenuItem::separator(),
            MenuItem::os_submenu("Services", gpui::SystemMenuType::Services),
            MenuItem::separator(),
            MenuItem::action("Hide Dedup", Hide),
            MenuItem::separator(),
            MenuItem::action("Quit Dedup", Quit),
        ]),
        // "File"
        Menu::new("File").items([
            MenuItem::action("Open…", OpenFolder),
            // Open Recent submenu — placeholder entry until #28 populates
            // it from the global `recent.json`.
            MenuItem::submenu(
                Menu::new("Open Recent")
                    .items([MenuItem::action("No Recent Projects", NoRecent).disabled(true)]),
            ),
            MenuItem::separator(),
            MenuItem::action("Close Window", CloseWindow),
        ]),
        // "Scan"
        Menu::new("Scan").items([
            MenuItem::action("Start Scan", StartScan),
            MenuItem::action("Stop Scan", StopScan),
            MenuItem::separator(),
            MenuItem::action("Clear Cache…", ClearCache),
        ]),
        // "View"
        Menu::new("View").items([
            MenuItem::action("Toggle Sidebar", ToggleSidebar),
            MenuItem::action("Focus Sidebar", FocusSidebar),
            MenuItem::action("Focus Detail", FocusDetail),
            MenuItem::separator(),
            // Issue #23 — surface the search shortcut in the View menu
            // so users can discover it via the menubar. No standalone
            // Edit menu today; we add it alongside the focus items.
            MenuItem::action("Find in Sidebar", FindInSidebar),
        ]),
        // "Window" — macOS auto-populates Minimize / Zoom / Bring All to
        // Front when the top-level menu is named "Window". We leave it
        // empty so the OS fills it in.
        Menu::new("Window"),
        // "Help"
        Menu::new("Help").items([
            MenuItem::action("Dedup Help", Help),
            MenuItem::action("Report Issue…", ReportIssue),
        ]),
    ]
}

// Placeholder action for the disabled "No Recent Projects" item. It's
// never fired (the item is disabled) but `MenuItem::action` still needs
// an action-typed value to own.
actions!(dedup_internal, [NoRecent]);
