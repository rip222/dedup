//! Dismissed-groups interaction surface (#54).
//!
//! Dismissed groups used to be a static list of hash-hex rows in the
//! sidebar with no click behaviour. Clicking on one now selects the
//! group and shows its occurrences in the detail pane with a read-only
//! banner, so the user can review what they dismissed without digging
//! into the SQLite cache or running CLI subcommands. A `Restore` action
//! (both per-row and on the detail-pane banner) undoes the dismissal.
//!
//! This module owns:
//!
//! - the date / timestamp helpers used by the banner
//! - the click-routing glue that hops from a sidebar row into
//!   `ProjectView::select_group`
//! - the dedicated restore-group and restore-occurrence click handlers
//! - the `render_dismissed_banner` helper that paints the read-only
//!   banner atop the detail pane
//! - the `render_dismissed_row` helper that paints one clickable row
//!   in the sidebar's Dismissed section, complete with per-row
//!   `[Restore]` control
//!
//! `project_view.rs` stays free of dismissed-specific branches beyond
//! a single `if state.selected_dismissed().is_some()` switch in the
//! detail renderer. The view-model logic itself lives in
//! [`crate::app_state`] (`select_dismissed`, `restore_group`,
//! `restore_occurrence`); this module is the GPUI-adjacent glue.

use gpui::{MouseButton, black, div, prelude::*, px, rgb};

use crate::app_state::{AppState, OccurrenceDismissal, SuppressionView};
use crate::project_view::RootHandle;

/// Palette constants shared with `project_view`. Kept local so the
/// banner matches the toolbar's colour language without routing
/// through a third module.
const BANNER_BG: u32 = 0x3a2a2a;
const BANNER_TEXT: u32 = 0xe0d0d0;
const RESTORE_BG: u32 = 0x14532d;
const RESTORE_HOVER_BG: u32 = 0x166634;
const ROW_TEXT: u32 = 0xe0e0e4;
const ROW_TEXT_DIM: u32 = 0xa0a0a8;

/// Format a unix-epoch seconds value as `YYYY-MM-DD` in UTC. Small
/// hand-rolled formatter so we don't pull in `chrono` for one call
/// site — dedup otherwise has no dep on a date library. Days-per-month
/// / leap-year handling is exact for the Gregorian range we actually
/// render (1970+).
pub fn format_dismissed_date(ts: i64) -> String {
    if ts <= 0 {
        return "unknown date".to_string();
    }
    let secs = ts as u64;
    let days = secs / 86_400;
    // Date math via Howard Hinnant's civil_from_days algorithm. Widely
    // used (LLVM libc++, Rust's `time` crate when chrono isn't in
    // scope) and verified correct for every valid `i32` day since
    // -32768-01-01. We operate in unsigned arithmetic keyed on the
    // 0000-03-01 epoch to sidestep signed mod behaviour.
    let z = days as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// Build the text "Dismissed on YYYY-MM-DD" used as the banner's
/// leading copy. Public so unit tests can pin it.
pub fn banner_headline(s: &SuppressionView) -> String {
    format!("Dismissed on {}", format_dismissed_date(s.dismissed_at))
}

/// Render the dismissed-group read-only banner for the currently-
/// selected dismissed group. Returns `None` when the selected group
/// is not dismissed — callers chain it with `.child(...)` via
/// `into_iter().flatten()` / `Option::map`.
///
/// The banner stacks vertically:
///
/// ```text
/// Dismissed on 2025-05-01 — [Restore]
/// Per-occurrence dismissals:
///   src/foo.rs — dismissed 2025-05-02 — [Restore]
///   ...
/// ```
///
/// Per-occurrence rows are emitted only when the dismissed group has
/// any — otherwise the banner stays compact.
pub fn render_dismissed_banner(state: &AppState) -> Option<gpui::Div> {
    let s = state.selected_dismissed()?.clone();
    let headline = banner_headline(&s);
    let has_occurrences = !s.occurrence_dismissals.is_empty();
    let restore_hash = s.hash;
    let restore_btn = restore_button(restore_hash);

    let headline_row = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(10.0))
        .child(
            div()
                .flex_1()
                .text_size(px(12.0))
                .text_color(rgb(BANNER_TEXT))
                .child(format!("{headline} \u{2014}")),
        )
        .child(restore_btn);

    let mut banner = div()
        .w_full()
        .bg(rgb(BANNER_BG))
        .px(px(16.0))
        .py(px(10.0))
        .border_b_1()
        .border_color(black())
        .flex()
        .flex_col()
        .gap(px(6.0))
        .child(headline_row);

    if has_occurrences {
        banner = banner.child(
            div()
                .mt(px(4.0))
                .text_size(px(11.0))
                .text_color(rgb(ROW_TEXT_DIM))
                .child("Per-occurrence dismissals:"),
        );
        for od in &s.occurrence_dismissals {
            banner = banner.child(render_occurrence_dismissal_row(restore_hash, od));
        }
    }

    Some(banner)
}

/// One row under "Per-occurrence dismissals:" — `path — dismissed
/// <date> — [Restore]`.
fn render_occurrence_dismissal_row(hash: u64, od: &OccurrenceDismissal) -> gpui::Div {
    let date = format_dismissed_date(od.dismissed_at);
    let path_display = od.path.display().to_string();
    let path_for_click = od.path.clone();
    let restore = div()
        .id(gpui::ElementId::Name(
            format!("suppr-restore-occ-{hash:016x}-{path_display}").into(),
        ))
        .px(px(8.0))
        .py(px(3.0))
        .bg(rgb(RESTORE_BG))
        .rounded(px(3.0))
        .text_size(px(11.0))
        .text_color(rgb(ROW_TEXT))
        .cursor_pointer()
        .hover(|s| s.bg(rgb(RESTORE_HOVER_BG)))
        .child("Restore")
        .on_mouse_down(MouseButton::Left, move |_, _window, cx: &mut gpui::App| {
            cx.stop_propagation();
            let p = path_for_click.clone();
            if let Some(RootHandle(entity)) = cx.try_global::<RootHandle>().cloned() {
                entity.update(cx, |view, cx| view.restore_occurrence_click(hash, p, cx));
            }
        });

    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .pl(px(8.0))
        .child(
            div()
                .flex_1()
                .text_size(px(11.0))
                .text_color(rgb(BANNER_TEXT))
                .child(format!("{path_display} \u{2014} dismissed {date} \u{2014}")),
        )
        .child(restore)
}

/// Build the banner's primary `[Restore]` button. Also reused by the
/// sidebar row's per-row control so both surfaces route through the
/// same handler.
pub fn restore_button(hash: u64) -> gpui::Stateful<gpui::Div> {
    div()
        .id(gpui::ElementId::Name(
            format!("suppr-restore-{hash:016x}").into(),
        ))
        .px(px(10.0))
        .py(px(4.0))
        .bg(rgb(RESTORE_BG))
        .rounded(px(4.0))
        .text_size(px(12.0))
        .text_color(rgb(ROW_TEXT))
        .cursor_pointer()
        .hover(|s| s.bg(rgb(RESTORE_HOVER_BG)))
        .child("Restore")
        .on_mouse_down(MouseButton::Left, move |_, _window, cx: &mut gpui::App| {
            cx.stop_propagation();
            if let Some(RootHandle(entity)) = cx.try_global::<RootHandle>().cloned() {
                entity.update(cx, |view, cx| view.restore_group_click(hash, cx));
            }
        })
}

/// One clickable row in the sidebar's Dismissed section. Clicking the
/// row selects the group (routing through `ProjectView::select_group`
/// so every observer runs through the same code path); clicking the
/// inline `[Restore]` control short-circuits to restore the group
/// directly.
pub fn render_dismissed_row(s: &SuppressionView, selected: bool) -> gpui::Div {
    let click_id = s.last_group_id;
    let restore_hash = s.hash;
    let label = s.label();
    let row_bg = if selected { rgb(0x3b3b48) } else { rgb(0x0) };

    let label_cell = div()
        .flex_1()
        .text_size(px(11.0))
        .text_color(rgb(ROW_TEXT_DIM))
        .child(label);

    let mut row = div()
        .px(px(16.0))
        .py(px(4.0))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .cursor_pointer()
        .on_mouse_down(MouseButton::Left, move |_, _window, cx: &mut gpui::App| {
            if let Some(RootHandle(entity)) = cx.try_global::<RootHandle>().cloned()
                && let Some(gid) = click_id
            {
                entity.update(cx, |view, cx| view.select_group(gid, cx));
            }
        })
        .child(label_cell)
        .child(restore_button(restore_hash));
    if selected {
        row = row.bg(row_bg);
    }
    row
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_dismissed_date_examples() {
        // 1970-01-01 00:00 UTC.
        assert_eq!(format_dismissed_date(0), "unknown date");
        // One day in.
        assert_eq!(format_dismissed_date(86_400), "1970-01-02");
        // 2024-01-01 00:00 UTC = 1_704_067_200.
        assert_eq!(format_dismissed_date(1_704_067_200), "2024-01-01");
        // Leap day.
        assert_eq!(format_dismissed_date(1_709_164_800), "2024-02-29");
    }

    #[test]
    fn banner_headline_carries_date() {
        let s = SuppressionView {
            hash: 0xabcd,
            hash_hex: "000000000000abcd".into(),
            last_group_id: Some(1),
            dismissed_at: 1_704_067_200,
            occurrences: Vec::new(),
            occurrence_dismissals: Vec::new(),
        };
        assert_eq!(banner_headline(&s), "Dismissed on 2024-01-01");
    }
}
