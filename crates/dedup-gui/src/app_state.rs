//! GUI view-model + state for the "open a folder and render cached
//! results" flow (issue #20).
//!
//! This module is intentionally GPUI-free so every function here is unit-
//! testable off the main thread in the existing `cargo test -p dedup-gui`
//! lane. The GPUI `Render` impls live in [`crate::project_view`]; they
//! treat the types here as plain data.
//!
//! Data flow:
//!
//! 1. The user picks a folder via `File → Open…` (the `OpenFolder` action
//!    in [`crate::menubar`]).
//! 2. The app calls [`load_folder`], which opens the cache read-only via
//!    [`dedup_core::Cache::open_readonly`] and materializes every group +
//!    every suppression into [`GroupView`] / [`SuppressionView`] rows.
//! 3. [`AppState::set_folder_result`] stores the result and derives the
//!    right [`AppStatus`] variant — the sidebar + detail view read off
//!    that.
//!
//! No scan logic runs during `load_folder` — per issue #20 acceptance
//! criteria, "Re-opening an already-cached directory is instant" and
//! "no re-scan required".

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use dedup_core::{AtomicProgressSink, Cache, CacheError, MatchGroup, Tier};

/// View-model for one duplicate group as shown in the sidebar.
///
/// Materialized eagerly from the cache on open — the sidebar renders off
/// `Vec<GroupView>`, the detail view re-reads the selected group from the
/// same `Vec` by id. Keeping the data in memory (rather than querying
/// SQLite on every click) is fine at expected scale: even a large repo
/// has O(10^3) groups, each with O(10) occurrences.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupView {
    /// Cache row id. Used as the stable selection key.
    pub id: i64,
    /// Tier A = rolling-hash block, Tier B = tree-sitter function/class.
    pub tier: Tier,
    /// Human-readable label shown in the sidebar row. Computed by
    /// [`group_label`] at construction time so the sidebar is a pure map
    /// over `Vec<GroupView>`.
    pub label: String,
    /// All file-local occurrences, sorted path-asc then start-line-asc
    /// (mirrors [`dedup_core::Cache::get_group`]'s ordering).
    pub occurrences: Vec<OccurrenceView>,
}

/// View-model for one occurrence in the detail pane.
///
/// Line numbers are 1-based and inclusive on both ends, matching the
/// on-disk cache (which in turn matches `tokenizer`/`rolling_hash`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OccurrenceView {
    /// Repository-relative path in POSIX form (forward slashes on all
    /// platforms — that's the form the cache stores).
    pub path: PathBuf,
    pub start_line: i64,
    pub end_line: i64,
}

impl OccurrenceView {
    /// Render as `path:Lstart–end` (e.g. `src/auth/login.rs:L42–58`).
    /// Used as the Tier A / fallback sidebar label and as the detail-view
    /// header for each occurrence.
    pub fn label(&self) -> String {
        format!(
            "{}:L{}\u{2013}{}",
            self.path.display(),
            self.start_line,
            self.end_line
        )
    }
}

/// View-model for one dismissed-group row in the "Dismissed" section.
///
/// The cache only stores the normalized-block-hash (plus a breadcrumb
/// last-group-id); the original source content is not recoverable, so the
/// sidebar label is just `Dismissed block (hash <12-hex>…)`. #30 /
/// follow-ups can enrich this once dismissal rows grow first-class
/// label storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuppressionView {
    pub hash_hex: String,
    pub last_group_id: Option<i64>,
}

impl SuppressionView {
    /// Short label rendered in the collapsed/expanded Dismissed section.
    pub fn label(&self) -> String {
        // The hash is a u64 rendered as 16 hex digits; 12 chars is plenty
        // for humans to distinguish rows at a glance.
        let short: String = self.hash_hex.chars().take(12).collect();
        format!("Dismissed block (hash {short}\u{2026})")
    }
}

/// Top-level status the main window renders from.
///
/// The variants are mutually exclusive and drive both the window body and
/// (eventually) the menubar enable/disable state. `load_folder` is the
/// only thing that transitions between them.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub enum AppStatus {
    /// No folder has been opened yet — show the start-here empty state.
    #[default]
    NoFolderOpen,
    /// Folder opened, but no `.dedup/cache.sqlite` was found — treat as
    /// "no source files / never scanned" per acceptance criteria #5.
    Empty,
    /// Folder opened, cache present, but zero groups after reading —
    /// acceptance criteria #6 ("No duplicates found").
    NoDuplicates,
    /// Folder opened, cache present, at least one group. Sidebar and
    /// detail view render normally.
    Loaded,
    /// The on-disk cache declares a schema version newer than this build
    /// understands. We show a non-destructive banner; full toast UX is
    /// issue #30.
    NewerCache { found: u32, supported: u32 },
    /// Opening the cache failed for some other reason (I/O, SQLite
    /// corruption, ...). The message is surfaced verbatim — we don't try
    /// to beautify SQLite errors at this layer.
    Error(String),
}

/// State of the background scan pipeline (issue #21).
///
/// Orthogonal to [`AppStatus`]: `AppStatus` tracks *what the cache
/// contains*, `ScanState` tracks *whether a live scan is running*. Both
/// are read by the project view to decide what to render at the top of
/// the sidebar.
///
/// Transitions (#21 + #22):
///
/// - `Idle` → `Running` — the user clicked Scan.
/// - `Running` → `Cancelling` — user clicked Cancel.
/// - `Cancelling` → `Idle` — worker acknowledged the cancel flag.
/// - `Running` → `Completed` — the worker thread returned a result.
/// - `Completed` → `Idle` — the post-scan banner auto-dismissed.
#[derive(Debug, Clone, Default)]
pub enum ScanState {
    /// No scan has been requested (or the completion banner was
    /// dismissed). Default for a fresh `AppState`.
    #[default]
    Idle,
    /// A scan thread is running.
    Running {
        /// Wall-clock start time. Used to compute the live "elapsed"
        /// counter in the progress bar.
        started_at: Instant,
        /// Shared counters bumped by the scanner's [`AtomicProgressSink`]
        /// and read by the GUI's 250 ms timer.
        progress: AtomicProgressSink,
        /// Cooperative cancellation flag handed to the scanner. The GUI
        /// sets this when the user clicks Cancel; the scanner checks it
        /// between files and returns [`dedup_core::ScanError::Cancelled`]
        /// at the next stage boundary (issue #22).
        cancel: Arc<AtomicBool>,
    },
    /// Cancel was requested and we're waiting for the worker thread to
    /// notice the flag and return. Takes effect within ~500 ms on
    /// realistic workloads (cancel is checked **between files**; see
    /// [`dedup_core::ScanConfig::cancel`]).
    Cancelling {
        /// Wall-clock time the user clicked Cancel. Used to decide
        /// whether the wait is unreasonably long (cosmetic only).
        started_at: Instant,
    },
    /// Scan finished; the completion banner is showing (auto-dismisses).
    Completed {
        /// Number of duplicate groups produced by the scan.
        group_count: usize,
        /// Number of files tokenized.
        file_count: usize,
        /// End-to-end scan duration.
        duration: Duration,
    },
}

impl ScanState {
    /// Convenience predicate used by the Scan button's enable/disable
    /// logic — we don't want two scans in flight at once.
    pub fn is_running(&self) -> bool {
        matches!(self, ScanState::Running { .. })
    }

    /// True while the scanner is still in flight (Running or Cancelling
    /// — both represent "worker thread alive, user cannot start a new
    /// scan"). The Scan button uses this to decide enablement; the
    /// Cancel button uses the stricter [`ScanState::is_running`].
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            ScanState::Running { .. } | ScanState::Cancelling { .. }
        )
    }
}

/// Format a [`Duration`] for the live progress bar / completion banner.
///
/// Kept cheap: one-decimal seconds up to a minute (so the sidebar doesn't
/// flicker between `9.9s` and `10s`), then integer seconds. This mirrors
/// the indicatif spinner format used by the CLI — when both surfaces
/// report the same numbers the user's mental model stays intact.
pub fn format_elapsed(d: Duration) -> String {
    let secs = d.as_secs_f64();
    if secs < 60.0 {
        format!("{secs:.1}s")
    } else {
        format!("{}s", secs as u64)
    }
}

/// Format the post-scan completion banner per issue #21 acceptance
/// criteria:
///
/// ```text
/// Scan complete — 7 groups across 42 files in 3.4s.
/// ```
///
/// Singular / plural nouns are kept as-is ("1 groups", "1 files") to keep
/// the formatter deterministic — English pluralization is out of scope.
pub fn format_completion_banner(
    group_count: usize,
    file_count: usize,
    duration: Duration,
) -> String {
    format!(
        "Scan complete \u{2014} {group_count} groups across {file_count} files in {}.",
        format_elapsed(duration)
    )
}

/// Output of [`load_folder`] — everything the GUI needs to paint a newly-
/// opened project.
///
/// We return a plain struct rather than mutating `AppState` in place so
/// the I/O step is exercised by pure tests (construct fixtures, feed into
/// `AppState::set_folder_result`, assert on the resulting status +
/// sidebar rows).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderLoadResult {
    pub folder: PathBuf,
    pub groups: Vec<GroupView>,
    pub dismissed: Vec<SuppressionView>,
    pub status: AppStatus,
}

/// Central app state the top-level window reads from.
///
/// Holds the current folder, the cached-group list, the dismissed list,
/// the selected group id, and the derived [`AppStatus`]. All fields are
/// plain `pub` so the `Render` impls can project them directly into view
/// tree without ceremony — this is a pure-data view-model, not an object.
#[derive(Debug, Clone, Default)]
pub struct AppState {
    pub current_folder: Option<PathBuf>,
    pub groups: Vec<GroupView>,
    pub dismissed: Vec<SuppressionView>,
    pub selected_group: Option<i64>,
    pub status: AppStatus,
    /// Whether the "Dismissed" section is expanded. Collapsed by default
    /// per issue #20 acceptance criteria ("Dismissed section collapsed by
    /// default; expandable").
    pub dismissed_expanded: bool,
    /// Live scan pipeline state (issue #21). Independent of [`AppStatus`]
    /// — the sidebar can be `Loaded` while a fresh scan runs to refresh
    /// it.
    pub scan_state: ScanState,
    /// Streaming Tier A groups surfaced during an in-flight scan
    /// (issue #22). Rendered in the sidebar while `scan_state ==
    /// Running`; cleared on `begin_scan` and on cancel. Kept sorted by
    /// [`impact_key`] — see [`AppState::merge_streaming_groups`] for
    /// the binary-search insert that preserves stability.
    pub groups_streaming: Vec<GroupView>,
}

impl AppState {
    /// Fresh state — no folder open, start-here empty view.
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply a [`FolderLoadResult`] (from [`load_folder`]) to this state.
    ///
    /// Resets `selected_group` to the first group if any, so the detail
    /// pane isn't blank on open.
    pub fn set_folder_result(&mut self, result: FolderLoadResult) {
        self.current_folder = Some(result.folder);
        self.selected_group = result.groups.first().map(|g| g.id);
        self.groups = result.groups;
        self.dismissed = result.dismissed;
        self.status = result.status;
        // Keep the Dismissed section collapsed by default on every open,
        // including re-opens — the user can expand it if they want.
        self.dismissed_expanded = false;
    }

    /// Look up the currently selected group's occurrences, if any.
    pub fn selected_occurrences(&self) -> &[OccurrenceView] {
        match self.selected_group {
            Some(id) => self
                .groups
                .iter()
                .find(|g| g.id == id)
                .map(|g| g.occurrences.as_slice())
                .unwrap_or(&[]),
            None => &[],
        }
    }

    /// Partition groups into the two sidebar sections: Tier B first
    /// ("Duplicated functions / classes"), then Tier A ("Duplicated
    /// blocks"). Each returned slice preserves the cache's sort order
    /// (path-asc, start-line-asc) within its tier.
    pub fn tier_b_groups(&self) -> impl Iterator<Item = &GroupView> {
        self.groups.iter().filter(|g| g.tier == Tier::B)
    }

    pub fn tier_a_groups(&self) -> impl Iterator<Item = &GroupView> {
        self.groups.iter().filter(|g| g.tier == Tier::A)
    }

    /// Transition to [`ScanState::Running`] with a fresh
    /// [`AtomicProgressSink`] + cancellation flag. Returns the
    /// [`ScanHandles`] the caller hands to the worker thread.
    ///
    /// This is a no-op (and returns `None`) when a scan is already in
    /// flight — the Scan button is supposed to be disabled in that
    /// case, but we guard defensively so a double-click can't fork two
    /// scans. Also clears [`AppState::groups_streaming`] so the new
    /// scan starts from an empty streaming buffer.
    pub fn begin_scan(&mut self) -> Option<ScanHandles> {
        if self.scan_state.is_active() {
            return None;
        }
        let progress = AtomicProgressSink::new();
        let cancel = Arc::new(AtomicBool::new(false));
        self.scan_state = ScanState::Running {
            started_at: Instant::now(),
            progress: progress.clone(),
            cancel: cancel.clone(),
        };
        self.groups_streaming.clear();
        Some(ScanHandles { progress, cancel })
    }

    /// Request cooperative cancellation of an in-flight scan.
    ///
    /// Flips the cancel flag, transitions state to
    /// [`ScanState::Cancelling`], and clears the streaming sidebar
    /// buffer (partial results are discarded per #22 AC). No-op if no
    /// scan is running. The scanner checks the flag between files and
    /// returns [`dedup_core::ScanError::Cancelled`] at the next stage
    /// boundary, which the GUI polling loop interprets as "transition
    /// back to Idle".
    pub fn request_cancel(&mut self) {
        if let ScanState::Running { cancel, .. } = &self.scan_state {
            cancel.store(true, Ordering::Relaxed);
            self.scan_state = ScanState::Cancelling {
                started_at: Instant::now(),
            };
            self.groups_streaming.clear();
        }
    }

    /// Finalize a cancelled scan: drop the streaming buffer and return
    /// to Idle. Called by the GUI polling loop when the worker thread
    /// surfaces [`dedup_core::ScanError::Cancelled`] or disconnects
    /// without a result during Cancelling.
    pub fn cancel_completed(&mut self) {
        self.groups_streaming.clear();
        self.scan_state = ScanState::Idle;
    }

    /// Transition to [`ScanState::Completed`] with the given counts.
    ///
    /// The GUI calls this once the worker thread's result arrives; after
    /// the banner's auto-dismiss timer fires, [`Self::dismiss_completion`]
    /// drops back to `Idle`. Streaming buffer is cleared — by this point
    /// the sidebar has been reloaded from the freshly-written cache and
    /// the streaming buffer is redundant.
    pub fn finish_scan(&mut self, group_count: usize, file_count: usize, duration: Duration) {
        self.groups_streaming.clear();
        self.scan_state = ScanState::Completed {
            group_count,
            file_count,
            duration,
        };
    }

    /// Merge a batch of streaming Tier A groups into
    /// [`Self::groups_streaming`] while preserving the Impact-desc sort
    /// order. Uses `binary_search_by` + `insert` so already-rendered
    /// entries stay in place ("no visible shuffle" per #22 AC).
    ///
    /// Duplicate ids (same cache-row id already present) are ignored —
    /// re-delivery of an already-rendered group is a no-op.
    pub fn merge_streaming_groups(&mut self, incoming: Vec<GroupView>) {
        for g in incoming {
            if self.groups_streaming.iter().any(|x| x.id == g.id) {
                continue;
            }
            let key = impact_key(&g);
            let pos = self
                .groups_streaming
                .binary_search_by(|probe| impact_key(probe).cmp(&key))
                .unwrap_or_else(|e| e);
            self.groups_streaming.insert(pos, g);
        }
    }

    /// Drop the completion banner and return to the idle state. Called
    /// from the auto-dismiss timer in the project view.
    pub fn dismiss_completion(&mut self) {
        if matches!(self.scan_state, ScanState::Completed { .. }) {
            self.scan_state = ScanState::Idle;
        }
    }
}

/// Shared handles handed to the scanner worker thread.
///
/// Groups the progress sink and the cancellation flag so the caller can
/// pass one value around instead of two parallel arguments. Cheap to
/// clone — every field is an `Arc`.
#[derive(Debug, Clone)]
pub struct ScanHandles {
    pub progress: AtomicProgressSink,
    pub cancel: Arc<AtomicBool>,
}

/// Impact-desc sort key for streaming sidebar entries.
///
/// Impact is `occurrence_count * total_line_count` summed across all
/// occurrences — a cheap proxy for "how much duplicated code is in this
/// group". Higher impact groups sort first; ties break by **ascending**
/// hex label so the key is total and deterministic given a fixed group
/// set (no visible shuffle during streaming).
///
/// The returned key is shaped so `impact_key(a).cmp(&impact_key(b))`
/// does the right thing directly — higher impact yields a *smaller*
/// key (via `Reverse`-like inversion through `u64::MAX - impact`),
/// which sorts first under ascending `Ord`.
pub fn impact_key(group: &GroupView) -> (u64, String) {
    let total_lines: u64 = group
        .occurrences
        .iter()
        .map(|o| (o.end_line.saturating_sub(o.start_line).saturating_add(1)).max(0) as u64)
        .sum();
    let impact = (group.occurrences.len() as u64).saturating_mul(total_lines);
    // Invert impact for ascending sort = descending impact order.
    let inv = u64::MAX - impact;
    (inv, group.label.clone())
}

/// Convert a core [`MatchGroup`] into the GUI's [`GroupView`] — used by
/// the Tier A streaming callback. `id` is negative so it can't collide
/// with cache-row ids (which come from SQLite's `INTEGER PRIMARY KEY`
/// and are always `>= 1`). A scan that later completes and reloads the
/// sidebar from the cache will replace these rows with real-id rows.
pub fn group_view_from_match(group: &MatchGroup, index: usize) -> GroupView {
    let occurrences: Vec<OccurrenceView> = group
        .occurrences
        .iter()
        .map(|o| OccurrenceView {
            path: o.path.clone(),
            start_line: o.span.start_line as i64,
            end_line: o.span.end_line as i64,
        })
        .collect();
    let label = group_label(group.tier, None, None, occurrences.first());
    GroupView {
        // Negative sentinel ids keep streaming rows distinguishable from
        // cache-backed rows. The index keeps each streaming id unique
        // within the current scan (same scan can emit many groups).
        id: -1 - index as i64,
        tier: group.tier,
        label,
        occurrences,
    }
}

/// Compute the sidebar label for a group.
///
/// Tier A always uses `src/auth/login.rs:L42–58` — the first occurrence's
/// path + line range.
///
/// Tier B **should** use the function/class name (`fn validate_email`)
/// per acceptance criterion #3. The cache does not store unit names yet
/// (tracked alongside the `LanguageProfile` work in #6 follow-ups), so we
/// fall back to the same path:lines label for now. When the name becomes
/// available, passing `Some(name)` through `display_name` flips the label
/// without any caller change.
///
/// `kind_hint` is a free-form string like `"fn"` / `"class"`; only used
/// when a name is present, otherwise ignored.
pub fn group_label(
    tier: Tier,
    display_name: Option<&str>,
    kind_hint: Option<&str>,
    first: Option<&OccurrenceView>,
) -> String {
    match (tier, display_name) {
        (Tier::B, Some(name)) => match kind_hint {
            Some(kind) => format!("{kind} {name}"),
            None => name.to_string(),
        },
        // Tier A, or Tier B with no name available — fall back to the
        // stable path:lines form. Matches the CLI `run_list` header.
        (_, _) => match first {
            Some(occ) => occ.label(),
            None => "(empty group)".to_string(),
        },
    }
}

/// Open the cache at `folder` (if any) and materialize the full view-
/// model. Never runs a scan.
///
/// The function is synchronous and side-effect-free beyond a single
/// SQLite read transaction. It's called from the GPUI main thread
/// directly after the file picker returns.
pub fn load_folder(folder: &Path) -> FolderLoadResult {
    match Cache::open_readonly(folder) {
        Ok(Some(cache)) => materialize_from_cache(folder.to_path_buf(), &cache),
        Ok(None) => FolderLoadResult {
            folder: folder.to_path_buf(),
            groups: Vec::new(),
            dismissed: Vec::new(),
            // No cache file at all → treat as "never scanned / no source
            // files found". The message is identical to the empty-scan
            // case per issue #20 acceptance criterion #5.
            status: AppStatus::Empty,
        },
        Err(CacheError::NewerSchema { found, supported }) => FolderLoadResult {
            folder: folder.to_path_buf(),
            groups: Vec::new(),
            dismissed: Vec::new(),
            status: AppStatus::NewerCache { found, supported },
        },
        Err(err) => FolderLoadResult {
            folder: folder.to_path_buf(),
            groups: Vec::new(),
            dismissed: Vec::new(),
            status: AppStatus::Error(err.to_string()),
        },
    }
}

fn materialize_from_cache(folder: PathBuf, cache: &Cache) -> FolderLoadResult {
    // Surface a partial-failure as Error rather than silently showing an
    // empty sidebar — the distinction matters for the "empty vs broken"
    // UX.
    let summaries = match cache.list_groups() {
        Ok(s) => s,
        Err(e) => {
            return FolderLoadResult {
                folder,
                groups: Vec::new(),
                dismissed: Vec::new(),
                status: AppStatus::Error(format!("failed to read cache: {e}")),
            };
        }
    };

    let suppressed = match cache.suppressed_hashes() {
        Ok(h) => h,
        Err(e) => {
            return FolderLoadResult {
                folder,
                groups: Vec::new(),
                dismissed: Vec::new(),
                status: AppStatus::Error(format!("failed to read suppressions: {e}")),
            };
        }
    };

    let mut groups = Vec::with_capacity(summaries.len());
    for summary in summaries {
        // Filter suppressed groups out of the main sidebar; they'll show
        // up in the Dismissed section below with their hash.
        let hash = match cache.group_hash(summary.id) {
            Ok(h) => h,
            Err(e) => {
                return FolderLoadResult {
                    folder,
                    groups: Vec::new(),
                    dismissed: Vec::new(),
                    status: AppStatus::Error(format!(
                        "failed to read hash for group {}: {e}",
                        summary.id
                    )),
                };
            }
        };
        if let Some(h) = hash
            && suppressed.contains(&h)
        {
            continue;
        }

        let detail = match cache.get_group(summary.id) {
            Ok(Some(d)) => d,
            Ok(None) => continue,
            Err(e) => {
                return FolderLoadResult {
                    folder,
                    groups: Vec::new(),
                    dismissed: Vec::new(),
                    status: AppStatus::Error(format!("failed to read group {}: {e}", summary.id)),
                };
            }
        };

        let occurrences: Vec<OccurrenceView> = detail
            .occurrences
            .iter()
            .map(|o| OccurrenceView {
                path: o.path.clone(),
                start_line: o.start_line,
                end_line: o.end_line,
            })
            .collect();

        // Tier B display-name + kind are not yet plumbed through the
        // cache — see `group_label`'s doc comment. For now every Tier B
        // group falls back to path:lines.
        let label = group_label(detail.tier, None, None, occurrences.first());

        groups.push(GroupView {
            id: detail.id,
            tier: detail.tier,
            label,
            occurrences,
        });
    }

    let dismissed: Vec<SuppressionView> = match cache.list_suppressions() {
        Ok(s) => s
            .into_iter()
            .map(|sup| SuppressionView {
                hash_hex: format!("{:016x}", sup.hash),
                last_group_id: sup.last_group_id,
            })
            .collect(),
        Err(e) => {
            return FolderLoadResult {
                folder,
                groups: Vec::new(),
                dismissed: Vec::new(),
                status: AppStatus::Error(format!("failed to read suppressions: {e}")),
            };
        }
    };

    let status = if groups.is_empty() {
        AppStatus::NoDuplicates
    } else {
        AppStatus::Loaded
    };

    FolderLoadResult {
        folder,
        groups,
        dismissed,
        status,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn occ(path: &str, s: i64, e: i64) -> OccurrenceView {
        OccurrenceView {
            path: PathBuf::from(path),
            start_line: s,
            end_line: e,
        }
    }

    #[test]
    fn occurrence_label_uses_en_dash() {
        let o = occ("src/auth/login.rs", 42, 58);
        assert_eq!(o.label(), "src/auth/login.rs:L42\u{2013}58");
    }

    #[test]
    fn tier_a_label_is_first_occurrence_path_lines() {
        let first = occ("src/auth/login.rs", 42, 58);
        let label = group_label(Tier::A, None, None, Some(&first));
        assert_eq!(label, "src/auth/login.rs:L42\u{2013}58");
    }

    #[test]
    fn tier_b_label_without_name_falls_back_to_path_lines() {
        // Today the cache doesn't persist unit names, so Tier B groups
        // still fall back to the same path:lines form as Tier A. When
        // names land, only this test + `group_label` need updating.
        let first = occ("src/auth.rs", 10, 30);
        let label = group_label(Tier::B, None, None, Some(&first));
        assert_eq!(label, "src/auth.rs:L10\u{2013}30");
    }

    #[test]
    fn tier_b_label_with_name_and_kind() {
        let first = occ("src/auth.rs", 10, 30);
        let label = group_label(Tier::B, Some("validate_email"), Some("fn"), Some(&first));
        assert_eq!(label, "fn validate_email");
    }

    #[test]
    fn tier_b_label_with_name_without_kind() {
        let first = occ("src/a.rs", 1, 2);
        let label = group_label(Tier::B, Some("User"), None, Some(&first));
        assert_eq!(label, "User");
    }

    #[test]
    fn group_label_handles_empty_group_defensively() {
        // A group with zero occurrences should never escape the cache,
        // but if it does we want a non-panicking placeholder rather than
        // a blank row.
        let label = group_label(Tier::A, None, None, None);
        assert_eq!(label, "(empty group)");
    }

    #[test]
    fn suppression_label_truncates_hash() {
        let s = SuppressionView {
            hash_hex: "abcdef0123456789".to_string(),
            last_group_id: Some(3),
        };
        assert_eq!(s.label(), "Dismissed block (hash abcdef012345\u{2026})");
    }

    #[test]
    fn default_state_is_no_folder_open() {
        let s = AppState::new();
        assert_eq!(s.status, AppStatus::NoFolderOpen);
        assert!(s.current_folder.is_none());
        assert!(s.groups.is_empty());
        assert!(s.dismissed.is_empty());
        assert!(s.selected_group.is_none());
        // Dismissed collapsed by default per acceptance criterion.
        assert!(!s.dismissed_expanded);
    }

    #[test]
    fn set_folder_result_selects_first_group() {
        let mut s = AppState::new();
        let result = FolderLoadResult {
            folder: PathBuf::from("/tmp/x"),
            groups: vec![
                GroupView {
                    id: 7,
                    tier: Tier::A,
                    label: "a".into(),
                    occurrences: vec![occ("a.rs", 1, 2)],
                },
                GroupView {
                    id: 8,
                    tier: Tier::B,
                    label: "b".into(),
                    occurrences: vec![occ("b.rs", 3, 4)],
                },
            ],
            dismissed: vec![],
            status: AppStatus::Loaded,
        };
        s.set_folder_result(result);
        assert_eq!(s.selected_group, Some(7));
        assert_eq!(s.status, AppStatus::Loaded);
        assert_eq!(s.current_folder.as_deref(), Some(Path::new("/tmp/x")));
        // Re-collapse on every open.
        assert!(!s.dismissed_expanded);
    }

    #[test]
    fn set_folder_result_with_zero_groups_has_no_selection() {
        let mut s = AppState::new();
        // Simulate re-opening after the user previously had a selection.
        s.selected_group = Some(42);
        s.set_folder_result(FolderLoadResult {
            folder: PathBuf::from("/tmp/y"),
            groups: vec![],
            dismissed: vec![],
            status: AppStatus::NoDuplicates,
        });
        assert_eq!(s.selected_group, None);
        assert_eq!(s.status, AppStatus::NoDuplicates);
    }

    #[test]
    fn tier_partition_groups() {
        let mut s = AppState::new();
        s.groups = vec![
            GroupView {
                id: 1,
                tier: Tier::A,
                label: "a".into(),
                occurrences: vec![],
            },
            GroupView {
                id: 2,
                tier: Tier::B,
                label: "b".into(),
                occurrences: vec![],
            },
            GroupView {
                id: 3,
                tier: Tier::A,
                label: "a2".into(),
                occurrences: vec![],
            },
        ];
        let tier_a: Vec<i64> = s.tier_a_groups().map(|g| g.id).collect();
        let tier_b: Vec<i64> = s.tier_b_groups().map(|g| g.id).collect();
        assert_eq!(tier_a, vec![1, 3]);
        assert_eq!(tier_b, vec![2]);
    }

    #[test]
    fn selected_occurrences_returns_selected_group_rows() {
        let mut s = AppState::new();
        s.groups = vec![GroupView {
            id: 9,
            tier: Tier::A,
            label: "a".into(),
            occurrences: vec![occ("x.rs", 1, 2), occ("x.rs", 10, 12)],
        }];
        s.selected_group = Some(9);
        assert_eq!(s.selected_occurrences().len(), 2);
        // Unknown selection → empty slice rather than a panic.
        s.selected_group = Some(77);
        assert!(s.selected_occurrences().is_empty());
        s.selected_group = None;
        assert!(s.selected_occurrences().is_empty());
    }

    // ---------------------------------------------------------------
    // Empty-state selection logic (no-folder vs no-cache vs zero-groups
    // vs newer-schema). These are the branches of `load_folder` /
    // `set_folder_result` that drive acceptance criteria 5 + 6 + the
    // cache-upgrade banner.
    // ---------------------------------------------------------------

    #[test]
    fn load_folder_with_no_cache_file_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let r = load_folder(tmp.path());
        assert_eq!(r.status, AppStatus::Empty);
        assert!(r.groups.is_empty());
        assert!(r.dismissed.is_empty());
        assert_eq!(r.folder, tmp.path());
    }

    #[test]
    fn load_folder_with_empty_cache_reports_no_duplicates() {
        let tmp = tempfile::tempdir().unwrap();
        // `Cache::open` creates the .dedup/ dir + an empty schema.
        let _cache = Cache::open(tmp.path()).unwrap();
        let r = load_folder(tmp.path());
        assert_eq!(r.status, AppStatus::NoDuplicates);
        assert!(r.groups.is_empty());
    }

    // ---------------------------------------------------------------
    // Scan-state transitions (issue #21). These are pure, no GPUI —
    // they drive the project view's Scan button + progress bar logic.
    // ---------------------------------------------------------------

    #[test]
    fn default_scan_state_is_idle() {
        let s = AppState::new();
        assert!(matches!(s.scan_state, ScanState::Idle));
        assert!(!s.scan_state.is_running());
    }

    #[test]
    fn begin_scan_transitions_idle_to_running() {
        let mut s = AppState::new();
        let handles = s.begin_scan().expect("idle → running must succeed");
        assert!(s.scan_state.is_running());
        // Sink handed back to the caller must be the same one held in
        // state — otherwise the worker thread bumps one set of counters
        // and the UI polls a different set.
        match &s.scan_state {
            ScanState::Running {
                progress, cancel, ..
            } => {
                progress
                    .files_scanned
                    .fetch_add(3, std::sync::atomic::Ordering::Relaxed);
                assert_eq!(handles.progress.files_scanned(), 3);
                // Cancel flag must be shared Arc — flipping the one held
                // in state is visible to the handle.
                cancel.store(true, Ordering::Relaxed);
                assert!(handles.cancel.load(Ordering::Relaxed));
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn begin_scan_is_noop_while_running() {
        let mut s = AppState::new();
        let _ = s.begin_scan().unwrap();
        // Second call while still running must refuse to clobber state.
        assert!(s.begin_scan().is_none());
        assert!(s.scan_state.is_running());
    }

    #[test]
    fn begin_scan_clears_streaming_buffer() {
        let mut s = AppState::new();
        s.groups_streaming = vec![GroupView {
            id: -1,
            tier: Tier::A,
            label: "leftover".into(),
            occurrences: vec![occ("x.rs", 1, 2)],
        }];
        let _ = s.begin_scan().unwrap();
        assert!(s.groups_streaming.is_empty());
    }

    #[test]
    fn request_cancel_flips_flag_and_transitions_to_cancelling() {
        // The cancel flag must be the same Arc handed to the worker
        // thread, otherwise the scanner never sees the flip and the
        // 500 ms latency goal is unreachable.
        let mut s = AppState::new();
        let handles = s.begin_scan().unwrap();
        s.request_cancel();
        assert!(
            handles.cancel.load(Ordering::Relaxed),
            "request_cancel must flip the shared flag"
        );
        assert!(matches!(s.scan_state, ScanState::Cancelling { .. }));
    }

    #[test]
    fn cancel_completed_returns_to_idle_and_clears_stream() {
        let mut s = AppState::new();
        let _ = s.begin_scan();
        s.merge_streaming_groups(vec![GroupView {
            id: -1,
            tier: Tier::A,
            label: "x".into(),
            occurrences: vec![occ("x.rs", 1, 10)],
        }]);
        s.request_cancel();
        s.cancel_completed();
        assert!(matches!(s.scan_state, ScanState::Idle));
        assert!(s.groups_streaming.is_empty());
    }

    #[test]
    fn request_cancel_is_noop_when_idle() {
        let mut s = AppState::new();
        s.request_cancel();
        assert!(matches!(s.scan_state, ScanState::Idle));
    }

    #[test]
    fn finish_scan_moves_running_to_completed() {
        let mut s = AppState::new();
        let _ = s.begin_scan();
        s.finish_scan(7, 42, Duration::from_millis(3400));
        match s.scan_state {
            ScanState::Completed {
                group_count,
                file_count,
                duration,
            } => {
                assert_eq!(group_count, 7);
                assert_eq!(file_count, 42);
                assert_eq!(duration, Duration::from_millis(3400));
            }
            _ => panic!("expected Completed"),
        }
    }

    #[test]
    fn dismiss_completion_returns_to_idle() {
        let mut s = AppState::new();
        s.finish_scan(1, 1, Duration::from_secs(1));
        s.dismiss_completion();
        assert!(matches!(s.scan_state, ScanState::Idle));
    }

    #[test]
    fn dismiss_completion_from_running_is_noop() {
        // Defensive: if the auto-dismiss timer fires after a fresh scan
        // was started, the Running state must survive.
        let mut s = AppState::new();
        let _ = s.begin_scan();
        s.dismiss_completion();
        assert!(s.scan_state.is_running());
    }

    // -------------------------------------------------------------------
    // Impact-sort stability (issue #22).
    //
    // The streaming sidebar merges Tier A groups as they arrive; the
    // acceptance criterion requires "no visible shuffle". We prove that
    // by showing the binary-search-insert order equals the
    // sort-everything-at-once order for a shuffled input set.
    // -------------------------------------------------------------------

    fn streaming_group(id: i64, occurrences: Vec<OccurrenceView>) -> GroupView {
        GroupView {
            id,
            tier: Tier::A,
            // Label participates in the tiebreak; using the id makes
            // the ordering deterministic when impact collides.
            label: format!("g{id}"),
            occurrences,
        }
    }

    #[test]
    fn impact_key_is_higher_for_more_duplicated_code() {
        let small = streaming_group(1, vec![occ("a.rs", 1, 5), occ("b.rs", 1, 5)]);
        let big = streaming_group(
            2,
            vec![occ("a.rs", 1, 50), occ("b.rs", 1, 50), occ("c.rs", 1, 50)],
        );
        // Ascending key means descending impact: big.key < small.key.
        assert!(impact_key(&big) < impact_key(&small));
    }

    #[test]
    fn streaming_merge_stays_sorted_no_matter_the_arrival_order() {
        // Build a small corpus of groups with known impact values.
        let gs = vec![
            streaming_group(1, vec![occ("a.rs", 1, 5), occ("b.rs", 1, 5)]), // impact 12
            streaming_group(2, vec![occ("c.rs", 1, 20), occ("d.rs", 1, 20)]), // impact 84
            streaming_group(3, vec![occ("e.rs", 1, 3), occ("f.rs", 1, 3)]), // impact 8
            streaming_group(
                4,
                vec![occ("g.rs", 1, 10), occ("h.rs", 1, 10), occ("i.rs", 1, 10)],
            ), // impact 99
            streaming_group(5, vec![occ("j.rs", 1, 7), occ("k.rs", 1, 7)]), // impact 28
        ];

        // Reference: sort the full set at once.
        let mut reference = gs.clone();
        reference.sort_by_key(impact_key);
        let expected_ids: Vec<i64> = reference.iter().map(|g| g.id).collect();

        // Streaming: merge the same groups one-at-a-time in a shuffled
        // order. Every intermediate state must also be sorted (no
        // visible shuffle) and the final state must match the
        // reference.
        let shuffles: Vec<Vec<usize>> = vec![
            vec![0, 1, 2, 3, 4],
            vec![4, 3, 2, 1, 0],
            vec![2, 0, 4, 1, 3],
            vec![3, 1, 4, 0, 2],
        ];
        for order in shuffles {
            let mut s = AppState::new();
            for i in order {
                s.merge_streaming_groups(vec![gs[i].clone()]);
                // Intermediate invariant: groups_streaming is always
                // sorted by impact_key ascending.
                let keys: Vec<_> = s.groups_streaming.iter().map(impact_key).collect();
                let mut sorted = keys.clone();
                sorted.sort();
                assert_eq!(
                    keys, sorted,
                    "streaming buffer must stay sorted after every insert"
                );
            }
            let got_ids: Vec<i64> = s.groups_streaming.iter().map(|g| g.id).collect();
            assert_eq!(
                got_ids, expected_ids,
                "binary-search insert order must equal full-sort order"
            );
        }
    }

    #[test]
    fn streaming_merge_ignores_duplicate_ids() {
        let mut s = AppState::new();
        let g = streaming_group(1, vec![occ("a.rs", 1, 5), occ("b.rs", 1, 5)]);
        s.merge_streaming_groups(vec![g.clone()]);
        s.merge_streaming_groups(vec![g.clone()]);
        assert_eq!(s.groups_streaming.len(), 1);
    }

    #[test]
    fn format_elapsed_under_one_minute_is_one_decimal() {
        assert_eq!(format_elapsed(Duration::from_millis(300)), "0.3s");
        assert_eq!(format_elapsed(Duration::from_millis(1200)), "1.2s");
        assert_eq!(format_elapsed(Duration::from_millis(4500)), "4.5s");
        assert_eq!(format_elapsed(Duration::from_millis(59_900)), "59.9s");
    }

    #[test]
    fn format_elapsed_over_one_minute_is_integer() {
        assert_eq!(format_elapsed(Duration::from_secs(60)), "60s");
        assert_eq!(format_elapsed(Duration::from_secs(125)), "125s");
    }

    #[test]
    fn completion_banner_matches_acceptance_criterion() {
        // AC copy: "Scan complete — N groups across N files in Ns."
        let banner = format_completion_banner(7, 42, Duration::from_millis(3400));
        assert_eq!(
            banner,
            "Scan complete \u{2014} 7 groups across 42 files in 3.4s."
        );
    }
}
