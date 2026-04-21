//! Integration test: run [`Scanner::scan`] against the in-workspace
//! `fixtures/tier_a_basic/` corpus and assert on the resulting match
//! groups. The fixture contains three files with a known duplicated block
//! (≥ 6 lines, ≥ 50 tokens). A passing scan proves the full Tier A pipeline
//! end-to-end: walk → tokenize → rolling-hash → bucket → extend → filter.

use std::path::PathBuf;

use dedup_core::{ScanConfig, Scanner};

fn workspace_root() -> PathBuf {
    // `CARGO_MANIFEST_DIR` points at the crate dir; the workspace root is
    // two levels up (`.../crates/dedup-core`).
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); // dedup-core → crates
    p.pop(); // crates     → workspace root
    p
}

#[test]
fn scans_tier_a_basic_fixture() {
    let fixture = workspace_root().join("fixtures").join("tier_a_basic");
    assert!(
        fixture.exists(),
        "fixture dir not found at {}",
        fixture.display()
    );

    let scanner = Scanner::new(ScanConfig::default());
    let result = scanner.scan(&fixture).expect("scan");

    // 3 fixture files, all text.
    assert_eq!(result.files_scanned, 3, "expected 3 files scanned");

    // The shared duplicate block is ≥ 6 lines AND ≥ 50 tokens, so exactly
    // one Tier A match group should be emitted.
    assert_eq!(
        result.groups.len(),
        1,
        "expected exactly 1 match group, got {:#?}",
        result.groups
    );
    let group = &result.groups[0];
    assert_eq!(group.occurrences.len(), 3, "expected 3 occurrences");

    let paths: Vec<String> = group
        .occurrences
        .iter()
        .map(|o| o.path.to_string_lossy().to_string())
        .collect();
    assert!(paths.iter().any(|p| p.contains("alpha.rs")));
    assert!(paths.iter().any(|p| p.contains("beta.rs")));
    assert!(paths.iter().any(|p| p.contains("gamma.rs")));

    // Sanity-check: every occurrence should cover ≥ 6 lines.
    for occ in &group.occurrences {
        let line_span = occ.span.end_line - occ.span.start_line + 1;
        assert!(
            line_span >= 6,
            "occurrence {:?} covers only {} lines",
            occ.path,
            line_span
        );
    }
}

#[test]
fn output_lines_parseable_as_file_start_end() {
    // Acceptance criterion: CLI emits lines suitable for `| xargs -o nvim`,
    // which means `file:start-end`. We exercise the parse at the core level
    // against a well-formed occurrence so the invariant is mechanical.
    let fixture = workspace_root().join("fixtures").join("tier_a_basic");
    let result = Scanner::new(ScanConfig::default())
        .scan(&fixture)
        .expect("scan");

    for group in &result.groups {
        for occ in &group.occurrences {
            let line = format!(
                "{}:{}-{}",
                occ.path.display(),
                occ.span.start_line,
                occ.span.end_line
            );
            // path : start - end — the three fields round-trip.
            let (path_part, range_part) = line.rsplit_once(':').expect("has colon");
            let (start, end) = range_part.split_once('-').expect("has hyphen");
            assert!(!path_part.is_empty());
            let s: usize = start.parse().unwrap();
            let e: usize = end.parse().unwrap();
            assert!(s <= e);
        }
    }
}
