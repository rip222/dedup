//! Bench (1): tokenize + rolling-hash on a 1k-line synthetic Rust file.
//!
//! The input is built once per run (outside the timed loop) and fed to
//! each iteration via `iter_batched`. That keeps the measurement focused
//! on the `tokenize` + `rolling_hash` pipeline and avoids charging every
//! sample the string-construction cost. A synthetic source is used
//! rather than a fixture so the bench is stable across fixture churn.

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use dedup_core::rolling_hash::rolling_hash;
use dedup_core::tokenizer::tokenize;

/// Build a deterministic ~1k-line Rust source. Each "block" is 10 lines
/// so 100 blocks land us at ~1000 lines.
fn synthetic_rust_source(lines_target: usize) -> String {
    let mut out = String::with_capacity(lines_target * 64);
    let blocks = lines_target / 10;
    for i in 0..blocks {
        // 10-line function body; varied identifiers so tokenization
        // isn't trivially compressible.
        out.push_str(&format!(
            "fn worker_{i}(input: &[i32]) -> i32 {{\n\
             \x20   let mut acc_{i} = 0i32;\n\
             \x20   let mut count_{i} = 0usize;\n\
             \x20   for value_{i} in input.iter() {{\n\
             \x20       acc_{i} = acc_{i}.wrapping_add(*value_{i});\n\
             \x20       count_{i} += 1;\n\
             \x20   }}\n\
             \x20   let mean_{i} = if count_{i} > 0 {{ acc_{i} / count_{i} as i32 }} else {{ 0 }};\n\
             \x20   mean_{i} + {i}\n\
             }}\n"
        ));
    }
    out
}

fn bench_tokenize_hash(c: &mut Criterion) {
    let source = synthetic_rust_source(1000);
    assert!(
        source.lines().count() >= 1000,
        "synthetic source should cover at least 1k lines"
    );

    let mut group = c.benchmark_group("tokenize_hash");
    group.sample_size(20);

    group.bench_function("tokenize_1k_lines", |b| {
        b.iter_batched(
            || source.clone(),
            |src| {
                let tokens = tokenize(&src);
                std::hint::black_box(tokens);
            },
            BatchSize::SmallInput,
        );
    });

    group.bench_function("tokenize_plus_rolling_hash_1k_lines", |b| {
        b.iter_batched(
            || source.clone(),
            |src| {
                let tokens = tokenize(&src);
                let hashes = rolling_hash(&tokens, 50);
                std::hint::black_box(hashes);
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

criterion_group!(benches, bench_tokenize_hash);
criterion_main!(benches);
