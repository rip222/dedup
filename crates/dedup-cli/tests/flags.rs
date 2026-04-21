//! Integration tests for the global-flag surface added in issue #13.
//!
//! Each test shells out to the compiled `dedup` binary via `assert_cmd`
//! so that argv parsing, exit codes, and stderr/stdout routing are all
//! end-to-end.
//!
//! A few tests rely on `fixtures/tier_a_basic/` producing at least one
//! duplicate group (the committed snapshot confirms it does). If the
//! fixture is changed and no longer yields duplicates, these tests will
//! surface it.

use std::path::{Path, PathBuf};

use assert_cmd::Command;
use tempfile::tempdir;

fn workspace_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); // crates/dedup-cli -> crates
    p.pop(); // crates           -> workspace root
    p
}

/// Recursively copy `src` to `dst`.
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

/// Prepare a temp dir populated with `tier_a_basic`, so the cache can be
/// written into it without touching the checked-in fixture.
fn prepare_fixture() -> tempfile::TempDir {
    let tmp = tempdir().unwrap();
    let fixture = workspace_root().join("fixtures").join("tier_a_basic");
    copy_tree(&fixture, tmp.path());
    tmp
}

#[test]
fn strict_exits_one_when_findings_present() {
    let tmp = prepare_fixture();
    let out = Command::cargo_bin("dedup")
        .unwrap()
        .arg("scan")
        .arg("--strict")
        .arg(tmp.path())
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(1),
        "expected exit 1 on findings + --strict; got {:?} / stderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    // stdout should still contain the groups — `--strict` only flips
    // the exit code, not the output.
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.starts_with("--- group "),
        "expected groups on stdout, got: {stdout:?}"
    );
}

#[test]
fn strict_exits_zero_when_clean() {
    // Empty dir → zero groups → exit 0 even with --strict.
    let tmp = tempdir().unwrap();
    let out = Command::cargo_bin("dedup")
        .unwrap()
        .arg("scan")
        .arg("--strict")
        .arg(tmp.path())
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "expected exit 0 when no findings even with --strict; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn quiet_suppresses_progress_on_stderr() {
    let tmp = prepare_fixture();
    let out = Command::cargo_bin("dedup")
        .unwrap()
        .arg("scan")
        .arg("--quiet")
        .arg(tmp.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "scan failed: {:?}", out);

    // stderr may contain cache warnings or friendly messages, but must
    // NOT contain a spinner's ANSI escape or the spinner's message text.
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        !stderr.contains("scanning"),
        "quiet should suppress spinner text, got stderr: {stderr:?}"
    );
    assert!(
        !stderr.contains("\x1b["),
        "quiet should not emit ANSI on stderr, got: {stderr:?}"
    );
}

#[test]
fn color_never_emits_no_ansi_on_stdout() {
    let tmp = prepare_fixture();
    let out = Command::cargo_bin("dedup")
        .unwrap()
        .arg("scan")
        .arg("--color")
        .arg("never")
        .arg(tmp.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        !stdout.contains("\x1b["),
        "stdout should be ANSI-free with --color never, got: {stdout:?}"
    );
}

#[test]
fn tier_a_is_accepted_and_emits_groups() {
    let tmp = prepare_fixture();
    let out = Command::cargo_bin("dedup")
        .unwrap()
        .arg("scan")
        .arg("--tier")
        .arg("a")
        .arg(tmp.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "tier a scan failed: {:?}", out);
    let stdout = String::from_utf8(out.stdout).unwrap();
    // Tier A is the only tier that emits at MVP — we expect groups.
    assert!(
        stdout.contains("--- group "),
        "tier a should emit groups, got: {stdout:?}"
    );
}

#[test]
fn tier_b_filters_out_tier_a_groups_at_mvp() {
    // Tier B isn't emitted yet (lands in #6), so `--tier b` should
    // filter the Tier A groups out and leave stdout empty. This is the
    // documented MVP behavior — when #6 lands, Tier B groups appear and
    // this test needs an update.
    let tmp = prepare_fixture();
    let out = Command::cargo_bin("dedup")
        .unwrap()
        .arg("scan")
        .arg("--tier")
        .arg("b")
        .arg(tmp.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.is_empty(),
        "tier b should yield no groups pre-#6, got: {stdout:?}"
    );
}

#[test]
fn jobs_flag_is_accepted() {
    // `--jobs` is a stub pending #14; at this issue we just verify it
    // parses and doesn't break the scan.
    let tmp = prepare_fixture();
    let out = Command::cargo_bin("dedup")
        .unwrap()
        .arg("scan")
        .arg("--jobs")
        .arg("2")
        .arg(tmp.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "jobs=2 scan failed: {:?}", out);
}

#[test]
fn lang_flag_is_accepted() {
    // Like `--jobs`, this is a stub pending #6 Tier B — parse-only.
    let tmp = prepare_fixture();
    let out = Command::cargo_bin("dedup")
        .unwrap()
        .arg("scan")
        .arg("--lang")
        .arg("rust,ts")
        .arg(tmp.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "--lang scan failed: {:?}", out);
}

#[test]
fn no_gitignore_flag_is_accepted() {
    let tmp = prepare_fixture();
    let out = Command::cargo_bin("dedup")
        .unwrap()
        .arg("scan")
        .arg("--no-gitignore")
        .arg(tmp.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "--no-gitignore scan failed: {:?}",
        out
    );
}

#[test]
fn unknown_flag_exits_two() {
    let out = Command::cargo_bin("dedup")
        .unwrap()
        .arg("scan")
        .arg("--definitely-not-a-real-flag")
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(2),
        "unknown flag should exit 2 per PRD, got {:?} / stderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn help_succeeds() {
    let out = Command::cargo_bin("dedup")
        .unwrap()
        .arg("--help")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "--help must exit 0, got {:?}",
        out.status
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    // Spot-check that the global flags are documented in help output —
    // if clap stops showing them, the help surface has regressed.
    for needle in ["--strict", "--tier", "--lang", "--jobs", "--color"] {
        assert!(
            stdout.contains(needle),
            "--help missing documentation for {needle}: {stdout}"
        );
    }
}

#[test]
fn verbose_flag_is_accepted() {
    // -v sets RUST_LOG for the child process; we can only verify the
    // flag is accepted. Subscriber init lands in #16.
    let tmp = prepare_fixture();
    let out = Command::cargo_bin("dedup")
        .unwrap()
        .arg("-v")
        .arg("scan")
        .arg(tmp.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "-v scan failed: {:?}", out);
}
