//! Tier B normalization: walk a tree-sitter subtree and emit a
//! canonical token stream suitable for hashing.
//!
//! For each leaf node the profile's [`crate::RenameClass`] decides how
//! the text is emitted. The emission depends on the selected
//! [`NormalizationMode`]:
//!
//! - [`crate::RenameClass::Local`] → stable alias (`v1`, `v2`, ...)
//!   assigned in order of first occurrence within the subtree. Same in
//!   both modes.
//! - [`crate::RenameClass::Kept`] → verbatim. Same in both modes.
//! - [`crate::RenameClass::Literal`]
//!   - [`NormalizationMode::Conservative`] (default): verbatim.
//!   - [`NormalizationMode::Aggressive`]: replaced with the stable
//!     placeholder `<LIT>` so that functions differing only by a
//!     string / number constant hash together. Higher recall, higher
//!     false-positive rate — issue #10.
//!
//! Each emitted token carries its node kind as a prefix so two
//! different kinds of leaf never collide even if they share the same
//! surface text.
//!
//! Idempotency is a load-bearing invariant: `normalize(normalize(x))
//! == normalize(x)`. The property test lives in
//! `tests/rust_profile.rs`.

use std::collections::HashMap;

use tree_sitter::{Node, Tree, TreeCursor};

use crate::profile::{LanguageProfile, RenameClass};

/// Placeholder token text emitted for [`RenameClass::Literal`] leaves
/// in [`NormalizationMode::Aggressive`]. Kept as a single canonical
/// symbol (rather than split into `<STR>` / `<NUM>`) so that grammars
/// which break a literal into multiple leaves — e.g. Python's
/// `string_start` / `string_content` / `string_end` trio — still
/// produce a consistent, greppable footprint and keep hashes stable
/// across grammar upgrades.
pub const AGGRESSIVE_LITERAL_PLACEHOLDER: &str = "<LIT>";

/// Which normalization strategy to use when emitting the canonical
/// token stream for a syntactic unit.
///
/// Issue #10 — configured via the `normalization` key in the layered
/// config. Conservative is the default and leaves literal text
/// verbatim (byte-identical to the pre-#10 behaviour). Aggressive
/// rewrites [`RenameClass::Literal`] leaves to
/// [`AGGRESSIVE_LITERAL_PLACEHOLDER`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum NormalizationMode {
    /// Leave literals verbatim. Default. Byte-identical to the MVP
    /// normaliser so existing snapshots and caches remain stable.
    #[default]
    Conservative,
    /// Replace literal leaves with [`AGGRESSIVE_LITERAL_PLACEHOLDER`]
    /// so functions differing only by literal values still cluster.
    Aggressive,
}

/// One Tier B candidate subtree extracted from a source file.
#[derive(Debug, Clone)]
pub struct SyntacticUnit {
    /// The node kind that matched `profile.syntactic_units()`.
    pub kind: String,
    /// 0-based byte offset (inclusive) of the unit in its source.
    pub start_byte: usize,
    /// 0-based byte offset (exclusive) of the unit in its source.
    pub end_byte: usize,
    /// 1-based line of the first byte in the unit.
    pub start_line: usize,
    /// 1-based line of the last byte in the unit.
    pub end_line: usize,
    /// The normalized token stream for the unit.
    pub tokens: Vec<NormalizedToken>,
    /// Hash of [`Self::tokens`], computed with [`hash_tokens`].
    pub hash: u64,
}

/// Extract every Tier B candidate from a parsed `tree`, walking the
/// whole file and picking up any node whose `kind()` is in
/// `profile.syntactic_units()`.
///
/// Nested matches (e.g. an `impl_item` and the `function_item`s
/// inside it) are all reported — each can be a duplicate in its own
/// right. The caller decides how to de-duplicate by containment.
///
/// Uses [`NormalizationMode::Conservative`]. See
/// [`extract_units_with_mode`] to opt into aggressive literal
/// abstraction.
pub fn extract_units(
    tree: &Tree,
    source: &[u8],
    profile: &dyn LanguageProfile,
) -> Vec<SyntacticUnit> {
    extract_units_with_mode(tree, source, profile, NormalizationMode::Conservative)
}

/// Mode-aware counterpart of [`extract_units`]. Callers that read the
/// [`NormalizationMode`] from config (the scanner) route through this.
pub fn extract_units_with_mode(
    tree: &Tree,
    source: &[u8],
    profile: &dyn LanguageProfile,
    mode: NormalizationMode,
) -> Vec<SyntacticUnit> {
    let mut units = Vec::new();
    let kinds: &[&str] = profile.syntactic_units();
    let mut cursor = tree.walk();
    collect_units(&mut cursor, source, profile, kinds, mode, &mut units);
    units
}

fn collect_units(
    cursor: &mut TreeCursor<'_>,
    source: &[u8],
    profile: &dyn LanguageProfile,
    kinds: &[&str],
    mode: NormalizationMode,
    out: &mut Vec<SyntacticUnit>,
) {
    let node = cursor.node();
    if kinds.iter().any(|k| *k == node.kind()) {
        let tokens = normalize_with_mode(node, source, profile, mode);
        if !tokens.is_empty() {
            let start = node.start_position();
            let end = node.end_position();
            let hash = hash_tokens(&tokens);
            out.push(SyntacticUnit {
                kind: node.kind().to_string(),
                start_byte: node.start_byte(),
                end_byte: node.end_byte(),
                // tree-sitter rows are 0-based; bump to 1-based to
                // match `dedup_core::rolling_hash::Span` conventions.
                start_line: start.row + 1,
                end_line: end.row + 1,
                tokens,
                hash,
            });
        }
    }

    if cursor.goto_first_child() {
        loop {
            collect_units(cursor, source, profile, kinds, mode, out);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
        cursor.goto_parent();
    }
}

/// One normalized token in a subtree's canonical stream.
///
/// Storing the kind alongside the rewritten text avoids cross-kind
/// collisions at hash time (so `v1` as an identifier and `v1` as a
/// string literal can't hash-collide).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NormalizedToken {
    /// The tree-sitter node kind the token came from.
    pub kind: String,
    /// The text to hash: original source for [`RenameClass::Kept`] /
    /// [`RenameClass::Literal`], a stable alias for
    /// [`RenameClass::Local`].
    pub text: String,
}

/// Walk the subtree rooted at `node` and produce the canonical
/// normalized token stream for Tier B in
/// [`NormalizationMode::Conservative`].
///
/// `source` must be the full source bytes that `node` was parsed from.
///
/// Leaves are visited in source order. For each leaf:
///
/// - We look up the rename class from `profile.rename_class(kind)`.
/// - For `Local`, we assign a stable alias based on first-occurrence
///   order and rewrite every subsequent appearance of the same
///   identifier text to the same alias.
/// - For `Kept` / `Literal`, we emit the verbatim lexeme.
///
/// The function deliberately operates on leaves only — interior
/// syntax-tree node kinds don't make it into the hash so minor
/// grammar differences (wrapper nodes) don't disturb matching.
///
/// Callers that need aggressive literal abstraction should use
/// [`normalize_with_mode`] with [`NormalizationMode::Aggressive`].
pub fn normalize(node: Node, source: &[u8], profile: &dyn LanguageProfile) -> Vec<NormalizedToken> {
    normalize_with_mode(node, source, profile, NormalizationMode::Conservative)
}

/// Mode-aware counterpart of [`normalize`]. In
/// [`NormalizationMode::Aggressive`], every [`RenameClass::Literal`]
/// leaf is rewritten to [`AGGRESSIVE_LITERAL_PLACEHOLDER`] so two
/// units that differ only in literal values hash together.
pub fn normalize_with_mode(
    node: Node,
    source: &[u8],
    profile: &dyn LanguageProfile,
    mode: NormalizationMode,
) -> Vec<NormalizedToken> {
    let mut out = Vec::new();
    let mut locals: HashMap<String, String> = HashMap::new();
    let mut cursor = node.walk();
    visit(&mut cursor, source, profile, mode, &mut out, &mut locals);
    out
}

/// Internal DFS that drains each leaf through the profile's classifier.
fn visit(
    cursor: &mut TreeCursor<'_>,
    source: &[u8],
    profile: &dyn LanguageProfile,
    mode: NormalizationMode,
    out: &mut Vec<NormalizedToken>,
    locals: &mut HashMap<String, String>,
) {
    let node = cursor.node();

    if node.child_count() == 0 {
        // Leaf. Skip pure-punctuation nodes that carry no text of
        // interest to Tier B — anonymous tokens like `;`, `{`, `}` —
        // by checking `is_named()`. Named anonymous punctuation is
        // still kept (operators, keywords).
        if let Ok(text) = node.utf8_text(source) {
            // Drop pure whitespace / extras.
            if text.trim().is_empty() {
                return;
            }
            let kind = node.kind().to_string();
            let class = profile.classify_node(&node);
            let emitted = match class {
                RenameClass::Local => {
                    let text_owned = text.to_string();
                    if let Some(alias) = locals.get(&text_owned) {
                        alias.clone()
                    } else {
                        let alias = format!("v{}", locals.len() + 1);
                        locals.insert(text_owned, alias.clone());
                        alias
                    }
                }
                RenameClass::Kept => text.to_string(),
                RenameClass::Literal => match mode {
                    NormalizationMode::Conservative => text.to_string(),
                    NormalizationMode::Aggressive => AGGRESSIVE_LITERAL_PLACEHOLDER.to_string(),
                },
            };
            out.push(NormalizedToken {
                kind,
                text: emitted,
            });
        }
        return;
    }

    // Interior: recurse into children.
    if cursor.goto_first_child() {
        loop {
            visit(cursor, source, profile, mode, out, locals);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
        cursor.goto_parent();
    }
}

/// FNV-1a-ish 64-bit hash over a normalized token stream. Mirrors the
/// constants used by `dedup_core::rolling_hash` so Tier A and Tier B
/// live in the same hash family and can share buckets if we ever need
/// to cross-compare.
pub fn hash_tokens(tokens: &[NormalizedToken]) -> u64 {
    const SEED: u64 = 0xcbf29ce484222325;
    const MUL: u64 = 0x100000001b3;
    let mut h = SEED;
    for tok in tokens {
        // Kind first, with a separator, then text, then a terminator.
        for &b in tok.kind.as_bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(MUL);
        }
        h ^= b'\x1f' as u64; // unit separator
        h = h.wrapping_mul(MUL);
        for &b in tok.text.as_bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(MUL);
        }
        h ^= b'\x1e' as u64; // record separator
        h = h.wrapping_mul(MUL);
    }
    h
}

#[cfg(test)]
mod tests {
    //! Per-profile mode tests for issue #10.
    //!
    //! These cover the load-bearing contract: aggressive mode replaces
    //! every [`RenameClass::Literal`] leaf with
    //! [`AGGRESSIVE_LITERAL_PLACEHOLDER`], while conservative mode
    //! leaves the stream byte-identical to the MVP normaliser.

    use super::*;
    use crate::{PYTHON_PROFILE, RUST_PROFILE, TYPESCRIPT_PROFILE};
    use tree_sitter::Parser;

    fn parse_with(profile: &dyn LanguageProfile, source: &str) -> tree_sitter::Tree {
        let mut parser = Parser::new();
        parser
            .set_language(&profile.tree_sitter_language())
            .expect("set grammar");
        parser.parse(source, None).expect("parse")
    }

    /// Extract the first unit whose kind is in `profile.syntactic_units()`
    /// via the given `mode`, failing the test if none match.
    fn first_unit(
        profile: &dyn LanguageProfile,
        source: &str,
        mode: NormalizationMode,
    ) -> SyntacticUnit {
        let tree = parse_with(profile, source);
        extract_units_with_mode(&tree, source.as_bytes(), profile, mode)
            .into_iter()
            .next()
            .expect("at least one unit")
    }

    /// Count how many times the aggressive literal placeholder appears
    /// in the token stream. Used as a lightweight assertion that the
    /// profile classified at least one leaf as `RenameClass::Literal`.
    fn placeholder_count(tokens: &[NormalizedToken]) -> usize {
        tokens
            .iter()
            .filter(|t| t.text == AGGRESSIVE_LITERAL_PLACEHOLDER)
            .count()
    }

    #[test]
    fn conservative_default_matches_bare_normalize() {
        // The default-mode API must be byte-identical to the old
        // `normalize` signature's output so existing snapshots /
        // caches / hashes don't drift.
        let src = "fn f() -> i32 { let x = 42; x + 1 }";
        let tree = parse_with(&RUST_PROFILE, src);
        let root = tree.root_node();
        let function_item = root
            .child(0)
            .and_then(|n| {
                if n.kind() == "function_item" {
                    Some(n)
                } else {
                    None
                }
            })
            .expect("function_item");
        let legacy = normalize(function_item, src.as_bytes(), &RUST_PROFILE);
        let via_mode = normalize_with_mode(
            function_item,
            src.as_bytes(),
            &RUST_PROFILE,
            NormalizationMode::Conservative,
        );
        assert_eq!(legacy, via_mode);
    }

    #[test]
    fn rust_aggressive_abstracts_integer_and_string_literals() {
        let src = r#"
            fn mixed() -> i32 {
                let n = 42;
                let s = "hello";
                s.len() as i32 + n
            }
        "#;
        let conservative = first_unit(&RUST_PROFILE, src, NormalizationMode::Conservative);
        let aggressive = first_unit(&RUST_PROFILE, src, NormalizationMode::Aggressive);

        // Conservative keeps the literal bytes verbatim.
        assert!(
            conservative.tokens.iter().any(|t| t.text == "42"),
            "conservative should keep integer literal verbatim"
        );
        assert!(
            conservative
                .tokens
                .iter()
                .any(|t| t.kind == "string_content" && t.text == "hello"),
            "conservative should keep string content verbatim"
        );

        // Aggressive replaces both with the placeholder.
        assert!(
            aggressive
                .tokens
                .iter()
                .any(|t| t.kind == "integer_literal" && t.text == AGGRESSIVE_LITERAL_PLACEHOLDER),
            "aggressive should abstract integer literals"
        );
        assert!(
            aggressive
                .tokens
                .iter()
                .any(|t| t.kind == "string_content" && t.text == AGGRESSIVE_LITERAL_PLACEHOLDER),
            "aggressive should abstract string content"
        );
        assert!(placeholder_count(&aggressive.tokens) >= 2);

        // And the hash must differ so the scanner sees a different
        // bucket key under the two modes.
        assert_ne!(conservative.hash, aggressive.hash);
    }

    #[test]
    fn rust_aggressive_makes_literal_diverging_pair_collide() {
        // Same function, different integer literal. Conservative keeps
        // them distinct; aggressive collapses them to one bucket.
        let a = "fn f() -> i32 { let x = 1; x * 2 }";
        let b = "fn f() -> i32 { let x = 99; x * 2 }";

        let ca = first_unit(&RUST_PROFILE, a, NormalizationMode::Conservative);
        let cb = first_unit(&RUST_PROFILE, b, NormalizationMode::Conservative);
        assert_ne!(
            ca.hash, cb.hash,
            "conservative must distinguish literal-diverging pairs"
        );

        let aa = first_unit(&RUST_PROFILE, a, NormalizationMode::Aggressive);
        let ab = first_unit(&RUST_PROFILE, b, NormalizationMode::Aggressive);
        assert_eq!(
            aa.hash, ab.hash,
            "aggressive must collapse literal-diverging pairs"
        );
    }

    #[test]
    fn typescript_aggressive_abstracts_string_and_number_literals() {
        let src = r#"
            function mixed() {
                const n = 42;
                const s = "hello";
                return s.length + n;
            }
        "#;
        let conservative = first_unit(&TYPESCRIPT_PROFILE, src, NormalizationMode::Conservative);
        let aggressive = first_unit(&TYPESCRIPT_PROFILE, src, NormalizationMode::Aggressive);

        assert!(conservative.tokens.iter().any(|t| t.text == "42"));
        assert!(
            conservative
                .tokens
                .iter()
                .any(|t| t.kind == "string_fragment" && t.text == "hello"),
        );

        assert!(
            aggressive
                .tokens
                .iter()
                .any(|t| t.kind == "number" && t.text == AGGRESSIVE_LITERAL_PLACEHOLDER),
        );
        assert!(
            aggressive
                .tokens
                .iter()
                .any(|t| t.kind == "string_fragment" && t.text == AGGRESSIVE_LITERAL_PLACEHOLDER),
        );
        assert_ne!(conservative.hash, aggressive.hash);
    }

    #[test]
    fn python_aggressive_abstracts_number_and_string_literals() {
        let src = "def mixed():\n    n = 42\n    s = \"hello\"\n    return len(s) + n\n";
        let conservative = first_unit(&PYTHON_PROFILE, src, NormalizationMode::Conservative);
        let aggressive = first_unit(&PYTHON_PROFILE, src, NormalizationMode::Aggressive);

        assert!(conservative.tokens.iter().any(|t| t.text == "42"));
        assert!(
            conservative
                .tokens
                .iter()
                .any(|t| t.kind == "string_content" && t.text == "hello"),
        );

        assert!(
            aggressive
                .tokens
                .iter()
                .any(|t| t.kind == "integer" && t.text == AGGRESSIVE_LITERAL_PLACEHOLDER),
        );
        assert!(
            aggressive
                .tokens
                .iter()
                .any(|t| t.kind == "string_content" && t.text == AGGRESSIVE_LITERAL_PLACEHOLDER),
        );
        assert_ne!(conservative.hash, aggressive.hash);
    }

    #[test]
    fn aggressive_does_not_rewrite_kept_names() {
        // Type / field identifiers are `RenameClass::Kept`; even in
        // aggressive mode they must stay verbatim. Otherwise we'd
        // cross-bucket semantically distinct types.
        //
        // Note: plain `identifier` in tree-sitter-rust includes the
        // function-declaration name, which is classified as
        // `RenameClass::Local` and alpha-renamed in both modes
        // (matches the pre-#10 behaviour captured in the snapshot
        // tests). We therefore assert on a `type_identifier` leaf.
        let src = "fn f() -> MyResult { let x = 1; MyResult { field: x } }";
        let aggressive = first_unit(&RUST_PROFILE, src, NormalizationMode::Aggressive);
        assert!(
            aggressive
                .tokens
                .iter()
                .any(|t| t.kind == "type_identifier" && t.text == "MyResult"),
            "kept type_identifier must survive aggressive mode verbatim"
        );
        assert!(
            aggressive
                .tokens
                .iter()
                .any(|t| t.kind == "field_identifier" && t.text == "field"),
            "kept field_identifier must survive aggressive mode verbatim"
        );
    }

    #[test]
    fn aggressive_still_alpha_renames_locals() {
        // Locals are `RenameClass::Local`, independent of mode — both
        // modes must produce the same `v1` / `v2` aliases.
        let src = "fn f(x: i32, y: i32) -> i32 { let z = x + y; z }";
        let conservative = first_unit(&RUST_PROFILE, src, NormalizationMode::Conservative);
        let aggressive = first_unit(&RUST_PROFILE, src, NormalizationMode::Aggressive);

        let conservative_locals: Vec<_> = conservative
            .tokens
            .iter()
            .filter(|t| t.kind == "identifier")
            .map(|t| t.text.clone())
            .collect();
        let aggressive_locals: Vec<_> = aggressive
            .tokens
            .iter()
            .filter(|t| t.kind == "identifier")
            .map(|t| t.text.clone())
            .collect();
        assert_eq!(conservative_locals, aggressive_locals);
    }
}
