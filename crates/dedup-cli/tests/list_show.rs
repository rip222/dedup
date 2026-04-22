//! End-to-end integration tests for `dedup list` and `dedup show`.
//!
//! These tests drive the compiled binary via [`assert_cmd`] so they
//! exercise the full argv → exit-code pipeline, including cache
//! persistence across separate process invocations (the warm-start
//! acceptance criterion from issue #4).

use assert_cmd::Command;
use tempfile::tempdir;

mod common;
use common::*;

/// Run `dedup scan` on a fresh copy of the tier_a_basic fixture and
/// return the temp dir so the caller can run follow-up commands.
fn prepare_scanned_fixture() -> tempfile::TempDir {
    let tmp = tempdir().unwrap();
    let fixture = workspace_root().join("fixtures").join("tier_a_basic");
    copy_tree(&fixture, tmp.path());

    let output = Command::cargo_bin("dedup")
        .unwrap()
        .arg("scan")
        .arg(tmp.path())
        .output()
        .unwrap();
    assert!(output.status.success(), "scan failed: {:?}", output);
    tmp
}

#[test]
fn scan_creates_dedup_directory_with_gitignore() {
    let tmp = prepare_scanned_fixture();
    let dedup_dir = tmp.path().join(".dedup");
    assert!(dedup_dir.is_dir(), ".dedup/ should exist after scan");

    let cache_file = dedup_dir.join("cache.sqlite");
    assert!(cache_file.exists(), "cache.sqlite should exist after scan");

    let gi = std::fs::read_to_string(dedup_dir.join(".gitignore")).unwrap();
    assert_eq!(gi, "*\n", ".gitignore should contain a single `*` line");
}

#[test]
fn list_reproduces_scan_output_without_rescanning() {
    let tmp = prepare_scanned_fixture();

    // Capture the scan's snapshot stdout for baseline comparison.
    // Force `--format text` so auto-select (non-TTY → JSON per #12)
    // doesn't change the semantics this test is guarding.
    let scan_again = Command::cargo_bin("dedup")
        .unwrap()
        .arg("--format")
        .arg("text")
        .arg("scan")
        .arg(tmp.path())
        .output()
        .unwrap();
    let scan_stdout = String::from_utf8(scan_again.stdout).unwrap();

    // Now delete the fixture's text files so a real re-scan would
    // produce an empty result. `list` must still emit the cached state.
    for entry in std::fs::read_dir(tmp.path()).unwrap() {
        let entry = entry.unwrap();
        if entry.file_name() == ".dedup" {
            continue;
        }
        let path = entry.path();
        if path.is_file() {
            std::fs::remove_file(&path).unwrap();
        }
    }

    let list_out = Command::cargo_bin("dedup")
        .unwrap()
        .arg("--format")
        .arg("text")
        .arg("list")
        .arg(tmp.path())
        .output()
        .unwrap();
    assert!(list_out.status.success(), "list failed: {:?}", list_out);
    let list_stdout = String::from_utf8(list_out.stdout).unwrap();

    assert_eq!(
        scan_stdout, list_stdout,
        "list output should match scan output byte-for-byte"
    );

    insta::assert_snapshot!("list_tier_a_basic", list_stdout);
}

#[test]
fn show_emits_group_detail() {
    let tmp = prepare_scanned_fixture();

    // Probe for a valid id via `list`. The fixture always yields exactly
    // one group; we read the list output to discover what id it uses.
    let list_out = Command::cargo_bin("dedup")
        .unwrap()
        .arg("--format")
        .arg("text")
        .arg("list")
        .arg(tmp.path())
        .output()
        .unwrap();
    assert!(list_out.status.success());

    // The id stored in SQLite starts at 1 after a fresh scan, so we can
    // just ask for id 1. A failing assertion here would indicate the
    // write path changed, which would be worth surfacing.
    let show_out = Command::cargo_bin("dedup")
        .unwrap()
        .arg("--format")
        .arg("text")
        .arg("show")
        .arg("1")
        .arg(tmp.path())
        .output()
        .unwrap();
    assert!(show_out.status.success(), "show 1 failed: {:?}", show_out);
    let show_stdout = String::from_utf8(show_out.stdout).unwrap();
    insta::assert_snapshot!("show_tier_a_basic_group1", show_stdout);
}

#[test]
fn list_without_cache_exits_with_code_two() {
    let tmp = tempdir().unwrap();
    // No scan, no .dedup/.
    let out = Command::cargo_bin("dedup")
        .unwrap()
        .arg("list")
        .arg(tmp.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        stderr.contains("No cached scan found"),
        "expected friendly stderr, got: {stderr:?}"
    );
    // And we must NOT have created .dedup/.
    assert!(!tmp.path().join(".dedup").exists());
}

#[test]
fn show_without_cache_exits_with_code_two() {
    let tmp = tempdir().unwrap();
    let out = Command::cargo_bin("dedup")
        .unwrap()
        .arg("show")
        .arg("1")
        .arg(tmp.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("No cached scan found"));
}

#[test]
fn show_unknown_id_exits_with_code_two() {
    let tmp = prepare_scanned_fixture();
    let out = Command::cargo_bin("dedup")
        .unwrap()
        .arg("show")
        .arg("9999")
        .arg(tmp.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        stderr.contains("no group with id 9999"),
        "unexpected stderr: {stderr:?}"
    );
}

#[test]
fn cache_survives_process_restart() {
    // First process: scan. Second process: list. If the cache didn't
    // persist, list would exit 2 or emit nothing. This is the warm-start
    // acceptance criterion from issue #4.
    let tmp = prepare_scanned_fixture();

    // Second, independent process:
    let list_out = Command::cargo_bin("dedup")
        .unwrap()
        .arg("--format")
        .arg("text")
        .arg("list")
        .arg(tmp.path())
        .output()
        .unwrap();
    assert!(list_out.status.success());
    let stdout = String::from_utf8(list_out.stdout).unwrap();
    assert!(
        !stdout.trim().is_empty(),
        "cache should survive process restart and yield groups"
    );
    assert!(
        stdout.starts_with("--- ["),
        "output shape changed: {stdout:?}"
    );
}
