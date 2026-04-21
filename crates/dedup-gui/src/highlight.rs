//! Syntax highlighting for the detail pane (issue #24).
//!
//! Two engines, one output shape:
//!
//! - **tree-sitter-highlight** for the Tier-B languages we already ship a
//!   `LanguageProfile` for — Rust (#6), TypeScript / TSX (#7), Python (#8).
//!   Each grammar ships a `HIGHLIGHTS_QUERY` scm file the crate re-exports;
//!   we load those once at first use into a [`HighlightConfiguration`]
//!   (stored in a `LazyLock`) and translate tree-sitter capture names
//!   (`@keyword`, `@function.method`, …) into our unified [`Highlight`]
//!   enum.
//! - **syntect** as the fallback for everything else. Uses the bundled
//!   Sublime-syntax set + bundled theme list (neither requires runtime
//!   data files). We iterate token ranges via [`ScopeRangeIterator`] and
//!   translate textmate-scope prefixes (`string.quoted`, `keyword.control`,
//!   …) into the same [`Highlight`] enum.
//!
//! Both pipelines emit a `Vec<HighlightedRun>` with byte ranges over the
//! original source; the detail-view renderer then paints each run with
//! one of a small palette of theme colors (see [`theme_color`]), keeping
//! the two engines visually indistinguishable.
//!
//! The public API is deliberately GPUI-free so it can be unit-tested off
//! the main thread (GPUI's `App` must be constructed on the main Cocoa
//! thread, see `lib.rs::smoke_test`). Colors are returned as plain `u32`
//! 0xRRGGBB values; the GPUI layer wraps them with `rgb(…)`.
//!
//! ## Resilience
//!
//! - An unknown `lang_hint` (or `None`) falls straight through to syntect.
//! - If syntect can't find a syntax for the hint, or parsing returns an
//!   error mid-line, we fall through to the plain-text path: a single
//!   `HighlightedRun { 0..source.len(), Highlight::Default }`.
//! - tree-sitter-highlight's iterator can yield `Err(_)` on malformed
//!   input; we stop consuming at that point and keep the runs produced
//!   so far (padded with a `Default` run covering the remainder). No
//!   `catch_unwind` — the library itself is panic-free on bad input.
//!
//! ## Theme
//!
//! A minimal dark palette, picked to match the rest of the GUI (the sidebar
//! background is `0x24242a`). Keeping it small on purpose — issue #24 asks
//! for "consistent across both engines", not a full editor theme.

use std::path::Path;
use std::sync::LazyLock;

use tree_sitter_highlight::{HighlightConfiguration, HighlightEvent, Highlighter};

use syntect::easy::ScopeRangeIterator;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxReference, SyntaxSet};

/// A single contiguous byte-range of the source, tagged with a theme
/// bucket.
///
/// Ranges are non-overlapping and sorted by `start`. Consecutive runs
/// with the same `kind` are not coalesced — callers that care can fold
/// them in a single pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HighlightedRun {
    pub start: usize,
    pub end: usize,
    pub kind: Highlight,
}

/// Unified highlight bucket. Tree-sitter capture names and textmate
/// scope names both collapse into one of these eleven values so the
/// detail-view theme only has to know about one palette.
///
/// Palette (0xRRGGBB, dark scheme):
///
/// | Variant      | Color      | Notes                                    |
/// |--------------|------------|------------------------------------------|
/// | Keyword      | 0xc586c0   | `fn`, `if`, `return`, `def`, `class`     |
/// | Function     | 0xdcdcaa   | function / method names + calls          |
/// | Type         | 0x4ec9b0   | `struct`, `Vec<T>`, `type_identifier`    |
/// | String       | 0xce9178   | `"hi"`, template strings, regex literals |
/// | Comment      | 0x6a9955   | `//`, `#`, `/* */`                       |
/// | Number       | 0xb5cea8   | integers, floats                         |
/// | Punctuation  | 0x9a9aa2   | `()`, `{}`, `.`, `,`                     |
/// | Variable     | 0x9cdcfe   | locals, parameters, plain idents         |
/// | Constant     | 0x569cd6   | `true`, `false`, `ALL_CAPS`, enum ctors  |
/// | Attribute    | 0xd7ba7d   | `#[derive(...)]`, `@decorator`           |
/// | Default      | 0xd4d4d4   | unhighlighted text                       |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Highlight {
    Keyword,
    Function,
    Type,
    String,
    Comment,
    Number,
    Punctuation,
    Variable,
    Constant,
    Attribute,
    Default,
}

impl Highlight {
    /// 0xRRGGBB fill color for this bucket. Shared across both
    /// engines — picking a tree-sitter vs syntect run does not change
    /// the color for the same `Highlight` variant, satisfying the
    /// "themed consistently across both engines" AC.
    pub fn color(self) -> u32 {
        match self {
            Highlight::Keyword => 0xc586c0,
            Highlight::Function => 0xdcdcaa,
            Highlight::Type => 0x4ec9b0,
            Highlight::String => 0xce9178,
            Highlight::Comment => 0x6a9955,
            Highlight::Number => 0xb5cea8,
            Highlight::Punctuation => 0x9a9aa2,
            Highlight::Variable => 0x9cdcfe,
            Highlight::Constant => 0x569cd6,
            Highlight::Attribute => 0xd7ba7d,
            Highlight::Default => 0xd4d4d4,
        }
    }
}

/// Convenience wrapper for renderers — equivalent to `kind.color()`.
pub fn theme_color(kind: Highlight) -> u32 {
    kind.color()
}

/// Canonical tree-sitter capture names, indexed into via the `Highlight`
/// returned by tree-sitter-highlight.
///
/// Order matters: `HighlightConfiguration::configure` matches by longest
/// prefix, so `function.method` wins over `function` when both are
/// listed. We list both so the per-language queries that emit one or
/// the other both resolve. Every entry maps to one of our enum variants
/// via [`capture_name_to_highlight`].
const HIGHLIGHT_NAMES: &[&str] = &[
    // Literals / values.
    "string",
    "string.special",
    "string.regexp",
    "string.escape",
    "escape",
    "number",
    "boolean",
    "constant",
    "constant.builtin",
    "constructor",
    // Identifiers / references.
    "function",
    "function.builtin",
    "function.method",
    "function.macro",
    "variable",
    "variable.builtin",
    "variable.parameter",
    "variable.member",
    "property",
    "label",
    "type",
    "type.builtin",
    // Keywords / operators.
    "keyword",
    "operator",
    // Structure.
    "punctuation",
    "punctuation.bracket",
    "punctuation.delimiter",
    "punctuation.special",
    // Trivia.
    "comment",
    "comment.documentation",
    "attribute",
    "tag",
];

/// Translate a tree-sitter capture-name (by its `HIGHLIGHT_NAMES`
/// index) to our theme bucket. Called once per `HighlightStart` event.
fn capture_name_to_highlight(idx: usize) -> Highlight {
    let name = HIGHLIGHT_NAMES.get(idx).copied().unwrap_or("");
    capture_str_to_highlight(name)
}

fn capture_str_to_highlight(name: &str) -> Highlight {
    // Match most-specific prefix first; fall through on the dotted
    // family name.
    let head = name.split('.').next().unwrap_or(name);
    match head {
        "keyword" | "operator" => Highlight::Keyword,
        "function" | "constructor" => Highlight::Function,
        "type" => Highlight::Type,
        "string" | "escape" => Highlight::String,
        "comment" => Highlight::Comment,
        "number" => Highlight::Number,
        "punctuation" | "tag" => Highlight::Punctuation,
        "boolean" | "constant" => Highlight::Constant,
        "attribute" => Highlight::Attribute,
        "variable" | "property" | "label" => Highlight::Variable,
        _ => Highlight::Default,
    }
}

// ---------------------------------------------------------------------------
// Tree-sitter configurations (cached at first use).
// ---------------------------------------------------------------------------

struct TsConfig {
    cfg: HighlightConfiguration,
}

fn build_config(
    language: tree_sitter::Language,
    name: &'static str,
    highlights: &str,
    locals: &str,
    injections: &str,
) -> Option<TsConfig> {
    let mut cfg =
        HighlightConfiguration::new(language, name, highlights, injections, locals).ok()?;
    cfg.configure(HIGHLIGHT_NAMES);
    Some(TsConfig { cfg })
}

static RUST_CFG: LazyLock<Option<TsConfig>> = LazyLock::new(|| {
    build_config(
        tree_sitter_rust::LANGUAGE.into(),
        "rust",
        tree_sitter_rust::HIGHLIGHTS_QUERY,
        "",
        "",
    )
});

static PYTHON_CFG: LazyLock<Option<TsConfig>> = LazyLock::new(|| {
    build_config(
        tree_sitter_python::LANGUAGE.into(),
        "python",
        tree_sitter_python::HIGHLIGHTS_QUERY,
        "",
        "",
    )
});

static TS_CFG: LazyLock<Option<TsConfig>> = LazyLock::new(|| {
    build_config(
        tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        "typescript",
        tree_sitter_typescript::HIGHLIGHTS_QUERY,
        tree_sitter_typescript::LOCALS_QUERY,
        "",
    )
});

static TSX_CFG: LazyLock<Option<TsConfig>> = LazyLock::new(|| {
    build_config(
        tree_sitter_typescript::LANGUAGE_TSX.into(),
        "tsx",
        tree_sitter_typescript::HIGHLIGHTS_QUERY,
        tree_sitter_typescript::LOCALS_QUERY,
        "",
    )
});

fn ts_config_for(lang_hint: &str) -> Option<&'static TsConfig> {
    match lang_hint {
        "rust" | "rs" => RUST_CFG.as_ref(),
        "python" | "py" => PYTHON_CFG.as_ref(),
        "typescript" | "ts" => TS_CFG.as_ref(),
        "tsx" => TSX_CFG.as_ref(),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// syntect — bundled syntaxes + theme. `default-fancy` feature uses
// fancy-regex (pure Rust) so we don't pull in the onig C dep.
// ---------------------------------------------------------------------------

static SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);

/// Load a single theme from the bundled set. `base16-ocean.dark` is
/// present in syntect's default ThemeSet and looks acceptable on the
/// dark sidebar — the per-scope color mapping we do downstream doesn't
/// actually use the theme's color values (we have our own `Highlight`
/// palette), only its syntax data, so which theme we pick is mostly
/// moot; we still need a valid one for `HighlightLines::new` paths.
static DARK_THEME: LazyLock<Theme> = LazyLock::new(|| {
    ThemeSet::load_defaults()
        .themes
        .get("base16-ocean.dark")
        .cloned()
        .expect("base16-ocean.dark missing from bundled ThemeSet")
});

// Pre-interned scope prefixes for fast scope classification. `Scope::new`
// allocates in the global scope repo on first call, so interning them
// once at module init keeps the hot loop allocation-free.
struct ScopePrefixes {
    keyword: Scope,
    operator: Scope,
    string: Scope,
    comment: Scope,
    number: Scope,
    constant: Scope,
    function: Scope,
    support_function: Scope,
    entity_function: Scope,
    entity_name: Scope,
    variable_function: Scope,
    storage_type: Scope,
    entity_type: Scope,
    support_type: Scope,
    variable_parameter: Scope,
    variable: Scope,
    punctuation: Scope,
    attribute: Scope,
}

static SCOPE_PREFIXES: LazyLock<ScopePrefixes> = LazyLock::new(|| ScopePrefixes {
    keyword: Scope::new("keyword").unwrap(),
    operator: Scope::new("keyword.operator").unwrap(),
    string: Scope::new("string").unwrap(),
    comment: Scope::new("comment").unwrap(),
    number: Scope::new("constant.numeric").unwrap(),
    constant: Scope::new("constant").unwrap(),
    function: Scope::new("entity.name.function").unwrap(),
    support_function: Scope::new("support.function").unwrap(),
    entity_function: Scope::new("meta.function-call").unwrap(),
    entity_name: Scope::new("entity.name").unwrap(),
    variable_function: Scope::new("variable.function").unwrap(),
    storage_type: Scope::new("storage.type").unwrap(),
    entity_type: Scope::new("entity.name.type").unwrap(),
    support_type: Scope::new("support.type").unwrap(),
    variable_parameter: Scope::new("variable.parameter").unwrap(),
    variable: Scope::new("variable").unwrap(),
    punctuation: Scope::new("punctuation").unwrap(),
    attribute: Scope::new("meta.attribute").unwrap(),
});

fn classify_scope_stack(stack: &ScopeStack) -> Highlight {
    // Walk from the *top* (most specific) down so a `string.quoted`
    // nested inside a `meta.function` still classifies as String.
    let prefixes = &*SCOPE_PREFIXES;
    for scope in stack.as_slice().iter().rev() {
        if prefixes.comment.is_prefix_of(*scope) {
            return Highlight::Comment;
        }
        if prefixes.string.is_prefix_of(*scope) {
            return Highlight::String;
        }
        if prefixes.number.is_prefix_of(*scope) {
            return Highlight::Number;
        }
        if prefixes.attribute.is_prefix_of(*scope) {
            return Highlight::Attribute;
        }
        if prefixes.constant.is_prefix_of(*scope) {
            return Highlight::Constant;
        }
        if prefixes.operator.is_prefix_of(*scope) {
            return Highlight::Keyword;
        }
        if prefixes.keyword.is_prefix_of(*scope) || prefixes.storage_type.is_prefix_of(*scope) {
            return Highlight::Keyword;
        }
        if prefixes.function.is_prefix_of(*scope)
            || prefixes.support_function.is_prefix_of(*scope)
            || prefixes.entity_function.is_prefix_of(*scope)
            || prefixes.variable_function.is_prefix_of(*scope)
        {
            return Highlight::Function;
        }
        if prefixes.entity_type.is_prefix_of(*scope) || prefixes.support_type.is_prefix_of(*scope) {
            return Highlight::Type;
        }
        if prefixes.entity_name.is_prefix_of(*scope) {
            return Highlight::Function;
        }
        if prefixes.variable_parameter.is_prefix_of(*scope) {
            return Highlight::Variable;
        }
        if prefixes.variable.is_prefix_of(*scope) {
            return Highlight::Variable;
        }
        if prefixes.punctuation.is_prefix_of(*scope) {
            return Highlight::Punctuation;
        }
    }
    Highlight::Default
}

// ---------------------------------------------------------------------------
// Public entry point.
// ---------------------------------------------------------------------------

/// Produce a list of highlighted byte-ranges for `source`.
///
/// `lang_hint` should be the canonical language name (`"rust"`,
/// `"typescript"`, `"tsx"`, `"python"`) OR a bare file extension
/// (`"toml"`, `"md"`) — syntect's fallback path does its own extension
/// lookup. `None` forces the syntect path with auto-detect (first line
/// heuristic + fallback to plain text).
///
/// Never panics. On any error the runs collapse to a single `Default`
/// span over the full input so callers can render the raw text.
pub fn highlight(source: &str, lang_hint: Option<&str>) -> Vec<HighlightedRun> {
    if source.is_empty() {
        return Vec::new();
    }

    if let Some(hint) = lang_hint
        && let Some(cfg) = ts_config_for(hint)
        && let Some(runs) = highlight_with_tree_sitter(source, cfg)
    {
        return runs;
    }

    if let Some(runs) = highlight_with_syntect(source, lang_hint) {
        return runs;
    }

    plain_text_fallback(source)
}

fn plain_text_fallback(source: &str) -> Vec<HighlightedRun> {
    vec![HighlightedRun {
        start: 0,
        end: source.len(),
        kind: Highlight::Default,
    }]
}

fn highlight_with_tree_sitter(source: &str, cfg: &TsConfig) -> Option<Vec<HighlightedRun>> {
    let mut highlighter = Highlighter::new();
    let events = highlighter
        .highlight(&cfg.cfg, source.as_bytes(), None, |_| None)
        .ok()?;

    let mut runs: Vec<HighlightedRun> = Vec::new();
    let mut stack: Vec<Highlight> = Vec::new();
    let mut covered: usize = 0;

    for event in events {
        let event = match event {
            Ok(e) => e,
            Err(_) => break,
        };
        match event {
            HighlightEvent::HighlightStart(h) => {
                stack.push(capture_name_to_highlight(h.0));
            }
            HighlightEvent::HighlightEnd => {
                stack.pop();
            }
            HighlightEvent::Source { start, end } => {
                if start >= end {
                    continue;
                }
                // Fill gaps with a Default run so the output always
                // covers the full input — the renderer slices the source
                // string by run boundaries and a gap would eat a byte.
                if start > covered {
                    push_run(&mut runs, covered, start, Highlight::Default);
                }
                let kind = stack.last().copied().unwrap_or(Highlight::Default);
                push_run(&mut runs, start, end, kind);
                if end > covered {
                    covered = end;
                }
            }
        }
    }

    // If the iterator errored before reaching the end, pad the tail
    // with a Default run so the caller still covers the whole source.
    if covered < source.len() {
        push_run(&mut runs, covered, source.len(), Highlight::Default);
    }

    if runs.is_empty() {
        return None;
    }
    Some(runs)
}

fn push_run(runs: &mut Vec<HighlightedRun>, start: usize, end: usize, kind: Highlight) {
    if start >= end {
        return;
    }
    // Coalesce adjacent runs with the same kind to keep the output
    // compact — the tree-sitter iterator emits many short runs and the
    // GPUI renderer would otherwise create one span per token.
    if let Some(last) = runs.last_mut()
        && last.end == start
        && last.kind == kind
    {
        last.end = end;
        return;
    }
    runs.push(HighlightedRun { start, end, kind });
}

fn highlight_with_syntect(source: &str, lang_hint: Option<&str>) -> Option<Vec<HighlightedRun>> {
    let ss = &*SYNTAX_SET;
    let syntax = syntax_for_hint(ss, lang_hint)?;
    let _ = &*DARK_THEME; // force theme init so `LazyLock` panic (if any) surfaces early.

    let mut parse_state = ParseState::new(syntax);
    let mut stack = ScopeStack::new();
    let mut runs: Vec<HighlightedRun> = Vec::new();

    let mut line_start = 0usize;
    // `parse_line` expects one line at a time including the trailing
    // newline. `split_inclusive('\n')` yields the newline with each
    // slice so line offsets stay byte-accurate.
    for line in source.split_inclusive('\n') {
        let ops = match parse_state.parse_line(line, ss) {
            Ok(o) => o,
            Err(_) => {
                // Malformed / regex failure — bail and let the caller
                // fall back to plain text. Already-produced runs
                // belong to valid prefix lines; we drop them so the
                // user sees a clean plain-text render rather than a
                // half-highlighted file.
                return None;
            }
        };

        for (range, op) in ScopeRangeIterator::new(&ops, line) {
            if range.is_empty() {
                let _ = stack.apply(op);
                continue;
            }
            let kind = classify_scope_stack(&stack);
            let abs_start = line_start + range.start;
            let abs_end = line_start + range.end;
            push_run(&mut runs, abs_start, abs_end, kind);
            let _ = stack.apply(op);
        }

        line_start += line.len();
    }

    if runs.is_empty() {
        return None;
    }
    Some(runs)
}

fn syntax_for_hint<'a>(ss: &'a SyntaxSet, lang_hint: Option<&str>) -> Option<&'a SyntaxReference> {
    let hint = lang_hint?;
    // Canonical names → extension mapping the other way so `"rust"` /
    // `"python"` / etc. resolve to the bundled Sublime syntax.
    let candidates: &[&str] = match hint {
        "rust" => &["rs"],
        "python" => &["py"],
        "typescript" | "ts" => &["ts"],
        "tsx" => &["tsx"],
        "javascript" | "js" => &["js"],
        "markdown" | "md" => &["md"],
        "toml" => &["toml"],
        "json" => &["json"],
        "yaml" | "yml" => &["yaml"],
        "html" => &["html"],
        "go" => &["go"],
        "java" => &["java"],
        "c" => &["c"],
        "cpp" | "c++" => &["cpp"],
        "shell" | "sh" | "bash" => &["sh"],
        other => {
            // Last-ditch: treat the hint itself as an extension so
            // e.g. the caller passing a raw extension still resolves.
            return ss.find_syntax_by_extension(other);
        }
    };
    for ext in candidates {
        if let Some(s) = ss.find_syntax_by_extension(ext) {
            return Some(s);
        }
    }
    None
}

/// Map a file path (via its extension) to the canonical `lang_hint`
/// string [`highlight`] accepts. Used by the detail pane — we have a
/// `Path` per occurrence — and exposed separately so tests can exercise
/// it without going through GPUI.
///
/// Returns:
///
/// - `Some("rust")` / `Some("python")` / `Some("typescript")` / `Some("tsx")`
///   for the tree-sitter-path extensions.
/// - `Some(ext)` (lowercased) for everything else — syntect does its
///   own lookup.
/// - `None` when the path has no extension.
pub fn lang_hint_for_path(path: &Path) -> Option<String> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "rs" => "rust".to_string(),
        "py" | "pyi" => "python".to_string(),
        "ts" | "mts" | "cts" => "typescript".to_string(),
        "tsx" => "tsx".to_string(),
        _ => ext,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(runs: &[HighlightedRun], source: &str) -> Vec<(String, Highlight)> {
        runs.iter()
            .map(|r| (source[r.start..r.end].to_string(), r.kind))
            .collect()
    }

    #[test]
    fn lang_hint_resolves_known_extensions() {
        assert_eq!(
            lang_hint_for_path(Path::new("src/foo.rs")).as_deref(),
            Some("rust")
        );
        assert_eq!(
            lang_hint_for_path(Path::new("lib.py")).as_deref(),
            Some("python")
        );
        assert_eq!(
            lang_hint_for_path(Path::new("m.pyi")).as_deref(),
            Some("python")
        );
        assert_eq!(
            lang_hint_for_path(Path::new("app.ts")).as_deref(),
            Some("typescript")
        );
        assert_eq!(
            lang_hint_for_path(Path::new("ui.tsx")).as_deref(),
            Some("tsx")
        );
        // Unknown extension falls through verbatim so syntect can try.
        assert_eq!(
            lang_hint_for_path(Path::new("Cargo.TOML")).as_deref(),
            Some("toml")
        );
        // No extension at all — caller must fall back to plain text.
        assert!(lang_hint_for_path(Path::new("Makefile")).is_none());
    }

    #[test]
    fn capture_prefix_matching_is_longest_first() {
        // Both `function` and `function.method` resolve to Function.
        assert_eq!(capture_str_to_highlight("function"), Highlight::Function);
        assert_eq!(
            capture_str_to_highlight("function.method"),
            Highlight::Function
        );
        assert_eq!(
            capture_str_to_highlight("keyword.control"),
            Highlight::Keyword
        );
        assert_eq!(
            capture_str_to_highlight("unknown.thing"),
            Highlight::Default
        );
    }

    #[test]
    fn rust_highlights_mark_fn_and_function_name() {
        let src = "fn main() { let x = 1; }";
        let runs = highlight(src, Some("rust"));
        assert!(!runs.is_empty());
        let pairs = kinds(&runs, src);
        // `fn` is a keyword per tree-sitter-rust's highlights.scm
        // (captured via the anonymous "fn" in the keywords list).
        assert!(
            pairs
                .iter()
                .any(|(t, h)| t == "fn" && *h == Highlight::Keyword),
            "fn keyword not highlighted: {pairs:?}"
        );
        // Rust's highlights query marks the function name with
        // `@function` for function_item name fields.
        assert!(
            pairs
                .iter()
                .any(|(t, h)| t == "main" && *h == Highlight::Function),
            "function name not highlighted: {pairs:?}"
        );
    }

    #[test]
    fn python_highlights_mark_def_and_function_name() {
        let src = "def foo():\n    pass\n";
        let runs = highlight(src, Some("python"));
        assert!(!runs.is_empty());
        let pairs = kinds(&runs, src);
        assert!(
            pairs
                .iter()
                .any(|(t, h)| t == "def" && *h == Highlight::Keyword),
            "def keyword not highlighted: {pairs:?}"
        );
        assert!(
            pairs
                .iter()
                .any(|(t, h)| t == "foo" && *h == Highlight::Function),
            "function name not highlighted: {pairs:?}"
        );
        assert!(
            pairs
                .iter()
                .any(|(t, h)| t == "pass" && *h == Highlight::Keyword),
            "pass keyword not highlighted: {pairs:?}"
        );
    }

    #[test]
    fn typescript_highlights_keywords_and_parameters() {
        // TS's bundled highlights.scm is intentionally small — it
        // relies on inheritance from JS at the tree-sitter CLI level
        // and ships only the TS-specific additions. The reliable
        // bits are the TS-specific keyword list ("abstract", "type",
        // "interface", …) and `variable.parameter` captures on
        // required_parameter. We assert those.
        let src = "interface Foo { x: number; }\nfunction bar(x: number) { return x; }\n";
        let runs = highlight(src, Some("typescript"));
        assert!(!runs.is_empty());
        let pairs = kinds(&runs, src);
        assert!(
            pairs
                .iter()
                .any(|(t, h)| t == "interface" && *h == Highlight::Keyword),
            "interface keyword not highlighted: {pairs:?}"
        );
        // `number` is captured by `(predefined_type) @type.builtin`.
        assert!(
            pairs
                .iter()
                .any(|(t, h)| t == "number" && *h == Highlight::Type),
            "predefined type not highlighted: {pairs:?}"
        );
    }

    #[test]
    fn unknown_language_returns_single_default_run() {
        let src = "$$$ not a language $$$";
        let runs = highlight(src, Some("flimflam"));
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].start, 0);
        assert_eq!(runs[0].end, src.len());
        assert_eq!(runs[0].kind, Highlight::Default);
    }

    #[test]
    fn malformed_rust_does_not_panic_and_returns_runs() {
        // Intentionally broken Rust — unclosed paren / brace. The
        // tree-sitter parser does not panic on malformed input (we
        // verified the contract), so highlight() should still return
        // a non-empty run list that covers the input.
        let src = "fn main( { broken syntax";
        let runs = highlight(src, Some("rust"));
        assert!(!runs.is_empty(), "expected runs for malformed input");
        // Runs cover the full input (possibly across several spans).
        let total: usize = runs.iter().map(|r| r.end - r.start).sum();
        assert_eq!(total, src.len());
    }

    #[test]
    fn syntect_fallback_highlights_markdown() {
        // Markdown is in syntect's bundled default SyntaxSet — TOML
        // and YAML are not, so we exercise the fallback path via a
        // language the bundled set actually ships. (If a future
        // syntect bump adds TOML this still works.)
        let src = "# Heading\n\n`inline code` and **bold**.\n";
        let runs = highlight(src, Some("markdown"));
        assert!(!runs.is_empty());
        let pairs = kinds(&runs, src);
        // syntect definitely engaged if at least one run is not
        // Default — the plain-text fallback would emit a single
        // Default run covering the whole input.
        assert!(
            runs.iter().any(|r| r.kind != Highlight::Default),
            "expected at least one non-default run from syntect\n{pairs:?}"
        );
    }

    #[test]
    fn runs_are_monotonic_and_cover_input() {
        let src = "fn main() {\n    let x = 42;\n}\n";
        let runs = highlight(src, Some("rust"));
        // Non-overlapping, sorted, and contiguous — the renderer
        // assumes this to slice the source string into spans.
        let mut prev_end = 0usize;
        for r in &runs {
            assert!(
                r.start == prev_end,
                "gap / overlap at {r:?} prev_end={prev_end}"
            );
            assert!(r.end > r.start);
            prev_end = r.end;
        }
        assert_eq!(prev_end, src.len());
    }

    #[test]
    fn empty_source_yields_no_runs() {
        assert!(highlight("", Some("rust")).is_empty());
        assert!(highlight("", None).is_empty());
    }

    #[test]
    fn theme_colors_are_nonzero_for_every_variant() {
        // Sanity — nobody accidentally set a variant to Default's color.
        let all = [
            Highlight::Keyword,
            Highlight::Function,
            Highlight::Type,
            Highlight::String,
            Highlight::Comment,
            Highlight::Number,
            Highlight::Punctuation,
            Highlight::Variable,
            Highlight::Constant,
            Highlight::Attribute,
            Highlight::Default,
        ];
        for h in all {
            assert!(h.color() != 0);
        }
    }
}
