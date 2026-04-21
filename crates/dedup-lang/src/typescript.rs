//! The TypeScript / TSX language profile.
//!
//! Parses `.ts` and `.tsx` sources with `tree-sitter-typescript`,
//! treats each `function_declaration`, `method_definition`,
//! `arrow_function`, and `class_declaration` as a syntactic unit, and
//! classifies leaf nodes so local bindings and parameters alpha-rename
//! while function / class / type / method / imported / JSX-element
//! names are kept verbatim.
//!
//! Unlike the Rust grammar, TypeScript's grammar reuses the plain
//! `identifier` leaf in both must-rename positions (locals, params,
//! usage inside expressions) and must-keep positions (function-
//! declaration name, JSX element tag, items inside an `import_statement`
//! subtree, callee side of a property lookup). The profile therefore
//! overrides [`LanguageProfile::classify_node`] and disambiguates by
//! walking the leaf's parent chain.

use tree_sitter::{Language, Node};

use crate::profile::{LanguageProfile, RenameClass};

/// Singleton TypeScript profile (`.ts`). Exposed through
/// [`crate::all_profiles`] / [`crate::profile_for_extension`].
pub struct TypeScriptProfile;

/// Singleton TSX profile (`.tsx`). Shares classification logic with
/// [`TypeScriptProfile`] and differs only in the tree-sitter grammar
/// it loads, which is the JSX-aware dialect.
pub struct TsxProfile;

/// The canonical static instance of [`TypeScriptProfile`].
pub static TYPESCRIPT_PROFILE: TypeScriptProfile = TypeScriptProfile;

/// The canonical static instance of [`TsxProfile`].
pub static TSX_PROFILE: TsxProfile = TsxProfile;

const TS_EXTENSIONS: &[&str] = &["ts"];
const TSX_EXTENSIONS: &[&str] = &["tsx"];

const SYNTACTIC_UNITS: &[&str] = &[
    "function_declaration",
    "method_definition",
    "arrow_function",
    "class_declaration",
];

/// Shared kind-only classification for both TS and TSX. See
/// [`shared_classify_node`] for the parent-context overrides.
fn shared_rename_class(node_kind: &str) -> RenameClass {
    match node_kind {
        // Literals. `string_fragment` is the inside of a string once
        // tree-sitter has split quote tokens off; `regex_pattern` is
        // the body of a regex literal.
        "string_fragment" | "template_chars" | "number" | "regex_pattern" | "regex_flags"
        | "true" | "false" | "null" | "undefined" => RenameClass::Literal,

        // Plain `identifier` defaults to Local (locals / params /
        // bare variable references). Must-keep cases are handled in
        // `shared_classify_node` via parent-context lookup.
        "identifier" => RenameClass::Local,

        // Names that carry meaning across the corpus — never rename.
        "type_identifier"
        | "property_identifier"
        | "shorthand_property_identifier"
        | "shorthand_property_identifier_pattern"
        | "predefined_type"
        | "statement_identifier"
        | "this"
        | "super" => RenameClass::Kept,

        _ => RenameClass::Kept,
    }
}

/// Parent-aware classification used by both the TS and TSX profiles.
///
/// The tree-sitter TypeScript grammar reuses the plain `identifier`
/// leaf across wildly different positions. For correctness we walk a
/// short parent chain and return [`RenameClass::Kept`] when the leaf
/// sits in one of:
///
/// - `function_declaration` / `function_expression` / `generator_function_declaration`
///   **name field** — keep so function names match across fixtures.
/// - Any ancestor in `import_statement` / `export_statement` /
///   `export_specifier` — imports / exports are verbatim.
/// - Any JSX tag node (`jsx_opening_element`, `jsx_closing_element`,
///   `jsx_self_closing_element`, `jsx_namespace_name`, `jsx_attribute`,
///   `jsx_member_expression`, `jsx_expression`'s tag side) — JSX
///   element and attribute names stay verbatim.
/// - RHS of a `member_expression` / `subscript_expression` — property
///   access targets are semantic, not renamable locals.
fn shared_classify_node(node: &Node<'_>) -> RenameClass {
    let kind = node.kind();
    if kind != "identifier" {
        return shared_rename_class(kind);
    }

    // Import specifier subtrees are small and every identifier in
    // them is a name we must keep (e.g. `Row`, `Total`, `util`).
    // Note: we intentionally do NOT walk `export_statement` as a
    // blanket rule — `export function foo() { ... }` has the whole
    // function body under an `export_statement`, and we still want
    // its locals to alpha-rename. Named re-exports
    // (`export { Row, Total }`) are caught below via
    // `export_specifier` / `export_clause` instead.
    let mut anc = node.parent();
    while let Some(p) = anc {
        match p.kind() {
            "import_statement" | "import_clause" | "import_specifier" | "namespace_import"
            | "export_specifier" | "export_clause" => return RenameClass::Kept,
            "program" => break,
            _ => {}
        }
        anc = p.parent();
    }

    // Direct-parent disambiguation.
    if let Some(parent) = node.parent() {
        match parent.kind() {
            // Function / class declaration names.
            "function_declaration"
            | "function_expression"
            | "generator_function_declaration"
            | "generator_function" => {
                if named_child_is_name(&parent, node) {
                    return RenameClass::Kept;
                }
            }
            // JSX tag / attribute names. The grammar attaches the tag
            // identifier (e.g. `Header`, `section`) as the `name`
            // field of the opening / closing / self-closing element.
            // JSX attribute names like `onClick` are a
            // `property_identifier` already Kept by kind, but nested
            // namespace / member tag names use `identifier`.
            "jsx_opening_element" | "jsx_closing_element" | "jsx_self_closing_element" => {
                if named_child_is_name(&parent, node) {
                    return RenameClass::Kept;
                }
            }
            // Nested JSX tag names: `<Foo.Bar />` and `<ns:Tag />`.
            "jsx_member_expression" | "jsx_namespace_name" => return RenameClass::Kept,
            // RHS of `foo.bar` — the `.bar` side is a property, not a
            // local. TS uses `property_identifier` there in most cases,
            // but some grammar variants emit `identifier`; keep either.
            "member_expression" | "subscript_expression" => {
                if parent
                    .child_by_field_name("property")
                    .is_some_and(|c| c.id() == node.id())
                {
                    return RenameClass::Kept;
                }
            }
            _ => {}
        }
    }

    RenameClass::Local
}

/// True when `child` is the `name` field of `parent`. We use the
/// tree-sitter field API rather than indexing because named children
/// include attributes / decorators on some nodes.
fn named_child_is_name<'a>(parent: &Node<'a>, child: &Node<'a>) -> bool {
    parent
        .child_by_field_name("name")
        .is_some_and(|c| c.id() == child.id())
}

impl LanguageProfile for TypeScriptProfile {
    fn name(&self) -> &'static str {
        "typescript"
    }

    fn extensions(&self) -> &[&'static str] {
        TS_EXTENSIONS
    }

    fn tree_sitter_language(&self) -> Language {
        tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
    }

    fn syntactic_units(&self) -> &[&'static str] {
        SYNTACTIC_UNITS
    }

    fn rename_class(&self, node_kind: &str) -> RenameClass {
        shared_rename_class(node_kind)
    }

    fn classify_node(&self, node: &Node<'_>) -> RenameClass {
        shared_classify_node(node)
    }
}

impl LanguageProfile for TsxProfile {
    fn name(&self) -> &'static str {
        "tsx"
    }

    fn extensions(&self) -> &[&'static str] {
        TSX_EXTENSIONS
    }

    fn tree_sitter_language(&self) -> Language {
        tree_sitter_typescript::LANGUAGE_TSX.into()
    }

    fn syntactic_units(&self) -> &[&'static str] {
        SYNTACTIC_UNITS
    }

    fn rename_class(&self, node_kind: &str) -> RenameClass {
        shared_rename_class(node_kind)
    }

    fn classify_node(&self, node: &Node<'_>) -> RenameClass {
        shared_classify_node(node)
    }
}
