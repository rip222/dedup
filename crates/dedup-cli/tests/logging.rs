//! Integration tests for issue #16: verify the CLI's `tracing` subscriber
//! respects `RUST_LOG` and the `--verbose` flag.
//!
//! We run the compiled `dedup` binary against a copy of `fixtures/tier_a_basic`
//! and inspect stderr. The library emits `info!` at scan start/end and
//! `debug!` per tokenized file, so at `dedup=debug` level we always see at
//! least one `DEBUG` line (the fixture has ≥ 1 text file) and at the
//! default `warn` level we see none.

use assert_cmd::Command;
use tempfile::tempdir;

mod common;
use common::*;

fn copy_fixture() -> tempfile::TempDir {
    let fixture = workspace_root().join("fixtures").join("tier_a_basic");
    let tmp = tempdir().unwrap();
    copy_tree(&fixture, tmp.path());
    tmp
}

#[test]
fn debug_events_appear_when_rust_log_enables_them() {
    let tmp = copy_fixture();

    let output = Command::cargo_bin("dedup")
        .expect("dedup binary")
        .env("RUST_LOG", "dedup=debug")
        .arg("scan")
        .arg(tmp.path())
        .output()
        .expect("dedup scan");

    assert!(output.status.success(), "dedup scan failed: {:?}", output);
    let stderr = String::from_utf8(output.stderr).expect("utf-8 stderr");
    assert!(
        stderr.contains("DEBUG"),
        "expected at least one DEBUG line in stderr; got: {stderr}"
    );
}

#[test]
fn debug_events_are_filtered_out_by_default() {
    let tmp = copy_fixture();

    let output = Command::cargo_bin("dedup")
        .expect("dedup binary")
        .env_remove("RUST_LOG")
        .arg("scan")
        .arg(tmp.path())
        .output()
        .expect("dedup scan");

    assert!(output.status.success(), "dedup scan failed: {:?}", output);
    let stderr = String::from_utf8(output.stderr).expect("utf-8 stderr");
    assert!(
        !stderr.contains("DEBUG"),
        "expected no DEBUG lines at default filter; got: {stderr}"
    );
    assert!(
        !stderr.contains("INFO"),
        "expected no INFO lines at default `warn` filter; got: {stderr}"
    );
}

#[test]
fn verbose_flag_enables_debug_filter() {
    let tmp = copy_fixture();

    let output = Command::cargo_bin("dedup")
        .expect("dedup binary")
        .env_remove("RUST_LOG")
        .arg("--verbose")
        .arg("scan")
        .arg(tmp.path())
        .output()
        .expect("dedup scan");

    assert!(output.status.success(), "dedup scan failed: {:?}", output);
    let stderr = String::from_utf8(output.stderr).expect("utf-8 stderr");
    assert!(
        stderr.contains("DEBUG"),
        "expected DEBUG lines under --verbose; got: {stderr}"
    );
}

#[test]
fn cli_logs_land_on_stderr_not_stdout() {
    // The scan `stdout` is the machine-parseable group listing; log
    // output must stay on stderr so `dedup scan | xargs -o nvim` works.
    let tmp = copy_fixture();

    let output = Command::cargo_bin("dedup")
        .expect("dedup binary")
        .env("RUST_LOG", "dedup=debug")
        .arg("scan")
        .arg(tmp.path())
        .output()
        .expect("dedup scan");

    let stdout = String::from_utf8(output.stdout).expect("utf-8 stdout");
    assert!(
        !stdout.contains("DEBUG"),
        "DEBUG lines leaked onto stdout: {stdout}"
    );
    assert!(
        !stdout.contains("INFO"),
        "INFO lines leaked onto stdout: {stdout}"
    );
}
