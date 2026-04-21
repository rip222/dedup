//! Empty-state view shown when no project is open.
//!
//! Per PRD §GUI user story #89: "I don't want a file picker auto-popping
//! on launch; empty-state panel with 'Open Folder…' button instead, so
//! that the app isn't aggressive."
//!
//! Wiring the button to an actual folder picker + cached-results load is
//! issue #20; for #19 the button dispatches the `OpenFolder` menubar
//! action, which currently fires the stub handler (log + no-op).

use gpui::{Context, MouseButton, Window, black, div, prelude::*, px, rgb, white};

use crate::menubar::OpenFolder;

/// GPUI view rendered when the app has no project loaded.
///
/// Layout: centered panel on a neutral background with a "No project
/// open" headline, a short helper line, and an `[Open Folder…]` button.
pub struct EmptyState;

impl EmptyState {
    pub fn new() -> Self {
        Self
    }
}

impl Default for EmptyState {
    fn default() -> Self {
        Self::new()
    }
}

impl Render for EmptyState {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        // Root: fills the window, centers the content panel.
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .bg(rgb(0x1e1e22))
            .text_color(white())
            .child(
                // Content panel.
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
                            .text_color(rgb(0xa0a0a8))
                            .child("Open a folder to scan for duplicate code."),
                    )
                    .child(
                        // Primary action button.
                        //
                        // TODO(#20): wire this to `NSOpenPanel` + cached
                        // results load. For #19 clicking dispatches the
                        // `OpenFolder` action (same one the menubar's
                        // `File → Open…` item fires), which currently hits
                        // the stub handler in `menubar::register_handlers`.
                        div()
                            .mt(px(12.0))
                            .px(px(20.0))
                            .py(px(10.0))
                            .bg(rgb(0x3b82f6))
                            .text_color(white())
                            .text_size(px(14.0))
                            .rounded(px(6.0))
                            .border_1()
                            .border_color(black())
                            .cursor_pointer()
                            .on_mouse_down(MouseButton::Left, |_, window, cx| {
                                // Dispatch the same action the menubar's
                                // File → Open… item fires, so both entry
                                // points converge on one stub today and
                                // on one real handler when #20 lands.
                                window.dispatch_action(Box::new(OpenFolder), cx);
                            })
                            .child("Open Folder…"),
                    ),
            )
    }
}
