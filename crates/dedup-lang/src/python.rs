//! The Python language profile.
//!
//! Parses `.py` sources with `tree-sitter-python`, treats each
//! `function_definition` and `class_definition` as a syntactic unit,
//! and classifies leaf nodes so local bindings and parameters
//! alpha-rename while function / class names, decorators, imports,
//! attribute accesses, and type annotations stay verbatim.
//!
//! The Python grammar reuses the plain `identifier` leaf in many
//! must-keep positions (decorator targets, import specifiers, the name
//! field of `function_definition` / `class_definition`, the right side
//! of an `attribute` access, class bases, keyword-argument names, type
//! annotations, ...). The profile therefore overrides
//! [`LanguageProfile::classify_node`] and disambiguates by walking the
//! leaf's parent chain — same shape as the TypeScript profile.

use tree_sitter::{Language, Node};

use crate::profile::{LanguageProfile, RenameClass};

/// Singleton Python profile. Exposed through [`crate::all_profiles`] /
/// [`crate::profile_for_extension`].
pub struct PythonProfile;

/// The canonical static instance of [`PythonProfile`].
pub static PYTHON_PROFILE: PythonProfile = PythonProfile;

const EXTENSIONS: &[&str] = &["py"];

const SYNTACTIC_UNITS: &[&str] = &["function_definition", "class_definition"];

/// Kind-only classification (fallback when parent context is not
/// needed). See [`python_classify_node`] for the parent-aware
/// overrides that actually drive behaviour on `identifier` leaves.
fn python_rename_class(node_kind: &str) -> RenameClass {
    match node_kind {
        // Literals. Python splits strings into `string_start` /
        // `string_content` / `string_end`; the content is the only
        // piece that carries the literal's value. `string_start` /
        // `string_end` are quote tokens — keep them verbatim so
        // b"..."  vs "..." still distinguish.
        "integer"
        | "float"
        | "string_content"
        | "escape_sequence"
        | "true"
        | "false"
        | "none"
        | "concatenated_string" => RenameClass::Literal,

        // Plain `identifier` defaults to Local (locals / params / bare
        // variable references). Must-keep cases are handled in
        // `python_classify_node` via parent-context lookup.
        "identifier" => RenameClass::Local,

        // Names that carry meaning — never rename.
        _ => RenameClass::Kept,
    }
}

/// Parent-aware classification for Python.
///
/// We walk a short ancestor chain and return [`RenameClass::Kept`]
/// when the leaf sits in one of the following positions:
///
/// - Any `decorator` ancestor — decorators stay verbatim (e.g.
///   `@staticmethod`, `@dataclass`, `@app.route`).
/// - Any `import_statement` / `import_from_statement` ancestor —
///   imports are verbatim, including `from x import y as z`.
/// - The `name` field of a `function_definition` or
///   `class_definition` — function / class names survive so cross-file
///   matches line up.
/// - The `name` field of a `keyword_argument` — in `foo(key=value)`,
///   `key` is a parameter name, not a rename-able local.
/// - The right side of an `attribute` access — `self.size` keeps
///   `size` because it's a property / field, not a local.
/// - Any child of a `type` node — type annotations (`: int`,
///   `-> Row`) reference names that must stay.
/// - Any identifier under the class-base `argument_list` of a
///   `class_definition` — base-class references are type-like.
fn python_classify_node(node: &Node<'_>) -> RenameClass {
    let kind = node.kind();
    if kind != "identifier" {
        return python_rename_class(kind);
    }

    // Ancestor scan: decorators + imports + type annotations + class
    // bases. We stop at the enclosing module / function boundary.
    let mut anc = node.parent();
    while let Some(p) = anc {
        match p.kind() {
            "decorator"
            | "import_statement"
            | "import_from_statement"
            | "aliased_import"
            | "relative_import"
            | "dotted_name"
            | "type" => return RenameClass::Kept,
            // Class-base argument list: its parent is `class_definition`.
            "argument_list" => {
                if let Some(gp) = p.parent()
                    && gp.kind() == "class_definition"
                {
                    return RenameClass::Kept;
                }
            }
            // Stop climbing once we reach a containing definition or
            // the module root — anything further up is context we've
            // already covered.
            "module" => break,
            _ => {}
        }
        anc = p.parent();
    }

    // Direct-parent disambiguation.
    if let Some(parent) = node.parent() {
        match parent.kind() {
            // Function / class name.
            "function_definition" | "class_definition" => {
                if named_child_is_name(&parent, node) {
                    return RenameClass::Kept;
                }
            }
            // `foo(key=value)` — the `key` side is a parameter name.
            "keyword_argument" => {
                if named_child_is_name(&parent, node) {
                    return RenameClass::Kept;
                }
            }
            // `obj.field` — the `field` side is a property access.
            "attribute" => {
                if parent
                    .child_by_field_name("attribute")
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

/// True when `child` is the `name` field of `parent`.
fn named_child_is_name<'a>(parent: &Node<'a>, child: &Node<'a>) -> bool {
    parent
        .child_by_field_name("name")
        .is_some_and(|c| c.id() == child.id())
}

impl LanguageProfile for PythonProfile {
    fn name(&self) -> &'static str {
        "python"
    }

    fn extensions(&self) -> &[&'static str] {
        EXTENSIONS
    }

    fn tree_sitter_language(&self) -> Language {
        tree_sitter_python::LANGUAGE.into()
    }

    fn syntactic_units(&self) -> &[&'static str] {
        SYNTACTIC_UNITS
    }

    fn rename_class(&self, node_kind: &str) -> RenameClass {
        python_rename_class(node_kind)
    }

    fn classify_node(&self, node: &Node<'_>) -> RenameClass {
        python_classify_node(node)
    }
}
