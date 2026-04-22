//! Delayed-hover tooltip wrapper (issue #53).
//!
//! Every icon button in the GUI (`[×]`, `[Copy path]`, the per-group
//! toolbar close, the sort dropdown, per-toast dismiss) is a single
//! glyph or short label whose meaning changes with context. Without
//! tooltips the user has no way to tell a per-occurrence dismiss from
//! a close-the-detail-pane dismiss — all three show `×`.
//!
//! The module exposes a single public wrapper, [`with_tooltip`], that
//! attaches a short hover label to any `StatefulInteractiveElement`
//! (i.e. any element with an `.id(...)`). Click and hover behaviour
//! of the wrapped element are preserved — `with_tooltip` only layers
//! a tooltip builder on top of whatever listeners the caller already
//! installed.
//!
//! ## Delay
//!
//! GPUI's built-in tooltip delay is 500 ms (see
//! `gpui::elements::div::TOOLTIP_SHOW_DELAY`). The issue calls for
//! "~400 ms"; 500 ms is within that tilde and lives in GPUI, so we
//! reuse the upstream timer rather than rolling our own. If the
//! upstream constant becomes configurable in a later GPUI release,
//! bump to 400 ms here in one place.
//!
//! ## Design
//!
//! `TooltipView` renders a rounded dark pill with the label text —
//! deliberately minimal so it works against both sidebar and detail
//! backgrounds. `with_tooltip(element, "Dismiss this occurrence")`
//! returns the same element with a tooltip builder wired up; the
//! caller chains the result into the surrounding `.child(...)` tree
//! exactly like before.

use gpui::{
    AnyView, App, AppContext, IntoElement, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, Window, div, px, rgb, white,
};

/// Background colour for the tooltip pill (dark neutral that reads on
/// both the sidebar and detail-pane backgrounds).
const TOOLTIP_BG: u32 = 0x18181c;
/// Border colour — a single pixel of accent-dim to separate the pill
/// from deeply dark backgrounds.
const TOOLTIP_BORDER: u32 = 0x3a3a44;

/// The renderable view shown by the tooltip. Holds its own label so
/// the builder closure passed to `.tooltip()` can construct a fresh
/// instance per hover.
struct TooltipView {
    label: SharedString,
}

impl Render for TooltipView {
    fn render(
        &mut self,
        _window: &mut Window,
        _cx: &mut gpui::Context<Self>,
    ) -> impl IntoElement {
        div()
            .bg(rgb(TOOLTIP_BG))
            .border_1()
            .border_color(rgb(TOOLTIP_BORDER))
            .rounded(px(4.0))
            .px(px(6.0))
            .py(px(3.0))
            .text_size(px(11.0))
            .text_color(white())
            .child(self.label.clone())
    }
}

/// Attach a delayed-hover tooltip to any stateful element.
///
/// `element` must be a [`StatefulInteractiveElement`] — i.e. it must
/// already carry an `.id(...)` so GPUI can track hover state across
/// frames. Every icon button in the GUI is already stateful (mouse-
/// down handlers require `.id()`), so this is a non-restriction in
/// practice.
///
/// The tooltip appears after GPUI's built-in show delay (~500 ms,
/// close to the PRD's "~400 ms" target) and disappears as soon as
/// the pointer leaves the element. Tooltips are not hoverable —
/// moving into the tooltip itself hides it, matching standard OS
/// chrome behaviour.
pub fn with_tooltip<E>(element: E, text: impl Into<SharedString>) -> E
where
    E: StatefulInteractiveElement,
{
    let text = text.into();
    element.tooltip(move |_window: &mut Window, cx: &mut App| -> AnyView {
        let label = text.clone();
        cx.new(|_cx| TooltipView { label }).into()
    })
}
