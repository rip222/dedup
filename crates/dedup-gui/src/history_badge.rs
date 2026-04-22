//! Pure classifier for sidebar history-diff badges (#62).
//!
//! Given a per-group pair of `(base_count, head_count)` occurrence
//! counts (drawn from [`dedup_core::Cache::diff_scans`] output or the
//! raw [`dedup_core::ScanGroupRow`] buckets), returns the badge the
//! sidebar should render on that group row:
//!
//! - [`HistoryBadge::New`]      — group absent from baseline, present now.
//! - [`HistoryBadge::Grew`]     — occurrence count grew (`delta = head - base > 0`).
//! - [`HistoryBadge::Shrank`]   — occurrence count shrank (`delta = base - head > 0`).
//! - [`HistoryBadge::Gone`]     — group was in baseline, absent now.
//! - [`HistoryBadge::Unchanged`] — everything else (same counts).
//!
//! The classifier is intentionally a pure fn so the test suite pins
//! the full truth table as a single golden-value assertion. The GUI
//! layer wraps this with the `HashMap<u64, HistoryBadge>` keyed by
//! group hash that the sidebar renderer consults per row.

use std::collections::{BTreeMap, HashMap};

use dedup_core::{DiffKind, DiffRow, ScanGroupRow};

/// The four badge states the sidebar may paint on a group row relative
/// to a chosen history baseline. `Unchanged` carries no visual — it is
/// represented as "no badge" in the UI — but is a distinct variant so
/// the pure classifier has a total domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HistoryBadge {
    /// Present in head, absent from baseline.
    New,
    /// Present in both; `head_count > base_count`. Carries the delta so
    /// the badge can render `GREW ↑N`.
    Grew { delta: i64 },
    /// Present in both; `head_count < base_count`. Carries the delta so
    /// the badge can render `SHRANK ↓N`.
    Shrank { delta: i64 },
    /// Present in baseline, absent from head. Rendered in the
    /// "Resolved since baseline" section at the bottom of the sidebar.
    Gone,
    /// Present in both with identical counts; no visual badge. Included
    /// so the classifier has a total domain and tests can pin the
    /// no-op case.
    Unchanged,
}

impl HistoryBadge {
    /// Short label used by the sidebar pill. `Unchanged` returns the
    /// empty string — the caller is expected to skip rendering a pill
    /// in that case.
    pub fn label(self) -> String {
        match self {
            HistoryBadge::New => "NEW".to_string(),
            HistoryBadge::Grew { delta } => format!("GREW \u{2191}{delta}"),
            HistoryBadge::Shrank { delta } => format!("SHRANK \u{2193}{delta}"),
            HistoryBadge::Gone => "GONE".to_string(),
            HistoryBadge::Unchanged => String::new(),
        }
    }

    /// Whether the badge renders any visual. Drives the "no pill on
    /// unchanged rows" branch in the sidebar renderer without requiring
    /// an `if let HistoryBadge::Unchanged` at every call site.
    pub fn is_visible(self) -> bool {
        !matches!(self, HistoryBadge::Unchanged)
    }
}

/// Classify a single `(base_count, head_count)` pair. Zero counts
/// encode absence on that side, matching the convention used by
/// [`dedup_core::DiffRow::base_count`] / `head_count`.
///
/// Pure and total: any `(i64, i64)` pair maps to exactly one variant.
pub fn classify(base_count: i64, head_count: i64) -> HistoryBadge {
    match (base_count, head_count) {
        (0, 0) => HistoryBadge::Unchanged,
        (0, h) if h > 0 => HistoryBadge::New,
        (b, 0) if b > 0 => HistoryBadge::Gone,
        (b, h) if h > b => HistoryBadge::Grew { delta: h - b },
        (b, h) if h < b => HistoryBadge::Shrank { delta: b - h },
        _ => HistoryBadge::Unchanged,
    }
}

/// Build the badge map the sidebar renderer consults per row. Keyed by
/// group-hash so lookups at render time are O(1) against the
/// per-row `GroupView::group_hash` breadcrumb.
///
/// `base` and `head` are the raw `scan_groups` rows for the two scans.
/// Groups present in either (or both) scans contribute one entry to
/// the output map. Groups missing from both are irrelevant to the UI
/// and are not inserted (callers treat "missing" as no badge).
pub fn build_badge_map(
    base: &[ScanGroupRow],
    head: &[ScanGroupRow],
) -> HashMap<u64, HistoryBadge> {
    let mut merged: BTreeMap<u64, (i64, i64)> = BTreeMap::new();
    for r in base {
        merged.entry(r.group_hash).or_insert((0, 0)).0 = r.occurrence_count;
    }
    for r in head {
        merged.entry(r.group_hash).or_insert((0, 0)).1 = r.occurrence_count;
    }
    let mut out = HashMap::with_capacity(merged.len());
    for (hash, (b, h)) in merged {
        let badge = classify(b, h);
        // `Unchanged` entries are still useful to the sidebar — they
        // tell us the group was observed in the baseline so the filter
        // chips know the row was part of the comparison. The renderer
        // checks `is_visible()` before painting a pill.
        out.insert(hash, badge);
    }
    out
}

/// Convenience: fold a `Vec<DiffRow>` (already classified by
/// [`dedup_core::Cache::diff_scans`]) into our badge variant. The CLI
/// `diff` path uses `DiffRow` directly; the GUI prefers our richer
/// `HistoryBadge` variant because it carries the numeric delta on the
/// `Grew` / `Shrank` arms, which [`DiffRow`] encodes as the base /
/// head counts but the renderer wants as a single pre-computed `N`.
pub fn from_diff_row(row: &DiffRow) -> HistoryBadge {
    match row.kind {
        DiffKind::New => HistoryBadge::New,
        DiffKind::Gone => HistoryBadge::Gone,
        DiffKind::Grew => HistoryBadge::Grew {
            delta: row.head_count - row.base_count,
        },
        DiffKind::Shrank => HistoryBadge::Shrank {
            delta: row.base_count - row.head_count,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Golden truth-table for the pure classifier. Any drift in the
    /// branch ordering, delta arithmetic, or boundary handling shows up
    /// here as a single failed assert_eq so we don't need per-case
    /// tests for every cell.
    #[test]
    fn classify_golden_table() {
        // (base, head, expected)
        let cases: &[(i64, i64, HistoryBadge)] = &[
            (0, 0, HistoryBadge::Unchanged),
            (0, 1, HistoryBadge::New),
            (0, 5, HistoryBadge::New),
            (1, 0, HistoryBadge::Gone),
            (7, 0, HistoryBadge::Gone),
            (2, 2, HistoryBadge::Unchanged),
            (2, 5, HistoryBadge::Grew { delta: 3 }),
            (2, 3, HistoryBadge::Grew { delta: 1 }),
            (5, 2, HistoryBadge::Shrank { delta: 3 }),
            (3, 2, HistoryBadge::Shrank { delta: 1 }),
            // Large numbers — the arithmetic is plain i64 sub.
            (100, 250, HistoryBadge::Grew { delta: 150 }),
            (250, 100, HistoryBadge::Shrank { delta: 150 }),
        ];
        for (b, h, expected) in cases {
            let got = classify(*b, *h);
            assert_eq!(
                got, *expected,
                "classify({b}, {h}) = {got:?}, want {expected:?}"
            );
        }
    }

    /// Label rendering pins the exact Unicode arrows so the sidebar
    /// always gets `↑` / `↓` and not `^` / `v` substitutes.
    #[test]
    fn label_strings_match_prd() {
        assert_eq!(HistoryBadge::New.label(), "NEW");
        assert_eq!(HistoryBadge::Gone.label(), "GONE");
        assert_eq!(
            HistoryBadge::Grew { delta: 3 }.label(),
            "GREW \u{2191}3",
            "Grew label must render with the up-arrow glyph"
        );
        assert_eq!(
            HistoryBadge::Shrank { delta: 2 }.label(),
            "SHRANK \u{2193}2",
            "Shrank label must render with the down-arrow glyph"
        );
        assert_eq!(HistoryBadge::Unchanged.label(), "");
    }

    #[test]
    fn is_visible_skips_only_unchanged() {
        assert!(HistoryBadge::New.is_visible());
        assert!(HistoryBadge::Gone.is_visible());
        assert!(HistoryBadge::Grew { delta: 1 }.is_visible());
        assert!(HistoryBadge::Shrank { delta: 1 }.is_visible());
        assert!(!HistoryBadge::Unchanged.is_visible());
    }

    fn sgrow(hash: u64, occ: i64) -> ScanGroupRow {
        ScanGroupRow {
            group_hash: hash,
            occurrence_count: occ,
            total_lines: occ * 10,
        }
    }

    /// The map-builder fans out over the union of both scans' groups,
    /// so a hash present only in `base` gets `Gone`, only in `head`
    /// gets `New`, and in both with a count delta gets `Grew`/`Shrank`.
    #[test]
    fn build_badge_map_spans_union_of_both_scans() {
        let base = vec![sgrow(1, 2), sgrow(2, 4), sgrow(3, 3)];
        let head = vec![sgrow(2, 6), sgrow(3, 3), sgrow(4, 1)];
        let map = build_badge_map(&base, &head);
        assert_eq!(map.get(&1).copied(), Some(HistoryBadge::Gone));
        assert_eq!(
            map.get(&2).copied(),
            Some(HistoryBadge::Grew { delta: 2 })
        );
        assert_eq!(map.get(&3).copied(), Some(HistoryBadge::Unchanged));
        assert_eq!(map.get(&4).copied(), Some(HistoryBadge::New));
        assert_eq!(map.len(), 4);
    }

    #[test]
    fn from_diff_row_delegates_to_kind() {
        let row = DiffRow {
            group_hash: 0xaa,
            kind: DiffKind::Grew,
            base_count: 2,
            head_count: 7,
        };
        assert_eq!(from_diff_row(&row), HistoryBadge::Grew { delta: 5 });
        let row = DiffRow {
            group_hash: 0xbb,
            kind: DiffKind::Shrank,
            base_count: 9,
            head_count: 4,
        };
        assert_eq!(from_diff_row(&row), HistoryBadge::Shrank { delta: 5 });
        let row = DiffRow {
            group_hash: 0xcc,
            kind: DiffKind::New,
            base_count: 0,
            head_count: 3,
        };
        assert_eq!(from_diff_row(&row), HistoryBadge::New);
        let row = DiffRow {
            group_hash: 0xdd,
            kind: DiffKind::Gone,
            base_count: 4,
            head_count: 0,
        };
        assert_eq!(from_diff_row(&row), HistoryBadge::Gone);
    }
}
