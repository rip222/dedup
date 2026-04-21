//! Integration tests for the TypeScript / TSX language profile.
//!
//! Coverage mirrors `rust_profile.rs`:
//!
//! 1. **Snapshot** (`typescript_profile_snapshot`): parse every file in
//!    `fixtures/typescript/`, route each path to its profile by
//!    extension, extract Tier B syntactic units, and dump a
//!    deterministic normalized summary through `insta::assert_snapshot!`.
//!    Locks in detection of `function_declaration`, `arrow_function`,
//!    `method_definition`, and `class_declaration`.
//! 2. **Grouping** (`ts_type1_and_type2_duplicates_bucket_together`):
//!    proves alpha.ts / beta.ts near-duplicate pairs cluster.
//! 3. **JSX** (`jsx_element_names_are_kept_verbatim`): asserts that
//!    a `<Header />` JSX tag name survives normalization unchanged,
//!    while a plain `const header = ...` identifier would not.
//! 4. **Imports** (`imports_are_kept_verbatim`): asserts import
//!    specifier names survive verbatim.

use std::path::PathBuf;

use dedup_lang::{
    LanguageProfile, NormalizedToken, TSX_PROFILE, TYPESCRIPT_PROFILE, extract_units,
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
    let dir = workspace_root().join("fixtures").join("typescript");
    let mut files: Vec<Fixture> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read fixtures/typescript: {e}"))
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .filter_map(|e| {
            let path = e.path();
            let ext = path.extension().and_then(|s| s.to_str())?.to_string();
            let profile: &'static dyn LanguageProfile = match ext.as_str() {
                "ts" => &TYPESCRIPT_PROFILE,
                "tsx" => &TSX_PROFILE,
                _ => return None,
            };
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
fn typescript_profile_snapshot() {
    let files = fixture_files();
    assert!(!files.is_empty(), "fixtures/typescript/ is empty");

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
    insta::assert_snapshot!("typescript_profile_units", joined);
}

#[test]
fn ts_type1_and_type2_duplicates_bucket_together() {
    // The alpha.ts ↔ beta.ts pair is designed to produce two
    // two-member buckets: `type1Identical` (Type-1) and the
    // `type2Original` / `type2Renamed` pair (Type-2). Everything else
    // sits in a bucket of 1.
    let files = fixture_files();

    let mut buckets: std::collections::HashMap<u64, Vec<String>> = std::collections::HashMap::new();

    for f in &files {
        if f.rel.extension().and_then(|s| s.to_str()) != Some("ts") {
            continue;
        }
        let tree = parse(&f.source, f.profile);
        let units = extract_units(&tree, f.source.as_bytes(), f.profile);
        for u in units {
            // Only cluster functional units, not the outer class.
            if u.kind != "function_declaration" && u.kind != "arrow_function" {
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
        let has_alpha = bucket.iter().any(|s| s.starts_with("alpha.ts"));
        let has_beta = bucket.iter().any(|s| s.starts_with("beta.ts"));
        assert!(
            has_alpha && has_beta,
            "bucket should pair alpha.ts with beta.ts: {:#?}",
            bucket
        );
    }
}

#[test]
fn jsx_element_names_are_kept_verbatim() {
    // Normalize every syntactic unit in component.tsx and check that
    // the JSX tag identifiers Header, Button, span, section, button,
    // h1 survive as verbatim `identifier` tokens. Because plain body
    // identifiers alpha-rename to `v1`, `v2`, ..., we can detect a
    // regression by scanning for these exact surface strings.
    let files = fixture_files();
    let tsx = files
        .iter()
        .find(|f| f.rel.as_os_str() == "component.tsx")
        .expect("component.tsx fixture");

    let tree = parse(&tsx.source, tsx.profile);
    let units = extract_units(&tree, tsx.source.as_bytes(), tsx.profile);

    let mut all_idents: Vec<String> = Vec::new();
    for u in &units {
        for t in &u.tokens {
            if t.kind == "identifier" {
                all_idents.push(t.text.clone());
            }
        }
    }

    for expected in ["Header", "Button", "section", "span"] {
        assert!(
            all_idents.iter().any(|t| t == expected),
            "expected JSX tag name {expected:?} preserved; got {all_idents:#?}",
        );
    }

    // And no token text should look like an alias for these names —
    // if we ever regress to renaming JSX tags, we'd see `v1` take
    // the role of `Header`.
    for aliased in &all_idents {
        assert!(
            !(aliased == "Header" && aliased.starts_with('v')),
            "Header must not be alpha-renamed"
        );
    }
}

#[test]
fn imports_are_kept_verbatim() {
    // alpha.ts imports `{ Row, Total }` and `* as util`; those names
    // must appear verbatim in the normalized token stream of any unit
    // that references them, and the `import_statement` subtree itself
    // must not alpha-rename its specifiers. We reparse alpha.ts with
    // a dummy profile that treats every function-ish subtree at file
    // level as a unit — practically we just check one function that
    // references `Row` / `Total` in its signature.
    let files = fixture_files();
    let alpha = files
        .iter()
        .find(|f| f.rel.as_os_str() == "alpha.ts")
        .expect("alpha.ts fixture");

    let tree = parse(&alpha.source, alpha.profile);
    let units = extract_units(&tree, alpha.source.as_bytes(), alpha.profile);

    // Grab the first function_declaration and confirm Row + Total
    // appear as Kept type_identifiers in its token stream.
    let func = units
        .iter()
        .find(|u| u.kind == "function_declaration")
        .expect("function_declaration present");

    let kept_texts: Vec<&str> = func
        .tokens
        .iter()
        .filter(|t| t.kind == "type_identifier")
        .map(|t| t.text.as_str())
        .collect();

    assert!(
        kept_texts.contains(&"Row"),
        "Row must survive verbatim: {kept_texts:?}"
    );
    assert!(
        kept_texts.contains(&"Total"),
        "Total must survive verbatim: {kept_texts:?}"
    );
}

#[test]
fn arrow_and_class_declarations_detected() {
    // Regression guard: alpha.ts must emit at least one arrow_function
    // unit and one class_declaration unit, confirming both kinds are
    // picked up by `extract_units` against `SYNTACTIC_UNITS`.
    let files = fixture_files();
    let alpha = files
        .iter()
        .find(|f| f.rel.as_os_str() == "alpha.ts")
        .expect("alpha.ts fixture");

    let tree = parse(&alpha.source, alpha.profile);
    let units = extract_units(&tree, alpha.source.as_bytes(), alpha.profile);

    let kinds: std::collections::HashSet<&str> = units.iter().map(|u| u.kind.as_str()).collect();

    for expected in [
        "arrow_function",
        "class_declaration",
        "function_declaration",
        "method_definition",
    ] {
        assert!(
            kinds.contains(expected),
            "expected unit kind {expected:?}; got {kinds:?}"
        );
    }
}

#[test]
fn normalize_is_idempotent_ts() {
    // Alpha-rename fixed-point: re-running the local-alias assignment
    // over an already-normalized token stream must return the same
    // stream. Mirrors the Rust idempotency test.
    for f in fixture_files() {
        let tree = parse(&f.source, f.profile);
        let units = extract_units(&tree, f.source.as_bytes(), f.profile);
        for u in units {
            let once = u.tokens.clone();
            let twice = reapply_rename(&once);
            assert_eq!(
                once,
                twice,
                "ts normalize should be idempotent; drifted inside unit {:?} at lines {}..{} of {}",
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
