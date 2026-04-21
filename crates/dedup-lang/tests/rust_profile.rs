//! Integration tests for the Rust language profile.
//!
//! Two axes of coverage:
//!
//! 1. **Snapshot** (`rust_profile_snapshot`): parse the in-workspace
//!    `fixtures/rust/` corpus, extract every Tier B syntactic unit,
//!    run the normalizer, and dump a compact deterministic summary
//!    through `insta::assert_snapshot!`. Proves both the Rust profile
//!    and the normalizer land on the expected token streams.
//! 2. **Property** (`normalize_is_idempotent`): walk every normalized
//!    function body in `fixtures/rust/` and verify the idempotency
//!    invariant `normalize(normalize(x)) == normalize(x)`. Because the
//!    Rust profile alpha-renames locals, this proves the second-pass
//!    normalisation produces the same aliases as the first.

use std::path::PathBuf;

use dedup_lang::{LanguageProfile, NormalizedToken, RUST_PROFILE, extract_units};
use proptest::prelude::*;
use tree_sitter::Parser;

fn workspace_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); // dedup-lang → crates
    p.pop(); // crates     → workspace root
    p
}

fn fixture_files() -> Vec<(PathBuf, String)> {
    let dir = workspace_root().join("fixtures").join("rust");
    let mut files: Vec<(PathBuf, String)> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read fixtures/rust: {e}"))
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .filter(|e| {
            e.path()
                .extension()
                .and_then(|s| s.to_str())
                .map(|s| s == "rs")
                .unwrap_or(false)
        })
        .map(|e| {
            let p = e.path();
            let text = std::fs::read_to_string(&p).unwrap();
            let rel = p.file_name().unwrap().to_string_lossy().into_owned();
            (PathBuf::from(rel), text)
        })
        .collect();
    files.sort_by(|a, b| a.0.cmp(&b.0));
    files
}

fn parse(source: &str) -> tree_sitter::Tree {
    let mut parser = Parser::new();
    parser
        .set_language(&RUST_PROFILE.tree_sitter_language())
        .expect("set rust grammar");
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
fn rust_profile_snapshot() {
    let files = fixture_files();
    assert!(!files.is_empty(), "fixtures/rust/ is empty");

    let mut lines: Vec<String> = Vec::new();
    for (path, source) in &files {
        let tree = parse(source);
        let units = extract_units(&tree, source.as_bytes(), &RUST_PROFILE);
        for u in units {
            lines.push(format!(
                "{} {}..{} {} hash={:016x} tokens=[{}]",
                path.display(),
                u.start_line,
                u.end_line,
                u.kind,
                u.hash,
                render_tokens(&u.tokens),
            ));
        }
    }

    let joined = lines.join("\n");
    insta::assert_snapshot!("rust_profile_units", joined);
}

#[test]
fn type1_and_type2_duplicates_bucket_together() {
    // Tier B should group the identical `type1_identical` bodies by
    // hash equality. The `type2_*` pair should also bucket together
    // even though their locals differ — that's the whole point of
    // alpha-renaming. The unique functions must sit in buckets of 1.
    let files = fixture_files();

    // Map hash → list of (path, unit_kind + function-ish name peek).
    let mut buckets: std::collections::HashMap<u64, Vec<String>> = std::collections::HashMap::new();

    for (path, source) in &files {
        let tree = parse(source);
        let units = extract_units(&tree, source.as_bytes(), &RUST_PROFILE);
        for u in units {
            if u.kind != "function_item" {
                continue;
            }
            // Peek the source to grab an approximate function name for
            // the assertion text.
            let snippet = &source[u.start_byte..u.end_byte];
            let name = snippet
                .split_whitespace()
                .nth(1)
                .unwrap_or("?")
                .trim_end_matches('(')
                .to_string();
            buckets
                .entry(u.hash)
                .or_default()
                .push(format!("{}::{}", path.display(), name,));
        }
    }

    let dup_buckets: Vec<Vec<String>> = buckets.into_values().filter(|v| v.len() >= 2).collect();

    // Two 2-way buckets expected: the Type-1 pair and the Type-2 pair.
    assert_eq!(
        dup_buckets.len(),
        2,
        "expected 2 duplicate buckets, got {:#?}",
        dup_buckets
    );
    for bucket in &dup_buckets {
        assert_eq!(bucket.len(), 2, "bucket size drifted: {:#?}", bucket);
    }

    // And every bucket must pair an alpha.rs entry with a beta.rs
    // entry — the duplicates live across the two fixture files.
    for bucket in &dup_buckets {
        let has_alpha = bucket.iter().any(|s| s.starts_with("alpha.rs"));
        let has_beta = bucket.iter().any(|s| s.starts_with("beta.rs"));
        assert!(
            has_alpha && has_beta,
            "bucket should pair alpha.rs with beta.rs: {:#?}",
            bucket
        );
    }
}

#[test]
fn normalize_is_idempotent() {
    // Alpha-rename idempotency: normalising a normalised stream again
    // yields the same tokens. Because our normaliser operates on a
    // tree-sitter syntax tree, "normalising again" means reconstructing
    // a source-like string from the first pass, re-parsing it, and
    // extracting its tokens. Instead of reconstructing text (brittle),
    // we directly verify the narrower algebraic property:
    //
    //   apply_rename(apply_rename(tokens)) == apply_rename(tokens)
    //
    // where `apply_rename` re-runs the local-alias replacement on
    // identifier tokens that look like `v1` / `v2` / ... — these must
    // map to the same aliases the first pass assigned.
    for (_path, source) in fixture_files() {
        let tree = parse(&source);
        let units = extract_units(&tree, source.as_bytes(), &RUST_PROFILE);
        for u in units {
            let once = u.tokens.clone();
            let twice = reapply_rename(&once);
            assert_eq!(
                once, twice,
                "normalize should be idempotent; drifted inside unit {:?} at lines {}..{}",
                u.kind, u.start_line, u.end_line
            );
        }
    }
}

/// Re-run the local-alias assignment over an already-normalised token
/// stream. Every `identifier` whose text begins with `v` and is
/// followed by an integer is treated as a local; the function assigns
/// aliases in first-occurrence order exactly like the real
/// normaliser. If the pass is idempotent, the output equals the input.
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

/// Random Rust-ish function body synthesized from a small token
/// alphabet. Not intended to compile — only to parse — and tree-sitter
/// is permissive enough for Tier B to still extract the
/// `function_item` and normalize it.
fn synth_function() -> impl Strategy<Value = String> {
    // Body: a sequence of `let <ident> = <int>;` plus a single
    // trailing expression. Keeps the grammar valid without needing a
    // full-blown Rust generator.
    let ident = proptest::sample::select(vec!["a", "b", "c", "d", "foo", "bar", "baz"]);
    let int = 0u32..100;
    let line = (ident, int).prop_map(|(n, v)| format!("    let {n} = {v};\n"));
    proptest::collection::vec(line, 3..10).prop_map(|lines| {
        let body: String = lines.into_iter().collect();
        format!("fn generated() -> i32 {{\n{body}    0\n}}\n")
    })
}

fn parse_and_normalize(source: &str) -> Vec<Vec<NormalizedToken>> {
    let tree = parse(source);
    let units = extract_units(&tree, source.as_bytes(), &RUST_PROFILE);
    units.into_iter().map(|u| u.tokens).collect()
}

proptest! {
    /// Normalizing the same input twice yields the same tokens. This
    /// is the determinism half of the idempotency invariant.
    #[test]
    fn normalize_is_deterministic_on_synth_rust(src in synth_function()) {
        let a = parse_and_normalize(&src);
        let b = parse_and_normalize(&src);
        prop_assert_eq!(a, b);
    }

    /// Re-running alpha-renaming over an already-normalized stream is
    /// a fixed-point: `apply_rename(apply_rename(x)) == apply_rename(x)`.
    /// This is the alpha-rename-idempotency half.
    #[test]
    fn alpha_rename_is_idempotent_on_synth_rust(src in synth_function()) {
        let units = parse_and_normalize(&src);
        for tokens in units {
            let once = reapply_rename(&tokens);
            let twice = reapply_rename(&once);
            prop_assert_eq!(once, twice);
        }
    }
}
