//! End-to-end integration tests for `dedup dismiss`, `dedup suppressions
//! {list,clear}`, and `dedup clean`.
//!
//! These drive the compiled binary via [`assert_cmd`] so the argv →
//! cache → stdout pipeline is exercised in the same shape a user would
//! hit it from a shell. We only cover the non-interactive paths — the
//! interactive confirmation for `dedup clean` is covered by the non-TTY
//! refusal test; actually typing "y" into stdin isn't worth the
//! complexity and would be brittle.

use std::path::{Path, PathBuf};

use assert_cmd::Command;
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

/// Collect every cached group id by scraping `dedup list` output. The
/// tier_a_basic fixture yields multiple groups (one Tier A, one Tier B
/// from `compute_totals`), so tests that want to clear the entire
/// report-visible set dismiss all of them in one go.
fn every_group_id(tmp_path: &Path) -> Vec<i64> {
    // list_groups returns rows in tier A / B order with stable id
    // ordering. The persisted ids start at 1 and are contiguous. Rather
    // than parse the text format, we just probe ids 1..=N by calling
    // `show` until one fails.
    let mut ids = Vec::new();
    for id in 1_i64..100 {
        let out = Command::cargo_bin("dedup")
            .unwrap()
            .arg("show")
            .arg(id.to_string())
            .arg(tmp_path)
            .output()
            .unwrap();
        if out.status.success() {
            ids.push(id);
        } else {
            break;
        }
    }
    ids
}

fn dismiss_all(tmp_path: &Path) {
    for id in every_group_id(tmp_path) {
        let out = Command::cargo_bin("dedup")
            .unwrap()
            .arg("dismiss")
            .arg(id.to_string())
            .arg(tmp_path)
            .output()
            .unwrap();
        assert!(out.status.success(), "dismiss {id} failed: {:?}", out);
    }
}

#[test]
fn dismiss_hides_group_from_subsequent_list() {
    let tmp = prepare_scanned_fixture();

    // Baseline: list emits at least one group.
    let before = Command::cargo_bin("dedup")
        .unwrap()
        .arg("--format")
        .arg("text")
        .arg("list")
        .arg(tmp.path())
        .output()
        .unwrap();
    assert!(before.status.success());
    let before_stdout = String::from_utf8(before.stdout).unwrap();
    assert!(before_stdout.contains("group"), "expected a group header");

    // Dismiss every cached group (the fixture yields both a Tier A and a
    // Tier B group; we want `list` to go empty).
    dismiss_all(tmp.path());

    // After: list hides all dismissed groups.
    let after = Command::cargo_bin("dedup")
        .unwrap()
        .arg("--format")
        .arg("text")
        .arg("list")
        .arg(tmp.path())
        .output()
        .unwrap();
    assert!(after.status.success());
    let after_stdout = String::from_utf8(after.stdout).unwrap();
    assert!(
        after_stdout.trim().is_empty(),
        "expected empty list output after dismissal, got: {after_stdout:?}"
    );
}

#[test]
fn dismiss_persists_across_rescan() {
    let tmp = prepare_scanned_fixture();
    dismiss_all(tmp.path());

    // Re-scan: the suppressions should survive.
    let rescan = Command::cargo_bin("dedup")
        .unwrap()
        .arg("--format")
        .arg("text")
        .arg("scan")
        .arg(tmp.path())
        .output()
        .unwrap();
    assert!(rescan.status.success());
    let stdout = String::from_utf8(rescan.stdout).unwrap();
    assert!(
        !stdout.contains("--- ["),
        "re-scan output should not print any dismissed groups, got: {stdout:?}"
    );
}

#[test]
fn suppressions_list_emits_dismissed_entry() {
    let tmp = prepare_scanned_fixture();
    let _ = Command::cargo_bin("dedup")
        .unwrap()
        .arg("dismiss")
        .arg("1")
        .arg(tmp.path())
        .output()
        .unwrap();

    let list = Command::cargo_bin("dedup")
        .unwrap()
        .arg("--format")
        .arg("text")
        .arg("suppressions")
        .arg("list")
        .arg(tmp.path())
        .output()
        .unwrap();
    assert!(list.status.success());
    let stdout = String::from_utf8(list.stdout).unwrap();
    assert!(
        stdout.contains("dismissed_at="),
        "expected at least one row, got: {stdout:?}"
    );
    assert!(stdout.contains("last_group_id=1"));
}

#[test]
fn suppressions_clear_empties_the_table() {
    let tmp = prepare_scanned_fixture();
    let _ = Command::cargo_bin("dedup")
        .unwrap()
        .arg("dismiss")
        .arg("1")
        .arg(tmp.path())
        .output()
        .unwrap();

    let clear = Command::cargo_bin("dedup")
        .unwrap()
        .arg("suppressions")
        .arg("clear")
        .arg(tmp.path())
        .output()
        .unwrap();
    assert!(clear.status.success());
    let stdout = String::from_utf8(clear.stdout).unwrap();
    assert!(
        stdout.starts_with("cleared "),
        "unexpected clear output: {stdout:?}"
    );

    // After clear, list is empty and the group is visible again.
    let list = Command::cargo_bin("dedup")
        .unwrap()
        .arg("--format")
        .arg("text")
        .arg("suppressions")
        .arg("list")
        .arg(tmp.path())
        .output()
        .unwrap();
    assert!(list.status.success());
    let list_stdout = String::from_utf8(list.stdout).unwrap();
    assert!(list_stdout.contains("(no suppressions)"));

    let relist = Command::cargo_bin("dedup")
        .unwrap()
        .arg("--format")
        .arg("text")
        .arg("list")
        .arg(tmp.path())
        .output()
        .unwrap();
    assert!(relist.status.success());
    let relist_stdout = String::from_utf8(relist.stdout).unwrap();
    assert!(
        relist_stdout.contains("--- ["),
        "group should re-surface after clear, got: {relist_stdout:?}"
    );
}

#[test]
fn dismiss_unknown_id_exits_code_two() {
    let tmp = prepare_scanned_fixture();
    let out = Command::cargo_bin("dedup")
        .unwrap()
        .arg("dismiss")
        .arg("9999")
        .arg(tmp.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("no group with id 9999"));
}

#[test]
fn dismiss_without_cache_exits_code_two() {
    let tmp = tempdir().unwrap();
    let out = Command::cargo_bin("dedup")
        .unwrap()
        .arg("dismiss")
        .arg("1")
        .arg(tmp.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("No cached scan found"));
    // No scan happened, no .dedup/ was created.
    assert!(!tmp.path().join(".dedup").exists());
}

#[test]
fn clean_with_yes_removes_dedup_directory() {
    let tmp = prepare_scanned_fixture();
    let dedup = tmp.path().join(".dedup");
    assert!(dedup.is_dir());

    let out = Command::cargo_bin("dedup")
        .unwrap()
        .arg("clean")
        .arg(tmp.path())
        .arg("--yes")
        .output()
        .unwrap();
    assert!(out.status.success(), "clean --yes failed: {:?}", out);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("removed"));
    assert!(!dedup.exists(), ".dedup/ must be gone after `clean --yes`");
}

#[test]
fn clean_without_dedup_dir_is_noop() {
    let tmp = tempdir().unwrap();
    let out = Command::cargo_bin("dedup")
        .unwrap()
        .arg("clean")
        .arg(tmp.path())
        .arg("--yes")
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("nothing to clean"));
}

#[test]
fn clean_refuses_non_tty_without_yes() {
    // assert_cmd doesn't attach a TTY to the child's stdin — it pipes it.
    // So this invocation MUST hit the "stdin is not a TTY" branch and
    // exit 2 rather than hang on stdin.
    let tmp = prepare_scanned_fixture();
    let out = Command::cargo_bin("dedup")
        .unwrap()
        .arg("clean")
        .arg(tmp.path())
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(2),
        "clean without --yes on non-TTY must exit 2, not hang"
    );
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("--yes"));
    // And .dedup/ must still be there.
    assert!(tmp.path().join(".dedup").exists());
}
