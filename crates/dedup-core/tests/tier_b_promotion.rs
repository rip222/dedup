//! Integration test: Tier A → Tier B promotion.
//!
//! Builds a temp directory with two Rust files, each containing a
//! function whose body is large enough to trip both Tier A (token
//! count + line count) and Tier B (syntactic unit). The two functions
//! are byte-identical.
//!
//! The expected outcome:
//!
//! - Without the promotion rule, both Tier A and Tier B would emit a
//!   group for the same function.
//! - With the promotion rule, any Tier A occurrence whose span lines
//!   match a Tier B unit's span lines is dropped; if that leaves the
//!   Tier A group with < 2 occurrences, the whole group goes away.
//! - The reported group must be tagged [`Tier::B`] and must contain
//!   both files.
//!
//! Keeping the two functions as the entire file content makes the
//! Tier A span line range equal to the Tier B unit line range, which
//! is the precondition for promotion.

use std::path::PathBuf;

use dedup_core::{ScanConfig, Scanner, Tier};
use tempfile::tempdir;

fn write(dir: &std::path::Path, name: &str, body: &str) {
    std::fs::write(dir.join(name), body).unwrap();
}

/// A body large enough to trip BOTH the Tier A thresholds (≥ 6 lines,
/// ≥ 50 tokens) AND the Tier B thresholds (≥ 3 lines, ≥ 15 tokens).
/// Tier A windows default to 50 tokens, so the span boundary lands
/// exactly at the function's start and end — the promotion rule fires.
const DUP_FN: &str = r#"fn duplicated_work(rows: &[i32]) -> i32 {
    let mut sum = 0;
    let mut count = 0;
    for r in rows {
        sum += r;
        count += 1;
    }
    let mean = if count > 0 { sum / count } else { 0 };
    let boosted = mean + count + 42;
    let doubled = boosted * 2;
    let tripled = doubled + boosted;
    tripled + sum + count
}
"#;

#[test]
fn tier_a_span_aligned_with_tier_b_unit_is_promoted_to_b_only() {
    let tmp = tempdir().unwrap();
    write(tmp.path(), "one.rs", DUP_FN);
    write(tmp.path(), "two.rs", DUP_FN);

    let scanner = Scanner::new(ScanConfig::default());
    let result = scanner.scan(tmp.path()).expect("scan");

    // Promotion: no Tier A group survives.
    let tier_a: Vec<_> = result.groups.iter().filter(|g| g.tier == Tier::A).collect();
    assert!(
        tier_a.is_empty(),
        "Tier A group should be promoted away, got: {:#?}",
        tier_a
    );

    // Exactly one Tier B group, two occurrences covering both files.
    let tier_b: Vec<_> = result.groups.iter().filter(|g| g.tier == Tier::B).collect();
    assert_eq!(
        tier_b.len(),
        1,
        "expected 1 Tier B group, got {:#?}",
        tier_b
    );
    let group = tier_b[0];
    assert_eq!(group.occurrences.len(), 2);

    let paths: Vec<PathBuf> = group.occurrences.iter().map(|o| o.path.clone()).collect();
    assert!(
        paths.contains(&PathBuf::from("one.rs")),
        "missing one.rs in {:?}",
        paths
    );
    assert!(
        paths.contains(&PathBuf::from("two.rs")),
        "missing two.rs in {:?}",
        paths
    );
}

#[test]
fn non_overlapping_tier_a_and_b_both_survive() {
    // If a Tier A match's lines do NOT coincide with a Tier B unit,
    // both groups survive — promotion must not over-prune.
    //
    // This fixture bolts extra shared code before and after the
    // duplicated function so the Tier A rolling-hash window extends
    // past the function body on at least one side. With Tier A's
    // default 50-token window we need a sizable chunk of common
    // tokens; a pair of mirrored helper functions gives us the tokens
    // without crossing Tier B's own thresholds.
    let tmp = tempdir().unwrap();

    let prefix_fn = "fn common_prefix_helper() -> i32 {\n    let a = 1;\n    let b = 2;\n    let c = 3;\n    let d = 4;\n    let e = 5;\n    let f = 6;\n    a + b + c + d + e + f\n}\n\n";
    let suffix_fn = "\nfn common_suffix_helper() -> i32 {\n    let aa = 10;\n    let bb = 20;\n    let cc = 30;\n    let dd = 40;\n    let ee = 50;\n    let ff = 60;\n    aa + bb + cc + dd + ee + ff\n}\n";
    let contents = format!("{prefix_fn}{DUP_FN}{suffix_fn}");
    write(tmp.path(), "alpha.rs", &contents);
    write(tmp.path(), "beta.rs", &contents);

    let scanner = Scanner::new(ScanConfig::default());
    let result = scanner.scan(tmp.path()).expect("scan");

    let tier_a: Vec<_> = result.groups.iter().filter(|g| g.tier == Tier::A).collect();
    let tier_b: Vec<_> = result.groups.iter().filter(|g| g.tier == Tier::B).collect();

    assert!(
        !tier_b.is_empty(),
        "expected at least one Tier B group, got {:#?}",
        result.groups
    );
    assert!(
        !tier_a.is_empty(),
        "Tier A span spans the whole file so it should NOT be promoted away, got {:#?}",
        result.groups
    );
}

#[test]
fn tier_b_thresholds_filter_tiny_units() {
    // A function with a one-line body (`fn f() { 1 }`) must NOT be
    // reported as a Tier B group even if duplicated, because it falls
    // under the 3-line / 15-token threshold.
    let tmp = tempdir().unwrap();
    let tiny = "fn f() -> i32 { 1 }\n";
    write(tmp.path(), "a.rs", tiny);
    write(tmp.path(), "b.rs", tiny);

    let scanner = Scanner::new(ScanConfig::default());
    let result = scanner.scan(tmp.path()).expect("scan");

    let tier_b: Vec<_> = result.groups.iter().filter(|g| g.tier == Tier::B).collect();
    assert!(
        tier_b.is_empty(),
        "tiny function must not produce a Tier B group, got {:#?}",
        tier_b
    );
}
