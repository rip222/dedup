//! Snapshot test for `dedup scan <fixture>`.
//!
//! Invokes the compiled `dedup` binary against a **copy** of
//! `fixtures/tier_a_basic/` (so the persisted `.dedup/cache.sqlite` goes
//! into a temp dir rather than the checked-in fixture) and compares
//! stdout to a committed `insta` snapshot. If the scanner output format
//! or the fixture drifts, the snapshot test loudly fails and a human has
//! to `cargo insta review` the change.

use assert_cmd::Command;
use tempfile::tempdir;

mod common;
use common::*;

#[test]
fn scan_fixture_snapshot() {
    let fixture = workspace_root().join("fixtures").join("tier_a_basic");
    let tmp = tempdir().unwrap();
    copy_tree(&fixture, tmp.path());

    let output = Command::cargo_bin("dedup")
        .expect("dedup binary")
        .arg("--format")
        .arg("text")
        .arg("scan")
        .arg(tmp.path())
        .output()
        .expect("dedup scan");

    assert!(output.status.success(), "dedup scan failed: {:?}", output);
    let stdout = String::from_utf8(output.stdout).expect("utf-8 stdout");
    insta::assert_snapshot!("scan_tier_a_basic", stdout);
}
