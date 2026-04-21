//! The [`LanguageProfile`] trait and its supporting types.
//!
//! A `LanguageProfile` captures everything Tier B needs to analyse a
//! single source language:
//!
//! - Which file extensions identify the language.
//! - The `tree-sitter` grammar used to parse it.
//! - The set of "syntactic units" we care about (function-like bodies,
//!   type declarations, and so on) — these become the candidate units
//!   that Tier B hashes and groups.
//! - A [`RenameClass`] policy that says, for each leaf node kind in the
//!   subtree, whether an identifier/literal should be alpha-renamed,
//!   kept verbatim, or abstracted as a literal placeholder.
//!
//! Profiles are singletons: each language crate ships a `&'static dyn
//! LanguageProfile` and registers it with [`crate::all_profiles`]. The
//! Scanner in `dedup-core` takes a slice of these and runs Tier B on
//! any file whose extension matches a registered profile.
//!
//! # Contributing a new profile
//!
//! See `docs/contributing-language-profile.md` in the repository root
//! for the end-to-end checklist. In brief:
//!
//! 1. Add a `tree-sitter-<lang>` dependency to `dedup-lang`'s
//!    `Cargo.toml`.
//! 2. Create a new module (e.g. `src/<lang>.rs`) with a unit struct
//!    implementing [`LanguageProfile`] and a
//!    `pub static <LANG>_PROFILE: <Lang>Profile` singleton.
//! 3. Implement the required methods:
//!    - [`LanguageProfile::name`] — human-readable name for logs.
//!    - [`LanguageProfile::extensions`] — file extensions (no dot).
//!    - [`LanguageProfile::tree_sitter_language`] — the grammar's
//!      `LANGUAGE` constant.
//!    - [`LanguageProfile::syntactic_units`] — the tree-sitter node
//!      kinds that count as candidate subtrees (e.g. `"function_item"`,
//!      `"impl_item"`).
//!    - [`LanguageProfile::rename_class`] — per-kind literal /
//!      identifier classification. Default to [`RenameClass::Kept`]
//!      when unsure; over-keeping is safe.
//!    - Optionally override [`LanguageProfile::classify_node`] when the
//!      grammar reuses the same leaf kind across must-keep and
//!      must-rename positions (e.g. TSX's plain `identifier`).
//! 4. Append `&<LANG>_PROFILE` to [`crate::all_profiles`] and re-export
//!    the singleton + type from `lib.rs`.
//! 5. Extend the CLI's `--lang` filter in
//!    `crates/dedup-cli/src/main.rs`.
//! 6. Ship fixtures + insta snapshots under
//!    `crates/dedup-lang/tests/fixtures/<lang>/` that exercise
//!    [`crate::extract_units`] and assert on the normalised shape.
//!
//! The existing profiles are the reference implementations:
//!
//! - [`crate::rust`] — straightforward: Rust's grammar uses distinct
//!   leaf kinds, so the default [`LanguageProfile::classify_node`] is
//!   enough.
//! - [`crate::typescript`] — overrides `classify_node` to disambiguate
//!   TSX element tags from identifier bindings.
//! - [`crate::python`] — decorator-aware classification so
//!   `@staticmethod` does not collapse with `@classmethod`.

use tree_sitter::{Language, Node};

/// How an identifier / literal node should be treated when normalising
/// a syntactic unit's token stream for hashing.
///
/// This is the core of Tier B's "rename-resilient" behaviour: two
/// functions that differ only in local variable names normalise to the
/// same token stream and therefore to the same hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenameClass {
    /// Local binding / parameter. Replaced with a stable alias
    /// (`v1`, `v2`, ...) based on order of first occurrence within the
    /// syntactic unit's subtree.
    Local,
    /// Identifier that must be kept verbatim — function names, type
    /// names, imported items, macro names, ...
    ///
    /// These carry meaning across the corpus (renaming them would
    /// change what the code does), so Tier B must not alpha-rename
    /// them.
    Kept,
    /// Literal (string, number, ...). At MVP the literal is kept
    /// verbatim; issue #10 introduces "aggressive mode" where
    /// literals are replaced with a placeholder for broader matching.
    Literal,
}

/// A language-specific Tier B profile.
///
/// Implementors live in their own modules (see [`crate::rust`] for the
/// Rust profile) and are exposed as `&'static dyn LanguageProfile`
/// through [`crate::all_profiles`] and [`crate::profile_for_extension`].
///
/// The trait is `Send + Sync` so the Scanner can share profiles across
/// threads in the future (parallelism lands in #14).
pub trait LanguageProfile: Send + Sync {
    /// Human-readable profile name. Used in log lines and error text.
    fn name(&self) -> &'static str;

    /// File extensions this profile claims, without the leading dot
    /// (e.g. `["rs"]`). Matched case-sensitively against a path's
    /// extension.
    fn extensions(&self) -> &[&'static str];

    /// The `tree-sitter` grammar used to parse source files for this
    /// profile.
    fn tree_sitter_language(&self) -> Language;

    /// Tree-sitter node kinds that count as "syntactic units" — the
    /// candidate subtrees Tier B hashes and groups.
    ///
    /// For Rust this is `function_item`, `impl_item`, `struct_item`,
    /// `enum_item`. Impl methods are found recursively as nested
    /// `function_item` nodes while walking; callers don't need to
    /// special-case them.
    fn syntactic_units(&self) -> &[&'static str];

    /// Classify a leaf node's kind for normalisation. See
    /// [`RenameClass`] for the semantics of each variant.
    ///
    /// The default for unknown kinds should be [`RenameClass::Kept`]
    /// — when in doubt, preserve the original text rather than
    /// risking over-aggressive matching.
    fn rename_class(&self, node_kind: &str) -> RenameClass;

    /// Context-aware classification hook. Defaults to
    /// [`Self::rename_class`] on the leaf's own kind, which is correct
    /// for languages whose grammar uses distinct node kinds for every
    /// position we care about (e.g. Rust). Languages whose grammar
    /// reuses the same leaf kind across must-keep and must-rename
    /// positions (e.g. TypeScript's plain `identifier`, which shows up
    /// both as a local binding *and* as a JSX element name) override
    /// this to disambiguate via the leaf's parent chain.
    fn classify_node(&self, node: &Node<'_>) -> RenameClass {
        self.rename_class(node.kind())
    }
}
