//! SARIF 2.1.0 report emission for the dedup CLI (issue #12).
//!
//! The report is hand-built with `serde_json::Value` — the schema is
//! small enough that pulling in `serde-sarif` is unjustified. Output
//! validates against the SARIF 2.1.0 JSON Schema (see
//! `tests/fixtures/sarif-2.1.0.json` for the vendored copy used in tests).
//!
//! # Shape
//!
//! One `runs[0].tool.driver` describes dedup; `runs[0].results[]`
//! contains one entry per duplicate group. Each result:
//!
//! - `ruleId = "duplicate-block"`
//! - `level = "note"` (informational; dedup findings don't block CI)
//! - `message.text` describes the group (`"Duplicate block (N
//!   occurrences, Tier A|B)"`)
//! - `partialFingerprints["groupHash/v1"]` carries the stable
//!   normalized-block-hash so consumers can dedupe across runs.
//! - `locations[]` holds every occurrence's `physicalLocation` with
//!   `artifactLocation.uri` + `region.startLine` / `endLine`.

use dedup_core::{GroupDetail, MatchGroup, Tier};
use serde_json::{Value, json};

fn tier_label(t: Tier) -> &'static str {
    match t {
        Tier::A => "A",
        Tier::B => "B",
    }
}

fn path_display(p: &std::path::Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

fn physical_location(uri: &str, start_line: u64, end_line: u64) -> Value {
    // SARIF requires startLine >= 1. Clamp defensively so a zero-lined
    // span (should never happen for dedup, but the schema is strict)
    // still validates.
    let start = start_line.max(1);
    let end = end_line.max(start);
    json!({
        "physicalLocation": {
            "artifactLocation": { "uri": uri },
            "region": {
                "startLine": start,
                "endLine": end,
            }
        }
    })
}

fn result_for(hash_hex: &str, tier: &str, occurrence_count: u64, locations: Vec<Value>) -> Value {
    json!({
        "ruleId": "duplicate-block",
        "level": "note",
        "message": {
            "text": format!(
                "Duplicate block ({occurrence_count} occurrences, Tier {tier})"
            ),
        },
        "partialFingerprints": {
            "groupHash/v1": hash_hex,
        },
        "locations": locations,
    })
}

fn driver() -> Value {
    json!({
        "name": "dedup",
        "informationUri": env!("CARGO_PKG_REPOSITORY"),
        "version": env!("CARGO_PKG_VERSION"),
        "rules": [{
            "id": "duplicate-block",
            "name": "DuplicateBlock",
            "shortDescription": {
                "text": "Duplicate block detected",
            },
            "fullDescription": {
                "text": "A block of source code matches one or more other blocks in the repository.",
            },
            "defaultConfiguration": { "level": "note" },
            "helpUri": env!("CARGO_PKG_REPOSITORY"),
        }],
    })
}

/// Build a SARIF 2.1.0 report from live `MatchGroup` references.
pub fn build_sarif(groups: &[&MatchGroup]) -> Value {
    let results: Vec<Value> = groups
        .iter()
        .map(|g| {
            let locations: Vec<Value> = g
                .occurrences
                .iter()
                .map(|o| {
                    physical_location(
                        &path_display(&o.path),
                        o.span.start_line as u64,
                        o.span.end_line as u64,
                    )
                })
                .collect();
            result_for(
                &format!("{:016x}", g.hash),
                tier_label(g.tier),
                g.occurrences.len() as u64,
                locations,
            )
        })
        .collect();

    sarif_envelope(results)
}

/// Build a SARIF 2.1.0 report from cached `GroupDetail` rows.
///
/// `details[i].1` is the optional stable hash; cached rows usually
/// provide one (via `Cache::group_hash`), but `None` is tolerated —
/// the fingerprint just falls back to the group's decimal id.
pub fn build_sarif_from_details(details: &[(GroupDetail, Option<u64>)]) -> Value {
    let results: Vec<Value> = details
        .iter()
        .map(|(d, h)| {
            let locations: Vec<Value> = d
                .occurrences
                .iter()
                .map(|o| {
                    physical_location(
                        &path_display(&o.path),
                        o.start_line as u64,
                        o.end_line as u64,
                    )
                })
                .collect();
            let hash_hex = h
                .map(|h| format!("{h:016x}"))
                .unwrap_or_else(|| format!("id-{}", d.id));
            result_for(
                &hash_hex,
                tier_label(d.tier),
                d.occurrence_count as u64,
                locations,
            )
        })
        .collect();

    sarif_envelope(results)
}

fn sarif_envelope(results: Vec<Value>) -> Value {
    json!({
        "$schema": "https://raw.githubusercontent.com/oasis-tcs/sarif-spec/master/Schemata/sarif-schema-2.1.0.json",
        "version": "2.1.0",
        "runs": [{
            "tool": { "driver": driver() },
            "results": results,
        }],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_has_required_fields() {
        let v = build_sarif(&[]);
        assert_eq!(v["version"], "2.1.0");
        assert_eq!(v["runs"][0]["tool"]["driver"]["name"], "dedup");
        assert_eq!(v["runs"][0]["results"].as_array().unwrap().len(), 0);
    }
}
