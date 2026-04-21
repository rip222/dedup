//! Integration test for issue #17: the CLI must print a per-file-issue
//! summary to stderr after `dedup scan` when at least one file was
//! degraded (read error, UTF-8 decode failure, Tier B parse / panic).
//!
//! We assert two contract shapes:
//!
//! - A clean scan emits no summary line on stderr (no spurious noise).
//! - A scan over a directory with invalid-UTF-8 bytes emits a summary
//!   line counting exactly one `utf8` issue.
//!
//! We deliberately pick the UTF-8 case over the permission-denied case
//! because it's portable (no `chmod`) and deterministic across CI
//! platforms. The permission-denied and Tier B panic paths are covered
//! by the unit tests in `dedup-core::scanner::tests`.

use std::path::Path;

use assert_cmd::Command;
use tempfile::tempdir;

fn write_fixture(dir: &Path) {
    // Two plain UTF-8 files so Tier A has something to hash, plus one
    // file with invalid UTF-8 bytes that passes the binary sniff (no
    // NUL bytes) and therefore reaches the decode step.
    std::fs::write(dir.join("a.txt"), "hello world, friend\n").unwrap();
    std::fs::write(dir.join("b.txt"), "hello world, friend\n").unwrap();
    std::fs::write(dir.join("bad.txt"), [0xC3, 0x28, 0xC3, 0x28]).unwrap();
}

#[test]
fn utf8_failure_surfaces_in_cli_summary() {
    let tmp = tempdir().unwrap();
    write_fixture(tmp.path());

    let output = Command::cargo_bin("dedup")
        .expect("dedup binary")
        .arg("--format")
        .arg("text")
        .arg("scan")
        .arg(tmp.path())
        .output()
        .expect("dedup scan");

    assert!(output.status.success(), "dedup scan failed: {output:?}");
    let stderr = String::from_utf8(output.stderr).expect("utf-8 stderr");
    assert!(
        stderr.contains("1 issues"),
        "expected a summary line reporting 1 issue; got stderr: {stderr}"
    );
    assert!(
        stderr.contains("1 utf8"),
        "expected `1 utf8` in summary line; got stderr: {stderr}"
    );
}

#[test]
fn clean_scan_emits_no_issue_summary() {
    // Just the two good files — no bad inputs, so nothing to summarize.
    let tmp = tempdir().unwrap();
    std::fs::write(tmp.path().join("a.txt"), "hello world").unwrap();
    std::fs::write(tmp.path().join("b.txt"), "hello world").unwrap();

    let output = Command::cargo_bin("dedup")
        .expect("dedup binary")
        .arg("--format")
        .arg("text")
        .arg("scan")
        .arg(tmp.path())
        .output()
        .expect("dedup scan");

    assert!(output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("utf-8 stderr");
    assert!(
        !stderr.contains("issues ("),
        "clean scan must not emit the issue-summary line; got stderr: {stderr}"
    );
}
