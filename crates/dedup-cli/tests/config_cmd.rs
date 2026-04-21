//! Integration tests for `dedup config` + config-aware `dedup scan`.
//!
//! Each test runs the compiled binary under a sandboxed HOME (set via
//! env on the child process) so we never read or write the developer's
//! real `~/.config/dedup/`. Repo-scoped project configs live in a
//! `tempdir` and go away at test exit.

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

/// Build an `assert_cmd::Command` wired to a sandboxed HOME and empty
/// XDG_CONFIG_HOME so `Config::global_path()` resolves to
/// `<home>/.config/dedup/config.toml`.
fn dedup_with_home(home: &Path) -> Command {
    let mut cmd = Command::cargo_bin("dedup").unwrap();
    cmd.env("HOME", home).env_remove("XDG_CONFIG_HOME");
    cmd
}

#[test]
fn config_path_prints_both_layers_with_presence() {
    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();

    let out = dedup_with_home(home.path())
        .arg("config")
        .arg("path")
        .arg(repo.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "{:?}", out);

    let stdout = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines.len(),
        2,
        "expected two lines (global + project), got: {stdout:?}"
    );
    assert!(lines[0].starts_with("global: "), "bad line: {}", lines[0]);
    assert!(lines[0].ends_with("(not present)"), "bad: {}", lines[0]);
    assert!(lines[1].starts_with("project: "), "bad line: {}", lines[1]);
    assert!(lines[1].ends_with("(not present)"), "bad: {}", lines[1]);
}

#[test]
fn config_edit_creates_project_file_and_invokes_editor() {
    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();

    // `EDITOR=true` is a no-op editor available on every POSIX system:
    // it simply exits 0 regardless of args.
    let out = dedup_with_home(home.path())
        .env("EDITOR", "true")
        .env_remove("VISUAL")
        .arg("config")
        .arg("edit")
        .arg(repo.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`dedup config edit` exited non-zero: {:?}",
        out
    );

    // The project-scoped file was materialized.
    let project_file = repo.path().join(".dedup").join("config.toml");
    assert!(
        project_file.exists(),
        "expected {} to exist after edit",
        project_file.display()
    );
}

#[test]
fn config_edit_falls_back_to_visual_when_editor_unset() {
    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();

    let out = dedup_with_home(home.path())
        .env_remove("EDITOR")
        .env("VISUAL", "true")
        .arg("config")
        .arg("edit")
        .arg(repo.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "{:?}", out);
    assert!(repo.path().join(".dedup").join("config.toml").exists());
}

#[test]
fn scan_honors_project_config_thresholds_end_to_end() {
    // Copy the real fixture, drop a project config that sets the Tier A
    // threshold absurdly high, and assert that `scan` emits zero groups.
    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    let fixture = workspace_root().join("fixtures").join("tier_a_basic");
    copy_tree(&fixture, repo.path());

    // Bigger than any possible match in the fixture (files are < 50 LOC).
    // Both tier blocks are pinned sky-high so Tier A *and* Tier B are
    // both squelched — the fixture's `.rs` files would otherwise yield
    // Tier B groups from the embedded function bodies.
    let dedup_dir = repo.path().join(".dedup");
    std::fs::create_dir_all(&dedup_dir).unwrap();
    std::fs::write(
        dedup_dir.join("config.toml"),
        r#"
[thresholds.tier_a]
min_lines = 1000
min_tokens = 10000

[thresholds.tier_b]
min_lines = 1000
min_tokens = 10000
"#,
    )
    .unwrap();

    let out = dedup_with_home(home.path())
        .arg("scan")
        .arg(repo.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "scan failed: {:?}", out);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.is_empty(),
        "expected no groups with sky-high thresholds, got: {stdout:?}"
    );
}

#[test]
fn scan_with_invalid_config_exits_two_with_stderr_message() {
    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    let dedup_dir = repo.path().join(".dedup");
    std::fs::create_dir_all(&dedup_dir).unwrap();
    std::fs::write(dedup_dir.join("config.toml"), "this = = not valid\n").unwrap();

    let out = dedup_with_home(home.path())
        .arg("scan")
        .arg(repo.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        stderr.contains("config error"),
        "expected 'config error' in stderr, got: {stderr:?}"
    );
}

#[test]
fn scan_with_future_schema_warns_and_uses_defaults() {
    // Fixture: two byte-identical files with a duplicate block large
    // enough to clear the default tier_a thresholds.
    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    let fixture = workspace_root().join("fixtures").join("tier_a_basic");
    copy_tree(&fixture, repo.path());

    let dedup_dir = repo.path().join(".dedup");
    std::fs::create_dir_all(&dedup_dir).unwrap();
    std::fs::write(dedup_dir.join("config.toml"), "schema_version = 9999\n").unwrap();

    let out = dedup_with_home(home.path())
        .arg("scan")
        .arg(repo.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "expected success, got: {:?}", out);
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        stderr.contains("schema_version"),
        "expected schema_version warning in stderr, got: {stderr:?}"
    );
    // Defaults were applied, so the fixture's real duplicate block still
    // gets detected.
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("--- ["),
        "expected at least one group with defaults, got: {stdout:?}"
    );
}
