//! Integration tests for dismissal / suppression behavior.
//!
//! The acceptance criterion we lean into hardest here: dismissal is keyed
//! by the **normalized-block-hash**, not by group id or file path. Two
//! consequences, each with a test below:
//!
//! 1. Cosmetic churn (re-scans that produce identical blocks) keeps the
//!    group hidden — the hash is stable, so the suppression still matches.
//! 2. Altering the block (rewriting the body enough that the normalized
//!    token stream changes) surfaces the group again — the new hash isn't
//!    in the suppressions table.
//!
//! These run against the real Tier A pipeline via [`Scanner::scan`], not
//! against synthetic `ScanResult`s, so the "hash produced by the scanner
//! matches the one we dismissed" invariant is end-to-end.

use std::path::{Path, PathBuf};

use dedup_core::{Cache, ScanConfig, Scanner, Tier};

fn workspace_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); // dedup-core → crates
    p.pop(); // crates     → workspace root
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

/// Scan the given root, persist the result, and return the cache handle
/// alongside the hash of the (single) Tier A group in the tier_a_basic
/// fixture.
fn scan_and_cache(root: &Path) -> (Cache, dedup_core::Hash) {
    let scanner = Scanner::new(ScanConfig::default());
    let result = scanner.scan(root).expect("scan");
    let tier_a: Vec<_> = result.groups.iter().filter(|g| g.tier == Tier::A).collect();
    assert_eq!(
        tier_a.len(),
        1,
        "tier_a_basic fixture should yield exactly one Tier A group"
    );
    let hash = tier_a[0].hash;

    let mut cache = Cache::open(root).unwrap();
    cache.write_scan_result(&result).unwrap();
    (cache, hash)
}

#[test]
fn dismissal_persists_across_rescans() {
    // Arrange: copy the fixture into a temp dir, scan, dismiss the one
    // Tier A group.
    let tmp = tempfile::tempdir().unwrap();
    let fixture = workspace_root().join("fixtures").join("tier_a_basic");
    copy_tree(&fixture, tmp.path());

    let (mut cache, hash) = scan_and_cache(tmp.path());
    cache.dismiss_hash(hash, Some(1)).unwrap();
    assert!(cache.suppressed_hashes().unwrap().contains(&hash));

    // Re-scan in the same dir. The cache is overwritten, but
    // `suppressions` is untouched, so the hash must still be dismissed.
    let scanner = Scanner::new(ScanConfig::default());
    let result = scanner.scan(tmp.path()).expect("rescan");
    cache.write_scan_result(&result).unwrap();

    let set = cache.suppressed_hashes().unwrap();
    assert!(
        set.contains(&hash),
        "suppression should survive a re-scan / write_scan_result"
    );

    // The new scan still produces the same Tier A hash (identical
    // fixture), so a report-time filter would still hide it.
    let tier_a: Vec<_> = result.groups.iter().filter(|g| g.tier == Tier::A).collect();
    assert_eq!(tier_a.len(), 1);
    assert_eq!(tier_a[0].hash, hash);
}

#[test]
fn altered_block_resurfaces_after_mutation() {
    // The mutation test. Dismiss the Tier A group; rewrite every fixture
    // file enough to change the normalized token stream; re-scan; assert
    // that the new group's hash is NOT in the suppressions table so a
    // report-time filter would surface it.
    let tmp = tempfile::tempdir().unwrap();
    let fixture = workspace_root().join("fixtures").join("tier_a_basic");
    copy_tree(&fixture, tmp.path());

    let (mut cache, original_hash) = scan_and_cache(tmp.path());
    cache.dismiss_hash(original_hash, Some(1)).unwrap();

    // Mutate the shared block: replace the entire fixture body with a
    // new, still-duplicated block that normalizes to a different hash.
    // We write the same synthetic body to every .rs file so the scanner
    // still finds ≥ 2 occurrences and therefore still produces a group.
    let mutated = MUTATED_BODY;
    for entry in std::fs::read_dir(tmp.path()).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("rs") {
            std::fs::write(&path, mutated).unwrap();
        }
    }

    // Re-scan the mutated tree.
    let scanner = Scanner::new(ScanConfig::default());
    let rescan = scanner.scan(tmp.path()).expect("rescan");
    cache.write_scan_result(&rescan).unwrap();

    // The scanner may emit the shared block as Tier A *or* Tier B
    // (Rust's Tier B profile will pick up a well-formed top-level `fn`
    // and Tier A occurrences that align with a Tier B unit get promoted
    // out of Tier A). Either way, the block must produce *some* group
    // and its hash must differ from the original — otherwise the test
    // proves nothing.
    assert!(
        !rescan.groups.is_empty(),
        "a duplicated block across three files must produce at least one \
         group"
    );
    let new_hash = rescan.groups[0].hash;
    assert_ne!(
        new_hash, original_hash,
        "the mutation must change the normalized-block-hash; if it doesn't, \
         the test proves nothing"
    );

    // Key assertion: the suppression does NOT match the new hash, so
    // this group will re-surface on the next report.
    let set = cache.suppressed_hashes().unwrap();
    assert!(
        !set.contains(&new_hash),
        "altered block should no longer be suppressed (new hash: {new_hash:016x})"
    );
    assert!(
        set.contains(&original_hash),
        "original suppression is still there, just doesn't match anymore"
    );
}

/// A replacement body for the fixture files. Intentionally long enough
/// (well over the Tier A thresholds of 6 lines / 50 tokens) and
/// structurally different from the original fixture's `compute_totals`
/// block so the normalized-token hash comes out different.
const MUTATED_BODY: &str = r#"
pub fn mutated_routine(alpha: i64, beta: i64, gamma: i64, delta: i64) -> i64 {
    let first = alpha * 31 + beta * 2 + gamma;
    let second = beta * 17 + gamma * 4 + delta;
    let third = gamma * 13 + alpha * 5 + delta;
    let fourth = first + second + third + delta;
    let fifth = fourth * 2 + alpha + beta;
    let sixth = fifth.saturating_sub(7).saturating_add(alpha);
    let seventh = sixth.wrapping_mul(3).wrapping_add(beta);
    let eighth = seventh + alpha - beta + gamma;
    let ninth = eighth - beta + delta * 2;
    let tenth = ninth.max(gamma).min(alpha + 100);
    let eleventh = tenth + first - second;
    let twelfth = eleventh.wrapping_mul(9) + third;
    let thirteenth = twelfth.saturating_add(fifth);
    let fourteenth = thirteenth - sixth + seventh;
    let fifteenth = fourteenth.max(eighth).min(ninth);
    fifteenth + tenth + eleventh + twelfth
}
"#;
