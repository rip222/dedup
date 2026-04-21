//! Integration tests for the rayon-parallel scanner with warm cache
//! (issue #14).
//!
//! Covers:
//! - Determinism: a warm scan on an unchanged fixture returns
//!   byte-identical match groups to the cold scan.
//! - `jobs=1` fallback: setting `ScanConfig::jobs = Some(1)` still
//!   produces the same groups (no reliance on the pool).
//! - Cache hit path: after a cold scan, the `file_hashes` /
//!   `file_blocks` tables have rows for every tokenized file.

use std::path::PathBuf;

use dedup_core::{Cache, MatchGroup, ScanConfig, Scanner};
use tempfile::tempdir;

type Occurrence = (String, usize, usize);
type GroupSig = (String, Vec<Occurrence>);

fn workspace_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p
}

fn group_signature(groups: &[MatchGroup]) -> Vec<GroupSig> {
    let mut sig: Vec<GroupSig> = groups
        .iter()
        .map(|g| {
            let mut occs: Vec<Occurrence> = g
                .occurrences
                .iter()
                .map(|o| {
                    (
                        o.path.to_string_lossy().into_owned(),
                        o.span.start_line,
                        o.span.end_line,
                    )
                })
                .collect();
            occs.sort();
            (format!("{:016x}:{}", g.hash, g.tier.label()), occs)
        })
        .collect();
    sig.sort();
    sig
}

fn copy_dir_all(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

fn cfg_with_cache(cache_root: &std::path::Path) -> ScanConfig {
    ScanConfig {
        cache_root: Some(cache_root.to_path_buf()),
        ..ScanConfig::default()
    }
}

#[test]
fn warm_scan_matches_cold_scan() {
    // Copy the fixture into a tempdir so the cache side effects are
    // isolated from other tests.
    let fixture = workspace_root().join("fixtures").join("tier_a_basic");
    let tmp = tempdir().unwrap();
    copy_dir_all(&fixture, tmp.path()).unwrap();

    let cfg = cfg_with_cache(tmp.path());

    // Cold scan: empty cache, everything goes through rolling_hash.
    let cold = Scanner::new(cfg.clone()).scan(tmp.path()).unwrap();
    let cold_sig = group_signature(&cold.groups);

    // Warm scan: fingerprint + blocks are persisted; rolling_hash is
    // skipped for every unchanged file. Groups must be byte-identical.
    let warm = Scanner::new(cfg).scan(tmp.path()).unwrap();
    let warm_sig = group_signature(&warm.groups);

    assert_eq!(cold.files_scanned, warm.files_scanned);
    assert_eq!(
        cold_sig, warm_sig,
        "warm scan produced different groups than cold scan"
    );
}

#[test]
fn warm_cache_populates_file_tables() {
    // After one scan, every tokenized file should have a file_hashes
    // and file_blocks row. Tested through the public Cache API.
    let fixture = workspace_root().join("fixtures").join("tier_a_basic");
    let tmp = tempdir().unwrap();
    copy_dir_all(&fixture, tmp.path()).unwrap();

    let cfg = cfg_with_cache(tmp.path());
    let result = Scanner::new(cfg).scan(tmp.path()).unwrap();

    let cache = Cache::open_readonly(tmp.path()).unwrap().expect("cache");
    let mut hits = 0usize;
    for name in ["alpha.rs", "beta.rs", "gamma.rs"] {
        let p = PathBuf::from(name);
        let fp = cache.file_fingerprint(&p).unwrap().expect("fingerprint");
        let blocks = cache
            .file_blocks(&p, fp.content_hash)
            .unwrap()
            .expect("blocks");
        assert!(
            !blocks.block_hashes.is_empty(),
            "empty block list for {name}"
        );
        hits += 1;
    }
    assert_eq!(hits, 3);
    assert_eq!(result.files_scanned, 3);
}

#[test]
fn jobs_one_produces_same_groups_as_default() {
    let fixture = workspace_root().join("fixtures").join("tier_a_basic");

    // Default parallelism.
    let default_sig = group_signature(
        &Scanner::new(ScanConfig::default())
            .scan(&fixture)
            .unwrap()
            .groups,
    );

    // Forced single-threaded.
    let serial_cfg = ScanConfig {
        jobs: Some(1),
        ..ScanConfig::default()
    };
    let serial_sig = group_signature(&Scanner::new(serial_cfg).scan(&fixture).unwrap().groups);

    assert_eq!(default_sig, serial_sig);
}

#[test]
fn editing_a_file_invalidates_its_cached_blocks() {
    // Warm-cache correctness: after editing one file, its cached block
    // list must NOT be used (stale content hash) — the scan result
    // must reflect the new content.
    let tmp = tempdir().unwrap();
    std::fs::write(tmp.path().join("a.rs"), "fn a() { 1 + 2 }\n").unwrap();
    std::fs::write(tmp.path().join("b.rs"), "fn b() { 3 + 4 }\n").unwrap();

    let cfg = cfg_with_cache(tmp.path());

    // Cold scan.
    let _ = Scanner::new(cfg.clone()).scan(tmp.path()).unwrap();

    // Overwrite a.rs with different content.
    std::fs::write(
        tmp.path().join("a.rs"),
        "fn a() { let x = 42; let y = 43; let z = 44; }\n",
    )
    .unwrap();

    let before = Cache::open_readonly(tmp.path())
        .unwrap()
        .unwrap()
        .file_fingerprint(&PathBuf::from("a.rs"))
        .unwrap()
        .expect("present")
        .content_hash;

    let _ = Scanner::new(cfg).scan(tmp.path()).unwrap();

    let after = Cache::open_readonly(tmp.path())
        .unwrap()
        .unwrap()
        .file_fingerprint(&PathBuf::from("a.rs"))
        .unwrap()
        .expect("present")
        .content_hash;

    assert_ne!(
        before, after,
        "content_hash should change after the file is edited"
    );
}
