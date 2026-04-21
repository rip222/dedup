//! End-to-end cache persistence tests.
//!
//! Exercise [`Cache`] as an external consumer would: open, write a
//! synthetic [`ScanResult`], round-trip through `list_groups` /
//! `get_group`, and assert invariants that the unit tests in the cache
//! module don't cover (WAL mode on disk, process-restart round-trip,
//! auto-`.gitignore` contents).

use std::path::PathBuf;

use dedup_core::{Cache, MatchGroup, Occurrence, ScanResult, Span, Tier};
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
