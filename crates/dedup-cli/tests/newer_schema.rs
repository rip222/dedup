//! CLI integration tests for issue #18's newer-schema prompt.
//!
//! Acceptance criterion: running any cache-opening subcommand against a
//! `.dedup/cache.sqlite` whose `PRAGMA user_version` is newer than the
//! running build's `CURRENT_SCHEMA_VERSION` must (a) refuse without
//! mutating the file and (b) print a user-facing "Cache created by
//! newer Dedup version. Rescan?" prompt to stderr.

use std::path::{Path, PathBuf};

use assert_cmd::Command;
use rusqlite::Connection;
use tempfile::tempdir;

fn workspace_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p
}

fn copy_tree(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let ty = entry.file_type().unwrap();
        let target = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_tree(&entry.path(), &target);
        } else if ty.is_file() {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

/// Run `dedup scan` once against a copied fixture so the cache file
/// exists at a known-good schema version, then bump `PRAGMA user_version`
/// out-of-band to simulate a future build.
fn prepare_future_schema_cache() -> tempfile::TempDir {
    let tmp = tempdir().unwrap();
    let fixture = workspace_root().join("fixtures").join("tier_a_basic");
    copy_tree(&fixture, tmp.path());

    // Seed the cache at the current schema version via a real scan.
    let seed = Command::cargo_bin("dedup")
        .unwrap()
        .arg("scan")
        .arg(tmp.path())
        .output()
        .unwrap();
    assert!(seed.status.success(), "seed scan failed: {seed:?}");

    // Bump the on-disk user_version past anything this build supports.
    let db_path = tmp.path().join(".dedup").join("cache.sqlite");
    let conn = Connection::open(&db_path).unwrap();
    conn.pragma_update(None, "user_version", 999_u32).unwrap();
    drop(conn);

    tmp
}

#[test]
fn scan_refuses_newer_schema_cache_and_preserves_file() {
    let tmp = prepare_future_schema_cache();
    let db_path = tmp.path().join(".dedup").join("cache.sqlite");
    let bytes_before = std::fs::read(&db_path).unwrap();

    let output = Command::cargo_bin("dedup")
        .unwrap()
        .arg("scan")
        .arg(tmp.path())
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "scan should fail with non-zero exit when cache is newer"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Cache created by newer Dedup version") && stderr.contains("Rescan?"),
        "stderr should contain the newer-schema prompt, got: {stderr}"
    );

    let bytes_after = std::fs::read(&db_path).unwrap();
    assert_eq!(
        bytes_before, bytes_after,
        "cache file must be preserved byte-for-byte after refused scan"
    );
}

#[test]
fn list_refuses_newer_schema_cache() {
    let tmp = prepare_future_schema_cache();
    let output = Command::cargo_bin("dedup")
        .unwrap()
        .arg("list")
        .arg(tmp.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Cache created by newer Dedup version"),
        "list stderr: {stderr}"
    );
}

#[test]
fn show_refuses_newer_schema_cache() {
    let tmp = prepare_future_schema_cache();
    let output = Command::cargo_bin("dedup")
        .unwrap()
        .arg("show")
        .arg("1")
        .arg(tmp.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Cache created by newer Dedup version"),
        "show stderr: {stderr}"
    );
}
