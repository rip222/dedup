//! Bench (3): bucket-fill + singleton-drop on 100k hash pairs.
//!
//! Mirrors the two-pass logic in `dedup_core::scanner` that tallies
//! hash counts, then materializes `Vec<WindowKey>` only for hashes with
//! at least two occurrences — the memory-discipline step added in
//! issue #14. The real scanner keeps this inline; re-implementing it in
//! the bench file avoids bloating `dedup_core`'s public API just to get
//! a benchmarkable handle. If we extract it for reuse later, flip this
//! bench to call the extracted function.
//!
//! Input: two synthetic hash streams totalling ~100k windows with a
//! tunable overlap fraction (currently ~5%). Hashes are drawn from a
//! deterministic PRNG so runs are comparable across invocations.

use std::hash::BuildHasherDefault;

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use rustc_hash::FxHasher;

/// Mirror of the scanner's internal `WindowKey` — a cheap `Copy` stub
/// that keeps the shape of the data the bucketer actually holds. The
/// fields are never read by the bench itself but are populated to
/// keep the struct's memory footprint representative.
#[derive(Clone, Copy)]
#[allow(dead_code)]
struct WindowKey {
    file: u32,
    win: u32,
}

type FxMap<K, V> = std::collections::HashMap<K, V, BuildHasherDefault<FxHasher>>;

/// Deterministic xorshift64*; we do not need cryptographic quality, just
/// a reproducible spread of 64-bit values.
fn xs64(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    *state = x;
    x.wrapping_mul(0x2545F4914F6CDD1D)
}

/// Build two files' worth of `(hash, ..)` windows so the combined stream
/// has `total` entries and roughly `overlap_pct` of hashes appear in
/// both files (so they land in non-singleton buckets).
fn build_streams(total: usize, overlap_pct: usize) -> Vec<Vec<u64>> {
    let per_file = total / 2;
    let overlap = per_file * overlap_pct / 100;
    let unique_per_file = per_file - overlap;

    let mut rng = 0xdeadbeef_cafef00du64;
    // Shared portion: emit the same sequence into both files.
    let shared: Vec<u64> = (0..overlap).map(|_| xs64(&mut rng)).collect();
    let mut file_a: Vec<u64> = Vec::with_capacity(per_file);
    let mut file_b: Vec<u64> = Vec::with_capacity(per_file);
    file_a.extend(&shared);
    file_b.extend(&shared);
    for _ in 0..unique_per_file {
        file_a.push(xs64(&mut rng));
    }
    for _ in 0..unique_per_file {
        file_b.push(xs64(&mut rng));
    }
    vec![file_a, file_b]
}

/// Two-pass bucket-fill: count occurrences first, then populate
/// non-singleton buckets. Matches scanner.rs step 3 behaviour.
fn bucketize(per_file_hashes: &[Vec<u64>]) -> FxMap<u64, Vec<WindowKey>> {
    let mut counts: FxMap<u64, u32> = FxMap::default();
    for windows in per_file_hashes {
        for h in windows {
            *counts.entry(*h).or_insert(0) += 1;
        }
    }

    let mut by_hash: FxMap<u64, Vec<WindowKey>> = FxMap::default();
    for (fi, windows) in per_file_hashes.iter().enumerate() {
        for (wi, h) in windows.iter().enumerate() {
            if counts.get(h).copied().unwrap_or(0) < 2 {
                continue;
            }
            by_hash.entry(*h).or_default().push(WindowKey {
                file: fi as u32,
                win: wi as u32,
            });
        }
    }
    by_hash
}

fn bench_bucket_intersection(c: &mut Criterion) {
    let streams = build_streams(100_000, 5);
    let singleton_hashes: usize = streams.iter().map(|v| v.len()).sum();
    assert_eq!(singleton_hashes, 100_000);

    let mut group = c.benchmark_group("bucket_intersection");
    group.sample_size(20);

    group.bench_function("bucketize_100k_5pct_overlap", |b| {
        b.iter_batched(
            || streams.clone(),
            |s| {
                let by_hash = bucketize(&s);
                std::hint::black_box(by_hash);
            },
            BatchSize::LargeInput,
        );
    });

    group.finish();
}

criterion_group!(benches, bench_bucket_intersection);
criterion_main!(benches);
