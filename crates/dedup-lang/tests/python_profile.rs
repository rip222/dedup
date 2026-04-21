//! Integration tests for the Python language profile.
//!
//! Coverage mirrors `typescript_profile.rs`:
//!
//! 1. **Snapshot** (`python_profile_snapshot`): parse every file in
//!    `fixtures/python/`, extract Tier B syntactic units, and dump a
//!    deterministic normalized summary through `insta::assert_snapshot!`.
//!    Locks in detection of `function_definition` and `class_definition`.
//! 2. **Grouping** (`py_type1_and_type2_duplicates_bucket_together`):
//!    proves alpha.py / beta.py near-duplicate pairs cluster.
//! 3. **Decorators** (`decorators_are_kept_verbatim`): asserts decorator
//!    identifiers (`@staticmethod`, `@dataclass`, `@lru_cache`) survive
//!    normalization unchanged.
//! 4. **Imports** (`imports_are_kept_verbatim_py`): asserts `from x
//!    import y as z` specifier names survive verbatim.
//! 5. **Extension detection** (`python_detected_by_extension`): proves
//!    `.py` routes to the Python profile via the registry.
//! 6. **Idempotency** (`normalize_is_idempotent_py`): reapplying the
//!    local-alias rewrite over an already-normalized token stream is a
//!    fixed point.

use std::path::PathBuf;

use dedup_lang::{
    LanguageProfile, NormalizedToken, PYTHON_PROFILE, extract_units, profile_for_extension,
};
use tree_sitter::Parser;

fn workspace_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); // dedup-lang → crates
    p.pop(); // crates     → workspace root
    p
}

struct Fixture {
    rel: PathBuf,
    source: String,
    profile: &'static dyn LanguageProfile,
}

fn fixture_files() -> Vec<Fixture> {
    let dir = workspace_root().join("fixtures").join("python");
    let mut files: Vec<Fixture> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read fixtures/python: {e}"))
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .filter_map(|e| {
            let path = e.path();
            let ext = path.extension().and_then(|s| s.to_str())?.to_string();
            if ext != "py" {
                return None;
            }
            let profile: &'static dyn LanguageProfile = &PYTHON_PROFILE;
            let source = std::fs::read_to_string(&path).ok()?;
            let rel = PathBuf::from(path.file_name()?.to_string_lossy().into_owned());
            Some(Fixture {
                rel,
                source,
                profile,
            })
        })
        .collect();
    files.sort_by(|a, b| a.rel.cmp(&b.rel));
    files
}

fn parse(source: &str, profile: &dyn LanguageProfile) -> tree_sitter::Tree {
    let mut parser = Parser::new();
    parser
        .set_language(&profile.tree_sitter_language())
        .expect("set grammar");
    parser.parse(source, None).expect("parse")
}

fn render_tokens(tokens: &[NormalizedToken]) -> String {
    tokens
        .iter()
        .map(|t| format!("{}:{}", t.kind, t.text))
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn python_profile_snapshot() {
    let files = fixture_files();
    assert!(!files.is_empty(), "fixtures/python/ is empty");

    let mut lines: Vec<String> = Vec::new();
    for f in &files {
        let tree = parse(&f.source, f.profile);
        let units = extract_units(&tree, f.source.as_bytes(), f.profile);
        for u in units {
            lines.push(format!(
                "{} {}..{} {} hash={:016x} tokens=[{}]",
                f.rel.display(),
                u.start_line,
                u.end_line,
                u.kind,
                u.hash,
                render_tokens(&u.tokens),
            ));
        }
    }

    let joined = lines.join("\n");
    insta::assert_snapshot!("python_profile_units", joined);
}

#[test]
fn py_type1_and_type2_duplicates_bucket_together() {
    // The alpha.py ↔ beta.py pair is designed to produce two
    // two-member function buckets: `type1_identical` (Type-1) and the
    // `type2_original` / `type2_renamed` pair (Type-2).
    let files = fixture_files();

    let mut buckets: std::collections::HashMap<u64, Vec<String>> = std::collections::HashMap::new();

    for f in &files {
        // Only cluster top-level functions in alpha/beta.
        let name = f.rel.to_string_lossy();
        if name != "alpha.py" && name != "beta.py" {
            continue;
        }
        let tree = parse(&f.source, f.profile);
        let units = extract_units(&tree, f.source.as_bytes(), f.profile);
        for u in units {
            if u.kind != "function_definition" {
                continue;
            }
            buckets.entry(u.hash).or_default().push(format!(
                "{}::{}",
                f.rel.display(),
                u.start_line
            ));
        }
    }

    let dup_buckets: Vec<Vec<String>> = buckets.into_values().filter(|v| v.len() >= 2).collect();

    assert_eq!(
        dup_buckets.len(),
        2,
        "expected 2 duplicate buckets, got {:#?}",
        dup_buckets
    );
    for bucket in &dup_buckets {
        assert_eq!(bucket.len(), 2, "bucket size drifted: {:#?}", bucket);
        let has_alpha = bucket.iter().any(|s| s.starts_with("alpha.py"));
        let has_beta = bucket.iter().any(|s| s.starts_with("beta.py"));
        assert!(
            has_alpha && has_beta,
            "bucket should pair alpha.py with beta.py: {:#?}",
            bucket
        );
    }
}

#[test]
fn decorators_are_kept_verbatim() {
    // Decorator identifiers must be classified as Kept so they survive
    // normalization verbatim. Two cases matter:
    //
    // 1. Nested decorators (e.g. `@staticmethod`, `@property`) live
    //    inside a `class_definition` syntactic unit, so they surface
    //    directly in `extract_units` output.
    // 2. Top-level decorators (e.g. `@lru_cache`, `@dataclass`) wrap
    //    the unit from outside as a `decorated_definition` parent, so
    //    they don't appear in the extracted token stream. We verify
    //    those separately by normalising the `decorated_definition`
    //    subtree directly through the profile's classifier.
    use dedup_lang::{RenameClass, normalize};

    let files = fixture_files();
    let dec = files
        .iter()
        .find(|f| f.rel.as_os_str() == "decorated.py")
        .expect("decorated.py fixture");

    let tree = parse(&dec.source, dec.profile);

    // Case 1: nested decorators appear in the class unit's stream.
    let units = extract_units(&tree, dec.source.as_bytes(), dec.profile);
    let class_unit = units
        .iter()
        .find(|u| u.kind == "class_definition")
        .expect("class_definition present");
    let class_idents: Vec<&str> = class_unit
        .tokens
        .iter()
        .filter(|t| t.kind == "identifier")
        .map(|t| t.text.as_str())
        .collect();
    for expected in ["staticmethod", "property"] {
        assert!(
            class_idents.contains(&expected),
            "nested decorator {expected:?} must be kept verbatim in class unit; got {class_idents:#?}",
        );
    }

    // Case 2: top-level decorators — walk the module and classify the
    // identifier leaf under each `decorator` node.
    let mut top_level_decorator_names: Vec<String> = Vec::new();
    collect_decorator_identifiers(
        tree.root_node(),
        dec.source.as_bytes(),
        dec.profile,
        &mut top_level_decorator_names,
    );
    for expected in ["lru_cache", "dataclass"] {
        assert!(
            top_level_decorator_names.iter().any(|t| t == expected),
            "top-level decorator {expected:?} must classify as Kept; got {top_level_decorator_names:#?}",
        );
    }

    // And additionally: normalising a whole `decorated_definition`
    // subtree must emit the decorator identifier verbatim (no `v`
    // alias).
    let root = tree.root_node();
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() == "decorated_definition" {
            let toks = normalize(child, dec.source.as_bytes(), dec.profile);
            // Every decorator identifier should surface as itself.
            let idents: Vec<&str> = toks
                .iter()
                .filter(|t| t.kind == "identifier")
                .map(|t| t.text.as_str())
                .collect();
            // At least one of lru_cache / dataclass should appear
            // unaliased under a top-level decorated_definition.
            let ok = idents
                .iter()
                .any(|i| matches!(*i, "lru_cache" | "dataclass"));
            if ok {
                // Sanity: ensure the decorator identifier is classified
                // as Kept by the profile in this context.
                for decorator_name in ["lru_cache", "dataclass"] {
                    if idents.contains(&decorator_name) {
                        // Find the leaf identifier node in the decorator
                        // and assert it classifies to Kept.
                        let node =
                            find_identifier_with_text(child, decorator_name, dec.source.as_bytes())
                                .expect("decorator identifier node");
                        assert_eq!(
                            dec.profile.classify_node(&node),
                            RenameClass::Kept,
                            "{decorator_name} must classify as Kept",
                        );
                    }
                }
            }
        }
    }
}

fn collect_decorator_identifiers(
    node: tree_sitter::Node,
    source: &[u8],
    profile: &dyn LanguageProfile,
    out: &mut Vec<String>,
) {
    if node.kind() == "decorator" {
        let mut cur = node.walk();
        for child in node.children(&mut cur) {
            walk_and_collect_kept_identifiers(child, source, profile, out);
        }
        return;
    }
    let mut cur = node.walk();
    for child in node.children(&mut cur) {
        collect_decorator_identifiers(child, source, profile, out);
    }
}

fn walk_and_collect_kept_identifiers(
    node: tree_sitter::Node,
    source: &[u8],
    profile: &dyn LanguageProfile,
    out: &mut Vec<String>,
) {
    if node.child_count() == 0 {
        if node.kind() == "identifier"
            && matches!(profile.classify_node(&node), dedup_lang::RenameClass::Kept)
            && let Ok(text) = node.utf8_text(source)
        {
            out.push(text.to_string());
        }
        return;
    }
    let mut cur = node.walk();
    for c in node.children(&mut cur) {
        walk_and_collect_kept_identifiers(c, source, profile, out);
    }
}

fn find_identifier_with_text<'a>(
    node: tree_sitter::Node<'a>,
    want: &str,
    source: &[u8],
) -> Option<tree_sitter::Node<'a>> {
    if node.kind() == "identifier"
        && node.child_count() == 0
        && node.utf8_text(source).ok() == Some(want)
    {
        return Some(node);
    }
    let mut cur = node.walk();
    for c in node.children(&mut cur) {
        if let Some(found) = find_identifier_with_text(c, want, source) {
            return Some(found);
        }
    }
    None
}

#[test]
fn imports_are_kept_verbatim_py() {
    // alpha.py imports `{ Row, Total }` via `from .types import ...`
    // plus `import util.helpers`. None of these names must alpha-rename.
    // They appear outside the syntactic units proper (module-level
    // imports), so we scan the full module by normalising each
    // function_definition and checking that names referenced by type
    // annotations / calls survive. Specifically, `List` and `Total`
    // appear in function signatures.
    let files = fixture_files();
    let alpha = files
        .iter()
        .find(|f| f.rel.as_os_str() == "alpha.py")
        .expect("alpha.py fixture");

    let tree = parse(&alpha.source, alpha.profile);
    let units = extract_units(&tree, alpha.source.as_bytes(), alpha.profile);

    let func = units
        .iter()
        .find(|u| u.kind == "function_definition")
        .expect("function_definition present");

    let kept_texts: Vec<&str> = func
        .tokens
        .iter()
        .filter(|t| t.kind == "identifier")
        .map(|t| t.text.as_str())
        .collect();

    // `List` is the parameter's type annotation, `Total` is the return
    // type annotation; both must be Kept so they appear verbatim.
    assert!(
        kept_texts.contains(&"List"),
        "List must survive verbatim: {kept_texts:?}"
    );
    assert!(
        kept_texts.contains(&"Total"),
        "Total must survive verbatim: {kept_texts:?}"
    );

    // And the function name itself (`type1_identical`) must be kept.
    assert!(
        kept_texts.contains(&"type1_identical"),
        "function name must survive verbatim: {kept_texts:?}"
    );
}

#[test]
fn python_detected_by_extension() {
    let profile = profile_for_extension("py").expect("py profile registered");
    assert_eq!(profile.name(), "python");
}

#[test]
fn normalize_is_idempotent_py() {
    // Alpha-rename fixed-point: re-running the local-alias assignment
    // over an already-normalized token stream must return the same
    // stream.
    for f in fixture_files() {
        let tree = parse(&f.source, f.profile);
        let units = extract_units(&tree, f.source.as_bytes(), f.profile);
        for u in units {
            let once = u.tokens.clone();
            let twice = reapply_rename(&once);
            assert_eq!(
                once,
                twice,
                "python normalize should be idempotent; drifted inside unit {:?} at lines {}..{} of {}",
                u.kind,
                u.start_line,
                u.end_line,
                f.rel.display(),
            );
        }
    }
}

fn reapply_rename(tokens: &[NormalizedToken]) -> Vec<NormalizedToken> {
    let mut seen: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut out: Vec<NormalizedToken> = Vec::with_capacity(tokens.len());
    for t in tokens {
        if t.kind == "identifier" && is_alias(&t.text) {
            let alias = match seen.get(&t.text) {
                Some(a) => a.clone(),
                None => {
                    let a = format!("v{}", seen.len() + 1);
                    seen.insert(t.text.clone(), a.clone());
                    a
                }
            };
            out.push(NormalizedToken {
                kind: t.kind.clone(),
                text: alias,
            });
        } else {
            out.push(t.clone());
        }
    }
    out
}

fn is_alias(s: &str) -> bool {
    if let Some(rest) = s.strip_prefix('v') {
        !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit())
    } else {
        false
    }
}
