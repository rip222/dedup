//! Integration test for the sidebar tier A/B badge (#61).
//!
//! Strategy:
//!
//! 1. Build a tempdir with two fixtures that together produce a mixed
//!    scan: one Tier A group (byte-level copy-paste where neither side
//!    lines up with a Tier B syntactic unit — the Tier A→B promotion
//!    rule bails out) and one Tier B group (two identical functions
//!    that *do* line up with a Tier B unit, so the promotion rule
//!    fires and the result comes back tagged `Tier::B`).
//! 2. Run the core scanner with a cache root under the fixture's own
//!    `.dedup/` dir, persist via `Cache::write_scan_result`.
//! 3. Call `dedup_gui::load_folder` — the same entry point the GUI
//!    uses when the user picks `File → Open…`.
//! 4. Assert the materialized sidebar view carries both a Tier::A and
//!    a Tier::B `GroupView`, and that `render_tier_badge`'s label /
//!    tooltip helpers pin the expected glyph + hover copy for each.
//!
//! The rendered `Stateful<Div>` itself can't be asserted without
//! spinning up GPUI (which requires a main-thread runloop), so the
//! helpers are exposed as pure fns and the test exercises them
//! alongside a real scan — confirming (a) the pipeline still produces
//! mixed tiers end-to-end and (b) the rendering code path consumes
//! the right per-tier inputs.

#![cfg(target_os = "macos")]

use std::path::Path;

use dedup_core::{Cache, ScanConfig, Scanner, Tier};
use dedup_gui::load_folder;
use dedup_gui::project_view::{render_tier_badge, tier_badge_label, tier_badge_tooltip};
use dedup_gui::{AppStatus, FolderLoadResult};

/// Large duplicated function — clears Tier A's 50-token window AND the
/// Tier B syntactic-unit thresholds, so the Tier A span lines up with
/// a Tier B unit and the promotion rule fires. Exact copy of the body
/// used by `crates/dedup-core/tests/tier_b_promotion.rs` — reusing it
/// keeps the fixture grounded in a known-good Tier B trigger.
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

/// Tier A-only fixture: a large block of repeated top-level
/// declarations whose rolling-hash window does NOT align with any
/// single Tier B unit, so promotion leaves the Tier A group alone.
/// The block is a long sequence of `const` declarations — tree-sitter
/// sees them as many small units (none individually duplicated), but
/// the rolling hash over the whole slab finds the byte-level match.
fn tier_a_block() -> String {
    let mut s = String::new();
    for i in 0..60 {
        s.push_str(&format!("pub const TIER_A_CONST_{i}: i32 = {i};\n"));
    }
    s
}

fn write(dir: &Path, name: &str, body: &str) {
    std::fs::write(dir.join(name), body).unwrap();
}

#[test]
fn sidebar_carries_both_tiers_and_badge_helpers_match() {
    let tmp = tempfile::tempdir().unwrap();

    // Tier B pair — identical function bodies, nothing else in the
    // file so the Tier A window aligns exactly with the Tier B unit
    // and promotion fires.
    write(tmp.path(), "b_one.rs", DUP_FN);
    write(tmp.path(), "b_two.rs", DUP_FN);

    // Tier A pair — identical slab of const declarations. Each
    // individual const is too small to be a Tier B unit, but the
    // 60-line block easily clears Tier A's token / line thresholds.
    let a_body = tier_a_block();
    write(tmp.path(), "a_one.rs", &a_body);
    write(tmp.path(), "a_two.rs", &a_body);

    // `Cache::open` appends `.dedup/` to the repo root itself and
    // `load_folder` calls `Cache::open_readonly(folder)` — so the
    // scanner's `cache_root` must be the fixture root (NOT `.dedup`)
    // for both sides to agree on the cache location.
    let cfg = ScanConfig {
        cache_root: Some(tmp.path().to_path_buf()),
        ..ScanConfig::default()
    };
    let scanner = Scanner::new(cfg);
    let result = scanner.scan(tmp.path()).expect("scan");

    // Persist match groups — the warm-cache file tables are written by
    // `scanner` itself, but `match_groups` / `occurrences` rows land
    // here and that's what `load_folder` reads.
    {
        let mut cache = Cache::open(tmp.path()).expect("open cache");
        cache
            .write_scan_result(&result)
            .expect("write_scan_result");
    }

    // The scan must have produced at least one of each tier — the
    // whole premise of the badge is mixed-tier output.
    let n_a = result.groups.iter().filter(|g| g.tier == Tier::A).count();
    let n_b = result.groups.iter().filter(|g| g.tier == Tier::B).count();
    assert!(
        n_a >= 1 && n_b >= 1,
        "fixture must yield a mixed Tier A + Tier B scan; got A={n_a} B={n_b}, groups={:#?}",
        result.groups
    );

    let FolderLoadResult {
        groups,
        status,
        ..
    } = load_folder(tmp.path());

    assert!(
        matches!(status, AppStatus::Loaded),
        "expected AppStatus::Loaded, got {status:?}"
    );

    let gui_a = groups.iter().filter(|g| g.tier == Tier::A).count();
    let gui_b = groups.iter().filter(|g| g.tier == Tier::B).count();
    assert!(
        gui_a >= 1,
        "GroupView list must carry at least one Tier::A row (badge 'A' path), got {gui_a}; groups={groups:#?}"
    );
    assert!(
        gui_b >= 1,
        "GroupView list must carry at least one Tier::B row (badge 'B' path), got {gui_b}; groups={groups:#?}"
    );

    // Pin the badge glyph + tooltip copy. The render helper returns a
    // `Stateful<Div>` whose child / tooltip wiring we can't introspect
    // without a GPUI runtime, so we test the fn-shaped inputs that
    // feed that wiring directly.
    assert_eq!(tier_badge_label(Tier::A), "A");
    assert_eq!(tier_badge_label(Tier::B), "B");
    assert_eq!(tier_badge_tooltip(Tier::A), "Tier A \u{2014} byte-level match");
    assert_eq!(
        tier_badge_tooltip(Tier::B),
        "Tier B \u{2014} AST match, alpha-renamed locals may differ"
    );

    // Smoke-call the render fn for both tiers so a future refactor
    // that (say) panics on `ElementId` construction fails here. The
    // returned `Stateful<Div>` is discarded — we only want the
    // constructor path to complete.
    let _ = render_tier_badge(Tier::A, "group", 0xdead_beef);
    let _ = render_tier_badge(Tier::B, "dismissed", 0xcafe_f00d);
}
