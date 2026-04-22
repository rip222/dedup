//! "Copy as LLM prompt" pure formatter (issue #57).
//!
//! Builds a paste-ready markdown prompt containing every occurrence of
//! a duplicate group. The output is designed to be dropped into a chat
//! with an LLM so the user can ask it to refactor the duplicates into a
//! single shared helper.
//!
//! Shape of the output: an instruction header, then one section per
//! occurrence consisting of a `### path:start-end` heading followed by
//! a fenced code block with a language hint (e.g. `rust`, `python`).
//! See the snapshot tests in this module for a verbatim example.
//!
//! The module is GPUI-free and has no I/O — callers pass in the already-
//! read source text per occurrence. That keeps the formatter pure (so
//! the detail pane's existing `read_occurrence_source` can feed it) and
//! makes it trivially reusable from the CLI later without dragging in
//! the GPUI runtime.
//!
//! ## Inputs
//!
//! * `group` — the duplicate group. Used for the instruction header
//!   (language + occurrence count).
//! * `occurrences` — the occurrences to include. Must be the *visible*
//!   set (after per-occurrence dismissals), in the order the detail
//!   pane shows them.
//! * `sources` — one entry per occurrence, same length as
//!   `occurrences`. `Some(full_file_source)` when the file was readable;
//!   `None` when the file is missing / unreadable. `None` causes the
//!   caller-facing button to be disabled upstream (per issue #57 AC),
//!   but the formatter still handles the mixed case defensively so a
//!   later streaming caller can't panic us.
//!
//! ## Output guarantees
//!
//! * Trailing newline at end of string (so concatenation is easy).
//! * Exactly one blank line between sections.
//! * Fenced code blocks use triple backticks + language hint derived
//!   from [`crate::highlight::lang_hint_for_path`] so syntax highlight
//!   in the target chat surface works for Rust / Python / TypeScript /
//!   generic extensions.
//! * Line numbers in the section header match `start_line..=end_line`
//!   (1-based, inclusive on both ends — same convention as
//!   [`OccurrenceView`]).

use crate::app_state::{GroupView, OccurrenceView};
use crate::highlight::lang_hint_for_path;

/// Render a paste-ready LLM prompt for `group`.
///
/// `occurrences` and `sources` are parallel slices; `sources[i]` is
/// either the full file contents for `occurrences[i]` (the formatter
/// slices out the `start_line..=end_line` window) or `None` if the file
/// could not be read. `None` entries emit an explanatory stub so the
/// prompt is still syntactically valid markdown even if a partial copy
/// ever reaches this fn.
///
/// Returns an empty string when `occurrences` is empty — the caller
/// (detail-pane button) already disables the control in that case, so
/// this is belt-and-braces defence.
pub fn llm_prompt(
    group: &GroupView,
    occurrences: &[OccurrenceView],
    sources: &[Option<String>],
) -> String {
    if occurrences.is_empty() {
        return String::new();
    }

    let count = occurrences.len();
    let lang_label = group
        .language
        .as_deref()
        .map(|l| l.to_string())
        .unwrap_or_else(|| "code".to_string());

    // Instruction header — tells the LLM what to do with the blocks
    // that follow. Phrasing is intentionally directive (not "please")
    // so the model has a clear task even when the user pastes without
    // adding any of their own words.
    let mut out = String::new();
    out.push_str(&format!(
        "The following {count} {lang_label} snippets were flagged as \
         duplicates by Dedup. Propose a refactor that eliminates the \
         duplication — e.g. extract a shared helper / method / \
         constant — and explain trade-offs.\n\n",
    ));

    for (i, occ) in occurrences.iter().enumerate() {
        let source = sources.get(i).and_then(|s| s.as_ref());
        let snippet = match source {
            Some(full) => slice_lines(full, occ.start_line, occ.end_line),
            None => String::from("(source not available)\n"),
        };
        let lang_hint = lang_hint_for_path(&occ.path).unwrap_or_default();

        out.push_str(&format!(
            "### {path}:{start}-{end}\n\n",
            path = occ.path.display(),
            start = occ.start_line,
            end = occ.end_line,
        ));
        // Fence with a language hint so markdown renderers highlight
        // the block. Empty hint → plain fence.
        if lang_hint.is_empty() {
            out.push_str("```\n");
        } else {
            out.push_str(&format!("```{lang_hint}\n"));
        }
        out.push_str(&snippet);
        if !snippet.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("```\n");
        if i + 1 < occurrences.len() {
            out.push('\n');
        }
    }

    out
}

/// Extract lines `start..=end` (1-based, inclusive) from `source`.
///
/// Clamps out-of-range inputs rather than panicking — a stale cache
/// row against a file that's been edited may reference a line past
/// EOF, and we'd rather emit a short snippet than crash the copy.
/// Always ends with a newline.
fn slice_lines(source: &str, start_line: i64, end_line: i64) -> String {
    let start = start_line.max(1) as usize;
    let end = end_line.max(start_line) as usize;
    let mut out = String::new();
    for (idx, line) in source.lines().enumerate() {
        let lineno = idx + 1;
        if lineno < start {
            continue;
        }
        if lineno > end {
            break;
        }
        out.push_str(line);
        out.push('\n');
    }
    if out.is_empty() {
        out.push('\n');
    }
    out
}

/// Quick check: is every source entry present?
///
/// The detail-pane button uses this to decide whether to enable the
/// "Copy as LLM prompt" control. A missing source means the clipboard
/// output would contain a `(source not available)` stub, which is not
/// useful to paste into a model — better to disable the button and
/// tell the user why via the tooltip.
pub fn all_sources_available(sources: &[Option<String>]) -> bool {
    !sources.is_empty() && sources.iter().all(Option::is_some)
}

/// Disabled-state tooltip text. Exposed so the detail-pane button and
/// any future CLI surface share one source of truth for the copy.
pub fn disabled_tooltip(no_group: bool, missing_sources: bool) -> &'static str {
    match (no_group, missing_sources) {
        (true, _) => "Select a group to copy it as an LLM prompt",
        (_, true) => {
            "One or more source files are unavailable — can't build a \
             complete prompt"
        }
        _ => "Copy as LLM prompt",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_state::{GroupView, OccurrenceView};
    use dedup_core::Tier;
    use std::path::PathBuf;

    fn occ(path: &str, start: i64, end: i64) -> OccurrenceView {
        OccurrenceView {
            path: PathBuf::from(path),
            start_line: start,
            end_line: end,
            alpha_rename_spans: Vec::new(),
        }
    }

    fn group(language: &str, occurrences: Vec<OccurrenceView>) -> GroupView {
        GroupView {
            id: 1,
            tier: Tier::A,
            label: "test".to_string(),
            occurrences,
            language: Some(language.to_string()),
            group_hash: Some(0xdead_beef),
        }
    }

    /// Snapshot — 2-occurrence Rust group. Exercises the "Rust" lang
    /// label in the instruction header + the `rust` fence hint.
    #[test]
    fn snapshot_rust_two_occurrences() {
        // Build deterministic per-line sources so the snippet cut is
        // visible: line number matches the literal line contents, so a
        // regression in the slicing math shows up as a changed snapshot.
        let src_a: String = (1..=12)
            .map(|i| {
                if i >= 10 {
                    format!("FN_A_L{i}\n")
                } else {
                    format!("PAD_A_L{i}\n")
                }
            })
            .collect();
        let src_b: String = (1..=22)
            .map(|i| {
                if i >= 20 {
                    format!("FN_B_L{i}\n")
                } else {
                    format!("PAD_B_L{i}\n")
                }
            })
            .collect();
        let occs = vec![
            occ("src/auth/login.rs", 10, 12),
            occ("src/auth/signup.rs", 20, 22),
        ];
        let g = group("Rust", occs.clone());
        let sources = vec![Some(src_a), Some(src_b)];

        let got = llm_prompt(&g, &occs, &sources);
        let expected = "The following 2 Rust snippets were flagged as duplicates by Dedup. \
                        Propose a refactor that eliminates the duplication — e.g. extract a \
                        shared helper / method / constant — and explain trade-offs.\n\
                        \n\
                        ### src/auth/login.rs:10-12\n\
                        \n\
                        ```rust\n\
                        FN_A_L10\n\
                        FN_A_L11\n\
                        FN_A_L12\n\
                        ```\n\
                        \n\
                        ### src/auth/signup.rs:20-22\n\
                        \n\
                        ```rust\n\
                        FN_B_L20\n\
                        FN_B_L21\n\
                        FN_B_L22\n\
                        ```\n";
        assert_eq!(got, expected);
    }

    /// Snapshot — 3-occurrence Python group. Exercises the `python`
    /// fence hint + the plural-snippet header.
    #[test]
    fn snapshot_python_three_occurrences() {
        let src_a: String = (1..=5)
            .map(|i| format!("py_a_line_{i}\n"))
            .collect();
        let src_b: String = (1..=8)
            .map(|i| format!("py_b_line_{i}\n"))
            .collect();
        let src_c: String = (1..=6)
            .map(|i| format!("py_c_line_{i}\n"))
            .collect();
        let occs = vec![
            occ("pkg/a.py", 1, 3),
            occ("pkg/b.py", 5, 7),
            occ("pkg/c.py", 2, 4),
        ];
        let g = group("Python", occs.clone());
        let sources = vec![Some(src_a), Some(src_b), Some(src_c)];

        let got = llm_prompt(&g, &occs, &sources);
        let expected = "The following 3 Python snippets were flagged as duplicates by Dedup. \
                        Propose a refactor that eliminates the duplication — e.g. extract a \
                        shared helper / method / constant — and explain trade-offs.\n\
                        \n\
                        ### pkg/a.py:1-3\n\
                        \n\
                        ```python\n\
                        py_a_line_1\n\
                        py_a_line_2\n\
                        py_a_line_3\n\
                        ```\n\
                        \n\
                        ### pkg/b.py:5-7\n\
                        \n\
                        ```python\n\
                        py_b_line_5\n\
                        py_b_line_6\n\
                        py_b_line_7\n\
                        ```\n\
                        \n\
                        ### pkg/c.py:2-4\n\
                        \n\
                        ```python\n\
                        py_c_line_2\n\
                        py_c_line_3\n\
                        py_c_line_4\n\
                        ```\n";
        assert_eq!(got, expected);
    }

    #[test]
    fn empty_occurrences_returns_empty_string() {
        let g = group("Rust", Vec::new());
        let got = llm_prompt(&g, &[], &[]);
        assert!(got.is_empty());
    }

    #[test]
    fn missing_source_emits_stub() {
        let occs = vec![occ("src/x.rs", 1, 2)];
        let g = group("Rust", occs.clone());
        let got = llm_prompt(&g, &occs, &[None]);
        assert!(got.contains("(source not available)"));
        assert!(got.contains("```rust"));
    }

    #[test]
    fn all_sources_available_guards() {
        assert!(!all_sources_available(&[]));
        assert!(!all_sources_available(&[Some("a".into()), None]));
        assert!(all_sources_available(&[Some("a".into()), Some("b".into())]));
    }

    #[test]
    fn disabled_tooltip_matches_state() {
        assert_eq!(
            disabled_tooltip(true, false),
            "Select a group to copy it as an LLM prompt"
        );
        assert!(disabled_tooltip(false, true).contains("unavailable"));
        assert_eq!(disabled_tooltip(false, false), "Copy as LLM prompt");
    }

    #[test]
    fn slice_lines_clamps_out_of_range() {
        let src = "a\nb\nc\n";
        // Past EOF — returns empty-with-newline rather than panicking.
        let out = slice_lines(src, 10, 20);
        assert_eq!(out, "\n");
        // Negative start clamps to 1.
        let out = slice_lines(src, -5, 2);
        assert_eq!(out, "a\nb\n");
    }
}
