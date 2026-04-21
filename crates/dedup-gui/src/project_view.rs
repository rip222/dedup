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

use gpui::{Context, MouseButton, Window, black, div, prelude::*, px, rgb, white};

use crate::app_state::{AppState, AppStatus, GroupView, SuppressionView, load_folder};
use crate::menubar::OpenFolder;

// Colors pulled out so the whole view uses one palette — the empty-state
// view already picked `0x1e1e22` as the background, so we match.
const BG: u32 = 0x1e1e22;
const SIDEBAR_BG: u32 = 0x24242a;
const SECTION_HEADER: u32 = 0x9a9aa2;
const ROW_TEXT: u32 = 0xe0e0e4;
const ROW_TEXT_DIM: u32 = 0xa0a0a8;
const ROW_SELECTED_BG: u32 = 0x3b3b48;
const ACCENT: u32 = 0x3b82f6;

/// GPUI view for the main window after a folder is opened.
///
/// Holds the application state directly — the root entity is registered
/// as a global (see [`register_root`]) so the menubar's `OpenFolder`
/// handler can reach back in and update it when the file picker returns.
pub struct ProjectView {
    pub state: AppState,
}

impl ProjectView {
    pub fn new() -> Self {
        Self {
            state: AppState::new(),
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

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(BG))
            .text_color(white())
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
            .text_size(px(11.0))
            .text_color(rgb(ROW_TEXT_DIM))
            .child(format!("Folder: {}", p.display())),
        None => div(),
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
    let tier_b: Vec<&GroupView> = state.tier_b_groups().collect();
    let tier_a: Vec<&GroupView> = state.tier_a_groups().collect();

    div()
        .w(px(320.0))
        .h_full()
        .bg(rgb(SIDEBAR_BG))
        .border_r_1()
        .border_color(black())
        .flex()
        .flex_col()
        .overflow_hidden()
        // Header — folder name
        .child(
            div()
                .p(px(12.0))
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
