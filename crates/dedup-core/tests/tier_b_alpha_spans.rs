//! Integration tests for per-occurrence Tier B alpha-rename spans (#25).
//!
//! The scanner should attach `alpha_rename_spans` to every Tier B
//! occurrence, the cache should round-trip those bytes through
//! `write_scan_result` → `get_group`, and the same `placeholder_idx`
//! must refer to the same logical local across every occurrence of a
//! group — this is what the GUI relies on to tint corresponding
//! identifiers consistently across files.
//!
//! Tier A occurrences are explicitly covered as the negative case: they
//! must carry an empty span vector so the GUI renderer's "skip tint for
//! Tier A" check is a pure data-path assertion, not a special case at
//! render time.

use std::path::Path;

use dedup_core::{Cache, ScanConfig, Scanner, Tier};
use tempfile::tempdir;

fn write(dir: &Path, name: &str, body: &str) {
    std::fs::write(dir.join(name), body).unwrap();
}

/// Two near-duplicate functions with different local names but the same
/// structure — the canonical Tier B alpha-rename target.
const FN_A: &str = r#"fn duplicated_work(rows: &[i32]) -> i32 {
    let mut total = 0;
    let mut count = 0;
    for row in rows {
        total += row;
        count += 1;
    }
    let mean = if count > 0 { total / count } else { 0 };
    let bumped = mean + count + 42;
    let twice = bumped * 2;
    let thrice = twice + bumped;
    thrice + total + count
}
"#;

const FN_B: &str = r#"fn duplicated_work(items: &[i32]) -> i32 {
    let mut accum = 0;
    let mut seen = 0;
    for item in items {
        accum += item;
        seen += 1;
    }
    let avg = if seen > 0 { accum / seen } else { 0 };
    let scaled = avg + seen + 42;
    let twofold = scaled * 2;
    let triple = twofold + scaled;
    triple + accum + seen
}
"#;

#[test]
fn scanner_attaches_alpha_spans_to_tier_b_occurrences() {
    let tmp = tempdir().unwrap();
    write(tmp.path(), "one.rs", FN_A);
    write(tmp.path(), "two.rs", FN_B);

    let scanner = Scanner::new(ScanConfig::default());
    let result = scanner.scan(tmp.path()).expect("scan");

    let tier_b: Vec<_> = result.groups.iter().filter(|g| g.tier == Tier::B).collect();
    assert_eq!(tier_b.len(), 1, "one Tier B group for the two functions");
    let group = tier_b[0];
    assert_eq!(group.occurrences.len(), 2);

    // Every Tier B occurrence must carry spans, and every span must
    // resolve to non-empty identifier text in its file.
    for occ in &group.occurrences {
        assert!(
            !occ.alpha_rename_spans.is_empty(),
            "tier B occurrence must carry alpha-rename spans"
        );
        let abs = tmp.path().join(&occ.path);
        let src = std::fs::read(&abs).unwrap();
        for (s, e, idx) in &occ.alpha_rename_spans {
            assert!(*s < *e);
            assert!(*e <= src.len());
            assert!(*idx >= 1);
            let text = std::str::from_utf8(&src[*s..*e]).unwrap();
            assert!(
                text.chars().all(|c| c.is_alphanumeric() || c == '_'),
                "span must cover an identifier, got {text:?}"
            );
        }
    }

    // Correspondence: the two occurrences share byte-identical token
    // streams, so they must produce matching placeholder-index
    // sequences (one entry per leaf, in source order).
    let seq = |occ: &dedup_core::Occurrence| -> Vec<u32> {
        occ.alpha_rename_spans.iter().map(|(_, _, i)| *i).collect()
    };
    assert_eq!(seq(&group.occurrences[0]), seq(&group.occurrences[1]));
}

#[test]
fn tier_a_occurrences_carry_no_alpha_spans() {
    // Synthetic Tier A occurrences built by hand (the scanner can't
    // easily emit a Tier A group that survives promotion without a lot
    // of fixture gymnastics — the promotion rule strips any Tier A
    // whose span aligns with a Tier B unit). The data-path assertion
    // is what matters: Tier A Occurrences must always carry an empty
    // alpha-rename span vector, regardless of how they were built.
    use dedup_core::{MatchGroup, Occurrence, ScanResult, Span};
    use std::path::PathBuf;

    let result = ScanResult {
        groups: vec![MatchGroup {
            hash: 0x1111_2222_3333_4444,
            tier: Tier::A,
            occurrences: vec![
                Occurrence {
                    path: PathBuf::from("a.rs"),
                    span: Span {
                        start_line: 1,
                        end_line: 8,
                        start_byte: 0,
                        end_byte: 120,
                    },
                    alpha_rename_spans: Vec::new(),
                },
                Occurrence {
                    path: PathBuf::from("b.rs"),
                    span: Span {
                        start_line: 3,
                        end_line: 10,
                        start_byte: 50,
                        end_byte: 170,
                    },
                    alpha_rename_spans: Vec::new(),
                },
            ],
        }],
        files_scanned: 2,
        issues: Vec::new(),
    };

    let tmp = tempdir().unwrap();
    let mut cache = Cache::open(tmp.path()).unwrap();
    cache.write_scan_result(&result).unwrap();

    let summaries = cache.list_groups().unwrap();
    assert_eq!(summaries.len(), 1);
    let detail = cache.get_group(summaries[0].id).unwrap().unwrap();
    for occ in &detail.occurrences {
        assert!(
            occ.alpha_rename_spans.is_empty(),
            "Tier A occurrence must carry no alpha-rename spans after round-trip, got {:?}",
            occ.alpha_rename_spans
        );
    }
}

#[test]
fn cache_roundtrips_alpha_spans_for_tier_b() {
    let tmp = tempdir().unwrap();
    write(tmp.path(), "one.rs", FN_A);
    write(tmp.path(), "two.rs", FN_B);

    let scanner = Scanner::new(ScanConfig::default());
    let result = scanner.scan(tmp.path()).expect("scan");

    let mut cache = Cache::open(tmp.path()).unwrap();
    cache.write_scan_result(&result).unwrap();

    let summaries = cache.list_groups().unwrap();
    let tier_b_summary = summaries
        .iter()
        .find(|s| s.tier == Tier::B)
        .expect("tier B row persisted");
    let detail = cache
        .get_group(tier_b_summary.id)
        .unwrap()
        .expect("tier B group loadable");

    assert_eq!(detail.occurrences.len(), 2);
    for occ in &detail.occurrences {
        assert!(
            !occ.alpha_rename_spans.is_empty(),
            "cached Tier B occurrence must retain alpha-rename spans"
        );
    }

    // Correspondence survives the round-trip.
    let seq = |o: &dedup_core::CachedOccurrence| -> Vec<u32> {
        o.alpha_rename_spans.iter().map(|(_, _, i)| *i).collect()
    };
    assert_eq!(seq(&detail.occurrences[0]), seq(&detail.occurrences[1]));
}
