//! Tier A + Tier B scanner: walks files, rolls hashes, buckets match groups.
//!
//! The scanner is the top-level orchestrator for this milestone. It:
//!
//! 1. Walks `root` with the [`ignore`] crate's `WalkBuilder` and filters
//!    each candidate through the four-layer [`IgnoreRules`] stack
//!    (binary sniff, size cap, `.git/`, gitignore stack, built-in
//!    defaults, `.dedupignore`).
//! 2. Reads each file as UTF-8 (silently skips decode failures — the PRD
//!    says skip at debug level; logging lands in #16).
//! 3. Tokenizes, then runs a fixed-window rolling hash with window size 50
//!    (the Tier A `min_tokens` default).
//! 4. Buckets windows by hash across all files, keeping only buckets with
//!    ≥ 2 members after collection.
//! 5. Greedily extends adjacent matching windows in each file into maximal
//!    spans, then filters by the `min_lines` / `min_tokens` thresholds.
//! 6. For any file whose extension matches a registered [`LanguageProfile`],
//!    also runs Tier B: parses with tree-sitter, extracts syntactic units
//!    (function / type / impl bodies), alpha-renames locals, subtree-hashes,
//!    and buckets those.
//! 7. Applies the **Tier A → B promotion rule**: a Tier A occurrence that
//!    exactly aligns (same file + same start/end lines) with a Tier B unit
//!    is dropped from its Tier A group so the duplicate is reported once,
//!    at the more semantic Tier B level. If dropping leaves a Tier A group
//!    with < 2 occurrences, the whole group is removed.
//! 8. Emits [`MatchGroup`]s tagged with [`Tier::A`] or [`Tier::B`] —
//!    suitable for the CLI to print or for later issues to consume.
//!
//! Parallelism (issue #14): the walk itself is serial (metadata-only and
//! cheap), but the read → tokenize → rolling-hash stage runs in a scoped
//! `rayon::ThreadPool`. Candidates are sorted by relative path before
//! fan-out so `file_index` is canonical no matter which worker finishes
//! first. `ScanConfig::jobs` caps pool width; `Some(1)` bypasses rayon
//! entirely. A warm-scan cache keyed by content hash + `(size, mtime)`
//! lets unchanged files skip the rolling-hash pass by rehydrating the
//! stored block-hash list out of SQLite.
//!
//! What this scanner deliberately does NOT do at this milestone:
//!
//! - No aggressive literal abstraction (lands in #10).
//! - No typed-error surfacing for per-file I/O or parse failures: they are
//!   silently skipped. `ScanError` exists for top-level catastrophic
//!   failures (none today, but the surface is ready for #17).

use crate::cache::{Cache, FileFingerprint};
use crate::ignore::{IgnoreRules, IgnoreRulesOptions};
use crate::rolling_hash::{Hash, Span, rolling_hash};
use crate::tokenizer::{Token, tokenize};
use dedup_lang::{
    LanguageProfile, NormalizationMode, SyntacticUnit, all_profiles, extract_units_with_mode,
    profile_for_extension,
};
use ignore::WalkBuilder;
use rayon::prelude::*;
use rustc_hash::FxHashMap;
use std::hash::Hasher;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use thiserror::Error;
use tracing::{debug, error, info, warn};
use tree_sitter::Parser;

/// The rolling-hash window size. Matches the Tier A `min_tokens` default.
const WINDOW_SIZE: usize = 50;

/// Tier A streaming callback (issue #22).
///
/// Fires exactly once per scan with the finalized Tier A group set,
/// before Tier B runs. See [`ScanConfig::on_tier_a_groups`].
pub type TierAStreamCallback = std::sync::Arc<dyn Fn(&[MatchGroup]) + Send + Sync>;

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
        source: ignore::Error,
    },
    /// A cooperative cancel was requested via
    /// [`ScanConfig::cancel`]. Partial work is discarded and callers are
    /// expected to revert to their previous state (see issue #22).
    #[error("scan cancelled")]
    Cancelled,
}

/// Which detection pass emitted a [`MatchGroup`].
///
/// Tier A is the fast, language-oblivious rolling-hash pass.
/// Tier B is the tree-sitter-backed, alpha-rename-aware pass.
///
/// The order of variants is stable: `A < B` lexicographically. Callers
/// (CLI output, JSON export, ...) can rely on `Tier::A` coming before
/// `Tier::B` when sorting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Tier {
    /// Fast, language-oblivious rolling-hash Tier A match.
    A,
    /// Tree-sitter Tier B match (normalization-aware; rename-resilient).
    B,
}

impl Tier {
    /// One-character label used in human-readable output (`[A]` / `[B]`).
    pub fn label(self) -> &'static str {
        match self {
            Tier::A => "A",
            Tier::B => "B",
        }
    }
}

/// Per-file issue category (issue #17). Every variant represents a file
/// that was encountered during the walk but could not be fully processed.
///
/// The scanner degrades gracefully on each of these: the offending file
/// is skipped (fully, or only for Tier B as noted) and the scan
/// continues. The aggregated list lives on [`ScanResult::issues`] so
/// callers can surface a summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FileIssueKind {
    /// `std::fs::read` failed (permissions, disappearing file, ...).
    /// Logged at `warn`; whole file is skipped.
    ReadError,
    /// File bytes did not decode as valid UTF-8. Logged at `debug`;
    /// whole file is skipped silently. Matches the pre-#17 behavior.
    Utf8,
    /// Tier B parse path returned an error (tree-sitter parser could not
    /// produce a tree, or the grammar ABI was incompatible). Tier A
    /// results for this file are preserved; only Tier B is skipped.
    /// Logged at `warn`.
    TierBParse,
    /// Tier B grammar work panicked. Caught via
    /// [`std::panic::catch_unwind`] so the scan continues. Tier A
    /// results for this file are preserved. Logged at `error`.
    TierBPanic,
}

impl FileIssueKind {
    /// Short camelCase label used in JSON / summary output.
    pub fn label(self) -> &'static str {
        match self {
            FileIssueKind::ReadError => "read_error",
            FileIssueKind::Utf8 => "utf8",
            FileIssueKind::TierBParse => "tier_b_parse",
            FileIssueKind::TierBPanic => "tier_b_panic",
        }
    }
}

/// One per-file issue recorded during the scan. Aggregated into
/// [`ScanResult::issues`] so the CLI can print a post-scan summary and
/// the JSON envelope can include a structured `issues` array.
#[derive(Debug, Clone)]
pub struct FileIssue {
    /// Path, forward-slashed relative to the scan root when available.
    /// Falls back to the absolute path for read-error cases where we
    /// never got to compute a relative form.
    pub path: PathBuf,
    /// Category of failure.
    pub kind: FileIssueKind,
    /// Human-readable detail (e.g. the underlying I/O error string).
    pub message: String,
}

/// Aggregate counts over a slice of [`FileIssue`]s. Cheap to compute;
/// the CLI uses it to print the post-scan summary line.
#[derive(Debug, Default, Clone, Copy)]
pub struct FileIssueCounts {
    pub read_error: usize,
    pub utf8: usize,
    pub tier_b_parse: usize,
    pub tier_b_panic: usize,
}

impl FileIssueCounts {
    /// Tally issues by kind.
    pub fn from_issues(issues: &[FileIssue]) -> Self {
        let mut c = Self::default();
        for i in issues {
            match i.kind {
                FileIssueKind::ReadError => c.read_error += 1,
                FileIssueKind::Utf8 => c.utf8 += 1,
                FileIssueKind::TierBParse => c.tier_b_parse += 1,
                FileIssueKind::TierBPanic => c.tier_b_panic += 1,
            }
        }
        c
    }

    /// Total across all kinds.
    pub fn total(&self) -> usize {
        self.read_error + self.utf8 + self.tier_b_parse + self.tier_b_panic
    }
}

/// Tunable scanner knobs. Defaults mirror the Tier A / Tier B thresholds
/// from the PRD.
#[derive(Clone)]
pub struct ScanConfig {
    /// Tier A: a match span must cover at least this many lines to be
    /// reported.
    pub tier_a_min_lines: usize,
    /// Tier A: a match span must cover at least this many tokens to be
    /// reported.
    pub tier_a_min_tokens: usize,
    /// Tier B: a syntactic unit must cover at least this many lines.
    pub tier_b_min_lines: usize,
    /// Tier B: a syntactic unit must cover at least this many normalized
    /// tokens.
    pub tier_b_min_tokens: usize,
    /// Files larger than this (in bytes) are skipped during scan.
    pub max_file_size: u64,
    /// If true, follow symlinks during the walk. Default false.
    pub follow_symlinks: bool,
    /// If true, descend into nested submodule directories (those
    /// containing a `.git` file/dir). Default false.
    pub include_submodules: bool,
    /// Disable layer 2 (`.gitignore` / `.git/info/exclude` / global
    /// gitignore) in the [`IgnoreRules`] stack. Layers 1, 3, and 4 still
    /// apply. CLI flag: `--no-gitignore`.
    pub no_gitignore: bool,
    /// Disable layers 1–3 of the [`IgnoreRules`] stack. Layer 4
    /// (`.dedupignore`) still applies. CLI flag: `--all`.
    pub ignore_all: bool,
    /// Normalization mode used by Tier B. See
    /// [`dedup_lang::NormalizationMode`]: conservative (default)
    /// leaves literals verbatim; aggressive rewrites literal leaves
    /// to a stable `<LIT>` placeholder so functions differing only
    /// in literal values still cluster. Issue #10.
    pub normalization: NormalizationMode,
    /// Parallelism budget for the read → tokenize → hash pipeline.
    /// `Some(0)` and `None` both fall through to rayon's default
    /// (currently `num_cpus::get()`). `Some(1)` forces single-threaded
    /// execution, which is the escape hatch for tests / debugging.
    /// Bounded to a scoped `rayon::ThreadPool`; never mutates the
    /// global pool.
    pub jobs: Option<usize>,
    /// Optional path to the repository cache. When set, the scanner
    /// will consult / update per-file content-hash entries to skip
    /// re-tokenizing unchanged files on warm scans. `None` (the
    /// default) disables the cache path entirely — the scanner
    /// behaves exactly as it did pre-#14.
    pub cache_root: Option<PathBuf>,
    /// Optional cooperative cancellation flag (issue #22). When set by
    /// the caller, the scanner checks it between per-file tasks and
    /// returns [`ScanError::Cancelled`] at the next boundary.
    ///
    /// Cancellation is cooperative and coarse-grained: we check
    /// **between files**, not mid-file. A scan blocked inside a very
    /// large `tokenize` / tree-sitter parse will not abort until that
    /// file's task finishes. For realistic workloads this stays well
    /// under the 500 ms target called out by the GUI's Cancel button.
    pub cancel: Option<std::sync::Arc<AtomicBool>>,
    /// Optional streaming hook fired once with the finalized Tier A
    /// groups, before Tier B is executed (issue #22).
    ///
    /// The callback is invoked **at most once** per scan, right after
    /// the bucket-fill / greedy-extension / promotion-eligible Tier A
    /// set is known — i.e. at *final membership*. Groups emitted here
    /// may later be trimmed or removed by the Tier A → B promotion
    /// step, but their own membership is stable. The GUI uses this
    /// single pulse to render Tier A groups during the scan without
    /// mid-stream "group grew from 2 to 3" shuffles.
    pub on_tier_a_groups: Option<TierAStreamCallback>,
}

// Manual Debug: the streaming callback + cancel flag are not useful to
// print, but the rest of the config is. Keeping the derive would force
// callers to come up with a `Debug` impl for `Arc<dyn Fn>`.
impl std::fmt::Debug for ScanConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScanConfig")
            .field("tier_a_min_lines", &self.tier_a_min_lines)
            .field("tier_a_min_tokens", &self.tier_a_min_tokens)
            .field("tier_b_min_lines", &self.tier_b_min_lines)
            .field("tier_b_min_tokens", &self.tier_b_min_tokens)
            .field("max_file_size", &self.max_file_size)
            .field("follow_symlinks", &self.follow_symlinks)
            .field("include_submodules", &self.include_submodules)
            .field("no_gitignore", &self.no_gitignore)
            .field("ignore_all", &self.ignore_all)
            .field("normalization", &self.normalization)
            .field("jobs", &self.jobs)
            .field("cache_root", &self.cache_root)
            .field("cancel", &self.cancel.is_some())
            .field("on_tier_a_groups", &self.on_tier_a_groups.is_some())
            .finish()
    }
}

impl Default for ScanConfig {
    fn default() -> Self {
        Self {
            tier_a_min_lines: 6,
            tier_a_min_tokens: 50,
            tier_b_min_lines: 3,
            tier_b_min_tokens: 15,
            max_file_size: 1_048_576,
            follow_symlinks: false,
            include_submodules: false,
            no_gitignore: false,
            ignore_all: false,
            normalization: NormalizationMode::Conservative,
            jobs: None,
            cache_root: None,
            cancel: None,
            on_tier_a_groups: None,
        }
    }
}

impl From<&crate::config::Config> for ScanConfig {
    fn from(cfg: &crate::config::Config) -> Self {
        Self {
            tier_a_min_lines: cfg.thresholds.tier_a.min_lines,
            tier_a_min_tokens: cfg.thresholds.tier_a.min_tokens,
            tier_b_min_lines: cfg.thresholds.tier_b.min_lines,
            tier_b_min_tokens: cfg.thresholds.tier_b.min_tokens,
            max_file_size: cfg.scan.max_file_size,
            follow_symlinks: cfg.scan.follow_symlinks,
            include_submodules: cfg.scan.include_submodules,
            no_gitignore: false,
            ignore_all: false,
            normalization: cfg.normalization.into(),
            jobs: None,
            cache_root: None,
            cancel: None,
            on_tier_a_groups: None,
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
    /// Per-occurrence alpha-rename spans (issue #25).
    ///
    /// Populated only for Tier B occurrences; Tier A occurrences always
    /// carry an empty vector. Each entry is
    /// `(start_byte, end_byte, placeholder_idx)` where the byte offsets
    /// are absolute in the occurrence's source file (same frame of
    /// reference as [`Span::start_byte`] / [`Span::end_byte`]) and
    /// `placeholder_idx` is the 1-based `vN` alias the Tier B
    /// alpha-renamer assigned to that identifier.
    ///
    /// Members of the same Tier B match group share byte-identical
    /// token streams, so the same `placeholder_idx` refers to the same
    /// logical local across every occurrence of a group — this is the
    /// correspondence key the GUI uses to tint matching identifiers
    /// the same color across files.
    pub alpha_rename_spans: Vec<(usize, usize, u32)>,
}

/// A cluster of file-local occurrences that share the same canonical
/// token stream.
#[derive(Debug, Clone)]
pub struct MatchGroup {
    /// Representative hash for the extended span (the hash of the first
    /// rolling window in the span; good enough for grouping at Tier A).
    pub hash: Hash,
    /// Which detection pass emitted this group.
    pub tier: Tier,
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
    /// Per-file issues encountered during the scan (issue #17). Gathered
    /// across every parallel task. Sorted by path + kind for stable
    /// output. Not persisted — in-memory-only; the PRD does not require
    /// persistence.
    pub issues: Vec<FileIssue>,
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

/// Lock-free [`ProgressSink`] backed by two [`AtomicUsize`] counters.
///
/// Designed for the GUI (issue #21): the scanner runs on a worker thread
/// and bumps the counters as it progresses; the GUI polls them from a
/// separate 250 ms timer and re-renders. Cloning the handles is cheap —
/// the inner `Arc`s are shared across both threads.
///
/// This is deliberately minimal. Streaming / cancel / richer progress
/// events (per-file paths, phase labels) are tracked in #22, which will
/// either extend this type or replace it with a proper channel sink.
#[derive(Debug, Default, Clone)]
pub struct AtomicProgressSink {
    pub files_scanned: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    pub matches: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl AtomicProgressSink {
    /// Fresh sink with both counters at zero.
    pub fn new() -> Self {
        Self::default()
    }

    /// Current file-processed count (monotonic).
    pub fn files_scanned(&self) -> usize {
        self.files_scanned
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Current match-group count. Only ever updated once — right before
    /// the scanner returns — because clustering / promotion is a whole-
    /// corpus step. The GUI treats the pre-completion value as "0 or
    /// more so far" and replaces it with the final count on completion.
    pub fn matches(&self) -> usize {
        self.matches.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl ProgressSink for AtomicProgressSink {
    fn on_file_processed(&self, _path: &Path) {
        self.files_scanned
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    fn on_match_group(&self, _group: &MatchGroup) {
        self.matches
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Public scanner handle. Cheap to construct; holds only configuration
/// plus the Tier B language-profile registry.
///
/// The set of Tier B profiles is injected at construction time so
/// callers can extend or restrict coverage. [`Scanner::default`] and
/// [`Scanner::new`] use every profile registered in
/// [`dedup_lang::all_profiles`].
pub struct Scanner {
    config: ScanConfig,
    profiles: Vec<&'static dyn LanguageProfile>,
}

impl std::fmt::Debug for Scanner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scanner")
            .field("config", &self.config)
            .field(
                "profiles",
                &self.profiles.iter().map(|p| p.name()).collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl Default for Scanner {
    fn default() -> Self {
        Self::new(ScanConfig::default())
    }
}

impl Scanner {
    /// Build a scanner with explicit configuration and every profile
    /// shipped with this build.
    pub fn new(config: ScanConfig) -> Self {
        Self {
            config,
            profiles: all_profiles(),
        }
    }

    /// Build a scanner with explicit configuration *and* a custom set
    /// of Tier B profiles. Passing an empty slice disables Tier B
    /// entirely — useful in tests that want to lock down Tier A output.
    pub fn with_profiles(config: ScanConfig, profiles: Vec<&'static dyn LanguageProfile>) -> Self {
        Self { config, profiles }
    }

    /// Walk `root`, run Tier A + Tier B, and return all match groups.
    /// Convenience wrapper around [`Scanner::scan_with_progress`] with a
    /// no-op progress sink.
    pub fn scan(&self, root: &Path) -> Result<ScanResult, ScanError> {
        self.scan_with_progress(root, &NoopSink)
    }

    /// Walk `root`, run Tier A + Tier B, and return all match groups,
    /// reporting progress through `sink`.
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
        info!(root = %root.display(), "scan: starting");

        let follow = self.config.follow_symlinks;
        let include_submodules = self.config.include_submodules;

        // Build the four-layer ignore-rule stack rooted at the scan root.
        // Layer 2 (gitignore) is enforced by the walker; layers 1, 3, and
        // 4 are consulted inline below.
        let ignore_rules = IgnoreRules::new(
            root,
            IgnoreRulesOptions {
                max_file_size: self.config.max_file_size,
                no_gitignore: self.config.no_gitignore,
                all: self.config.ignore_all,
            },
        );

        let mut walker = WalkBuilder::new(root);
        walker.follow_links(follow);
        ignore_rules.apply_to_walk_builder(&mut walker);

        // --- 1. Serial walk: collect candidate file paths. ----------------
        //
        // The walk itself is cheap (stat-only); parallelizing it yields
        // little and makes determinism harder. We produce a sorted list
        // of (abs, rel) paths so downstream indexing is stable across
        // runs even when the parallel read/tokenize/hash stage completes
        // out of order.
        let mut candidates: Vec<(PathBuf, PathBuf)> = Vec::new();
        for entry in walker.build() {
            let entry = match entry {
                Ok(e) => e,
                // Tolerate per-entry walk errors (permission denied on a
                // sibling, symlink loop, ...) so one bad file doesn't kill
                // the scan. Per-file graceful degradation lands in #17.
                Err(e) => {
                    warn!(error = %e, "scan: walker entry error, skipping");
                    continue;
                }
            };
            let abs = entry.path();
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            let rel = abs.strip_prefix(root).unwrap_or(abs).to_path_buf();

            // Layer 1: `.git/` and nested submodule directories.
            if is_dir {
                if ignore_rules.is_git_dir(&rel) {
                    continue;
                }
                if !include_submodules && !rel.as_os_str().is_empty() && abs.join(".git").exists() {
                    continue;
                }
                if ignore_rules.is_path_ignored(&rel, true) {
                    continue;
                }
                continue; // directory entries never get tokenized
            }

            if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }

            // Layer 1: `.git/` file contents.
            if ignore_rules.is_git_dir(&rel) {
                continue;
            }

            // Submodule guard for files (mirror of the dir check above,
            // catches files directly under a submodule root).
            if !include_submodules && file_is_under_submodule(root, abs) {
                continue;
            }

            // Layer 3 + Layer 4 path-based ignores.
            if ignore_rules.is_path_ignored(&rel, false) {
                continue;
            }

            // Layer 1: size cap. Uses metadata to avoid reading a huge
            // file just to discard it.
            if let Ok(meta) = entry.metadata()
                && ignore_rules.is_over_size_limit(meta.len())
            {
                continue;
            }

            candidates.push((abs.to_path_buf(), rel));
        }

        // Canonical ordering by relative path keeps `file_index` stable
        // irrespective of the walker's filesystem-dependent traversal
        // order — essential once the next stage runs in parallel.
        candidates.sort_by(|a, b| a.1.cmp(&b.1));

        // --- 2. Parallel read + (optional) tokenize + rolling-hash. ------
        //
        // Each task returns one of:
        //   - `Some(FileOutput)` — file survived the filter chain and
        //     produced a token stream plus a block-hash list;
        //   - `None`             — filtered out (binary / decode fail / …).
        //
        // Cache warmth: if a [`FileFingerprint`] exists with matching
        // (size, mtime) AND the freshly-computed content hash matches,
        // we pull the block-hash list out of `file_blocks` and skip
        // rolling-hash. Tokenize still runs — it's a linear pass that
        // Tier B also needs via its tree-sitter parse path, and a single
        // source-of-truth `Vec<Token>` is simpler than branching.
        //
        // The rayon pool is scoped to this call via
        // `ThreadPoolBuilder::install` so setting `jobs=1` in one scan
        // does not affect concurrent scans or tests running on the
        // global pool.
        let cache = self
            .config
            .cache_root
            .as_ref()
            .and_then(|p| match Cache::open_readonly(p) {
                Ok(Some(c)) => Some(Mutex::new(c)),
                Ok(None) => None,
                Err(e) => {
                    warn!(error = %e, "scan: cache open failed, running cold");
                    None
                }
            });

        let ignore_rules_ref = &ignore_rules;
        let cache_ref = cache.as_ref();

        // Stage 1 per-file pipeline (read → tokenize → Tier A block hashes).
        //
        // Returns:
        //   - `Ok(Some(FileOutput))` — file was tokenized and hashed.
        //   - `Ok(None)`              — file filtered out cleanly
        //                               (binary sniff / generated header);
        //                               these are not failures and do not
        //                               contribute a [`FileIssue`].
        //   - `Err(FileIssue)`        — read I/O failed or UTF-8 decode
        //                               failed; logged per PRD and the
        //                               issue is surfaced on the final
        //                               [`ScanResult`].
        //
        // Tier B per-file work (parse + unit extraction) happens later in
        // [`run_tier_b`] and is wrapped in `catch_unwind` there so a
        // panicking grammar does not abort the scan. Wrapping that narrowly
        // (just the Tier B grammar work) keeps the blast radius tight — the
        // PRD-specified contract per issue #17.
        // Cooperative cancellation (issue #22): checked once at task
        // entry. Cancelled tasks return `Ok(None)` so they're filtered
        // out cleanly alongside binary-sniff skips; the outer driver
        // looks at the same flag after the parallel stage and returns
        // [`ScanError::Cancelled`].
        let cancel_flag = self.config.cancel.clone();
        let cancel_ref = cancel_flag.as_ref();
        let process = |(abs, rel): &(PathBuf, PathBuf)| -> Result<Option<FileOutput>, FileIssue> {
            if let Some(flag) = cancel_ref
                && flag.load(Ordering::Relaxed)
            {
                return Ok(None);
            }
            let meta = std::fs::metadata(abs).ok();
            let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
            let mtime = meta
                .as_ref()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);

            // Try the stat-only fast path: if (size, mtime) match the
            // cached fingerprint, we'll still verify content_hash before
            // trusting the cached block list, but we can read + hash in
            // one pass and only decide to tokenize-vs-reuse afterward.
            let cached_fp = cache_ref.and_then(|m| {
                let c = m.lock().ok()?;
                c.file_fingerprint(rel).ok().flatten()
            });

            let bytes = match std::fs::read(abs) {
                Ok(b) => b,
                Err(e) => {
                    warn!(path = %abs.display(), error = %e, "scan: read failed, skipping");
                    return Err(FileIssue {
                        path: rel.clone(),
                        kind: FileIssueKind::ReadError,
                        message: e.to_string(),
                    });
                }
            };
            // Layer 1: binary content sniff.
            if ignore_rules_ref.looks_binary(&bytes) {
                return Ok(None);
            }
            let text = match std::str::from_utf8(&bytes) {
                Ok(s) => s.to_string(),
                Err(e) => {
                    IgnoreRules::log_utf8_skip(abs, &e);
                    return Err(FileIssue {
                        path: rel.clone(),
                        kind: FileIssueKind::Utf8,
                        message: e.to_string(),
                    });
                }
            };
            // Layer 3: `@generated` / `AUTO-GENERATED` header scan.
            if ignore_rules_ref.has_generated_header(&text) {
                return Ok(None);
            }

            let content_hash = hash_bytes(&bytes);
            let tokens = tokenize(&text);
            debug!(path = %rel.display(), tokens = tokens.len(), "scan: tokenized file");

            // Cache-hit path: content_hash matches the persisted one
            // AND (size, mtime) corroborate. Skip rolling_hash.
            let mut windows: Option<Vec<(Hash, Span)>> = None;
            let mut cache_hit = false;
            if let Some(fp) = &cached_fp
                && fp.content_hash == content_hash
                && fp.size == size
                && fp.mtime == mtime
                && let Some(m) = cache_ref
                && let Ok(c) = m.lock()
                && let Ok(Some(cached)) = c.file_blocks(rel, content_hash)
            {
                // Reconstruct spans from the freshly-computed tokens.
                // The block-hash list length must match `tokens.len() -
                // WINDOW_SIZE + 1`; if it doesn't, something is out of
                // sync — treat as a miss.
                let expected_len = tokens.len().saturating_sub(WINDOW_SIZE - 1).max(0);
                let actual_len = if tokens.len() >= WINDOW_SIZE {
                    tokens.len() - WINDOW_SIZE + 1
                } else {
                    0
                };
                if cached.block_hashes.len() == actual_len && actual_len == expected_len {
                    let reconstructed: Vec<(Hash, Span)> = cached
                        .block_hashes
                        .iter()
                        .enumerate()
                        .map(|(i, h)| {
                            let first = &tokens[i];
                            let last = &tokens[i + WINDOW_SIZE - 1];
                            (
                                *h,
                                Span {
                                    start_line: first.line,
                                    end_line: last.line,
                                    start_byte: first.start,
                                    end_byte: last.end,
                                },
                            )
                        })
                        .collect();
                    windows = Some(reconstructed);
                    cache_hit = true;
                }
            }
            let windows = windows.unwrap_or_else(|| rolling_hash(&tokens, WINDOW_SIZE));

            Ok(Some(FileOutput {
                abs: abs.clone(),
                rel: rel.clone(),
                source: text,
                tokens,
                windows,
                content_hash,
                size,
                mtime,
                cache_hit,
            }))
        };

        let outputs: Vec<Result<Option<FileOutput>, FileIssue>> = match self.config.jobs {
            Some(1) => candidates.iter().map(process).collect(),
            n => {
                let mut builder = rayon::ThreadPoolBuilder::new();
                if let Some(num) = n
                    && num > 0
                {
                    builder = builder.num_threads(num);
                }
                match builder.build() {
                    Ok(pool) => pool.install(|| candidates.par_iter().map(process).collect()),
                    Err(e) => {
                        warn!(error = %e, "scan: rayon pool build failed, falling back to serial");
                        candidates.iter().map(process).collect()
                    }
                }
            }
        };

        // Cancellation check after the parallel stage completes. Any
        // in-flight tasks that already passed the entry-guard will have
        // returned `Ok(None)` once the flag was flipped; here we abort
        // the rest of the pipeline and discard partial state.
        if let Some(flag) = cancel_ref
            && flag.load(Ordering::Relaxed)
        {
            info!("scan: cancelled after per-file stage");
            return Err(ScanError::Cancelled);
        }

        // Collapse into the scanner's internal file_bundle + per_file_hashes
        // shape. Order is preserved from `candidates`, which we sorted by
        // relative path above — so `file_index` is canonical. At the same
        // time, collect any Stage 1 [`FileIssue`]s so they can be reported
        // alongside the match groups on the final [`ScanResult`].
        let mut per_file: Vec<FileBundle> = Vec::new();
        let mut per_file_hashes: Vec<Vec<(Hash, Span)>> = Vec::new();
        let mut fresh_entries: Vec<(PathBuf, FileFingerprint, Vec<Hash>)> = Vec::new();
        let mut issues: Vec<FileIssue> = Vec::new();

        for result in outputs {
            let out = match result {
                Ok(Some(o)) => o,
                Ok(None) => continue,
                Err(issue) => {
                    issues.push(issue);
                    continue;
                }
            };
            sink.on_file_processed(&out.abs);
            if !out.cache_hit {
                let fp = FileFingerprint {
                    content_hash: out.content_hash,
                    size: out.size,
                    mtime: out.mtime,
                };
                let block_hashes: Vec<Hash> = out.windows.iter().map(|(h, _)| *h).collect();
                fresh_entries.push((out.rel.clone(), fp, block_hashes));
            }
            per_file.push(FileBundle {
                path: out.rel,
                source: out.source,
                tokens: out.tokens,
            });
            per_file_hashes.push(out.windows);
        }

        drop(cache);

        // Persist fresh fingerprints + block lists. Best-effort: a cache
        // write failure downgrades to a warning rather than failing the
        // scan. Batched in a single transaction for throughput on large
        // repos.
        if !fresh_entries.is_empty()
            && let Some(cache_root) = &self.config.cache_root
        {
            match Cache::open(cache_root) {
                Ok(mut c) => {
                    for (rel, fp, blocks) in &fresh_entries {
                        if let Err(e) = c.put_file_entry(rel, fp, blocks) {
                            warn!(path = %rel.display(), error = %e,
                                "scan: failed to persist file cache entry");
                        }
                    }
                }
                Err(e) => {
                    warn!(error = %e, "scan: cache re-open failed, skipping warm-cache writes");
                }
            }
        }

        let files_scanned = per_file.len();

        // --- 3. Bucket windows by hash, then drop singletons. ------------
        //
        // Memory discipline (issue #14 AC): we first tally per-hash
        // occurrence counts in a lightweight `FxHashMap<Hash, u32>`, then
        // materialize `Vec<WindowKey>` only for hashes that occurred at
        // least twice. On pathological inputs (100k+ singleton blocks)
        // this keeps the non-singleton bucket map bounded by the count
        // of actual duplicates rather than the total window count.
        #[derive(Clone, Copy)]
        struct WindowKey {
            file: usize,
            win: usize,
        }

        let mut counts: FxHashMap<Hash, u32> = FxHashMap::default();
        for windows in per_file_hashes.iter() {
            for (h, _) in windows {
                *counts.entry(*h).or_insert(0) += 1;
            }
        }

        let mut by_hash: FxHashMap<Hash, Vec<WindowKey>> = FxHashMap::default();
        for (fi, windows) in per_file_hashes.iter().enumerate() {
            for (wi, (h, _)) in windows.iter().enumerate() {
                if counts.get(h).copied().unwrap_or(0) < 2 {
                    continue;
                }
                by_hash
                    .entry(*h)
                    .or_default()
                    .push(WindowKey { file: fi, win: wi });
            }
        }
        // Free the count tally now that the non-singleton bucket index is
        // built; it's the peak memory contributor on singleton-heavy
        // corpora and we don't need it past this point.
        drop(counts);

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
                let path = &per_file[fi].path;
                let tokens = &per_file[fi].tokens;
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
                    // Tier A carries no alpha-rename spans — #25 tints
                    // only apply to Tier B, where alpha-renaming runs.
                    alpha_rename_spans: Vec::new(),
                });
            }
        }

        // --- 6. Finalize Tier A: keep only ≥ 2-occurrence clusters. -----
        let mut tier_a_groups: Vec<MatchGroup> = clusters
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
                    tier: Tier::A,
                    occurrences: occ,
                }
            })
            .collect();

        // Streaming pulse (issue #22): publish the Tier A set at final
        // membership — i.e. after bucket-fill + greedy-extension +
        // filtering, but before Tier A → B promotion possibly trims it.
        // Called at most once per scan. The GUI consumes this to paint
        // the sidebar during the scan without mid-stream membership
        // shuffles; the callback is responsible for its own ordering
        // (the GUI sorts by Impact — see `app_state::impact_key`).
        if let Some(cb) = self.config.on_tier_a_groups.as_ref() {
            cb(&tier_a_groups);
        }

        // Cancellation check before the (potentially slow) Tier B pass.
        // Tier B parse work is not instrumented with cancel checks —
        // cooperative cancel is between files/stages, not mid-parser.
        if let Some(flag) = cancel_ref
            && flag.load(Ordering::Relaxed)
        {
            info!("scan: cancelled after tier A");
            return Err(ScanError::Cancelled);
        }

        // --- 7. Tier B: tree-sitter-backed syntactic-unit matching. -----
        let (tier_b_groups, tier_b_issues) = self.run_tier_b(&per_file);
        issues.extend(tier_b_issues);

        // --- 8. Tier A → Tier B promotion.
        //
        // When a Tier A occurrence's span exactly aligns with a Tier B
        // syntactic unit in the same file (same path, same start/end
        // lines — i.e. the Tier A span and the Tier B unit are the same
        // block of source), we drop that Tier A occurrence so the
        // duplicate is reported only at the more-semantic Tier B level.
        // If dropping leaves a Tier A group with < 2 occurrences, the
        // whole group goes away.
        let tier_b_unit_index = build_unit_index(&tier_b_groups);
        for group in tier_a_groups.iter_mut() {
            group
                .occurrences
                .retain(|occ| !tier_b_unit_index.contains(&occurrence_key(occ)));
        }
        tier_a_groups.retain(|g| g.occurrences.len() >= 2);

        // --- 9. Merge and sort all groups for deterministic output. ----
        let mut groups: Vec<MatchGroup> = tier_a_groups.into_iter().chain(tier_b_groups).collect();
        groups.sort_by(|a, b| {
            let ap = &a.occurrences[0];
            let bp = &b.occurrences[0];
            a.tier
                .cmp(&b.tier)
                .then_with(|| ap.path.cmp(&bp.path))
                .then_with(|| ap.span.start_line.cmp(&bp.span.start_line))
        });

        // Replay groups through the progress sink so the CLI can flush a
        // final match-count before returning. Doing this after the sort
        // means the sink observes groups in their final, user-visible
        // order.
        for g in &groups {
            sink.on_match_group(g);
        }

        // Deterministic ordering for the issue list: by path, then kind
        // label. Stable across runs so snapshot-style assertions work.
        issues.sort_by(|a, b| a.path.cmp(&b.path).then(a.kind.cmp(&b.kind)));

        info!(
            files_scanned,
            groups = groups.len(),
            issues = issues.len(),
            "scan: complete"
        );

        Ok(ScanResult {
            groups,
            files_scanned,
            issues,
        })
    }

    /// Execute Tier B on every file whose extension matches a registered
    /// profile.
    ///
    /// Returns the Tier B [`MatchGroup`]s — pre-filtered by the
    /// `tier_b_min_lines` / `tier_b_min_tokens` thresholds and with
    /// hash-collision buckets already verified via exact normalized-token
    /// compare — alongside a [`FileIssue`] list for every file where
    /// Tier B parse or grammar work failed. Tier A results for those
    /// files are unaffected (issue #17 contract).
    ///
    /// The per-file grammar work (parse + unit extraction) is run inside
    /// [`catch_unwind`] with [`AssertUnwindSafe`] so a panicking
    /// tree-sitter grammar cannot abort the scan. The `AssertUnwindSafe`
    /// wrapper is required because tree-sitter types aren't
    /// `UnwindSafe`; this is the intended usage.
    fn run_tier_b(&self, per_file: &[FileBundle]) -> (Vec<MatchGroup>, Vec<FileIssue>) {
        if self.profiles.is_empty() {
            return (Vec::new(), Vec::new());
        }

        // Collect Tier B candidates per file.
        //
        // Each entry pairs a file's syntactic unit with a cheap
        // file-index reference so we can attach the path later.
        #[derive(Clone)]
        struct Candidate {
            file: usize,
            unit: SyntacticUnit,
        }

        let mut candidates: Vec<Candidate> = Vec::new();
        let mut issues: Vec<FileIssue> = Vec::new();
        for (fi, bundle) in per_file.iter().enumerate() {
            let ext = match bundle.path.extension().and_then(|s| s.to_str()) {
                Some(e) => e,
                None => continue,
            };
            // Prefer `self.profiles` — when callers hand in a custom set
            // via [`Scanner::with_profiles`], that list is authoritative.
            // Fall back to the global registry and then verify the match
            // is still in `self.profiles`, so a Scanner with an empty
            // profile slice keeps Tier B fully disabled.
            let profile = match self
                .profiles
                .iter()
                .copied()
                .find(|p| p.extensions().contains(&ext))
            {
                Some(p) => p,
                None => match profile_for_extension(ext) {
                    Some(p) if self.profiles.iter().any(|q| q.name() == p.name()) => p,
                    _ => continue,
                },
            };

            // Narrow `catch_unwind` to the grammar work. Reading,
            // tokenization, and Tier A already surfaced their failures
            // via `Result<_, FileIssue>` from Stage 1. The closure
            // returns a `Result<Vec<SyntacticUnit>, FileIssueKind>`:
            // - `Ok(units)`                       — successful parse +
            //   extraction (units may still be empty).
            // - `Err(FileIssueKind::TierBParse)`  — grammar ABI
            //   incompatibility or tree-sitter refused to produce a
            //   tree.
            //
            // On `catch_unwind` panic, we record
            // `FileIssueKind::TierBPanic` and keep going.
            let bundle_ref = bundle;
            let profile_ref = profile;
            let mode = self.config.normalization;
            let unit_result: std::thread::Result<Result<Vec<SyntacticUnit>, String>> =
                catch_unwind(AssertUnwindSafe(|| {
                    let mut parser = Parser::new();
                    if parser
                        .set_language(&profile_ref.tree_sitter_language())
                        .is_err()
                    {
                        return Err("incompatible tree-sitter grammar ABI".to_string());
                    }
                    let tree = match parser.parse(bundle_ref.source.as_bytes(), None) {
                        Some(t) => t,
                        None => {
                            return Err("tree-sitter failed to produce a parse tree".to_string());
                        }
                    };
                    Ok(extract_units_with_mode(
                        &tree,
                        bundle_ref.source.as_bytes(),
                        profile_ref,
                        mode,
                    ))
                }));

            let units = match unit_result {
                Ok(Ok(units)) => units,
                Ok(Err(msg)) => {
                    warn!(
                        path = %bundle.path.display(),
                        error = %msg,
                        "scan: tier B parse failed, skipping tier B for this file"
                    );
                    issues.push(FileIssue {
                        path: bundle.path.clone(),
                        kind: FileIssueKind::TierBParse,
                        message: msg,
                    });
                    continue;
                }
                Err(panic_payload) => {
                    let msg = panic_message(&panic_payload);
                    error!(
                        path = %bundle.path.display(),
                        error = %msg,
                        "scan: tier B grammar panicked, caught and skipping tier B for this file"
                    );
                    issues.push(FileIssue {
                        path: bundle.path.clone(),
                        kind: FileIssueKind::TierBPanic,
                        message: msg,
                    });
                    continue;
                }
            };

            for unit in units {
                // Threshold filtering lives here so candidates that
                // survive are all "big enough".
                let lines = unit.end_line.saturating_sub(unit.start_line) + 1;
                if lines < self.config.tier_b_min_lines {
                    continue;
                }
                if unit.tokens.len() < self.config.tier_b_min_tokens {
                    continue;
                }
                candidates.push(Candidate { file: fi, unit });
            }
        }

        // Bucket by hash and verify via exact normalized-token compare
        // to rule out hash collisions.
        let mut by_hash: FxHashMap<u64, Vec<Candidate>> = FxHashMap::default();
        for c in candidates {
            by_hash.entry(c.unit.hash).or_default().push(c);
        }

        let mut out: Vec<MatchGroup> = Vec::new();
        for (_h, bucket) in by_hash {
            if bucket.len() < 2 {
                continue;
            }
            // Exact-compare partition: group members whose normalized
            // token streams are byte-for-byte equal. This is the
            // collision-verification step spec'd for Tier B.
            let mut partitions: Vec<Vec<Candidate>> = Vec::new();
            'assign: for c in bucket {
                for part in partitions.iter_mut() {
                    if part[0].unit.tokens == c.unit.tokens {
                        part.push(c);
                        continue 'assign;
                    }
                }
                partitions.push(vec![c]);
            }

            for part in partitions {
                if part.len() < 2 {
                    continue;
                }
                let hash = part[0].unit.hash;
                let mut occurrences: Vec<Occurrence> = part
                    .iter()
                    .map(|c| Occurrence {
                        path: per_file[c.file].path.clone(),
                        span: Span {
                            start_line: c.unit.start_line,
                            end_line: c.unit.end_line,
                            start_byte: c.unit.start_byte,
                            end_byte: c.unit.end_byte,
                        },
                        // #25: propagate the Tier B alpha-rename spans
                        // so the GUI can paint tint overlays. Converted
                        // from `IdentSpan` to the lightweight tuple
                        // shape the cache layer persists.
                        alpha_rename_spans: c
                            .unit
                            .ident_spans
                            .iter()
                            .map(|s| (s.range.start, s.range.end, s.placeholder_idx))
                            .collect(),
                    })
                    .collect();
                occurrences.sort_by(|a, b| {
                    a.path
                        .cmp(&b.path)
                        .then(a.span.start_line.cmp(&b.span.start_line))
                });

                out.push(MatchGroup {
                    hash,
                    tier: Tier::B,
                    occurrences,
                });
            }
        }
        (out, issues)
    }
}

/// Per-file bundle collected during the walk.
///
/// Source text is retained so Tier B can re-parse with tree-sitter
/// without touching the filesystem again.
struct FileBundle {
    path: PathBuf,
    source: String,
    tokens: Vec<Token>,
}

/// Result of the parallel per-file pipeline. Converted into a
/// [`FileBundle`] + block-hash list on the main thread after the
/// parallel stage completes. Kept private so the parallel pipeline
/// shape is an implementation detail.
struct FileOutput {
    abs: PathBuf,
    rel: PathBuf,
    source: String,
    tokens: Vec<Token>,
    windows: Vec<(Hash, Span)>,
    content_hash: Hash,
    size: u64,
    mtime: i64,
    /// True iff `windows` was reconstructed from cached block hashes.
    cache_hit: bool,
}

/// Compute a 64-bit content fingerprint over raw file bytes using
/// [`rustc_hash::FxHasher`]. This is NOT cryptographic — collisions
/// can happen on adversarial input — but it is deterministic, fast,
/// and already in the dependency closure. Acceptable because the
/// fingerprint is only used as a cache-invalidation key; a collision
/// just means the cache serves stale block hashes and the
/// reconstruction step detects the mismatch via token-count
/// verification and falls back to a cold hash.
fn hash_bytes(bytes: &[u8]) -> Hash {
    let mut h = rustc_hash::FxHasher::default();
    h.write(bytes);
    h.finish()
}

/// Deterministic key identifying an occurrence's location (path + line
/// range) for the Tier A → B promotion check.
fn occurrence_key(occ: &Occurrence) -> (PathBuf, usize, usize) {
    (occ.path.clone(), occ.span.start_line, occ.span.end_line)
}

/// Flat set of every (path, start_line, end_line) covered by a Tier B
/// unit. Used to drop Tier A occurrences that coincide with a Tier B
/// unit (the promotion rule).
fn build_unit_index(groups: &[MatchGroup]) -> rustc_hash::FxHashSet<(PathBuf, usize, usize)> {
    let mut idx: rustc_hash::FxHashSet<(PathBuf, usize, usize)> = rustc_hash::FxHashSet::default();
    for g in groups {
        for occ in &g.occurrences {
            idx.insert(occurrence_key(occ));
        }
    }
    idx
}

/// Extract a human-readable message from a panic payload returned by
/// [`std::panic::catch_unwind`]. Panic payloads are typed `Box<dyn Any +
/// Send>`; in practice they're either a `&'static str` (from
/// `panic!("literal")`) or a `String` (from `panic!("{fmt}", …)`).
/// Anything else collapses to a generic label so the caller still gets
/// something useful to log.
fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "panic payload was not a string".to_string()
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

/// True iff `abs` lives strictly underneath a submodule-rooted directory
/// (identified by a `.git` entry, file or dir) below `scan_root`. The
/// scan root itself is never a submodule — its own `.git/` is a primary
/// checkout. Used to mirror the pre-#5 walkdir `filter_entry` behavior
/// with the new `ignore::Walk` iterator.
fn file_is_under_submodule(scan_root: &Path, abs: &Path) -> bool {
    let mut cur = abs.parent();
    while let Some(dir) = cur {
        if dir == scan_root {
            return false;
        }
        if dir.join(".git").exists() {
            return true;
        }
        cur = dir.parent();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

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
        // Tier A thresholds (≥ 6 lines, ≥ 50 tokens). `let`-at-file-
        // scope isn't valid Rust so tree-sitter won't emit a
        // `function_item` here — Tier B stays silent and the progress-
        // sink contract we're checking is the Tier A path.
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

    // ---------------------------------------------------------------
    // Issue #17 — per-file error graceful degradation.
    //
    // The scenarios covered here are the contract the CLI and callers
    // rely on:
    //
    // - read errors (permission denied) surface as
    //   [`FileIssueKind::ReadError`] and do NOT abort the scan;
    // - invalid UTF-8 surfaces as [`FileIssueKind::Utf8`] and is skipped
    //   silently (no panic, no stderr noise beyond the `debug!` line);
    // - a panicking tree-sitter grammar is caught via
    //   `catch_unwind` and surfaces as [`FileIssueKind::TierBPanic`],
    //   and — crucially — Tier A results for that file are preserved;
    // - the issue list is exposed on [`ScanResult::issues`] so the CLI
    //   can render a post-scan summary.
    // ---------------------------------------------------------------

    /// Test-only profile that panics from `tree_sitter_language()`. The
    /// catch_unwind around the grammar work must convert this into a
    /// [`FileIssueKind::TierBPanic`] without tearing down the scan.
    struct PanickingProfile;
    impl LanguageProfile for PanickingProfile {
        fn name(&self) -> &'static str {
            "panicking-test"
        }
        fn extensions(&self) -> &[&'static str] {
            &["panicrs"]
        }
        fn tree_sitter_language(&self) -> tree_sitter::Language {
            panic!("synthetic tree-sitter grammar panic");
        }
        fn syntactic_units(&self) -> &[&'static str] {
            &[]
        }
        fn rename_class(&self, _node_kind: &str) -> dedup_lang::RenameClass {
            dedup_lang::RenameClass::Kept
        }
    }
    static PANICKING_PROFILE: PanickingProfile = PanickingProfile;

    /// Test-only profile that returns a grammar whose ABI is
    /// incompatible with the bundled `tree-sitter` crate so
    /// `Parser::set_language` returns `Err`. Rather than fabricate an
    /// ABI-incompatible grammar (which is build-fragile), we panic with
    /// a *typed* payload (non-string) to exercise the `panic_message`
    /// fallback path alongside the `TierBPanic` arm.
    ///
    /// Tier B parse-path tests proper live in the integration suite at
    /// `crates/dedup-core/tests/per_file_errors.rs` where they exercise
    /// the real parser surface.
    struct NonStringPanicProfile;
    impl LanguageProfile for NonStringPanicProfile {
        fn name(&self) -> &'static str {
            "non-string-panic-test"
        }
        fn extensions(&self) -> &[&'static str] {
            &["nspanicrs"]
        }
        fn tree_sitter_language(&self) -> tree_sitter::Language {
            // Panic with a non-string payload to prove the message
            // extractor's fallback branch is reachable.
            std::panic::panic_any(42u32);
        }
        fn syntactic_units(&self) -> &[&'static str] {
            &[]
        }
        fn rename_class(&self, _node_kind: &str) -> dedup_lang::RenameClass {
            dedup_lang::RenameClass::Kept
        }
    }
    static NON_STRING_PANIC_PROFILE: NonStringPanicProfile = NonStringPanicProfile;

    #[cfg(unix)]
    #[test]
    fn read_error_surfaces_file_issue_and_scan_continues() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        // One readable file we expect to see processed, one with mode
        // 0o000 which `fs::read` will refuse.
        let readable = dir.path().join("readable.txt");
        fs::write(&readable, "hello world, this is readable content\n").unwrap();
        let locked = dir.path().join("locked.txt");
        fs::write(&locked, "secret").unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

        let result = Scanner::default().scan(dir.path()).unwrap();

        // Clean up permissions so the tempdir can be deleted.
        let _ = fs::set_permissions(&locked, fs::Permissions::from_mode(0o644));

        // Exactly one ReadError issue, and the other file was scanned.
        assert_eq!(result.files_scanned, 1);
        let counts = FileIssueCounts::from_issues(&result.issues);
        assert_eq!(counts.read_error, 1);
        assert_eq!(counts.total(), 1);
        assert!(
            result
                .issues
                .iter()
                .any(|i| i.kind == FileIssueKind::ReadError
                    && i.path.file_name().unwrap() == "locked.txt")
        );
    }

    #[test]
    fn utf8_failure_surfaces_as_issue_and_is_skipped() {
        let dir = tempdir().unwrap();
        // `0xC3 0x28` is a valid UTF-8 lead byte followed by an invalid
        // continuation — the classic "invalid UTF-8" sentinel. Neither
        // byte is NUL, so the binary sniff (`any(|b| *b == 0)`) lets it
        // through to the UTF-8 decode.
        fs::write(dir.path().join("bad.txt"), [0xC3, 0x28, 0xC3, 0x28]).unwrap();
        fs::write(dir.path().join("good.txt"), "abcdefgh\n").unwrap();

        let result = Scanner::default().scan(dir.path()).unwrap();
        assert_eq!(result.files_scanned, 1);
        let counts = FileIssueCounts::from_issues(&result.issues);
        assert_eq!(counts.utf8, 1);
        assert_eq!(counts.total(), 1);
    }

    #[test]
    fn tier_b_grammar_panic_is_caught_and_reported() {
        let dir = tempdir().unwrap();
        // Two files that claim the test profile's extension (`panicrs`).
        // Tier A on these will run cleanly (they're plain UTF-8); Tier B
        // will panic inside the catch_unwind and contribute two
        // `TierBPanic` issues to the summary.
        let body: String = (0..60)
            .map(|i| format!("let panic_me_{i} = {i};\n"))
            .collect::<Vec<_>>()
            .join("");
        fs::write(dir.path().join("a.panicrs"), &body).unwrap();
        fs::write(dir.path().join("b.panicrs"), &body).unwrap();

        let scanner = Scanner::with_profiles(ScanConfig::default(), vec![&PANICKING_PROFILE]);
        let result = scanner.scan(dir.path()).unwrap();

        // Scan completed (no abort).
        assert_eq!(result.files_scanned, 2);
        let counts = FileIssueCounts::from_issues(&result.issues);
        assert_eq!(counts.tier_b_panic, 2, "both files should record a panic");

        // Tier A survived the panic: the duplicated body produces at
        // least one Tier A group.
        let tier_a = result.groups.iter().filter(|g| g.tier == Tier::A).count();
        assert!(
            tier_a >= 1,
            "Tier A must still run for files whose Tier B panics"
        );
    }

    #[test]
    fn tier_b_panic_with_non_string_payload_still_reported() {
        // Covers the `panic_message` fallback arm (payload is neither
        // `&str` nor `String`). Without this test, the fallback branch
        // is dead code as far as the scanner's own tests are concerned.
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.nspanicrs"), "fn a() {}").unwrap();

        let scanner =
            Scanner::with_profiles(ScanConfig::default(), vec![&NON_STRING_PANIC_PROFILE]);
        let result = scanner.scan(dir.path()).unwrap();

        let counts = FileIssueCounts::from_issues(&result.issues);
        assert_eq!(counts.tier_b_panic, 1);
        // The message string from the fallback branch must be present
        // and non-empty — exact wording is an implementation detail.
        let only = result
            .issues
            .iter()
            .find(|i| i.kind == FileIssueKind::TierBPanic)
            .unwrap();
        assert!(
            !only.message.is_empty(),
            "panic message must be populated for non-string payloads"
        );
    }

    #[test]
    fn file_issue_counts_totals_match_sum() {
        let issues = vec![
            FileIssue {
                path: PathBuf::from("a"),
                kind: FileIssueKind::ReadError,
                message: String::new(),
            },
            FileIssue {
                path: PathBuf::from("b"),
                kind: FileIssueKind::Utf8,
                message: String::new(),
            },
            FileIssue {
                path: PathBuf::from("c"),
                kind: FileIssueKind::TierBParse,
                message: String::new(),
            },
            FileIssue {
                path: PathBuf::from("d"),
                kind: FileIssueKind::TierBPanic,
                message: String::new(),
            },
        ];
        let c = FileIssueCounts::from_issues(&issues);
        assert_eq!(c.total(), 4);
        assert_eq!(c.read_error, 1);
        assert_eq!(c.utf8, 1);
        assert_eq!(c.tier_b_parse, 1);
        assert_eq!(c.tier_b_panic, 1);
    }

    #[test]
    fn panic_message_extracts_static_str_and_string() {
        let payload_static: Box<dyn std::any::Any + Send> = Box::new("boom");
        assert_eq!(panic_message(&payload_static), "boom");
        let payload_string: Box<dyn std::any::Any + Send> = Box::new(String::from("bang"));
        assert_eq!(panic_message(&payload_string), "bang");
        let payload_other: Box<dyn std::any::Any + Send> = Box::new(7u64);
        assert!(!panic_message(&payload_other).is_empty());
    }

    #[test]
    fn atomic_progress_sink_counts_files_and_matches() {
        // Drives issue #21's GUI progress bar: the sink is polled from a
        // timer while `Scanner::scan_with_progress` runs on a worker
        // thread. If this test's counters don't advance, the bar freezes.
        let dir = tempdir().unwrap();
        // Two identical files — guarantees at least one Tier A match
        // group so the `matches` counter actually moves.
        let body: String = (0..80)
            .map(|i| format!("let always_shared_{i} = {i};\n"))
            .collect::<Vec<_>>()
            .join("");
        fs::write(dir.path().join("a.txt"), &body).unwrap();
        fs::write(dir.path().join("b.txt"), &body).unwrap();

        let sink = AtomicProgressSink::new();
        assert_eq!(sink.files_scanned(), 0);
        assert_eq!(sink.matches(), 0);

        let scanner = Scanner::with_profiles(ScanConfig::default(), vec![]);
        let result = scanner.scan_with_progress(dir.path(), &sink).unwrap();

        assert_eq!(sink.files_scanned(), result.files_scanned);
        assert!(sink.files_scanned() >= 2, "both files should be counted");
        assert_eq!(sink.matches(), result.groups.len());
        assert!(sink.matches() >= 1, "identical files produce ≥ 1 group");
    }

    // -------------------------------------------------------------------
    // Issue #22 — cooperative cancellation + Tier A streaming callback.
    // -------------------------------------------------------------------

    #[test]
    fn cancel_flag_before_scan_aborts_with_cancelled() {
        // The simplest cancel-path: the flag is already set before any
        // file task runs. Every task hits the entry guard and bails,
        // then the post-parallel cancel check returns
        // `ScanError::Cancelled`. No partial groups are returned and
        // no cache is written.
        let dir = tempdir().unwrap();
        // Populate the directory with enough material to actually
        // produce a Tier A group under a non-cancelled run — this
        // proves cancellation genuinely suppressed work, not that the
        // corpus was empty.
        let body: String = (0..80)
            .map(|i| format!("let shared_{i} = {i};\n"))
            .collect::<Vec<_>>()
            .join("");
        for i in 0..10 {
            fs::write(dir.path().join(format!("f{i}.rs")), &body).unwrap();
        }
        let flag = std::sync::Arc::new(AtomicBool::new(true));

        let cfg = ScanConfig {
            cancel: Some(flag),
            jobs: Some(1),
            ..ScanConfig::default()
        };
        let scanner = Scanner::new(cfg);
        let err = scanner.scan(dir.path()).unwrap_err();
        assert!(
            matches!(err, ScanError::Cancelled),
            "pre-set cancel flag must abort the scan"
        );
    }

    #[test]
    fn cancel_flag_flipped_mid_walk_aborts_before_tier_b() {
        // Flip the cancel flag from inside the Tier A streaming
        // callback — i.e. *after* the per-file pass has already
        // finished. The subsequent stage-boundary check must see the
        // flag and short-circuit with `Cancelled`, proving mid-run
        // cancellation is wired at stage boundaries (not only at
        // startup).
        let dir = tempdir().unwrap();
        let body: String = (0..80)
            .map(|i| format!("let shared_{i} = {i};\n"))
            .collect::<Vec<_>>()
            .join("");
        fs::write(dir.path().join("a.rs"), &body).unwrap();
        fs::write(dir.path().join("b.rs"), &body).unwrap();

        let flag = std::sync::Arc::new(AtomicBool::new(false));
        let flag_cb = flag.clone();
        let cb: TierAStreamCallback = std::sync::Arc::new(move |_groups: &[MatchGroup]| {
            flag_cb.store(true, Ordering::Relaxed);
        });

        let cfg = ScanConfig {
            cancel: Some(flag),
            on_tier_a_groups: Some(cb),
            jobs: Some(1),
            ..ScanConfig::default()
        };
        let scanner = Scanner::new(cfg);
        let err = scanner.scan(dir.path()).unwrap_err();
        assert!(matches!(err, ScanError::Cancelled));
    }

    #[test]
    fn tier_a_streaming_callback_fires_once_with_final_membership() {
        // The streaming hook must fire exactly once per scan, and the
        // groups it sees must match the Tier A subset of the final
        // `ScanResult.groups`. Tier B is disabled here so promotion
        // can't trim Tier A after the callback — that keeps the
        // "before promotion" and "after promotion" sets identical for
        // this assertion.
        let dir = tempdir().unwrap();
        let body: String = (0..80)
            .map(|i| format!("let always_shared_{i} = {i};\n"))
            .collect::<Vec<_>>()
            .join("");
        fs::write(dir.path().join("a.txt"), &body).unwrap();
        fs::write(dir.path().join("b.txt"), &body).unwrap();

        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::<usize>::new()));
        let calls_cb = calls.clone();
        let cb: TierAStreamCallback = std::sync::Arc::new(move |groups: &[MatchGroup]| {
            calls_cb.lock().unwrap().push(groups.len());
        });

        let cfg = ScanConfig {
            on_tier_a_groups: Some(cb),
            ..ScanConfig::default()
        };
        let scanner = Scanner::with_profiles(cfg, vec![]); // Tier B off.
        let result = scanner.scan(dir.path()).unwrap();

        let seen = calls.lock().unwrap().clone();
        assert_eq!(seen.len(), 1, "callback must fire exactly once");
        let tier_a_final = result.groups.iter().filter(|g| g.tier == Tier::A).count();
        assert_eq!(
            seen[0], tier_a_final,
            "streamed tier A count must match final tier A count"
        );
        assert!(seen[0] >= 1, "duplicate bodies produced a group");
    }

    #[test]
    fn atomic_progress_sink_clones_share_state() {
        // The GUI clones the sink so the worker thread bumps one handle
        // while the timer reads the other. This only works if `Arc`
        // cloning keeps both handles pointing at the same counters.
        let a = AtomicProgressSink::new();
        let b = a.clone();
        <AtomicProgressSink as ProgressSink>::on_file_processed(
            &a,
            std::path::Path::new("ignored"),
        );
        assert_eq!(a.files_scanned(), 1);
        assert_eq!(b.files_scanned(), 1);
    }
}
