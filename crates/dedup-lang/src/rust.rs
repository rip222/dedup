//! The Rust language profile.
//!
//! Parses Rust sources with `tree-sitter-rust`, treats each
//! `function_item` / `impl_item` / `struct_item` / `enum_item` as a
//! syntactic unit, and classifies leaf identifier/literal nodes so
//! local variables alpha-rename while function/type/trait/module/macro
//! names are kept verbatim.
//!
//! Impl methods are picked up automatically: a walker that collects
//! `syntactic_units` recursively will see the outer `impl_item` *and*
//! the inner `function_item` for each method body. Callers are free to
//! keep both or dedupe by containment; the scanner in `dedup-core`
//! emits every matching unit, which is the right default — an
//! `impl_item` and its methods can both be Type-1 duplicated
//! independently.

use tree_sitter::Language;

use crate::profile::{LanguageProfile, RenameClass};

/// Singleton Rust profile. Exposed through [`crate::all_profiles`] /
/// [`crate::profile_for_extension`].
pub struct RustProfile;

/// The canonical static instance of [`RustProfile`]. Held by
/// `dedup-lang`'s profile registry.
pub static RUST_PROFILE: RustProfile = RustProfile;

const EXTENSIONS: &[&str] = &["rs"];
const SYNTACTIC_UNITS: &[&str] = &["function_item", "impl_item", "struct_item", "enum_item"];

impl LanguageProfile for RustProfile {
    fn name(&self) -> &'static str {
        "rust"
    }

    fn extensions(&self) -> &[&'static str] {
        EXTENSIONS
    }

    fn tree_sitter_language(&self) -> Language {
        tree_sitter_rust::LANGUAGE.into()
    }

    fn syntactic_units(&self) -> &[&'static str] {
        SYNTACTIC_UNITS
    }

    /// Rust-specific node-kind → rename policy.
    ///
    /// - Literals (`string_literal`, `integer_literal`, `float_literal`,
    ///   `char_literal`, `raw_string_literal`, `boolean_literal`) →
    ///   [`RenameClass::Literal`].
    /// - Plain `identifier` → [`RenameClass::Local`]. This is the
    ///   common case for local bindings and parameters inside a
    ///   function body; function/type/macro/module references in Rust
    ///   use distinct node kinds (see below) so this default is safe.
    /// - `type_identifier`, `field_identifier`, `primitive_type`,
    ///   `shorthand_field_identifier`, `scoped_type_identifier`
    ///   → [`RenameClass::Kept`]. Type names and struct-field names
    ///   must stay stable.
    /// - `super`, `self`, `crate`, lifetime identifiers, attributes
    ///   → [`RenameClass::Kept`].
    /// - Everything else → [`RenameClass::Kept`] (conservative
    ///   default).
    fn rename_class(&self, node_kind: &str) -> RenameClass {
        match node_kind {
            // Literals.
            "string_literal"
            | "raw_string_literal"
            | "integer_literal"
            | "float_literal"
            | "char_literal"
            | "boolean_literal"
            | "byte_string_literal"
            | "byte_literal" => RenameClass::Literal,

            // Locals and parameters. `identifier` in a Rust syntax tree
            // is the plain variable-binding form — tree-sitter-rust
            // uses distinct kinds (`type_identifier`, `field_identifier`,
            // `scoped_identifier`, ...) for named positions that must
            // stay verbatim.
            "identifier" => RenameClass::Local,

            // Names that carry meaning — do NOT alpha-rename.
            "type_identifier"
            | "primitive_type"
            | "field_identifier"
            | "shorthand_field_identifier"
            | "scoped_type_identifier"
            | "scoped_identifier"
            | "super"
            | "self"
            | "crate"
            | "lifetime"
            | "attribute_item" => RenameClass::Kept,

            _ => RenameClass::Kept,
        }
    }
}
