//! Snapshot test for `dedup scan <fixture>`.
//!
//! Invokes the compiled `dedup` binary against `fixtures/tier_a_basic/`
//! and compares stdout to a committed `insta` snapshot. If the scanner
//! output format or the fixture drifts, the snapshot test loudly fails
//! and a human has to `cargo insta review` the change.

use std::path::PathBuf;

use assert_cmd::Command;

fn workspace_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); // dedup-cli → crates
    p.pop(); // crates    → workspace root
    p
}

#[test]
fn scan_fixture_snapshot() {
    let fixture = workspace_root().join("fixtures").join("tier_a_basic");

    let output = Command::cargo_bin("dedup")
        .expect("dedup binary")
        .arg("scan")
        .arg(&fixture)
        .output()
        .expect("dedup scan");

    assert!(output.status.success(), "dedup scan failed: {:?}", output);
    let stdout = String::from_utf8(output.stdout).expect("utf-8 stdout");
    insta::assert_snapshot!("scan_tier_a_basic", stdout);
}
