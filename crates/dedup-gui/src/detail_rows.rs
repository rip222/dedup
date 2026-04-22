//! Materialized detail-pane row types + cache (issue #49).
//!
//! `DetailRow` / `LineSegment` are the flat rows the detail pane renders
//! through `uniform_list`. They used to live inside `project_view.rs` and
//! get rebuilt on every frame by `build_detail_rows`, which allocated a
//! fresh `Vec<DetailRow>` + interior `String`s per render. That work
//! dominated detail-pane scroll jank on the dogfood scan.
//!
//! This module lifts the types out of `project_view.rs` so `AppState`
//! can own an `Rc<Vec<DetailRow>>` cache + its invalidation key. The
//! cache key fingerprints the inputs `build_detail_rows` depends on:
//!
//! - the selected group's id + occurrence list
//! - the per-occurrence collapse set, restricted to that group
//! - the per-occurrence selection set, restricted to that group
//! - the session-dismissed occurrence set (which filters
//!   `selected_occurrences`)
//! - the `DetailConfig::context_lines` tunable (changes the number of
//!   `CodeLine` rows per occurrence)
//!
//! `render_detail` hashes those inputs once per frame, compares against
//! the cached key, and reuses the cached `Rc<Vec<DetailRow>>` on a hit —
//! cutting the hot-path cost to a single `Hasher::finish` call.
//!
//! The module is GPUI-free. The render closure in `project_view.rs`
//! consumes `DetailRow` via `render_detail_row`, keeping the cache type
//! independently unit-testable.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::rc::Rc;

use crate::app_state::OccurrenceView;

/// One flattened row of the detail pane's virtualized list (issue #26).
///
/// A group with `N` occurrences and `M` rendered lines per occurrence
/// flattens to `N * (M + spacing_rows) + header_rows` rows, all of
/// which get shoveled into `gpui::uniform_list`. Rows are a uniform
/// pixel height (see `project_view::DETAIL_ROW_HEIGHT`); the list
/// lazy-renders only the visible window, so a group with 100+
/// occurrences scrolls smoothly even though the underlying vec may be
/// tens of thousands of rows long.
#[derive(Debug, Clone)]
pub enum DetailRow {
    /// The `{occurrences.len()} occurrences` preamble at the top of
    /// the pane.
    Summary(String),
    /// One per occurrence — `path:Lstart–end` + inline checkbox /
    /// `[Copy path]` / `[×]` controls.
    OccurrenceHeader {
        group_id: i64,
        occ_idx: usize,
        label: String,
        checked: bool,
        path: PathBuf,
    },
    /// Blank row between consecutive occurrence cards, for visual
    /// separation in the flattened list.
    Gap,
    /// One rendered source line, pre-tokenised into text segments.
    CodeLine {
        /// Absolute 1-based file line number (shown in the gutter).
        line_number: u32,
        /// Whether this line is dimmed context or focus.
        is_context: bool,
        /// Pre-coloured segments for this line, in source order.
        segments: Vec<LineSegment>,
    },
    /// Placeholder when reading the source file failed.
    Unavailable,
}

/// One styled sub-string of a rendered code line.
///
/// `fg_color` is the highlight palette colour; `bg_color` is
/// `Some(rgb)` when the byte range lies inside an alpha-rename tint
/// span (#25).
#[derive(Debug, Clone)]
pub struct LineSegment {
    pub text: String,
    pub fg_color: u32,
    pub bg_color: Option<u32>,
}

/// Fingerprint of every `build_detail_rows` input.
///
/// Stored alongside the cached `Rc<Vec<DetailRow>>` so `render_detail`
/// can skip the rebuild when the fingerprint matches. We collapse the
/// whole state into a single `u64` hash — each mutation that touches a
/// dependency (collapse toggle, selection toggle, dismiss, scan reload,
/// config change) produces a different hash. Cheaper than cloning the
/// inputs and comparing structurally, and fingerprint-size is fixed so
/// the key is always cheap to copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DetailRowsCacheKey {
    /// Selected group id at time of build. `None` when no group is
    /// selected — we still cache the empty result to avoid rebuilding
    /// it while the user pages between empty states.
    pub group_id: Option<i64>,
    /// Rolling hash of the inputs above. A mismatched hash forces a
    /// rebuild.
    pub fingerprint: u64,
}

/// Cached detail rows + the key they were built from.
///
/// Held by `AppState` behind a `RefCell` so `render_detail` (which
/// takes `&AppState`) can populate it on miss. `Rc<Vec<DetailRow>>`
/// rather than `Vec<DetailRow>` so the render closure passed into
/// `uniform_list` can hold its own reference-counted handle without
/// cloning the vec.
#[derive(Debug, Clone)]
pub struct DetailRowsCache {
    pub key: DetailRowsCacheKey,
    pub rows: Rc<Vec<DetailRow>>,
}

/// Compute a `DetailRowsCacheKey` from every `build_detail_rows` input.
///
/// Hashes (in order):
///
/// 1. `group_id`
/// 2. `context_lines`
/// 3. Each occurrence's `path`, `start_line`, `end_line`, and its
///    `alpha_rename_spans` (content affects `CodeLine::segments`).
/// 4. The `(group_id, occ_idx)` collapse flags for this group only.
/// 5. The `(group_id, occ_idx)` selection flags for this group only —
///    they flip `OccurrenceHeader::checked`.
/// 6. The session-dismissed set (it filters `selected_occurrences`, so
///    any mutation changes the occurrence list the cache encodes).
///
/// The session-dismissed set is folded in as an XOR of hashed pairs so
/// the result is order-independent — avoiding a collect+sort on the
/// hot path.
pub fn compute_cache_key(
    group_id: Option<i64>,
    occurrences: &[OccurrenceView],
    collapsed_occurrences: &HashSet<(i64, usize)>,
    selected_occurrence_indices: &HashMap<i64, HashSet<usize>>,
    session_occurrence_dismissed: &HashSet<(u64, PathBuf)>,
    context_lines: usize,
) -> DetailRowsCacheKey {
    use std::collections::hash_map::DefaultHasher;

    let mut hasher = DefaultHasher::new();
    group_id.hash(&mut hasher);
    context_lines.hash(&mut hasher);

    occurrences.len().hash(&mut hasher);
    for occ in occurrences {
        occ.path.hash(&mut hasher);
        occ.start_line.hash(&mut hasher);
        occ.end_line.hash(&mut hasher);
        occ.alpha_rename_spans.len().hash(&mut hasher);
        for span in &occ.alpha_rename_spans {
            span.hash(&mut hasher);
        }
    }

    // Collapse + selection sets are small (bounded by occurrence count
    // of the active group). Restrict to the active group's keys so
    // unrelated mutations don't invalidate the cache.
    if let Some(gid) = group_id {
        // Folded via XOR on per-index hashes so iteration order of the
        // HashSet doesn't leak into the fingerprint.
        let mut collapse_fold: u64 = 0;
        for (g, idx) in collapsed_occurrences.iter() {
            if *g == gid {
                let mut h = DefaultHasher::new();
                idx.hash(&mut h);
                collapse_fold ^= h.finish();
            }
        }
        collapse_fold.hash(&mut hasher);

        let mut select_fold: u64 = 0;
        if let Some(set) = selected_occurrence_indices.get(&gid) {
            for idx in set {
                let mut h = DefaultHasher::new();
                idx.hash(&mut h);
                select_fold ^= h.finish();
            }
        }
        select_fold.hash(&mut hasher);
    }

    // Session-dismissed set affects `selected_occurrences`. Fold
    // order-independently.
    let mut dismiss_fold: u64 = 0;
    for (gh, path) in session_occurrence_dismissed {
        let mut h = DefaultHasher::new();
        gh.hash(&mut h);
        path.hash(&mut h);
        dismiss_fold ^= h.finish();
    }
    dismiss_fold.hash(&mut hasher);

    DetailRowsCacheKey {
        group_id,
        fingerprint: hasher.finish(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_state::OccurrenceView;

    fn occ(path: &str, s: i64, e: i64) -> OccurrenceView {
        OccurrenceView {
            path: PathBuf::from(path),
            start_line: s,
            end_line: e,
            alpha_rename_spans: Vec::new(),
        }
    }

    #[test]
    fn cache_key_stable_on_identical_inputs() {
        let occs = vec![occ("a.rs", 1, 5), occ("b.rs", 10, 20)];
        let collapsed = HashSet::new();
        let selected = HashMap::new();
        let dismissed = HashSet::new();
        let k1 = compute_cache_key(Some(1), &occs, &collapsed, &selected, &dismissed, 3);
        let k2 = compute_cache_key(Some(1), &occs, &collapsed, &selected, &dismissed, 3);
        assert_eq!(k1, k2);
    }

    #[test]
    fn cache_key_changes_on_group_change() {
        let occs = vec![occ("a.rs", 1, 5)];
        let c = HashSet::new();
        let s = HashMap::new();
        let d = HashSet::new();
        let k1 = compute_cache_key(Some(1), &occs, &c, &s, &d, 3);
        let k2 = compute_cache_key(Some(2), &occs, &c, &s, &d, 3);
        assert_ne!(k1, k2);
    }

    #[test]
    fn cache_key_changes_on_collapse_toggle() {
        let occs = vec![occ("a.rs", 1, 5), occ("b.rs", 10, 20)];
        let mut c1 = HashSet::new();
        let c2: HashSet<(i64, usize)> = [(1i64, 0usize)].into_iter().collect();
        let s = HashMap::new();
        let d = HashSet::new();
        let k1 = compute_cache_key(Some(1), &occs, &c1, &s, &d, 3);
        let k2 = compute_cache_key(Some(1), &occs, &c2, &s, &d, 3);
        assert_ne!(k1, k2);

        // Collapse flag on a different group must not invalidate.
        c1.insert((99, 0));
        let k3 = compute_cache_key(Some(1), &occs, &c1, &s, &d, 3);
        let k4 = compute_cache_key(Some(1), &occs, &HashSet::new(), &s, &d, 3);
        assert_eq!(k3, k4);
    }

    #[test]
    fn cache_key_changes_on_occurrences_change() {
        let c = HashSet::new();
        let s = HashMap::new();
        let d = HashSet::new();
        let occs1 = vec![occ("a.rs", 1, 5)];
        let occs2 = vec![occ("a.rs", 1, 5), occ("b.rs", 10, 20)];
        let k1 = compute_cache_key(Some(1), &occs1, &c, &s, &d, 3);
        let k2 = compute_cache_key(Some(1), &occs2, &c, &s, &d, 3);
        assert_ne!(k1, k2);

        // Start line shift invalidates too.
        let occs3 = vec![occ("a.rs", 2, 5)];
        let k3 = compute_cache_key(Some(1), &occs3, &c, &s, &d, 3);
        assert_ne!(k1, k3);
    }

    #[test]
    fn cache_key_changes_on_context_lines() {
        let occs = vec![occ("a.rs", 1, 5)];
        let c = HashSet::new();
        let s = HashMap::new();
        let d = HashSet::new();
        let k1 = compute_cache_key(Some(1), &occs, &c, &s, &d, 3);
        let k2 = compute_cache_key(Some(1), &occs, &c, &s, &d, 5);
        assert_ne!(k1, k2);
    }

    #[test]
    fn cache_key_changes_on_selection_toggle() {
        let occs = vec![occ("a.rs", 1, 5), occ("b.rs", 10, 20)];
        let c = HashSet::new();
        let mut s1: HashMap<i64, HashSet<usize>> = HashMap::new();
        let mut sel: HashSet<usize> = HashSet::new();
        sel.insert(0);
        s1.insert(1, sel);
        let s2: HashMap<i64, HashSet<usize>> = HashMap::new();
        let d = HashSet::new();
        let k1 = compute_cache_key(Some(1), &occs, &c, &s1, &d, 3);
        let k2 = compute_cache_key(Some(1), &occs, &c, &s2, &d, 3);
        assert_ne!(k1, k2);
    }
}
