//! Tier B normalization: walk a tree-sitter subtree and emit a
//! canonical token stream suitable for hashing.
//!
//! For each leaf node the profile's [`crate::RenameClass`] decides how
//! the text is emitted:
//!
//! - [`crate::RenameClass::Local`] → stable alias (`v1`, `v2`, ...)
//!   assigned in order of first occurrence within the subtree.
//! - [`crate::RenameClass::Kept`] → verbatim.
//! - [`crate::RenameClass::Literal`] → verbatim at MVP (aggressive
//!   literal abstraction lands in #10).
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
pub fn extract_units(
    tree: &Tree,
    source: &[u8],
    profile: &dyn LanguageProfile,
) -> Vec<SyntacticUnit> {
    let mut units = Vec::new();
    let kinds: &[&str] = profile.syntactic_units();
    let mut cursor = tree.walk();
    collect_units(&mut cursor, source, profile, kinds, &mut units);
    units
}

fn collect_units(
    cursor: &mut TreeCursor<'_>,
    source: &[u8],
    profile: &dyn LanguageProfile,
    kinds: &[&str],
    out: &mut Vec<SyntacticUnit>,
) {
    let node = cursor.node();
    if kinds.iter().any(|k| *k == node.kind()) {
        let tokens = normalize(node, source, profile);
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
            collect_units(cursor, source, profile, kinds, out);
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
/// normalized token stream for Tier B.
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
pub fn normalize(node: Node, source: &[u8], profile: &dyn LanguageProfile) -> Vec<NormalizedToken> {
    let mut out = Vec::new();
    let mut locals: HashMap<String, String> = HashMap::new();
    let mut cursor = node.walk();
    visit(&mut cursor, source, profile, &mut out, &mut locals);
    out
}

/// Internal DFS that drains each leaf through the profile's classifier.
fn visit(
    cursor: &mut TreeCursor<'_>,
    source: &[u8],
    profile: &dyn LanguageProfile,
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
            let class = profile.rename_class(&kind);
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
                RenameClass::Kept | RenameClass::Literal => text.to_string(),
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
            visit(cursor, source, profile, out, locals);
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
