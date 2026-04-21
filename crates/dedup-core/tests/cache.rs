//! End-to-end cache persistence tests.
//!
//! Exercise [`Cache`] as an external consumer would: open, write a
//! synthetic [`ScanResult`], round-trip through `list_groups` /
//! `get_group`, and assert invariants that the unit tests in the cache
//! module don't cover (WAL mode on disk, process-restart round-trip,
//! auto-`.gitignore` contents).

use std::path::PathBuf;

use dedup_core::{
    Cache, CacheError, FileFingerprint, MatchGroup, Occurrence, ScanResult, Span, Tier,
};
use tempfile::tempdir;

fn sample_result() -> ScanResult {
    ScanResult {
        groups: vec![
            MatchGroup {
                hash: 0x1122_3344_5566_7788,
                tier: Tier::A,
                occurrences: vec![
                    Occurrence {
                        path: PathBuf::from("alpha.rs"),
                        span: Span {
                            start_line: 5,
                            end_line: 23,
                            start_byte: 80,
                            end_byte: 420,
                        },
                    },
                    Occurrence {
                        path: PathBuf::from("beta.rs"),
                        span: Span {
                            start_line: 4,
                            end_line: 22,
                            start_byte: 60,
                            end_byte: 400,
                        },
                    },
                    Occurrence {
                        path: PathBuf::from("gamma.rs"),
                        span: Span {
                            start_line: 11,
                            end_line: 29,
                            start_byte: 160,
                            end_byte: 500,
                        },
                    },
                ],
            },
            MatchGroup {
                hash: 0xdead_beef_0000_0001,
                tier: Tier::A,
                occurrences: vec![
                    Occurrence {
                        path: PathBuf::from("x.rs"),
                        span: Span {
                            start_line: 1,
                            end_line: 7,
                            start_byte: 0,
                            end_byte: 60,
                        },
                    },
                    Occurrence {
                        path: PathBuf::from("y.rs"),
                        span: Span {
                            start_line: 10,
                            end_line: 16,
                            start_byte: 100,
                            end_byte: 160,
                        },
                    },
                ],
            },
        ],
        files_scanned: 5,
        issues: Vec::new(),
    }
}

#[test]
fn roundtrips_a_full_scan_result() {
    let dir = tempdir().unwrap();
    let mut cache = Cache::open(dir.path()).unwrap();
    let result = sample_result();

    cache.write_scan_result(&result).unwrap();

    let summaries = cache.list_groups().unwrap();
    assert_eq!(summaries.len(), 2);
    // The group whose smallest path is "alpha.rs" must come before the
    // one whose smallest is "x.rs".
    let first = cache.get_group(summaries[0].id).unwrap().unwrap();
    assert_eq!(first.occurrences[0].path, PathBuf::from("alpha.rs"));
    let second = cache.get_group(summaries[1].id).unwrap().unwrap();
    assert_eq!(second.occurrences[0].path, PathBuf::from("x.rs"));

    // Occurrence counts round-trip.
    assert_eq!(first.occurrence_count, 3);
    assert_eq!(second.occurrence_count, 2);
}

#[test]
fn wal_mode_and_schema_v1_on_open() {
    let dir = tempdir().unwrap();
    let cache = Cache::open(dir.path()).unwrap();
    assert_eq!(
        cache.journal_mode().unwrap().to_ascii_lowercase(),
        "wal",
        "SQLite must be opened in WAL mode"
    );
    assert_eq!(cache.schema_version().unwrap(), 1);
}

#[test]
fn gitignore_auto_written_with_single_star() {
    let dir = tempdir().unwrap();
    let _cache = Cache::open(dir.path()).unwrap();
    let gi = dir.path().join(".dedup").join(".gitignore");
    let contents = std::fs::read_to_string(&gi).unwrap();
    assert_eq!(contents, "*\n");
}

#[test]
fn second_write_replaces_first() {
    let dir = tempdir().unwrap();
    let mut cache = Cache::open(dir.path()).unwrap();

    cache.write_scan_result(&sample_result()).unwrap();

    let smaller = ScanResult {
        groups: vec![MatchGroup {
            hash: 0xaabb_ccdd_eeff_0011,
            tier: Tier::A,
            occurrences: vec![
                Occurrence {
                    path: PathBuf::from("only.rs"),
                    span: Span {
                        start_line: 1,
                        end_line: 6,
                        start_byte: 0,
                        end_byte: 50,
                    },
                },
                Occurrence {
                    path: PathBuf::from("only_too.rs"),
                    span: Span {
                        start_line: 1,
                        end_line: 6,
                        start_byte: 0,
                        end_byte: 50,
                    },
                },
            ],
        }],
        files_scanned: 2,
        issues: Vec::new(),
    };
    cache.write_scan_result(&smaller).unwrap();

    let summaries = cache.list_groups().unwrap();
    assert_eq!(summaries.len(), 1, "second write should replace, not union");
    let detail = cache.get_group(summaries[0].id).unwrap().unwrap();
    let paths: Vec<_> = detail.occurrences.iter().map(|o| o.path.clone()).collect();
    assert_eq!(
        paths,
        vec![PathBuf::from("only.rs"), PathBuf::from("only_too.rs")]
    );
}

#[test]
fn cache_persists_across_reopen() {
    // Simulate the "cache survives process restart" acceptance criterion
    // by dropping the Cache and reopening it from the same directory.
    let dir = tempdir().unwrap();
    {
        let mut cache = Cache::open(dir.path()).unwrap();
        cache.write_scan_result(&sample_result()).unwrap();
    } // drop → connection closes, WAL checkpoints to main db.

    let reopened = Cache::open_readonly(dir.path()).unwrap().expect("present");
    let summaries = reopened.list_groups().unwrap();
    assert_eq!(summaries.len(), 2);
}

#[test]
fn readonly_open_returns_none_without_cache() {
    let dir = tempdir().unwrap();
    assert!(Cache::open_readonly(dir.path()).unwrap().is_none());
    // And .dedup/ was never created.
    assert!(!dir.path().join(".dedup").exists());
}

// --- Schema versioning + WAL concurrency (issue #18) --------------------

#[test]
fn concurrent_writers_both_succeed() {
    // The acceptance-criterion test: two writers, each holding their own
    // Connection to the same `.dedup/cache.sqlite`, both write, both
    // read, no corruption.
    //
    // We use threads-with-separate-Connections rather than two processes.
    // For SQLite's WAL + busy_timeout path, the unit of serialization is
    // a Connection, not a process: a writer from any process with its
    // own connection takes the same reserved-lock path and is subject to
    // the same `SQLITE_BUSY` + retry loop. Threads in one test binary
    // are faster to run in CI, deterministic, and easier to observe on
    // failure — while exercising the exact same SQLite code paths.
    use std::sync::Arc;
    use std::thread;

    let dir = tempdir().unwrap();
    // Materialize the schema in a single-threaded pass first so the two
    // writer threads don't race on the v1 migration's CREATE TABLE
    // statements.
    {
        let _ = Cache::open(dir.path()).unwrap();
    }
    let root = Arc::new(dir.path().to_path_buf());

    let writer = |thread_id: usize, root: Arc<PathBuf>| {
        let mut cache = Cache::open(&root).expect("open in writer thread");
        // Confirm the writer sees WAL mode.
        assert_eq!(
            cache.journal_mode().unwrap().to_ascii_lowercase(),
            "wal",
            "each writer connection must see WAL journal mode"
        );
        for i in 0..100 {
            let shared = i % 3 == 0;
            let path = if shared {
                PathBuf::from(format!("shared/f{i}.rs"))
            } else {
                PathBuf::from(format!("t{thread_id}/f{i}.rs"))
            };
            // Shared keys use the same content_hash from both threads —
            // that's the "idempotent on same hash" property. Per-thread
            // keys use per-thread hashes so we can count both writers'
            // contributions afterwards.
            let fp = FileFingerprint {
                content_hash: if shared {
                    0xabcd_ef01_0000_0000 | i as u64
                } else {
                    0xdead_0000_0000_0000 | ((thread_id as u64) << 32) | i as u64
                },
                size: i as u64,
                mtime: 1_700_000_000 + i as i64,
            };
            cache
                .put_file_entry(&path, &fp, &[i as u64, 0xffff])
                .expect("put");
        }
    };

    let t1 = {
        let r = Arc::clone(&root);
        thread::spawn(move || writer(1, r))
    };
    let t2 = {
        let r = Arc::clone(&root);
        thread::spawn(move || writer(2, r))
    };
    t1.join().unwrap();
    t2.join().unwrap();

    // Both writers ran; reopen fresh and confirm state is consistent.
    let cache = Cache::open_readonly(dir.path()).unwrap().expect("present");
    // Shared keys are the same path from both threads — each one should
    // exist exactly once (idempotent convergence).
    for i in (0..100).step_by(3) {
        let fp = cache
            .file_fingerprint(&PathBuf::from(format!("shared/f{i}.rs")))
            .unwrap()
            .expect("shared key present");
        let expected = 0xabcd_ef01_0000_0000_u64 | i as u64;
        assert_eq!(
            fp.content_hash, expected,
            "shared key must have converged content_hash"
        );
    }
    // Per-thread keys: every non-shared index from each thread must
    // be present.
    for thread_id in 1..=2 {
        for i in 0..100 {
            if i % 3 == 0 {
                continue;
            }
            assert!(
                cache
                    .file_fingerprint(&PathBuf::from(format!("t{thread_id}/f{i}.rs")))
                    .unwrap()
                    .is_some(),
                "missing per-thread key t{thread_id}/f{i}.rs"
            );
        }
    }
}

#[test]
fn newer_schema_on_open_surfaces_error_and_preserves_file() {
    // Core #18 acceptance: a cache with PRAGMA user_version > build's
    // CURRENT_SCHEMA_VERSION must surface NewerSchema and leave the file
    // byte-for-byte unchanged.
    use rusqlite::Connection;

    let dir = tempdir().unwrap();
    {
        // Bootstrap a normal cache at v1.
        let _ = Cache::open(dir.path()).unwrap();
    }
    let db_path = dir.path().join(".dedup").join("cache.sqlite");

    // Bump the version out-of-band to simulate a future build.
    {
        let conn = Connection::open(&db_path).unwrap();
        conn.pragma_update(None, "user_version", 42_u32).unwrap();
    }

    let bytes_before = std::fs::read(&db_path).unwrap();

    match Cache::open(dir.path()) {
        Err(CacheError::NewerSchema { found, supported }) => {
            assert_eq!(found, 42);
            assert!(supported < 42);
        }
        Err(other) => panic!("expected NewerSchema, got {other:?}"),
        Ok(_) => panic!("expected NewerSchema error, got success"),
    }

    let bytes_after = std::fs::read(&db_path).unwrap();
    assert_eq!(
        bytes_before, bytes_after,
        "refused open must preserve cache file bytes"
    );
}

#[test]
fn idempotent_content_hash_keyed_writes() {
    // Writing the same (path, content_hash) multiple times must be a
    // no-op past the first call: row count stays at 1.
    let dir = tempdir().unwrap();
    let mut cache = Cache::open(dir.path()).unwrap();

    let path = PathBuf::from("src/idempotent.rs");
    let fp = FileFingerprint {
        content_hash: 0x1234_5678_9abc_def0,
        size: 256,
        mtime: 1_700_100_200,
    };
    for _ in 0..10 {
        cache.put_file_entry(&path, &fp, &[1, 2, 3]).unwrap();
    }

    // Confirm exactly one row round-trips.
    let fp_loaded = cache.file_fingerprint(&path).unwrap().expect("present");
    assert_eq!(fp_loaded, fp);
    let blocks = cache
        .file_blocks(&path, fp.content_hash)
        .unwrap()
        .expect("blocks");
    assert_eq!(blocks.block_hashes, vec![1, 2, 3]);
}
