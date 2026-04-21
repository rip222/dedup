//! Integration tests for the four-layer [`IgnoreRules`] stack wired
//! through the [`Scanner`]. Each test sets up a temp tree, runs a real
//! scan with the relevant flag combination, and asserts on
//! `files_scanned` (the only user-visible proxy for "which files got
//! past every ignore layer").

use dedup_core::{ScanConfig, Scanner};
use std::fs;
use std::path::Path;
use tempfile::tempdir;

/// Build a [`ScanConfig`] with thresholds so low that every tokenized
/// file passes filtering. The layer tests only care *which* files get
/// scanned, not whether duplicates are found.
fn low_threshold_config() -> ScanConfig {
    ScanConfig {
        tier_a_min_lines: 1,
        tier_a_min_tokens: 1,
        tier_b_min_lines: 1,
        tier_b_min_tokens: 1,
        ..ScanConfig::default()
    }
}

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&p, body).unwrap();
}

/// Check whether `target` (relative to `root`) survived every ignore
/// layer under `cfg` by comparing `files_scanned` before and after
/// temporarily removing the target from disk. A delta of 1 means the
/// target was tokenized; 0 means some layer dropped it.
///
/// This is robust against small files that would otherwise fall below
/// Tier A's rolling-hash window — we only care about "did the scanner
/// read + tokenize this file", not "did it participate in a match".
fn file_survives_layers(root: &Path, target: &str, cfg: ScanConfig) -> bool {
    let target_abs = root.join(target);
    // Snapshot the target so we can restore it.
    let body = fs::read(&target_abs).expect("target exists");

    let with_n = Scanner::new(cfg.clone())
        .scan(root)
        .expect("scan with")
        .files_scanned;

    fs::remove_file(&target_abs).unwrap();
    let without_n = Scanner::new(cfg)
        .scan(root)
        .expect("scan without")
        .files_scanned;

    // Restore for the next probe in the same test.
    fs::write(&target_abs, &body).unwrap();

    with_n > without_n
}

#[test]
fn layer1_binary_and_size_and_git_are_skipped_by_default() {
    let dir = tempdir().unwrap();
    let root = dir.path();

    write(root, "plain.rs", "fn main() {}\n");
    // Binary (NUL byte in first 512).
    fs::write(root.join("payload.bin"), b"\0\0\0binary").unwrap();
    // Oversized.
    write(root, "big.rs", &"x".repeat(2_000_000));
    // `.git/` contents.
    write(root, ".git/config", "[core]\n");

    let cfg = low_threshold_config();
    assert!(file_survives_layers(root, "plain.rs", cfg.clone()));
    assert!(!file_survives_layers(root, "payload.bin", cfg.clone()));
    assert!(!file_survives_layers(root, "big.rs", cfg.clone()));
    assert!(!file_survives_layers(root, ".git/config", cfg));
}

#[test]
fn layer2_gitignore_is_respected_by_default() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join(".git")).unwrap();
    write(root, ".gitignore", "build/\nsecret.rs\n");
    write(root, "build/artifact.rs", "fn x() {}\n");
    write(root, "secret.rs", "fn s() {}\n");
    write(root, "kept.rs", "fn k() {}\n");

    let cfg = low_threshold_config();
    assert!(file_survives_layers(root, "kept.rs", cfg.clone()));
    assert!(!file_survives_layers(
        root,
        "build/artifact.rs",
        cfg.clone()
    ));
    assert!(!file_survives_layers(root, "secret.rs", cfg));
}

#[test]
fn layer2_git_info_exclude_is_respected() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join(".git/info")).unwrap();
    write(root, ".git/info/exclude", "private.rs\n");
    write(root, "private.rs", "fn p() {}\n");
    write(root, "public.rs", "fn q() {}\n");

    let cfg = low_threshold_config();
    assert!(file_survives_layers(root, "public.rs", cfg.clone()));
    assert!(!file_survives_layers(root, "private.rs", cfg));
}

#[test]
fn no_gitignore_flag_disables_layer2_only() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join(".git")).unwrap();
    write(root, ".gitignore", "ignored.rs\n");
    write(root, "ignored.rs", "fn a() {}\n");
    write(root, "app.min.js", "var x=1;\n");
    write(root, "kept.rs", "fn b() {}\n");

    // Defaults: layer 2 drops ignored.rs; layer 3 drops app.min.js.
    let cfg = low_threshold_config();
    assert!(!file_survives_layers(root, "ignored.rs", cfg.clone()));
    assert!(!file_survives_layers(root, "app.min.js", cfg.clone()));
    assert!(file_survives_layers(root, "kept.rs", cfg));

    // `--no-gitignore`: layer 2 off → ignored.rs passes. Layer 3
    // unchanged → app.min.js still drops.
    let mut cfg = low_threshold_config();
    cfg.no_gitignore = true;
    assert!(file_survives_layers(root, "ignored.rs", cfg.clone()));
    assert!(!file_survives_layers(root, "app.min.js", cfg));
}

#[test]
fn layer3_built_in_defaults_are_applied_by_default() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(root, "Cargo.lock", "[[package]]\n");
    write(root, "app.min.js", "var a=1;\n");
    write(root, "assets/style.min.css", "a{}\n");
    write(root, "main.bundle.js", "console.log(0);\n");
    write(root, "api.generated.ts", "export {};\n");
    write(root, "vendor/lib.rs", "fn v() {}\n");
    write(root, "third_party/x.rs", "fn t() {}\n");
    write(root, "third-party/y.rs", "fn u() {}\n");
    write(root, "src/main.rs", "fn main() {}\n");

    let cfg = low_threshold_config();
    for ignored in [
        "Cargo.lock",
        "app.min.js",
        "assets/style.min.css",
        "main.bundle.js",
        "api.generated.ts",
        "vendor/lib.rs",
        "third_party/x.rs",
        "third-party/y.rs",
    ] {
        assert!(
            !file_survives_layers(root, ignored, cfg.clone()),
            "{ignored} should be dropped by layer 3"
        );
    }
    assert!(file_survives_layers(root, "src/main.rs", cfg));
}

#[test]
fn layer3_generated_header_is_detected_in_first_5_lines() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(root, "auto.rs", "// @generated\nfn a() {}\n");
    write(
        root,
        "banner.rs",
        "// line1\n// line2\n// AUTO-GENERATED\nfn b() {}\n",
    );
    // Marker beyond line 5 — must NOT be detected.
    write(
        root,
        "late.rs",
        "l1\nl2\nl3\nl4\nl5\n// @generated\nfn c() {}\n",
    );
    write(root, "plain.rs", "fn d() {}\n");

    let cfg = low_threshold_config();
    assert!(!file_survives_layers(root, "auto.rs", cfg.clone()));
    assert!(!file_survives_layers(root, "banner.rs", cfg.clone()));
    assert!(file_survives_layers(root, "late.rs", cfg.clone()));
    assert!(file_survives_layers(root, "plain.rs", cfg));
}

#[test]
fn layer4_dedupignore_parses_gitignore_syntax() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(root, ".dedupignore", "ignored_dir/\n*.tmp\n");
    write(root, "ignored_dir/a.rs", "fn a() {}\n");
    write(root, "notes.tmp", "fn t() {}\n");
    write(root, "main.rs", "fn main() {}\n");

    let cfg = low_threshold_config();
    assert!(!file_survives_layers(root, "ignored_dir/a.rs", cfg.clone()));
    assert!(!file_survives_layers(root, "notes.tmp", cfg.clone()));
    assert!(file_survives_layers(root, "main.rs", cfg));
}

#[test]
fn all_flag_disables_layers_1_through_3_but_not_layer_4() {
    let dir = tempdir().unwrap();
    let root = dir.path();

    write(root, "Cargo.lock", "fn pretend() {}\n"); // layer 3 bait
    write(root, "gen.rs", "// @generated\nfn x() {}\n"); // layer 3 bait
    write(root, ".dedupignore", "nuke.rs\n"); // layer 4 ignore
    write(root, "nuke.rs", "fn n() {}\n");
    write(root, "keep.rs", "fn k() {}\n");

    let mut cfg = low_threshold_config();
    cfg.ignore_all = true;

    // With --all, layers 1–3 are off → Cargo.lock and gen.rs now pass.
    assert!(file_survives_layers(root, "Cargo.lock", cfg.clone()));
    assert!(file_survives_layers(root, "gen.rs", cfg.clone()));
    assert!(file_survives_layers(root, "keep.rs", cfg.clone()));
    // Layer 4 still filters.
    assert!(!file_survives_layers(root, "nuke.rs", cfg));
}

#[test]
fn layer4_whitelist_overrides_layer3() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(root, ".dedupignore", "!Cargo.lock\n");
    write(root, "Cargo.lock", "fn pretend() {}\n");
    write(root, "app.min.js", "var x=1;\n");
    write(root, "main.rs", "fn main() {}\n");

    let cfg = low_threshold_config();
    // Layer-4 whitelist re-includes.
    assert!(file_survives_layers(root, "Cargo.lock", cfg.clone()));
    // Other layer-3 hits still filter.
    assert!(!file_survives_layers(root, "app.min.js", cfg.clone()));
    assert!(file_survives_layers(root, "main.rs", cfg));
}

#[test]
fn utf8_decode_failure_skips_silently() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    // Invalid UTF-8 byte sequence (lone continuation byte).
    fs::write(root.join("bad.rs"), [b'f', b'n', b' ', 0xC3, 0x28]).unwrap();
    write(root, "good.rs", "fn main() {}\n");

    let cfg = low_threshold_config();
    assert!(file_survives_layers(root, "good.rs", cfg.clone()));
    assert!(!file_survives_layers(root, "bad.rs", cfg));
}

#[test]
fn layer_precedence_each_combination() {
    // Exhaustive (no_gitignore, all) matrix on a tree with baits for
    // layers 2, 3, and 4. Asserts per-layer behavior in every
    // combination.
    let dir = tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join(".git")).unwrap();
    write(root, ".gitignore", "gi.rs\n");
    write(root, "gi.rs", "fn g() {}\n"); // layer 2 bait
    write(root, "Cargo.lock", "fn l() {}\n"); // layer 3 bait
    write(root, ".dedupignore", "di.rs\n");
    write(root, "di.rs", "fn d() {}\n"); // layer 4 bait
    write(root, "keep.rs", "fn k() {}\n"); // always survives

    let expected = [
        // (no_gitignore, all, file, survives?)
        (false, false, "gi.rs", false),
        (false, false, "Cargo.lock", false),
        (false, false, "di.rs", false),
        (false, false, "keep.rs", true),
        (true, false, "gi.rs", true), // layer 2 off
        (true, false, "Cargo.lock", false),
        (true, false, "di.rs", false),
        (true, false, "keep.rs", true),
        (false, true, "gi.rs", true), // `--all` disables layers 1–3 incl. gitignore per PRD
        (false, true, "Cargo.lock", true), // layer 3 off
        (false, true, "di.rs", false), // layer 4 still on
        (false, true, "keep.rs", true),
        (true, true, "gi.rs", true),
        (true, true, "Cargo.lock", true),
        (true, true, "di.rs", false),
        (true, true, "keep.rs", true),
    ];
    for (no_git, all, file, expect) in expected {
        let mut cfg = low_threshold_config();
        cfg.no_gitignore = no_git;
        cfg.ignore_all = all;
        let got = file_survives_layers(root, file, cfg);
        assert_eq!(
            got, expect,
            "(no_gitignore={no_git}, all={all}) file {file}: expected survives={expect}, got {got}"
        );
    }
}
