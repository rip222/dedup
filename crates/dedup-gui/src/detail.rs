//! Pure extraction helpers for the GUI detail pane (issue #26).
//!
//! Two jobs:
//!
//! 1. [`line_starts`] — precompute the byte offset at which each 1-based
//!    line begins. The caller can then slice `&source[line_starts[L - 1]
//!    .. line_starts[L]]` to get the bytes of line `L` (including the
//!    trailing `\n` if any). The last entry is always `source.len()` so
//!    the "line after the last line" lookup is always safe.
//! 2. [`extract_with_context`] — given a source file, a 1-based inclusive
//!    line range `[start_line, end_line]`, and a desired context-line
//!    count `context`, produce a [`ContextualSlice`] that lists every
//!    line from `start_line - context` through `end_line + context`
//!    (clamped to the file's bounds), tagged so the renderer can dim
//!    context lines and render the focus range with the existing
//!    highlight + tint machinery.
//!
//! Both functions are GPUI-free so the behaviour is testable from
//! `cargo test`. The detail renderer (`project_view.rs::render_detail`)
//! consumes [`ContextualSlice`] and produces one row per line with a
//! gutter, horizontal-scroll code cell, and dimming based on
//! [`LineKind`].

use std::ops::Range;

/// Precompute byte offsets at which each 1-based line begins.
///
/// Returns a vector of length `N + 1` where `N` is the number of lines
/// in `source`, so that `line_starts[i - 1] .. line_starts[i]` always
/// yields the bytes for line `i` (1-based), including any trailing
/// `\n`. The last entry equals `source.len()` — this is the sentinel
/// "end of line N+1" that makes range arithmetic uniform at the edges.
///
/// An empty input returns `vec![0]` (one virtual line 1 of zero length).
/// A file ending in `\n` gets an extra zero-length line at the end,
/// matching most editors' "N+1 lines visible" UX.
pub fn line_starts(source: &str) -> Vec<usize> {
    // Pre-size: one entry per newline plus one for "line 1 starts at 0"
    // plus one sentinel at `source.len()`. Close enough; vec grows
    // transparently if we underestimate.
    let mut out = Vec::with_capacity(source.bytes().filter(|b| *b == b'\n').count() + 2);
    out.push(0);
    for (idx, b) in source.bytes().enumerate() {
        if b == b'\n' {
            out.push(idx + 1);
        }
    }
    // Sentinel so `out[i] .. out[i + 1]` is valid for the last real
    // line. If the file ended in `\n`, `out.last() == source.len()`
    // already and this push creates the (empty) trailing "line N+1"
    // most editors display. Otherwise the push records the final
    // partial line's end offset.
    out.push(source.len());
    out
}

/// Role of a line inside a [`ContextualSlice`].
///
/// The renderer uses this to decide whether to dim the line (context)
/// or keep it at full opacity (focus — the duplicate range itself).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// Dimmed context line before or after the duplicate range.
    Context,
    /// Line that is part of the duplicate range itself.
    Focus,
}

/// One line's worth of extracted source with the metadata the detail
/// renderer needs to paint it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextLine {
    /// 1-based absolute line number within the file — the value to
    /// show in the gutter. Not restarted per occurrence (AC: "real
    /// file line numbers, not restarted").
    pub line_number: u32,
    /// Byte range within the whole-file source that spans this line,
    /// *without* the trailing `\n`. Renderers should clip highlight
    /// runs to this range.
    pub byte_range: Range<usize>,
    /// Context vs. focus.
    pub kind: LineKind,
}

/// Result of [`extract_with_context`].
///
/// Describes the contiguous line window `[focus_start - context ..=
/// focus_end + context]` clamped to the file's real line count. The
/// [`lines`] vector is ordered top-to-bottom and always covers the
/// whole window; the renderer only has to walk it once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextualSlice {
    /// Per-line records, top-to-bottom.
    pub lines: Vec<ContextLine>,
    /// 1-based line number of the first rendered line (= first
    /// `lines` entry's `line_number`). Handy short-circuit for
    /// callers that want to render "starting at line N".
    pub first_line: u32,
    /// 1-based line number of the last rendered line (inclusive).
    pub last_line: u32,
    /// 1-based focus range as clamped to the file — always
    /// `focus_start <= focus_end` and both sit inside
    /// `[first_line, last_line]`.
    pub focus_start: u32,
    pub focus_end: u32,
}

/// Extract the context window around a 1-based inclusive line range.
///
/// - `source` is the whole file.
/// - `start_line` / `end_line` are 1-based inclusive (the cache's line
///   number convention). They are clamped to `[1, total_lines]` so a
///   stale cache still produces a renderable slice rather than a panic.
/// - `context` is the number of lines of context to include on each
///   side; `0` means "only the focus range".
///
/// The function is byte-safe: it slices on line-start boundaries,
/// which are always on a `\n` (ASCII) so no UTF-8 code point can be
/// split. The returned byte ranges include no trailing newline.
pub fn extract_with_context(
    source: &str,
    start_line: u32,
    end_line: u32,
    context: usize,
) -> ContextualSlice {
    let starts = line_starts(source);
    // Compute the file's real line count — every `\n` bumps the count,
    // plus a trailing partial line if the file does not end in `\n`. An
    // empty file still has "line 1" (zero length), matching most
    // editors' gutter. `starts` has `newline_count + 2` entries when the
    // file ends in `\n` (the extra is the post-newline sentinel) and
    // `newline_count + 2` entries otherwise — so the correct count is
    // `starts.len() - 1` minus the trailing-empty slot when present.
    let mut total_lines = (starts.len() - 1).max(1) as u32;
    if source.ends_with('\n') && total_lines > 1 {
        total_lines -= 1;
    }

    // Clamp the focus range into the file's line count. Swap if the
    // caller passed end < start so downstream arithmetic is sane.
    let mut fs = start_line.max(1).min(total_lines);
    let mut fe = end_line.max(1).min(total_lines);
    if fs > fe {
        std::mem::swap(&mut fs, &mut fe);
    }

    // Compute the windowed range, clamped to the file's bounds.
    let ctx = context as u32;
    let window_start = fs.saturating_sub(ctx).max(1);
    let window_end = fe.saturating_add(ctx).min(total_lines);

    let mut lines = Vec::with_capacity((window_end - window_start + 1) as usize);
    for line_no in window_start..=window_end {
        // `starts` is 0-based; line N → starts[N - 1] .. starts[N].
        let lo = starts[(line_no - 1) as usize];
        let hi_with_newline = starts[line_no as usize];
        // Strip a trailing `\n` from the byte range so renderers don't
        // paint an empty trailing span for every line.
        let hi = if hi_with_newline > lo && source.as_bytes()[hi_with_newline - 1] == b'\n' {
            hi_with_newline - 1
        } else {
            hi_with_newline
        };
        let kind = if line_no >= fs && line_no <= fe {
            LineKind::Focus
        } else {
            LineKind::Context
        };
        lines.push(ContextLine {
            line_number: line_no,
            byte_range: lo..hi,
            kind,
        });
    }

    ContextualSlice {
        lines,
        first_line: window_start,
        last_line: window_end,
        focus_start: fs,
        focus_end: fe,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fifty_line_file() -> String {
        // "line-1\nline-2\n…\nline-50\n" — each line has distinct
        // content so byte ranges are easy to eyeball in failures.
        (1..=50)
            .map(|i| format!("line-{i}\n"))
            .collect::<Vec<_>>()
            .join("")
    }

    #[test]
    fn line_starts_matches_expected_offsets() {
        let src = "a\nbb\nccc\n";
        let starts = line_starts(src);
        // Lines: "a" (0..2), "bb" (2..5), "ccc" (5..9), "" (sentinel at 9).
        assert_eq!(starts, vec![0, 2, 5, 9, 9]);
        // Empty file — one line, zero length, plus sentinel.
        assert_eq!(line_starts(""), vec![0, 0]);
        // No trailing newline — last line has no sentinel beyond src.len().
        let src2 = "hi\nthere";
        let starts2 = line_starts(src2);
        assert_eq!(starts2, vec![0, 3, src2.len()]);
    }

    #[test]
    fn context_of_three_around_midfile_range() {
        // AC: range 20–25 with context=3 in a 50-line file ⇒ window
        // 17–28, focus 20–25, context = {17,18,19,26,27,28}.
        let src = fifty_line_file();
        let slice = extract_with_context(&src, 20, 25, 3);
        assert_eq!(slice.first_line, 17);
        assert_eq!(slice.last_line, 28);
        assert_eq!(slice.focus_start, 20);
        assert_eq!(slice.focus_end, 25);

        let line_nos: Vec<u32> = slice.lines.iter().map(|l| l.line_number).collect();
        assert_eq!(line_nos, (17..=28).collect::<Vec<_>>());

        // Before the focus: three context lines.
        let before: Vec<u32> = slice
            .lines
            .iter()
            .filter(|l| l.kind == LineKind::Context && l.line_number < 20)
            .map(|l| l.line_number)
            .collect();
        assert_eq!(before, vec![17, 18, 19]);

        // After the focus: three context lines.
        let after: Vec<u32> = slice
            .lines
            .iter()
            .filter(|l| l.kind == LineKind::Context && l.line_number > 25)
            .map(|l| l.line_number)
            .collect();
        assert_eq!(after, vec![26, 27, 28]);

        // Byte ranges should not include the trailing newline.
        for line in &slice.lines {
            assert!(
                !src.as_bytes()[line.byte_range.clone()].contains(&b'\n'),
                "line {} byte range should exclude trailing newline",
                line.line_number
            );
            let text = &src[line.byte_range.clone()];
            assert_eq!(text, format!("line-{}", line.line_number));
        }
    }

    #[test]
    fn range_at_file_start_clamps_before_lines() {
        // Lines 1–5 with context=3 ⇒ only zero lines before (clamped).
        let src = fifty_line_file();
        let slice = extract_with_context(&src, 1, 5, 3);
        assert_eq!(slice.first_line, 1);
        assert_eq!(slice.last_line, 8);
        assert_eq!(slice.focus_start, 1);
        assert_eq!(slice.focus_end, 5);
        // No context lines before line 1.
        let before: Vec<u32> = slice
            .lines
            .iter()
            .filter(|l| l.kind == LineKind::Context && l.line_number < 1)
            .map(|l| l.line_number)
            .collect();
        assert!(before.is_empty());
        // Three context lines after.
        let after: Vec<u32> = slice
            .lines
            .iter()
            .filter(|l| l.kind == LineKind::Context)
            .map(|l| l.line_number)
            .collect();
        assert_eq!(after, vec![6, 7, 8]);
    }

    #[test]
    fn range_at_file_end_clamps_after_lines() {
        // Last 3 lines: 48..=50. Context=3 ⇒ lines 45..=50 (only 2
        // after-context lines slip in... wait, actually zero. The
        // focus already includes line 50 and there is no line 51+.).
        let src = fifty_line_file();
        let slice = extract_with_context(&src, 48, 50, 3);
        assert_eq!(slice.first_line, 45);
        assert_eq!(slice.last_line, 50);
        // Focus is 48..=50.
        let focus: Vec<u32> = slice
            .lines
            .iter()
            .filter(|l| l.kind == LineKind::Focus)
            .map(|l| l.line_number)
            .collect();
        assert_eq!(focus, vec![48, 49, 50]);
        // Context = 45,46,47; no "after" context available.
        let ctx: Vec<u32> = slice
            .lines
            .iter()
            .filter(|l| l.kind == LineKind::Context)
            .map(|l| l.line_number)
            .collect();
        assert_eq!(ctx, vec![45, 46, 47]);
    }

    #[test]
    fn zero_context_returns_focus_only() {
        let src = fifty_line_file();
        let slice = extract_with_context(&src, 10, 12, 0);
        assert_eq!(slice.first_line, 10);
        assert_eq!(slice.last_line, 12);
        assert_eq!(slice.lines.len(), 3);
        for line in &slice.lines {
            assert_eq!(line.kind, LineKind::Focus);
        }
    }

    #[test]
    fn byte_safe_with_multibyte_chars() {
        // Each line has a multi-byte emoji — the line_starts splitter
        // walks bytes, but since `\n` is ASCII it never splits a code
        // point. Confirm the reported ranges decode as valid UTF-8.
        let src = "α\nβ\nγ\n";
        let slice = extract_with_context(src, 2, 2, 1);
        assert_eq!(slice.first_line, 1);
        assert_eq!(slice.last_line, 3);
        for line in &slice.lines {
            // `&src[range]` would panic on a bad boundary.
            let _ = &src[line.byte_range.clone()];
        }
        assert_eq!(&src[slice.lines[0].byte_range.clone()], "α");
        assert_eq!(&src[slice.lines[1].byte_range.clone()], "β");
        assert_eq!(&src[slice.lines[2].byte_range.clone()], "γ");
    }

    #[test]
    fn stale_range_past_end_still_renders() {
        // Cache is stale — claimed line range 45..60 but file is only
        // 50 lines. The extractor clamps silently rather than panics.
        let src = fifty_line_file();
        let slice = extract_with_context(&src, 45, 60, 2);
        assert_eq!(slice.focus_end, 50);
        assert!(slice.last_line <= 50);
    }

    #[test]
    fn swapped_range_is_normalized() {
        let src = fifty_line_file();
        let swapped = extract_with_context(&src, 20, 10, 2);
        let normal = extract_with_context(&src, 10, 20, 2);
        assert_eq!(swapped, normal);
    }
}
