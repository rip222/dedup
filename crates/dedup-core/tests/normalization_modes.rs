//! Integration tests for issue #10 — conservative / aggressive
//! normalization modes at the scanner level.
//!
//! The two contracts this file locks in:
//!
//! 1. **Conservative is the default** and matches the pre-#10 output
//!    byte-for-byte on the existing fixture corpus.
//! 2. **Aggressive is a superset** — for a crafted pair of files that
//!    differ only in a literal value, aggressive clusters them while
//!    conservative does not, producing *strictly more* groups.
//!
//! The second test is the acceptance check: aggressive ≥ conservative
//! in group count, with at least one strictly-extra group traceable to
//! a literal-diverging pair.

use std::fs;
use std::path::PathBuf;

use dedup_core::{Normalization, ScanConfig, Scanner, Tier};
use dedup_lang::NormalizationMode;
use tempfile::tempdir;

fn workspace_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); // dedup-core → crates
    p.pop(); // crates     → workspace root
    p
}

/// Scanner config tuned so small crafted fixtures clear Tier B filters
/// without dragging in unrelated Tier A noise. Tier A thresholds are
/// raised so the rolling hash stays silent; Tier B is the pass under
/// test.
fn tuned_scan_config(mode: NormalizationMode) -> ScanConfig {
    ScanConfig {
        tier_a_min_lines: 9999,
        tier_a_min_tokens: 9999,
        tier_b_min_lines: 3,
        tier_b_min_tokens: 10,
        max_file_size: 1_048_576,
        follow_symlinks: false,
        include_submodules: false,
        no_gitignore: false,
        ignore_all: false,
        normalization: mode,
        jobs: None,
        cache_root: None,
        ..ScanConfig::default()
    }
}

#[test]
fn conservative_is_the_default_mode() {
    // Default `ScanConfig::default()` and `Normalization::default()`
    // must both land on Conservative so the scanner's behaviour
    // remains byte-identical to pre-#10 when no config is written.
    assert_eq!(
        ScanConfig::default().normalization,
        NormalizationMode::Conservative,
    );
    assert_eq!(
        NormalizationMode::from(Normalization::default()),
        NormalizationMode::Conservative,
    );
}

#[test]
fn aggressive_emits_strictly_more_groups_than_conservative() {
    // Two Rust files that are identical except for one string literal
    // and one integer literal. Under conservative, their function
    // hashes differ → no Tier B group. Under aggressive, the literal
    // leaves collapse to `<LIT>` and the pair clusters → +1 group.

    let dir = tempdir().expect("tempdir");
    let root = dir.path();

    // `literal_diverge_a.rs` and `literal_diverge_b.rs` — identical
    // structure, divergent literals.
    fs::write(
        root.join("literal_diverge_a.rs"),
        r#"
fn literal_pair(input: &str) -> String {
    let prefix = "alpha";
    let n = 1;
    let mut out = String::new();
    out.push_str(prefix);
    out.push_str(input);
    for _ in 0..n {
        out.push('!');
    }
    out
}
"#,
    )
    .unwrap();
    fs::write(
        root.join("literal_diverge_b.rs"),
        r#"
fn literal_pair(input: &str) -> String {
    let prefix = "omega";
    let n = 99;
    let mut out = String::new();
    out.push_str(prefix);
    out.push_str(input);
    for _ in 0..n {
        out.push('!');
    }
    out
}
"#,
    )
    .unwrap();

    let conservative = Scanner::new(tuned_scan_config(NormalizationMode::Conservative))
        .scan(root)
        .expect("conservative scan");
    let aggressive = Scanner::new(tuned_scan_config(NormalizationMode::Aggressive))
        .scan(root)
        .expect("aggressive scan");

    // Core acceptance: aggressive produces strictly more groups.
    assert!(
        aggressive.groups.len() > conservative.groups.len(),
        "expected aggressive ({}) > conservative ({}) groups\n\
         conservative = {:#?}\naggressive = {:#?}",
        aggressive.groups.len(),
        conservative.groups.len(),
        conservative.groups,
        aggressive.groups,
    );

    // Stronger check: conservative found nothing here, aggressive
    // found exactly the literal-pair group with both files in it.
    assert!(
        conservative.groups.is_empty(),
        "conservative must not cluster literal-diverging pair: {:#?}",
        conservative.groups,
    );
    let tier_b: Vec<_> = aggressive
        .groups
        .iter()
        .filter(|g| g.tier == Tier::B)
        .collect();
    assert_eq!(
        tier_b.len(),
        1,
        "expected exactly one Tier B group under aggressive mode, got {:#?}",
        aggressive.groups
    );
    let occurrences = &tier_b[0].occurrences;
    assert_eq!(
        occurrences.len(),
        2,
        "aggressive group should pair both fixture files"
    );
    let paths: Vec<String> = occurrences
        .iter()
        .map(|o| o.path.to_string_lossy().to_string())
        .collect();
    assert!(
        paths.iter().any(|p| p.contains("literal_diverge_a.rs")),
        "missing a-side: {paths:?}"
    );
    assert!(
        paths.iter().any(|p| p.contains("literal_diverge_b.rs")),
        "missing b-side: {paths:?}"
    );
}

#[test]
fn conservative_scan_matches_baseline_on_existing_fixtures() {
    // Conservative mode on the shipped `fixtures/tier_a_basic` corpus
    // must produce the same group count as the pre-#10 default so
    // downstream snapshot tests (scan_snapshot.rs) stay byte-stable.
    let fixture = workspace_root().join("fixtures").join("tier_a_basic");
    assert!(fixture.exists(), "fixture missing at {}", fixture.display());

    let default_cfg = ScanConfig::default();
    assert_eq!(default_cfg.normalization, NormalizationMode::Conservative);

    let explicit_cfg = ScanConfig {
        normalization: NormalizationMode::Conservative,
        ..ScanConfig::default()
    };

    let via_default = Scanner::new(default_cfg).scan(&fixture).expect("scan");
    let via_explicit = Scanner::new(explicit_cfg).scan(&fixture).expect("scan");

    assert_eq!(via_default.groups.len(), via_explicit.groups.len());
    assert_eq!(via_default.files_scanned, via_explicit.files_scanned);
}
