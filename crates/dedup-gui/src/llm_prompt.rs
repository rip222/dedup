//! "Copy as LLM prompt" pure formatter (issue #57).
//!
//! Builds a paste-ready markdown prompt that lists every occurrence of
//! a duplicate group as `path:start-end` references. The agent on the
//! receiving end is expected to read each file itself, so we only emit
//! the locations — embedding the source contents would just pollute the
//! agent's context window.
//!
//! Shape of the output: an instruction header followed by a bullet list
//! of `path:start-end` entries. See the snapshot tests in this module
//! for a verbatim example.
//!
//! ## Inputs
//!
//! * `group` — the duplicate group. Used for the instruction header
//!   (language + occurrence count).
//! * `occurrences` — the occurrences to include. Must be the *visible*
//!   set (after per-occurrence dismissals), in the order the detail
//!   pane shows them.
//!
//! ## Output guarantees
//!
//! * Trailing newline at end of string (so concatenation is easy).
//! * Line numbers in each entry match `start_line..=end_line` (1-based,
//!   inclusive on both ends — same convention as [`OccurrenceView`]).

use crate::app_state::{GroupView, OccurrenceView};

/// Render a paste-ready LLM prompt for `group`.
///
/// Returns an empty string when `occurrences` is empty — the caller
/// (detail-pane button) already disables the control in that case, so
/// this is belt-and-braces defence.
pub fn llm_prompt(group: &GroupView, occurrences: &[OccurrenceView]) -> String {
    if occurrences.is_empty() {
        return String::new();
    }

    let count = occurrences.len();
    let lang_prefix = group
        .language
        .as_deref()
        .map(|l| format!("{l} "))
        .unwrap_or_default();

    let mut out = String::new();
    out.push_str(&format!(
        "The following {count} {lang_prefix}code locations were flagged as \
         duplicates by Dedup. Read each location, then propose a refactor \
         that eliminates the duplication — e.g. extract a shared helper / \
         method / constant — and explain trade-offs.\n\n",
    ));

    for occ in occurrences {
        out.push_str(&format!(
            "- {path}:{start}-{end}\n",
            path = occ.path.display(),
            start = occ.start_line,
            end = occ.end_line,
        ));
    }

    out
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

    #[test]
    fn snapshot_rust_two_occurrences() {
        let occs = vec![
            occ("src/auth/login.rs", 10, 12),
            occ("src/auth/signup.rs", 20, 22),
        ];
        let g = group("Rust", occs.clone());

        let got = llm_prompt(&g, &occs);
        let expected = "The following 2 Rust code locations were flagged as duplicates by Dedup. \
                        Read each location, then propose a refactor that eliminates the \
                        duplication — e.g. extract a shared helper / method / constant — and \
                        explain trade-offs.\n\
                        \n\
                        - src/auth/login.rs:10-12\n\
                        - src/auth/signup.rs:20-22\n";
        assert_eq!(got, expected);
    }

    #[test]
    fn snapshot_python_three_occurrences() {
        let occs = vec![
            occ("pkg/a.py", 1, 3),
            occ("pkg/b.py", 5, 7),
            occ("pkg/c.py", 2, 4),
        ];
        let g = group("Python", occs.clone());

        let got = llm_prompt(&g, &occs);
        let expected = "The following 3 Python code locations were flagged as duplicates by Dedup. \
                        Read each location, then propose a refactor that eliminates the \
                        duplication — e.g. extract a shared helper / method / constant — and \
                        explain trade-offs.\n\
                        \n\
                        - pkg/a.py:1-3\n\
                        - pkg/b.py:5-7\n\
                        - pkg/c.py:2-4\n";
        assert_eq!(got, expected);
    }

    #[test]
    fn empty_occurrences_returns_empty_string() {
        let g = group("Rust", Vec::new());
        let got = llm_prompt(&g, &[]);
        assert!(got.is_empty());
    }

    #[test]
    fn drops_language_label_when_missing() {
        let occs = vec![occ("a.txt", 1, 2)];
        let mut g = group("Rust", occs.clone());
        g.language = None;
        let got = llm_prompt(&g, &occs);
        assert!(got.starts_with("The following 1 code locations"));
        assert!(got.contains("- a.txt:1-2"));
    }
}
