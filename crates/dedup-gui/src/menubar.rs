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
//!
//! ## Rebuilding the menubar
//!
//! NSMenu is a static tree in GPUI — the `Menu` / `MenuItem` values we
//! hand to [`gpui::App::set_menus`] are owned once and not mutated
//! afterwards. To make "Open Recent" dynamic (issue #28), we expose
//! [`rebuild_menus`], which takes a fresh snapshot of the current
//! [`crate::recent::RecentProjects`] list, re-builds the entire tree,
//! and calls `cx.set_menus` again. Callers invoke it after any mutation
//! of the recents list. `cx.set_menus` is idempotent — no handlers are
//! dropped, no keybindings are reshuffled.

use gpui::{App, KeyBinding, Menu, MenuItem, actions};

use crate::recent::RecentProject;

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
        /// File → Open Recent → (entry 0). Issue #28 — clicking this
        /// opens `AppState.recent_projects.entries[0]`. We use five
        /// indexed unit actions instead of a single
        /// `OpenRecent { path }` so the action types stay zero-sized
        /// and the `actions!` macro (which doesn't take fields) keeps
        /// working. The five-variant shape also makes it obvious that
        /// the menu is capped at MAX_RECENTS = 5.
        OpenRecent0,
        /// File → Open Recent → (entry 1). See [`OpenRecent0`].
        OpenRecent1,
        /// File → Open Recent → (entry 2). See [`OpenRecent0`].
        OpenRecent2,
        /// File → Open Recent → (entry 3). See [`OpenRecent0`].
        OpenRecent3,
        /// File → Open Recent → (entry 4). See [`OpenRecent0`].
        OpenRecent4,
        /// File → Open Recent → Clear Menu. Wipes the MRU + persists.
        /// Issue #28.
        ClearRecents,
        /// Inline banner action — "Remove from recents" (issue #28).
        /// Dismisses the stale-entry banner and drops the offending
        /// path from the MRU. Dispatched by the banner button in the
        /// project view; no menubar entry.
        RemoveStaleRecent,
        /// Inline banner dismiss — hides the stale-entry banner without
        /// touching the MRU. The user can still remove the entry later
        /// via a fresh click. Issue #28.
        DismissRecentBanner,
        /// Inline banner dismiss for the editor-launch banner (issue
        /// #29). Dispatched by the "Dismiss" button on the banner.
        DismissEditorBanner,
        /// Preferences dialog — "Edit config file…" shortcut (issue
        /// #29). Opens the active config TOML in `$EDITOR` so the user
        /// can pick their editor preset. GPUI's text-input primitives
        /// aren't rich enough in this revision for a full modal with
        /// textboxes, so the dialog surfaces this single action instead
        /// (see `project_view::render_preferences_dialog`).
        OpenConfigInEditor,
        /// Close the Preferences dialog without saving (issue #29).
        ClosePreferences,
        /// Issue #30 — dismiss the top-most toast via the global
        /// "Escape" shortcut (only fires when a toast is showing; the
        /// handler is a no-op otherwise). Individual `[×]` clicks
        /// dispatch with a synthesized id via the view layer, so no
        /// action value is needed here for those.
        DismissTopToast,
        /// Issue #30 — open the post-scan issues dialog from the
        /// sidebar's "N files had issues" link.
        ShowScanIssues,
        /// Issue #30 — close the post-scan issues dialog.
        CloseScanIssues,
        /// Issue #30 — "Copy details" button on the post-scan issues
        /// dialog. Writes the GitHub-issue-ready markdown block to the
        /// clipboard.
        CopyScanIssues,
        /// Issue #30 — "Fix config" button on the startup-error modal.
        /// Opens the failing config file in `$EDITOR` so the user can
        /// correct it.
        StartupFixConfig,
        /// Issue #30 — "Reset to defaults" button on the startup-error
        /// modal. Overwrites the failing config with a defaults-only
        /// TOML and retries the load.
        StartupResetConfig,
        /// Issue #30 — "Rescan (overwrites cache)" toast action.
        RescanCache,
        /// Issue #30 — "Delete .dedup/ and rescan" toast action.
        DeleteCacheAndRescan,
    ]
);

/// Install the NSMenu menubar + register global action handlers.
///
/// Called once during app startup, before the first window opens.
/// `initial_recents` is the MRU loaded from `recent.json`; subsequent
/// mutations call [`rebuild_menus`] to swap in a fresh submenu.
pub fn install(cx: &mut App, initial_recents: &[RecentProject]) {
    register_handlers(cx);
    register_keybindings(cx);
    cx.set_menus(build_menus(initial_recents));
}

/// Rebuild the NSMenu tree with a fresh recents snapshot.
///
/// Safe to call from any action handler — GPUI's `set_menus` replaces
/// the owned `Vec<Menu>` on the `App`. Keybindings and action handlers
/// are registered separately (see [`register_handlers`] and
/// [`register_keybindings`]) so they survive across rebuilds.
///
/// Called from `project_view` after every MRU mutation (push on open,
/// remove on stale-entry banner, clear on the Clear Menu item).
pub fn rebuild_menus(cx: &mut App, recents: &[RecentProject]) {
    cx.set_menus(build_menus(recents));
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
    // `Preferences` is no longer a stub — `crate::project_view::register_root`
    // wires it to the inline Preferences dialog once the root view exists.
    // Firing it before then is a safe no-op.
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
    // `ToggleSidebar` is no longer a stub — `crate::project_view::register_root`
    // wires it to the real sidebar-visibility toggle (issue #52) once
    // the root view is created. Firing the action before `register_root`
    // runs is a safe no-op.
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
            // `FindInSidebar` is global (cmd-f) — context predicate `None`
            // so it fires regardless of whether the search input already
            // owns focus (issue #50).
            "FindInSidebar" => KeyBinding::new(keystroke, FindInSidebar, None),
            // The sidebar-navigation bindings carry a `!SearchInput`
            // predicate so plain `j`/`k`/`x`/`o`/`enter`/arrow keys go to
            // the real text input when it is focused instead of
            // triggering list navigation (issue #50 acceptance criterion
            // "printable keystrokes go to the input and do not trigger
            // j/k/x/o actions").
            "NextGroup" => KeyBinding::new(keystroke, NextGroup, Some("!SearchInput")),
            "PrevGroup" => KeyBinding::new(keystroke, PrevGroup, Some("!SearchInput")),
            "ActivateGroup" => KeyBinding::new(keystroke, ActivateGroup, Some("!SearchInput")),
            "DismissCurrentGroup" => {
                KeyBinding::new(keystroke, DismissCurrentGroup, Some("!SearchInput"))
            }
            "OpenSelectedInEditor" => {
                KeyBinding::new(keystroke, OpenSelectedInEditor, Some("!SearchInput"))
            }
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
///
/// `recents` drives the File → Open Recent submenu contents — see
/// [`build_open_recent_submenu`] for the per-entry layout.
pub(crate) fn build_menus(recents: &[RecentProject]) -> Vec<Menu> {
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
            // Open Recent submenu — dynamic per #28. Rebuilt via
            // `rebuild_menus` whenever the MRU mutates.
            MenuItem::submenu(build_open_recent_submenu(recents)),
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

/// Build the File → Open Recent submenu.
///
/// Layout (matches the PRD / issue #28):
/// - Empty MRU → single disabled "No Recent Projects" item. The
///   "Clear Menu" entry is omitted because it would be a no-op.
/// - Non-empty MRU → up to [`crate::recent::MAX_RECENTS`] entries
///   (each labelled via [`RecentProject::menu_label`]), then a
///   separator, then "Clear Menu".
///
/// The per-entry click dispatches `OpenRecentN` where N is the entry's
/// index; `project_view` reads the index back out into
/// `AppState.recent_projects.entries[N].path` so actions stay
/// zero-sized.
fn build_open_recent_submenu(recents: &[RecentProject]) -> Menu {
    if recents.is_empty() {
        return Menu::new("Open Recent")
            .items([MenuItem::action("No Recent Projects", NoRecent).disabled(true)]);
    }

    let mut items: Vec<MenuItem> = Vec::with_capacity(recents.len() + 2);
    for (idx, entry) in recents.iter().enumerate().take(crate::recent::MAX_RECENTS) {
        items.push(menu_item_for_index(idx, entry.menu_label()));
    }
    items.push(MenuItem::separator());
    items.push(MenuItem::action("Clear Menu", ClearRecents));

    Menu::new("Open Recent").items(items)
}

/// Map an MRU index (0..=4) to the matching `OpenRecentN` unit action.
///
/// Kept as a single match so adding / removing an MRU slot is a
/// compile-time error (missing arm) rather than a silent runtime
/// fallthrough. `MAX_RECENTS` is 5; we panic on `idx >= 5` because
/// callers already `take(MAX_RECENTS)`.
fn menu_item_for_index(idx: usize, label: String) -> MenuItem {
    match idx {
        0 => MenuItem::action(label, OpenRecent0),
        1 => MenuItem::action(label, OpenRecent1),
        2 => MenuItem::action(label, OpenRecent2),
        3 => MenuItem::action(label, OpenRecent3),
        4 => MenuItem::action(label, OpenRecent4),
        other => unreachable!(
            "Open Recent submenu index {other} exceeds MAX_RECENTS \
             ({}); callers must take(MAX_RECENTS) first",
            crate::recent::MAX_RECENTS
        ),
    }
}

/// How many recent entries are shown in the File → Open Recent submenu
/// today. Mirrors [`crate::recent::MAX_RECENTS`] but re-exposed here so
/// `project_view` can iterate indices without depending on the recent
/// module directly.
pub const OPEN_RECENT_SLOTS: usize = crate::recent::MAX_RECENTS;
