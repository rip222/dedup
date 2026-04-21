//! Snapshot test for `dedup scan <fixture>`.
//!
//! Invokes the compiled `dedup` binary against a **copy** of
//! `fixtures/tier_a_basic/` (so the persisted `.dedup/cache.sqlite` goes
//! into a temp dir rather than the checked-in fixture) and compares
//! stdout to a committed `insta` snapshot. If the scanner output format
//! or the fixture drifts, the snapshot test loudly fails and a human has
//! to `cargo insta review` the change.

use std::path::{Path, PathBuf};

use assert_cmd::Command;
use tempfile::tempdir;

fn workspace_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); // dedup-cli → crates
    p.pop(); // crates    → workspace root
    p
}

/// Recursively copy `src` into `dst`, creating `dst` if needed. Only
/// files and directories (no symlinks, no special modes).
fn copy_tree(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let file_type = entry.file_type().unwrap();
        let target = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_tree(&entry.path(), &target);
        } else if file_type.is_file() {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

#[test]
fn scan_fixture_snapshot() {
    let fixture = workspace_root().join("fixtures").join("tier_a_basic");
    let tmp = tempdir().unwrap();
    copy_tree(&fixture, tmp.path());

    let output = Command::cargo_bin("dedup")
        .expect("dedup binary")
        .arg("scan")
        .arg(tmp.path())
        .output()
        .expect("dedup scan");

    assert!(output.status.success(), "dedup scan failed: {:?}", output);
    let stdout = String::from_utf8(output.stdout).expect("utf-8 stdout");
    insta::assert_snapshot!("scan_tier_a_basic", stdout);
}
