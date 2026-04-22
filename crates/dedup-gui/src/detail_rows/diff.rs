//! Cross-occurrence diff overlay for the detail pane (issue #55).
//!
//! Tier B alpha-rename tinting (#25) colors matching locals so the eye
//! can follow renames, but the same tint blankets the whole body —
//! making byte-level differences between occurrences blend in. This
//! module produces a second overlay that highlights *where*
//! occurrences actually differ so a reviewer can spot the non-trivial
//! deltas at a glance.
//!
//! Algorithm: line-level byte comparison across the focus region of
//! every occurrence in the group. Simple and correct — we walk the
//! focus lines of each occurrence in parallel (by relative position
//! within the focus range) and flag any line whose bytes don't match
//! the bytes of the corresponding line in *every* peer occurrence.
//!
//! Line-level (not char-level LCS) because:
//! - occurrences in a dedup group share identical structure by
//!   construction — they're matches of the same normalized block, so
//!   differences concentrate in identifier substitutions and literal
//!   tweaks that already show up at line granularity;
//! - line ranges are trivially byte ranges (`ContextLine::byte_range`),
//!   so the output plugs straight into the existing segment splitter
//!   without a second intersection pass;
//! - when all occurrences are byte-identical (common Tier A case) the
//!   loop produces zero flags, meeting the "no diff marks on identical
//!   groups" acceptance criterion at zero cost.
//!
//! The function is pure — no I/O, no panics on mixed-language or
//! unequal-length occurrences. Mixed languages work because we compare
//! raw bytes, not tokens. Unequal focus-line counts pad with an empty
//! line so shorter occurrences still flag their trailing peers.

use std::ops::Range;

use crate::app_state::OccurrenceView;
use crate::detail::{ContextualSlice, LineKind, extract_with_context};

/// Per-occurrence list of byte ranges flagged as differing from at
/// least one peer occurrence in the same group. Byte ranges are in
/// absolute file-byte coordinates of that occurrence's source, so the
/// renderer can intersect them against a `ContextLine::byte_range`
/// directly.
pub type DiffFlags = Vec<Range<usize>>;

/// Compute cross-occurrence diff flags for a group.
///
/// Inputs:
/// - `occurrences`: all occurrences in the group, in display order.
/// - `sources`: one entry per occurrence — `Some(full_source)` when
///   the file was readable, `None` otherwise. Length must match
///   `occurrences`.
/// - `context_lines`: same tunable the detail pane uses so this sees
///   the exact byte ranges `build_detail_rows` will render. Context
///   lines are excluded from diffing (they're surrounding file lines,
///   not part of the duplicate body).
///
/// Output: one `DiffFlags` per occurrence. Ranges cover whole lines
/// (no `\n` — matches `ContextLine::byte_range`). An occurrence whose
/// source is `None` gets an empty flag list (nothing to render).
///
/// When `occurrences.len() < 2` the flags are all empty — diffing a
/// single occurrence against itself is vacuous.
pub fn diff(
    occurrences: &[OccurrenceView],
    sources: &[Option<String>],
    context_lines: usize,
) -> Vec<DiffFlags> {
    debug_assert_eq!(occurrences.len(), sources.len());

    let n = occurrences.len();
    let mut out: Vec<DiffFlags> = vec![Vec::new(); n];
    if n < 2 {
        return out;
    }

    // Extract each occurrence's focus lines once — (byte_range, line_text).
    // Context lines are filtered out so diff marks only land on the
    // duplicate body itself.
    let slices: Vec<Option<ContextualSlice>> = occurrences
        .iter()
        .zip(sources.iter())
        .map(|(occ, src)| {
            src.as_ref().map(|s| {
                extract_with_context(
                    s,
                    occ.start_line.max(1) as u32,
                    occ.end_line.max(1) as u32,
                    context_lines,
                )
            })
        })
        .collect();

    // Per-occurrence focus-line texts + byte ranges. Empty vec when
    // the source wasn't readable.
    let focus: Vec<Vec<(Range<usize>, &str)>> = slices
        .iter()
        .enumerate()
        .map(|(i, slice)| match (slice, sources[i].as_deref()) {
            (Some(s), Some(src)) => s
                .lines
                .iter()
                .filter(|l| l.kind == LineKind::Focus)
                .map(|l| (l.byte_range.clone(), &src[l.byte_range.clone()]))
                .collect(),
            _ => Vec::new(),
        })
        .collect();

    // Treat occurrences with no readable source as invisible: they
    // contribute neither a line to be flagged (nothing to paint) nor a
    // peer's missing-line to drive a flag on others. Collect the
    // indices of live occurrences up front.
    let live: Vec<usize> = focus
        .iter()
        .enumerate()
        .filter(|(_, f)| !f.is_empty())
        .map(|(i, _)| i)
        .collect();
    if live.len() < 2 {
        return out;
    }

    // Walk by relative focus-line index. `max_len` covers any live
    // peer so occurrences with extra trailing lines still get their
    // peers' "missing" lines flagged on them.
    let max_len = live.iter().map(|&i| focus[i].len()).max().unwrap_or(0);
    for i in 0..max_len {
        // Collect `(idx, text)` pairs at position i across *live*
        // occurrences only. `None` for a live occurrence means it ran
        // out of focus lines — represented as `""` so a shorter peer
        // still drives a flag on a longer one's extra trailing line.
        let mut texts: Vec<(usize, &str)> = Vec::with_capacity(live.len());
        for &idx in &live {
            let t = focus[idx].get(i).map(|(_, t)| *t).unwrap_or("");
            texts.push((idx, t));
        }

        // For each live occurrence, flag its line if it differs from
        // any live peer.
        for &idx in &live {
            if let Some((range, text)) = focus[idx].get(i) {
                let mine = *text;
                let differs = texts.iter().any(|(j, t)| *j != idx && *t != mine);
                if differs {
                    out[idx].push(range.clone());
                }
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn occ(s: i64, e: i64) -> OccurrenceView {
        OccurrenceView {
            path: PathBuf::from("x.rs"),
            start_line: s,
            end_line: e,
            alpha_rename_spans: Vec::new(),
        }
    }

    #[test]
    fn two_identical_occurrences_produce_no_flags() {
        let a = "fn f() {\n    a + b\n}\n".to_string();
        let b = a.clone();
        let occs = vec![occ(1, 3), occ(1, 3)];
        let flags = diff(&occs, &[Some(a), Some(b)], 0);
        assert_eq!(flags.len(), 2);
        assert!(flags[0].is_empty(), "identical A must have no flags");
        assert!(flags[1].is_empty(), "identical B must have no flags");
    }

    #[test]
    fn two_occurrences_flag_only_differing_line() {
        // Same top + bottom, different middle line.
        let a = "fn f() {\n    a + b\n}\n".to_string();
        let b = "fn f() {\n    x + y\n}\n".to_string();
        let occs = vec![occ(1, 3), occ(1, 3)];
        let flags = diff(&occs, &[Some(a.clone()), Some(b.clone())], 0);
        assert_eq!(flags[0].len(), 1, "A should flag one line");
        assert_eq!(flags[1].len(), 1, "B should flag one line");

        // Flagged range on A must span `    a + b`.
        let fa = &flags[0][0];
        assert_eq!(&a[fa.clone()], "    a + b");
        let fb = &flags[1][0];
        assert_eq!(&b[fb.clone()], "    x + y");
    }

    #[test]
    fn three_occurrences_majority_match_still_flags_odd_one() {
        // Two agree, one differs on middle line — all three flag that
        // line (the agreeing pair flags it because it differs from the
        // third; the third flags it because it differs from the pair).
        let a = "fn f() {\n    a + b\n}\n".to_string();
        let b = a.clone();
        let c = "fn f() {\n    x + y\n}\n".to_string();
        let occs = vec![occ(1, 3), occ(1, 3), occ(1, 3)];
        let flags = diff(
            &occs,
            &[Some(a.clone()), Some(b.clone()), Some(c.clone())],
            0,
        );
        assert_eq!(flags[0].len(), 1);
        assert_eq!(flags[1].len(), 1);
        assert_eq!(flags[2].len(), 1);
    }

    #[test]
    fn single_occurrence_has_no_flags() {
        let a = "fn f() {\n    a\n}\n".to_string();
        let occs = vec![occ(1, 3)];
        let flags = diff(&occs, &[Some(a)], 0);
        assert_eq!(flags.len(), 1);
        assert!(flags[0].is_empty());
    }

    #[test]
    fn missing_source_produces_empty_flags_for_that_occurrence() {
        let a = "fn f() {\n    a\n}\n".to_string();
        let b = "fn f() {\n    b\n}\n".to_string();
        let occs = vec![occ(1, 3), occ(1, 3), occ(1, 3)];
        let flags = diff(&occs, &[Some(a), None, Some(b)], 0);
        // Occurrence with no source: no flags.
        assert!(flags[1].is_empty());
        // The other two still diff against each other and flag their
        // middle lines.
        assert_eq!(flags[0].len(), 1);
        assert_eq!(flags[2].len(), 1);
    }

    #[test]
    fn unequal_focus_lengths_dont_panic() {
        // One occurrence covers 3 lines, another covers 2 — the
        // shorter one can't flag its missing trailing line (no range
        // on its side) but the longer one flags its extra line
        // because the peer's "virtual empty line" differs.
        let a = "l1\nl2\nl3\n".to_string();
        let b = "l1\nl2\n".to_string();
        let occs = vec![occ(1, 3), occ(1, 2)];
        let flags = diff(&occs, &[Some(a), Some(b)], 0);
        // A's 3rd line differs from B's (empty) 3rd — flag it.
        assert_eq!(flags[0].len(), 1);
        // B has no 3rd line, so it can't flag one. First two lines match.
        assert!(flags[1].is_empty());
    }

    #[test]
    fn context_lines_are_not_diffed() {
        // Two files, same focus range but different surrounding
        // context. With context_lines=1 we'd naively see the context
        // differ, but the diff fn filters context out — so the result
        // is empty.
        let a = "ctx_a\nfocus\ntail_a\n".to_string();
        let b = "ctx_b\nfocus\ntail_b\n".to_string();
        let occs = vec![occ(2, 2), occ(2, 2)];
        let flags = diff(&occs, &[Some(a), Some(b)], 1);
        assert!(flags[0].is_empty(), "context lines must not drive diff");
        assert!(flags[1].is_empty());
    }

    #[test]
    fn mixed_language_bytes_dont_panic() {
        // Rust vs Python-ish bytes in the same group — we just compare
        // raw bytes, no tokenizer involved.
        let a = "fn f() {\n    ok\n}\n".to_string();
        let b = "def f():\n    ok\n".to_string();
        let occs = vec![occ(1, 3), occ(1, 2)];
        let flags = diff(&occs, &[Some(a), Some(b)], 0);
        // First line differs (`fn f() {` vs `def f():`), second matches
        // (`    ok`), third only exists on A.
        assert!(flags[0].len() >= 1);
        assert!(flags[1].len() >= 1);
    }
}
