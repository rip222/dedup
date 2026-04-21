//! End-to-end tests for the `--format` matrix introduced in issue #12.
//!
//! Every test shells out to the compiled `dedup` binary via `assert_cmd`
//! so argv → stdout routing, the TTY auto-select, and the SARIF envelope
//! are all exercised in the shape a user would hit them.
//!
//! SARIF output is validated against a vendored copy of the SARIF 2.1.0
//! JSON Schema (`tests/fixtures/sarif-2.1.0.json`). The schema is not
//! self-referencing any external registry we cannot resolve, so the
//! validator needs no network access.

use std::path::{Path, PathBuf};

use assert_cmd::Command;
use serde_json::Value;
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

/// Populate a temp dir with the `tier_a_basic` fixture and run a scan so
/// a cache exists for `list` / `show`.
fn prepare_scanned_fixture() -> tempfile::TempDir {
    let tmp = tempdir().unwrap();
    let fixture = workspace_root().join("fixtures").join("tier_a_basic");
    copy_tree(&fixture, tmp.path());

    // Force `--format text` on the prep scan so the cache-write path
    // doesn't depend on which format the CI environment happened to
    // auto-select.
    let output = Command::cargo_bin("dedup")
        .unwrap()
        .arg("--format")
        .arg("text")
        .arg("scan")
        .arg(tmp.path())
        .output()
        .unwrap();
    assert!(output.status.success(), "scan failed: {:?}", output);
    tmp
}

// ---------------------------------------------------------------------------
// NDJSON (`list`) — one group per line, streamable via `jq`.
// ---------------------------------------------------------------------------

#[test]
fn list_ndjson_emits_one_group_per_line() {
    let tmp = prepare_scanned_fixture();
    let out = Command::cargo_bin("dedup")
        .unwrap()
        .arg("--format")
        .arg("json")
        .arg("list")
        .arg(tmp.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "list --format json failed: {:?}", out);
    let stdout = String::from_utf8(out.stdout).unwrap();

    // One JSON object per line, each independently parseable.
    let mut parsed = Vec::new();
    for line in stdout.lines() {
        assert!(!line.is_empty(), "NDJSON must not emit blank lines");
        let v: Value =
            serde_json::from_str(line).unwrap_or_else(|e| panic!("bad NDJSON line {line:?}: {e}"));
        parsed.push(v);
    }

    // The fixture yields exactly two groups (one Tier A + one Tier B).
    assert_eq!(
        parsed.len(),
        2,
        "expected 2 NDJSON rows from tier_a_basic, got {}: {stdout}",
        parsed.len()
    );

    // Spot-check the schema of row 0.
    let row = &parsed[0];
    assert!(row["tier"].is_string());
    assert!(row["hash"].is_string());
    assert!(row["occurrence_count"].as_u64().unwrap() >= 2);
    assert!(row["occurrences"].is_array());
    let occ = &row["occurrences"][0];
    assert!(occ["path"].is_string());
    assert!(occ["start_line"].is_u64());
    assert!(occ["end_line"].is_u64());
}

// ---------------------------------------------------------------------------
// `show` — a single JSON object.
// ---------------------------------------------------------------------------

#[test]
fn show_json_emits_a_single_object() {
    let tmp = prepare_scanned_fixture();
    let out = Command::cargo_bin("dedup")
        .unwrap()
        .arg("--format")
        .arg("json")
        .arg("show")
        .arg("1")
        .arg(tmp.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "show --format json failed: {:?}", out);
    let stdout = String::from_utf8(out.stdout).unwrap();
    let v: Value = serde_json::from_str(stdout.trim()).unwrap();

    assert!(v.is_object(), "show json must be a single object");
    assert_eq!(v["id"].as_i64(), Some(1));
    assert!(v["occurrences"].is_array());
    assert_eq!(v["tier"].as_str().unwrap(), "A");
}

// ---------------------------------------------------------------------------
// Non-TTY auto-select: no `--format` + piped stdout → JSON.
// ---------------------------------------------------------------------------

#[test]
fn list_auto_selects_json_when_piped() {
    // `assert_cmd` always pipes the child's stdout, so `IsTerminal`
    // returns false and the resolver picks JSON. This covers the
    // PRD acceptance criterion directly.
    let tmp = prepare_scanned_fixture();
    let out = Command::cargo_bin("dedup")
        .unwrap()
        .arg("list")
        .arg(tmp.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "list failed: {:?}", out);
    let stdout = String::from_utf8(out.stdout).unwrap();
    // Every line should be valid JSON.
    for line in stdout.lines() {
        serde_json::from_str::<Value>(line)
            .unwrap_or_else(|e| panic!("auto-select should produce JSON, got {line:?}: {e}"));
    }
}

#[test]
fn scan_auto_selects_json_when_piped() {
    let tmp = tempdir().unwrap();
    let fixture = workspace_root().join("fixtures").join("tier_a_basic");
    copy_tree(&fixture, tmp.path());
    let out = Command::cargo_bin("dedup")
        .unwrap()
        .arg("scan")
        .arg(tmp.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    for line in stdout.lines() {
        serde_json::from_str::<Value>(line)
            .unwrap_or_else(|e| panic!("scan auto-select should be JSON, got {line:?}: {e}"));
    }
}

#[test]
fn format_text_override_forces_text_even_when_piped() {
    // The other half of the override contract: `--format text` under
    // a piped stdout must still emit text, not JSON.
    let tmp = prepare_scanned_fixture();
    let out = Command::cargo_bin("dedup")
        .unwrap()
        .arg("--format")
        .arg("text")
        .arg("list")
        .arg(tmp.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.starts_with("--- ["),
        "--format text must stay text when piped, got: {stdout:?}"
    );
}

// ---------------------------------------------------------------------------
// SARIF — structural assertions AND full schema validation.
// ---------------------------------------------------------------------------

#[test]
fn scan_sarif_has_required_envelope() {
    let tmp = tempdir().unwrap();
    let fixture = workspace_root().join("fixtures").join("tier_a_basic");
    copy_tree(&fixture, tmp.path());
    let out = Command::cargo_bin("dedup")
        .unwrap()
        .arg("--format")
        .arg("sarif")
        .arg("scan")
        .arg(tmp.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "scan --format sarif failed: {:?}",
        out
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    let v: Value = serde_json::from_str(stdout.trim()).unwrap();

    assert_eq!(v["version"], "2.1.0");
    assert_eq!(v["runs"][0]["tool"]["driver"]["name"], "dedup");
    let results = v["runs"][0]["results"].as_array().unwrap();
    assert!(
        !results.is_empty(),
        "expected at least one result for tier_a_basic"
    );
    // Mandatory location shape for GitHub Code Scanning.
    let loc0 = &results[0]["locations"][0]["physicalLocation"];
    assert!(loc0["artifactLocation"]["uri"].is_string());
    assert!(loc0["region"]["startLine"].as_u64().unwrap() >= 1);
}

#[test]
fn scan_sarif_validates_against_schema() {
    let tmp = tempdir().unwrap();
    let fixture = workspace_root().join("fixtures").join("tier_a_basic");
    copy_tree(&fixture, tmp.path());
    let out = Command::cargo_bin("dedup")
        .unwrap()
        .arg("--format")
        .arg("sarif")
        .arg("scan")
        .arg(tmp.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    let report: Value = serde_json::from_str(stdout.trim()).unwrap();

    let schema_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sarif-2.1.0.json");
    let schema_src = std::fs::read_to_string(&schema_path).expect("schema fixture");
    let schema: Value = serde_json::from_str(&schema_src).expect("schema parse");

    // `jsonschema` 0.46: `options()` builder + `build(&schema)`.
    let validator = jsonschema::options()
        .build(&schema)
        .expect("build SARIF validator");
    let errors: Vec<String> = validator
        .iter_errors(&report)
        .map(|e| format!("{e}"))
        .collect();
    assert!(
        errors.is_empty(),
        "SARIF report did not validate against 2.1.0 schema. First errors:\n{}",
        errors
            .iter()
            .take(5)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn list_sarif_envelope_matches_scan() {
    let tmp = prepare_scanned_fixture();
    let out = Command::cargo_bin("dedup")
        .unwrap()
        .arg("--format")
        .arg("sarif")
        .arg("list")
        .arg(tmp.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "list --format sarif failed: {:?}",
        out
    );
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["version"], "2.1.0");
    assert!(!v["runs"][0]["results"].as_array().unwrap().is_empty());
}
