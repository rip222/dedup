//! Output rendering for the dedup CLI (issue #12).
//!
//! Every user-visible stdout stream flows through this module. There are
//! three formats:
//!
//! - [`OutputFormat::Text`] — the legacy human-readable layout. Kept
//!   byte-for-byte identical to the pre-#12 output.
//! - [`OutputFormat::Json`] — structured JSON. For `list` / `scan` this
//!   is NDJSON (one group per line, streamable via `jq`). For `show` it
//!   is a single JSON object. For `suppressions list` it is NDJSON (one
//!   suppression per line).
//! - [`OutputFormat::Sarif`] — SARIF 2.1.0. Consumable by GitHub Code
//!   Scanning; each duplicate group becomes one `results[]` entry whose
//!   `locations[]` list the block's occurrences.
//!
//! # TTY auto-selection
//!
//! When the user did not pass `--format`, [`resolve_format`] picks
//! `Text` on a TTY and `Json` when stdout is piped / redirected. The
//! `--format` flag overrides this in both directions.
//!
//! # Stable data shapes
//!
//! The JSON representation of a group is deliberately kept small and
//! self-describing ([`GroupJson`]). It is used both for NDJSON rows and
//! as the body of `show`'s object-shaped output so downstream consumers
//! only have to learn one shape.

use std::io::{self, IsTerminal, Write};
use std::path::Path;

use clap::ValueEnum;
use dedup_core::{GroupDetail, MatchGroup, Suppression, Tier};
use serde::Serialize;

use crate::sarif;

/// Output format selector. `Text` is the pre-#12 legacy layout; `Json`
/// (NDJSON for lists, single object for `show`) and `Sarif` are the
/// structured formats added in #12.
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    /// Human-readable layout (current default on a TTY).
    Text,
    /// JSON output. NDJSON for the list/scan/suppressions commands;
    /// a single JSON object for `show`.
    Json,
    /// SARIF 2.1.0 report (one result per duplicate group). `scan` is
    /// the canonical target; other commands fall back to `Text`.
    Sarif,
}

/// Resolve the concrete format to use, given whether the user passed
/// `--format` and whether stdout is a TTY.
///
/// - Explicit `--format X` wins (forces `X` in both directions).
/// - Otherwise, non-TTY → `Json`, TTY → `Text`.
pub fn resolve_format(requested: Option<OutputFormat>, stdout_is_tty: bool) -> OutputFormat {
    match requested {
        Some(f) => f,
        None => {
            if stdout_is_tty {
                OutputFormat::Text
            } else {
                OutputFormat::Json
            }
        }
    }
}

/// Convenience wrapper that asks [`io::stdout`] whether it is a
/// terminal. Split out so tests can bypass TTY detection by calling
/// [`resolve_format`] directly.
pub fn stdout_is_tty() -> bool {
    io::stdout().is_terminal()
}

// ---------------------------------------------------------------------------
// JSON data shapes — stable contract consumed by downstream tooling.
// ---------------------------------------------------------------------------

/// One occurrence of a duplicate block. The `start_byte` / `end_byte`
/// fields are optional because not every source site (e.g. `scan`'s
/// live groups) tracks byte offsets yet — callers may emit them when
/// available.
#[derive(Debug, Serialize)]
pub struct OccurrenceJson<'a> {
    /// Forward-slashed path relative to the scan root.
    pub path: String,
    /// 1-based inclusive start line.
    pub start_line: u64,
    /// 1-based inclusive end line.
    pub end_line: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_byte: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_byte: Option<u64>,
    // Lifetime phantom: keeps a single-shape impl when we eventually
    // cache string views.
    #[serde(skip)]
    pub _borrowed: std::marker::PhantomData<&'a ()>,
}

/// One duplicate group, serializable as a compact JSON object. The
/// `id` field is `Some` only for cache-backed groups (`list` / `show`
/// / cached `scan` rehydration); live in-memory groups from `scan` emit
/// `None` to signal "cache id not yet assigned".
#[derive(Debug, Serialize)]
pub struct GroupJson<'a> {
    /// Persisted group id (1-based). `None` for live scan output that
    /// has not yet been assigned a cache row.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<i64>,
    /// 1-based ordinal within the current result set. Matches the
    /// text-format header number.
    pub ordinal: usize,
    /// `"A"` or `"B"`.
    pub tier: &'static str,
    /// Representative 64-bit normalized-block-hash, as a zero-padded
    /// 16-char lowercase hex string. Stable across runs for a given
    /// block.
    pub hash: String,
    /// Number of occurrences in the group (always ≥ 2).
    pub occurrence_count: u64,
    pub occurrences: Vec<OccurrenceJson<'a>>,
}

/// JSON shape returned by `dedup suppressions list` in JSON / NDJSON
/// mode. Minimal and stable.
#[derive(Debug, Serialize)]
pub struct SuppressionJson {
    /// Hex-encoded normalized-block-hash (16 chars, lowercase).
    pub hash: String,
    /// Unix-epoch seconds at dismissal time.
    pub dismissed_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_group_id: Option<i64>,
}

// ---------------------------------------------------------------------------
// Conversions from core types to the stable JSON shapes.
// ---------------------------------------------------------------------------

/// Forward-slash a path for cross-platform stable output.
fn path_display(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

fn tier_label(t: Tier) -> &'static str {
    match t {
        Tier::A => "A",
        Tier::B => "B",
    }
}

pub fn match_group_to_json<'a>(ordinal: usize, g: &'a MatchGroup) -> GroupJson<'a> {
    GroupJson {
        id: None,
        ordinal,
        tier: tier_label(g.tier),
        hash: format!("{:016x}", g.hash),
        occurrence_count: g.occurrences.len() as u64,
        occurrences: g
            .occurrences
            .iter()
            .map(|o| OccurrenceJson {
                path: path_display(&o.path),
                start_line: o.span.start_line as u64,
                end_line: o.span.end_line as u64,
                start_byte: Some(o.span.start_byte as u64),
                end_byte: Some(o.span.end_byte as u64),
                _borrowed: std::marker::PhantomData,
            })
            .collect(),
    }
}

pub fn group_detail_to_json<'a>(
    ordinal: usize,
    hash: Option<u64>,
    d: &'a GroupDetail,
) -> GroupJson<'a> {
    GroupJson {
        id: Some(d.id),
        ordinal,
        tier: tier_label(d.tier),
        hash: hash.map(|h| format!("{h:016x}")).unwrap_or_default(),
        occurrence_count: d.occurrence_count as u64,
        occurrences: d
            .occurrences
            .iter()
            .map(|o| OccurrenceJson {
                path: path_display(&o.path),
                start_line: o.start_line as u64,
                end_line: o.end_line as u64,
                start_byte: Some(o.start_byte as u64),
                end_byte: Some(o.end_byte as u64),
                _borrowed: std::marker::PhantomData,
            })
            .collect(),
    }
}

pub fn suppression_to_json(s: &Suppression) -> SuppressionJson {
    SuppressionJson {
        hash: format!("{:016x}", s.hash),
        dismissed_at: s.dismissed_at,
        last_group_id: s.last_group_id,
    }
}

// ---------------------------------------------------------------------------
// Writers.
// ---------------------------------------------------------------------------

/// Write one NDJSON line per group from a slice of `MatchGroup` refs.
///
/// Each line is a standalone JSON object terminated by `\n`, making the
/// stream trivially consumable by `jq --stream` / `jq -c '.'` / `split`.
pub fn write_groups_ndjson<W: Write>(groups: &[&MatchGroup], out: &mut W) -> io::Result<()> {
    for (i, g) in groups.iter().enumerate() {
        let j = match_group_to_json(i + 1, g);
        serde_json::to_writer(&mut *out, &j)?;
        out.write_all(b"\n")?;
    }
    Ok(())
}

/// Write a single NDJSON line for one cached group, using the supplied
/// `ordinal` rather than re-deriving it from slice position. Used by
/// the streaming `dedup list` loop.
pub fn write_cached_group_ndjson_line<W: Write>(
    ordinal: usize,
    detail: &GroupDetail,
    hash: Option<u64>,
    out: &mut W,
) -> io::Result<()> {
    let j = group_detail_to_json(ordinal, hash, detail);
    serde_json::to_writer(&mut *out, &j)?;
    out.write_all(b"\n")?;
    Ok(())
}

/// Write a single JSON object for `dedup show`. Emits the same
/// [`GroupJson`] shape as one NDJSON row from `list`, just without the
/// trailing newline semantics — one object, pretty-printed-safe-but-
/// compact for machine consumption.
pub fn write_group_object<W: Write>(
    detail: &GroupDetail,
    hash: Option<u64>,
    out: &mut W,
) -> io::Result<()> {
    let j = group_detail_to_json(1, hash, detail);
    serde_json::to_writer(&mut *out, &j)?;
    out.write_all(b"\n")?;
    Ok(())
}

/// Write an NDJSON stream of suppressions.
pub fn write_suppressions_ndjson<W: Write>(entries: &[Suppression], out: &mut W) -> io::Result<()> {
    for s in entries {
        let j = suppression_to_json(s);
        serde_json::to_writer(&mut *out, &j)?;
        out.write_all(b"\n")?;
    }
    Ok(())
}

/// Write a SARIF 2.1.0 report covering the supplied groups. Wraps
/// [`sarif::build_sarif`] with a single JSON-encode + newline write.
pub fn write_groups_sarif<W: Write>(groups: &[&MatchGroup], out: &mut W) -> io::Result<()> {
    let report = sarif::build_sarif(groups);
    serde_json::to_writer(&mut *out, &report)?;
    out.write_all(b"\n")?;
    Ok(())
}

/// SARIF writer sourced from cached `GroupDetail` rows (for `list`).
pub fn write_cached_groups_sarif<W: Write>(
    details: &[(GroupDetail, Option<u64>)],
    out: &mut W,
) -> io::Result<()> {
    let report = sarif::build_sarif_from_details(details);
    serde_json::to_writer(&mut *out, &report)?;
    out.write_all(b"\n")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Text writers — byte-for-byte compatible with the pre-#12 layout.
// ---------------------------------------------------------------------------

pub fn write_groups_text<W: Write>(groups: &[&MatchGroup], out: &mut W) -> io::Result<()> {
    for (i, group) in groups.iter().enumerate() {
        writeln!(
            out,
            "--- [{}] group {} ({} occurrences) ---",
            group.tier.label(),
            i + 1,
            group.occurrences.len()
        )?;
        for occ in &group.occurrences {
            let path = path_display(&occ.path);
            writeln!(
                out,
                "{}:{}-{}",
                path, occ.span.start_line, occ.span.end_line
            )?;
        }
    }
    Ok(())
}

pub fn write_cached_group_text<W: Write>(
    ordinal: usize,
    detail: &GroupDetail,
    out: &mut W,
) -> io::Result<()> {
    writeln!(
        out,
        "--- [{}] group {} ({} occurrences) ---",
        tier_label(detail.tier),
        ordinal,
        detail.occurrence_count
    )?;
    for occ in &detail.occurrences {
        let path = path_display(&occ.path);
        writeln!(out, "{}:{}-{}", path, occ.start_line, occ.end_line)?;
    }
    Ok(())
}

pub fn write_show_text<W: Write>(detail: &GroupDetail, out: &mut W) -> io::Result<()> {
    writeln!(
        out,
        "--- [{}] group {} ({} occurrences) ---",
        tier_label(detail.tier),
        detail.id,
        detail.occurrence_count
    )?;
    for occ in &detail.occurrences {
        let path = path_display(&occ.path);
        writeln!(out, "{}:{}-{}", path, occ.start_line, occ.end_line)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_uses_explicit_override_on_tty() {
        let f = resolve_format(Some(OutputFormat::Json), true);
        assert_eq!(f, OutputFormat::Json);
    }

    #[test]
    fn resolve_uses_explicit_override_off_tty() {
        let f = resolve_format(Some(OutputFormat::Text), false);
        assert_eq!(f, OutputFormat::Text);
    }

    #[test]
    fn resolve_defaults_text_on_tty() {
        let f = resolve_format(None, true);
        assert_eq!(f, OutputFormat::Text);
    }

    #[test]
    fn resolve_defaults_json_off_tty() {
        let f = resolve_format(None, false);
        assert_eq!(f, OutputFormat::Json);
    }
}
