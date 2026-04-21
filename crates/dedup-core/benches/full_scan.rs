//! Bench (4): full fixture scan via `Scanner::scan`.
//!
//! Cold-scan measurement: each iteration uses a fresh temporary
//! `.dedup/` (no cache path is wired in for this bench — the default
//! `cache_root: None` already bypasses the warm-scan path). The fixture
//! root is resolved at compile time via `CARGO_MANIFEST_DIR` so the
//! bench works regardless of the workspace layout.

use std::path::PathBuf;

use criterion::{Criterion, criterion_group, criterion_main};
use dedup_core::{ScanConfig, Scanner};

fn fixtures_root() -> PathBuf {
    // CARGO_MANIFEST_DIR points at crates/dedup-core; the fixtures
    // directory lives at the workspace root.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("fixtures")
}

fn bench_full_scan(c: &mut Criterion) {
    let root = fixtures_root();
    assert!(
        root.is_dir(),
        "fixtures/ must exist at the workspace root: {}",
        root.display()
    );

    let scanner = Scanner::new(ScanConfig {
        // Single-threaded keeps wall-clock variance low enough for
        // criterion's differencing to be useful. The parallel path is
        // still exercised by the integration tests.
        jobs: Some(1),
        ..ScanConfig::default()
    });

    let mut group = c.benchmark_group("full_scan");
    group.sample_size(10);

    group.bench_function("fixtures_all_languages", |b| {
        b.iter(|| {
            let result = scanner.scan(&root).expect("scan fixtures");
            std::hint::black_box(result);
        });
    });

    group.finish();
}

criterion_group!(benches, bench_full_scan);
criterion_main!(benches);
