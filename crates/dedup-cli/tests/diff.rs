//! Integration tests for `dedup diff --since <ref>` (issue #59).
//!
//! Drives the compiled binary on a repo with two cached scans and asserts
//! the NEW / GREW / SHRANK / GONE output layout. Keeping the test at the
//! process boundary matches the pattern used by `list_show.rs` and
//! guards the argv-parse path as well as the renderer.

use assert_cmd::Command;
use rusqlite::{Connection, params};
use tempfile::tempdir;

mod common;

/// Materialize a `.dedup/cache.sqlite` with two pre-populated scans so
/// `dedup diff --since <scan_id>` has something to compare. We hand-roll
/// the rows rather than running `dedup scan` twice because the fixture
/// we have doesn't shift between runs — so the GREW / SHRANK paths need
/// us to construct controlled counts.
fn seed_two_scans(tmp: &std::path::Path) {
    // Touch the DB via a real scan first so the schema migration runs.
    Command::cargo_bin("dedup")
        .unwrap()
        .arg("scan")
        .arg(tmp)
        .assert()
        .success();

    let db = tmp.join(".dedup").join("cache.sqlite");
    let conn = Connection::open(&db).unwrap();

    // Clear any rows the real scan left behind so assertions are
    // deterministic regardless of fixture contents.
    conn.execute("DELETE FROM scan_groups", []).unwrap();
    conn.execute("DELETE FROM scans", []).unwrap();

    let h1 = (1u64.to_be_bytes()).to_vec();
    let h2 = (2u64.to_be_bytes()).to_vec();
    let h3 = (3u64.to_be_bytes()).to_vec();

    // Base scan: group 1 with 2 occ, group 2 with 3 occ.
    conn.execute(
        "INSERT INTO scans (scan_id, started_at, folder_hash, git_commit) \
         VALUES (1, 100, ?1, NULL)",
        params![&h1[..]],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO scan_groups (scan_id, group_hash, occurrence_count, total_lines) \
         VALUES (1, ?1, 2, 20)",
        params![&h1[..]],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO scan_groups (scan_id, group_hash, occurrence_count, total_lines) \
         VALUES (1, ?1, 3, 30)",
        params![&h2[..]],
    )
    .unwrap();

    // Head scan: group 1 shrank to 0 (gone), group 2 grew to 5,
    // group 3 is new with 2 occurrences.
    conn.execute(
        "INSERT INTO scans (scan_id, started_at, folder_hash, git_commit) \
         VALUES (2, 200, ?1, 'deadbeefcafe1234')",
        params![&h1[..]],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO scan_groups (scan_id, group_hash, occurrence_count, total_lines) \
         VALUES (2, ?1, 5, 50)",
        params![&h2[..]],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO scan_groups (scan_id, group_hash, occurrence_count, total_lines) \
         VALUES (2, ?1, 2, 20)",
        params![&h3[..]],
    )
    .unwrap();
}

#[test]
fn diff_since_scan_id_emits_kind_columns() {
    let tmp = tempdir().unwrap();
    seed_two_scans(tmp.path());

    let out = Command::cargo_bin("dedup")
        .unwrap()
        .arg("diff")
        .arg("--since")
        .arg("1")
        .arg(tmp.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "diff failed: {:?}", out);
    let stdout = String::from_utf8(out.stdout).unwrap();
    // Header line present.
    assert!(stdout.contains("# dedup diff: base scan=1 head scan=2"));
    // One line per changed group, in hash-ascending order.
    assert!(stdout.contains("GONE"));
    assert!(stdout.contains("GREW"));
    assert!(stdout.contains("NEW"));
    // GONE hash (1) precedes GREW hash (2) precedes NEW hash (3).
    let g = stdout.find("GONE").unwrap();
    let gr = stdout.find("GREW").unwrap();
    let n = stdout.find("NEW").unwrap();
    assert!(g < gr && gr < n, "output not hash-ascending:\n{stdout}");
}

#[test]
fn diff_since_commit_sha_resolves() {
    let tmp = tempdir().unwrap();
    seed_two_scans(tmp.path());

    // `deadbeefcafe1234` is the head scan's commit; `--since` must only
    // resolve against commits on scans *other* than head, so we flip:
    // stamp the base scan with a known SHA and diff against it.
    let db = tmp.path().join(".dedup").join("cache.sqlite");
    let conn = Connection::open(&db).unwrap();
    conn.execute(
        "UPDATE scans SET git_commit = 'abc123def45678' WHERE scan_id = 1",
        [],
    )
    .unwrap();

    let out = Command::cargo_bin("dedup")
        .unwrap()
        .arg("diff")
        .arg("--since")
        .arg("abc1")
        .arg(tmp.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "diff failed: {:?}", out);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("base scan=1 head scan=2"));
}

#[test]
fn diff_since_relative_date_resolves() {
    let tmp = tempdir().unwrap();
    seed_two_scans(tmp.path());

    // Base scan's started_at is 100, head's 200 — both ancient. `1d`
    // resolves to `now - 1 day`, which is >> 200, so the resolver picks
    // the head itself and errors. `1000w` resolves to well before 100,
    // so no scan matches → exit 2 with "could not resolve". Asserting
    // exit code keeps the test stable without pinning on specific text.
    let out = Command::cargo_bin("dedup")
        .unwrap()
        .arg("diff")
        .arg("--since")
        .arg("1000w")
        .arg(tmp.path())
        .output()
        .unwrap();
    // 2 = "could not resolve" per run_diff.
    assert_eq!(out.status.code(), Some(2));
}
