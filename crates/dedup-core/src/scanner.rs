//! Tier A scanner: walks files, rolls hashes, buckets match groups.
//!
//! The scanner is the top-level orchestrator for this milestone. It:
//!
//! 1. Walks `root` with [`walkdir`], skipping `.git/` directories and binary
//!    files (content-sniffed via the first 512 bytes).
//! 2. Reads each file as UTF-8 (silently skips decode failures — the PRD
//!    says skip at debug level; logging lands in #16).
//! 3. Tokenizes, then runs a fixed-window rolling hash with window size 50
//!    (the Tier A `min_tokens` default).
//! 4. Buckets windows by hash across all files, keeping only buckets with
//!    ≥ 2 members after collection.
//! 5. Greedily extends adjacent matching windows in each file into maximal
//!    spans, then filters by the `min_lines` / `min_tokens` thresholds.
//! 6. Emits [`MatchGroup`]s — one per cluster of equivalent extended spans
//!    across files — suitable for the CLI to print or for later issues
//!    (#4 cache, #12 formats) to consume.
//!
//! What this scanner deliberately does NOT do at this milestone:
//!
//! - No cache (lands in #4).
//! - No `ignore`-crate layers (lands in #5): we honor only the hard-coded
//!   binary + `.git/` skip.
//! - No parallelism (lands in #14).
//! - No tree-sitter Tier B (lands in #6).
//! - No typed-error surfacing for per-file I/O failures: they are silently
//!   skipped. `ScanError` exists for top-level catastrophic failures
//!   (none today, but the surface is ready for #17).

use crate::rolling_hash::{Hash, Span, rolling_hash};
use crate::tokenizer::{Token, tokenize};
use rustc_hash::FxHashMap;
use std::path::{Path, PathBuf};
use thiserror::Error;
use walkdir::WalkDir;

/// The rolling-hash window size. Matches the Tier A `min_tokens` default.
const WINDOW_SIZE: usize = 50;

/// Number of leading bytes inspected for binary content-sniff.
const BINARY_SNIFF_BYTES: usize = 512;

/// Errors the scanner can emit. All per-file errors today are silently
/// skipped; this enum exists so the API surface is stable as later issues
/// add real failure modes.
#[derive(Debug, Error)]
pub enum ScanError {
    /// The root path does not exist or cannot be walked.
    #[error("failed to walk {path}: {source}")]
    Walk {
        path: PathBuf,
        #[source]
        source: walkdir::Error,
    },
}

/// Tunable scanner knobs. Defaults mirror the Tier A thresholds from the PRD.
#[derive(Debug, Clone)]
pub struct ScanConfig {
    /// A match span must cover at least this many lines to be reported.
    pub tier_a_min_lines: usize,
    /// A match span must cover at least this many tokens to be reported.
    pub tier_a_min_tokens: usize,
}

impl Default for ScanConfig {
    fn default() -> Self {
        Self {
            tier_a_min_lines: 6,
            tier_a_min_tokens: 50,
        }
    }
}

/// One file-local occurrence of a duplicate block.
#[derive(Debug, Clone)]
pub struct Occurrence {
    /// Path relative to the scan root (always uses forward slashes on all
    /// platforms so snapshot output is cross-platform-stable).
    pub path: PathBuf,
    /// Source region covered.
    pub span: Span,
}

/// A cluster of file-local occurrences that share the same canonical
/// token stream.
#[derive(Debug, Clone)]
pub struct MatchGroup {
    /// Representative hash for the extended span (the hash of the first
    /// rolling window in the span; good enough for grouping at Tier A).
    pub hash: Hash,
    /// All file-local occurrences; always `len() >= 2` after filtering.
    pub occurrences: Vec<Occurrence>,
}

/// Result of a single [`Scanner::scan`] call.
#[derive(Debug, Clone)]
pub struct ScanResult {
    /// Confirmed match groups, sorted for deterministic output:
    /// by first occurrence path, then start line.
    pub groups: Vec<MatchGroup>,
    /// Count of files that were actually tokenized (binary + `.git/` skipped
    /// files do not count).
    pub files_scanned: usize,
}

/// Callback surface for reporting scan progress.
///
/// The scanner calls [`ProgressSink::on_file_processed`] after it finishes
/// tokenizing a file (binary / `.git/` / decode-skipped files are NOT
/// reported), and [`ProgressSink::on_match_group`] once per confirmed
/// group at the end of the scan. A silent default sink (`NoopSink`)
/// exists so callers that don't need progress can use [`Scanner::scan`]
/// directly.
///
/// Implementors are expected to be cheap — the file callback runs inline
/// on the scanner's hot path. The trait is object-safe so the CLI can
/// hand in an `indicatif`-backed sink via `&dyn ProgressSink` without
/// leaking `indicatif` into `dedup-core`.
pub trait ProgressSink {
    /// Called once per file that actually gets tokenized. The path is
    /// absolute (matches the walker's view, not the `Occurrence` path).
    fn on_file_processed(&self, path: &Path);
    /// Called once per confirmed match group, after all filtering.
    fn on_match_group(&self, group: &MatchGroup);
}

/// No-op progress sink. Used by [`Scanner::scan`] so the default path has
/// zero overhead.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopSink;

impl ProgressSink for NoopSink {
    fn on_file_processed(&self, _path: &Path) {}
    fn on_match_group(&self, _group: &MatchGroup) {}
}

/// Public scanner handle. Cheap to construct; holds only configuration.
#[derive(Debug, Clone, Default)]
pub struct Scanner {
    config: ScanConfig,
}

impl Scanner {
    /// Build a scanner with explicit configuration.
    pub fn new(config: ScanConfig) -> Self {
        Self { config }
    }

    /// Walk `root` and return all Tier A match groups. Convenience wrapper
    /// around [`Scanner::scan_with_progress`] with a no-op progress sink.
    pub fn scan(&self, root: &Path) -> Result<ScanResult, ScanError> {
        self.scan_with_progress(root, &NoopSink)
    }

    /// Walk `root` and return all Tier A match groups, reporting progress
    /// through `sink`.
    ///
    /// `sink` is called:
    ///
    /// - once per tokenized file (via `on_file_processed`), on the hot path;
    /// - once per confirmed match group (via `on_match_group`), after
    ///   clustering / filtering / sorting.
    ///
    /// Progress callbacks are advisory — the scanner does not re-enter
    /// them and makes no guarantees about ordering beyond "files first,
    /// groups at the end."
    pub fn scan_with_progress(
        &self,
        root: &Path,
        sink: &dyn ProgressSink,
    ) -> Result<ScanResult, ScanError> {
        // --- 1. Walk and tokenize each candidate file. --------------------
        let mut per_file: Vec<(PathBuf, Vec<Token>)> = Vec::new();

        for entry in WalkDir::new(root).into_iter().filter_entry(|e| {
            // Skip `.git/` directories wholesale.
            !(e.file_type().is_dir() && e.file_name() == ".git")
        }) {
            let entry = match entry {
                Ok(e) => e,
                // Tolerate per-entry walk errors (permission denied on a
                // sibling, symlink loop, ...) so one bad file doesn't kill
                // the scan. Logging lives in #16.
                Err(_) => continue,
            };
            if !entry.file_type().is_file() {
                continue;
            }

            let abs = entry.path();
            let bytes = match std::fs::read(abs) {
                Ok(b) => b,
                Err(_) => continue,
            };
            if looks_binary(&bytes) {
                continue;
            }
            let text = match std::str::from_utf8(&bytes) {
                Ok(s) => s.to_string(),
                Err(_) => continue,
            };

            let rel = abs.strip_prefix(root).unwrap_or(abs).to_path_buf();
            per_file.push((rel, tokenize(&text)));
            // Report progress *after* the file is staged so sinks that
            // update a spinner see work that has actually been done.
            sink.on_file_processed(abs);
        }

        let files_scanned = per_file.len();

        // --- 2. Roll hashes per file and index them. ----------------------
        // Each window is uniquely identified by (file_index, window_start).
        #[derive(Clone, Copy)]
        struct WindowKey {
            file: usize,
            win: usize,
        }

        let mut by_hash: FxHashMap<Hash, Vec<WindowKey>> = FxHashMap::default();
        let mut per_file_hashes: Vec<Vec<(Hash, Span)>> = Vec::with_capacity(per_file.len());

        for (fi, (_path, tokens)) in per_file.iter().enumerate() {
            let windows = rolling_hash(tokens, WINDOW_SIZE);
            for (wi, (h, _)) in windows.iter().enumerate() {
                by_hash
                    .entry(*h)
                    .or_default()
                    .push(WindowKey { file: fi, win: wi });
            }
            per_file_hashes.push(windows);
        }

        // --- 3. Drop singleton buckets. ----------------------------------
        by_hash.retain(|_, v| v.len() >= 2);

        // --- 4. Greedy extension.
        //
        // A "seed" is any window whose hash has ≥ 2 matching windows across
        // the corpus. For each file, we walk its windows in order and grow
        // runs where every window in the run has the *same* set of
        // counterpart (file, win) keys. Two seed windows `i` and `i+1` can
        // merge only if sliding both forward stays aligned in every peer.
        //
        // This is a simple O(F * W) pass over windows plus O(peer_count)
        // per extension step. It is correct for the common case
        // (copy-pasted blocks with identical token surroundings); blocks
        // that match only partially end up as multiple shorter groups,
        // which is acceptable at Tier A.
        #[derive(Debug, Clone)]
        struct ExtendedSpan {
            start_win: usize,
            end_win: usize, // inclusive
            hash: Hash,
        }

        let mut consumed: Vec<Vec<bool>> = per_file_hashes
            .iter()
            .map(|h| vec![false; h.len()])
            .collect();
        let mut extended: Vec<Vec<ExtendedSpan>> =
            (0..per_file.len()).map(|_| Vec::new()).collect();

        for fi in 0..per_file.len() {
            let windows = &per_file_hashes[fi];
            let mut wi = 0usize;
            while wi < windows.len() {
                if consumed[fi][wi] {
                    wi += 1;
                    continue;
                }
                let h = windows[wi].0;
                let peers = match by_hash.get(&h) {
                    Some(p) if p.len() >= 2 => p,
                    _ => {
                        wi += 1;
                        continue;
                    }
                };

                // Peers aligned at offset 0 (all `WindowKey`s matching the
                // seed hash, *including* ourselves). We track a cursor per
                // peer so we can check if the peer also has a matching
                // continuation at each extension step.
                let start = wi;
                let mut end = wi;
                let seed_peers: Vec<WindowKey> = peers.clone();

                'outer: loop {
                    let next = end + 1;
                    if next >= windows.len() {
                        break;
                    }
                    let next_hash = windows[next].0;
                    // Every seed peer must also extend to the next hash in
                    // its own file, at the next window offset, AND those
                    // extensions must all share the same hash `next_hash`.
                    for peer in &seed_peers {
                        let peer_windows = &per_file_hashes[peer.file];
                        let peer_next = peer.win + (next - start);
                        if peer_next >= peer_windows.len() {
                            break 'outer;
                        }
                        if peer_windows[peer_next].0 != next_hash {
                            break 'outer;
                        }
                    }
                    end = next;
                }

                // Record extended spans for the whole cluster so every
                // peer's windows are marked consumed. We only push the
                // span for the *current* file here; peers are pushed when
                // their own outer-loop iteration reaches them. That would
                // normally double-emit, so we guard via `consumed`.
                let cluster_hash = h;
                for peer in &seed_peers {
                    let plen = end - start;
                    let pe_start = peer.win;
                    let pe_end = peer.win + plen;
                    // Bounds-guard: the alignment check above guarantees
                    // `pe_end < per_file_hashes[peer.file].len()`.
                    for slot in consumed[peer.file][pe_start..=pe_end].iter_mut() {
                        *slot = true;
                    }
                    extended[peer.file].push(ExtendedSpan {
                        start_win: pe_start,
                        end_win: pe_end,
                        hash: cluster_hash,
                    });
                }

                wi = end + 1;
            }
        }

        // --- 5. Group extended spans back together by cluster hash. ------
        // Two extended spans are in the same cluster iff they share the
        // seed hash *and* the same length. (Different seed hashes can
        // theoretically extend to the same material, but at Tier A we
        // trust the hash as a cluster key.)
        let mut clusters: FxHashMap<(Hash, usize), Vec<Occurrence>> = FxHashMap::default();
        for (fi, spans) in extended.iter().enumerate() {
            for s in spans {
                let (path, tokens) = &per_file[fi];
                let windows = &per_file_hashes[fi];
                let first_span = windows[s.start_win].1;
                let last_span = windows[s.end_win].1;
                let combined = Span {
                    start_line: first_span.start_line,
                    end_line: last_span.end_line,
                    start_byte: first_span.start_byte,
                    end_byte: last_span.end_byte,
                };

                let tokens_covered =
                    token_count_between(tokens, combined.start_byte, combined.end_byte);
                let lines_covered = combined.end_line.saturating_sub(combined.start_line) + 1;

                if lines_covered < self.config.tier_a_min_lines {
                    continue;
                }
                if tokens_covered < self.config.tier_a_min_tokens {
                    continue;
                }

                let key = (s.hash, s.end_win - s.start_win);
                clusters.entry(key).or_default().push(Occurrence {
                    path: path.clone(),
                    span: combined,
                });
            }
        }

        // --- 6. Finalize: keep only ≥ 2-occurrence clusters and sort. ----
        let mut groups: Vec<MatchGroup> = clusters
            .into_iter()
            .filter(|(_, occ)| occ.len() >= 2)
            .map(|((hash, _), mut occ)| {
                occ.sort_by(|a, b| {
                    a.path
                        .cmp(&b.path)
                        .then(a.span.start_line.cmp(&b.span.start_line))
                });
                MatchGroup {
                    hash,
                    occurrences: occ,
                }
            })
            .collect();

        groups.sort_by(|a, b| {
            let ap = &a.occurrences[0];
            let bp = &b.occurrences[0];
            ap.path
                .cmp(&bp.path)
                .then(ap.span.start_line.cmp(&bp.span.start_line))
        });

        // Replay groups through the progress sink so the CLI can flush a
        // final match-count before returning. Doing this after the sort
        // means the sink observes groups in their final, user-visible
        // order.
        for g in &groups {
            sink.on_match_group(g);
        }

        Ok(ScanResult {
            groups,
            files_scanned,
        })
    }
}

/// Count how many tokens fall within `[start_byte, end_byte)`.
///
/// Tokens are stored in source order, so this is a simple linear scan; it
/// runs once per candidate match group, not per window.
fn token_count_between(tokens: &[Token], start_byte: usize, end_byte: usize) -> usize {
    tokens
        .iter()
        .filter(|t| t.start >= start_byte && t.end <= end_byte)
        .count()
}

/// Content-sniff for binary files: look at the first [`BINARY_SNIFF_BYTES`]
/// bytes and treat the file as binary if any NUL byte is present. This is
/// the same heuristic `git` and `ripgrep` use for their "is binary?" check.
fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(BINARY_SNIFF_BYTES).any(|&b| b == 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn binary_sniff_detects_nul() {
        assert!(looks_binary(b"hello\0world"));
        assert!(!looks_binary(b"hello world"));
    }

    #[test]
    fn empty_directory_yields_no_groups() {
        let dir = tempdir().unwrap();
        let r = Scanner::default().scan(dir.path()).unwrap();
        assert_eq!(r.files_scanned, 0);
        assert!(r.groups.is_empty());
    }

    #[test]
    fn git_directory_is_skipped() {
        let dir = tempdir().unwrap();
        let git = dir.path().join(".git");
        fs::create_dir_all(&git).unwrap();
        fs::write(git.join("config"), "some git config").unwrap();
        fs::write(dir.path().join("visible.txt"), "hello").unwrap();

        let r = Scanner::default().scan(dir.path()).unwrap();
        assert_eq!(r.files_scanned, 1);
    }

    #[test]
    fn binary_files_are_skipped() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("binary.bin"), b"\0\0\0\0data").unwrap();
        fs::write(dir.path().join("text.txt"), "hello world").unwrap();

        let r = Scanner::default().scan(dir.path()).unwrap();
        assert_eq!(r.files_scanned, 1);
    }

    #[test]
    fn tiny_file_under_threshold_is_not_reported() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "x y z").unwrap();
        fs::write(dir.path().join("b.txt"), "x y z").unwrap();

        let r = Scanner::default().scan(dir.path()).unwrap();
        assert!(r.groups.is_empty());
    }

    /// A [`ProgressSink`] that just counts callback hits, protected by a
    /// `Mutex` so the trait methods can remain `&self`.
    #[derive(Default)]
    struct CountingSink {
        files: std::sync::Mutex<usize>,
        groups: std::sync::Mutex<usize>,
    }

    impl ProgressSink for CountingSink {
        fn on_file_processed(&self, _path: &Path) {
            *self.files.lock().unwrap() += 1;
        }
        fn on_match_group(&self, _group: &MatchGroup) {
            *self.groups.lock().unwrap() += 1;
        }
    }

    #[test]
    fn progress_sink_sees_every_file_and_group() {
        // Two files with enough duplicated tokens to clear the default
        // Tier A thresholds (≥ 6 lines, ≥ 50 tokens).
        let body: String = (0..60)
            .map(|i| format!("let x{i} = {i};\n"))
            .collect::<Vec<_>>()
            .join("");
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.rs"), &body).unwrap();
        fs::write(dir.path().join("b.rs"), &body).unwrap();

        let sink = CountingSink::default();
        let result = Scanner::default()
            .scan_with_progress(dir.path(), &sink)
            .unwrap();

        assert_eq!(*sink.files.lock().unwrap(), 2);
        assert_eq!(result.groups.len(), 1);
        assert_eq!(*sink.groups.lock().unwrap(), 1);
    }

    #[test]
    fn noop_sink_matches_scan_convenience() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "hello world").unwrap();

        let via_scan = Scanner::default().scan(dir.path()).unwrap();
        let via_progress = Scanner::default()
            .scan_with_progress(dir.path(), &NoopSink)
            .unwrap();
        assert_eq!(via_scan.files_scanned, via_progress.files_scanned);
        assert_eq!(via_scan.groups.len(), via_progress.groups.len());
    }
}
