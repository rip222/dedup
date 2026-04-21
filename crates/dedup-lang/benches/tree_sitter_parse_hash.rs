//! Bench (2): tree-sitter parse + Tier B subtree normalization + hash.
//!
//! Uses the Rust profile against a representative fixture so the
//! measurement tracks real grammar output (not synthetic idealized
//! source). Each iteration builds a fresh `Parser` — the scanner does
//! the same, and tree-sitter parsers are not `Send` so reusing one
//! across criterion threads would require `thread_local!` or pooling.
//! We stay simple here to match scanner-path behaviour.

use std::path::PathBuf;

use criterion::{Criterion, criterion_group, criterion_main};
use dedup_lang::{
    LanguageProfile, NormalizationMode, RUST_PROFILE, extract_units_with_mode, hash_tokens,
};
use tree_sitter::Parser;

fn fixture_source() -> String {
    // CARGO_MANIFEST_DIR is crates/dedup-lang; fixtures live at the
    // workspace root. `alpha.rs` and `beta.rs` together exercise both
    // Type-1 and Type-2 duplicate shapes.
    let path: PathBuf = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("fixtures")
        .join("rust")
        .join("alpha.rs");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn bench_tree_sitter(c: &mut Criterion) {
    let source = fixture_source();
    let source_bytes = source.as_bytes();

    let mut group = c.benchmark_group("tree_sitter_parse_hash");
    group.sample_size(20);

    group.bench_function("parse_rust_fixture", |b| {
        b.iter(|| {
            let mut parser = Parser::new();
            parser
                .set_language(&RUST_PROFILE.tree_sitter_language())
                .expect("set rust grammar");
            let tree = parser.parse(&source, None).expect("parse");
            std::hint::black_box(tree);
        });
    });

    group.bench_function("parse_plus_extract_and_hash_units", |b| {
        b.iter(|| {
            let mut parser = Parser::new();
            parser
                .set_language(&RUST_PROFILE.tree_sitter_language())
                .expect("set rust grammar");
            let tree = parser.parse(&source, None).expect("parse");
            let units = extract_units_with_mode(
                &tree,
                source_bytes,
                &RUST_PROFILE,
                NormalizationMode::Conservative,
            );
            // Re-hash each unit's normalized tokens to exercise the full
            // normalize → hash pipeline (extract_units already computes
            // the hash once; re-hashing makes the bench less sensitive
            // to internal caching changes and mirrors "subtree hash" in
            // the issue wording).
            let mut sum: u64 = 0;
            for unit in &units {
                sum = sum.wrapping_add(hash_tokens(&unit.tokens));
            }
            std::hint::black_box(sum);
        });
    });

    group.finish();
}

criterion_group!(benches, bench_tree_sitter);
criterion_main!(benches);
