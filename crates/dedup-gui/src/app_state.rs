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
    /// Detected language label — derived from the first occurrence's file
    /// extension ([`language_from_path`]). Used by the sidebar search box
    /// (issue #23) as one of the substring haystacks alongside `label` and
    /// `occurrence.path`. `None` when the extension is unknown.
    pub language: Option<String>,
    /// Stable content-hash for the group (rolling-hash normalized block),
    /// populated from the cache for loaded rows. Used by the sort
    /// tiebreaker so equal sort keys produce a deterministic order, and
    /// by the `x` → dismiss flow (#23) to pass the right key into
    /// [`dedup_core::Cache::dismiss_hash`]. `None` for streaming rows
    /// (Tier A inflight) — they get a synthetic tiebreaker via `id`.
    pub group_hash: Option<u64>,
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
    /// Sidebar substring search query (issue #23). Empty string disables
    /// filtering. Matched case-insensitively against each group's
    /// `label`, every `occurrence.path`, and its `language`.
    pub search_query: String,
    /// Sidebar sort key (issue #23). Defaults to [`SortKey::Impact`].
    pub sort_key: SortKey,
    /// Which pane currently has logical focus — drives ⌘1 / ⌘2 from
    /// issue #23 and mirrors the View menu items. Keyboard-nav actions
    /// (`j`/`k`, `Enter`, `x`, `o`) only fire when the sidebar is
    /// focused so text input elsewhere isn't hijacked.
    pub focused_pane: Pane,
    /// Cursor into the filtered+sorted sidebar list (issue #23). `None`
    /// when the list is empty. `NextGroup` / `PrevGroup` clamp this to
    /// `[0, visible_groups().len())` and update `selected_group`
    /// alongside so the detail pane follows.
    pub selected_group_idx: Option<usize>,
    /// Hashes of groups dismissed this session — merged into the
    /// "already suppressed" set so an `x` → dismiss action immediately
    /// removes the row from the sidebar without waiting for a cache
    /// reload. Persists only in memory; the real write is
    /// [`dedup_core::Cache::dismiss_hash`] (invoked by the `x` action
    /// handler in `project_view`).
    pub session_dismissed: std::collections::HashSet<u64>,
}

/// Which pane currently has logical focus.
///
/// Used by issue #23's keyboard shortcuts: `j` / `k` / `Enter` / `x` /
/// `o` are ignored unless [`Pane::Sidebar`] is focused, and `⌘1` / `⌘2`
/// flip this value. We don't tie the GPUI focus system in here — this is
/// a plain-data flag the view layer reads during render.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    #[default]
    Sidebar,
    Detail,
}

/// Sidebar sort key (issue #23). Default is [`SortKey::Impact`] — the same
/// "match size × occurrence count" ordering the streaming buffer uses so
/// switching from streaming → cache-backed preserves ordering.
///
/// All five keys use the group's `group_hash` (or `id` for streaming rows)
/// as the deterministic tiebreaker so `sort_groups` is stable for equal
/// keys across runs (acceptance criterion).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    /// Match size × occurrence count. Higher impact sorts first.
    #[default]
    Impact,
    /// Number of distinct file paths across the group's occurrences.
    /// Higher counts sort first.
    FileCount,
    /// Total duplicated line count across the group's occurrences.
    /// Longer blocks sort first.
    LineCount,
    /// A → Z by label.
    Alphabetical,
    /// Places the set that matches `search_query` first, then puts
    /// recently dismissed groups at the bottom. For the main (non-
    /// dismissed) list this behaves like `Alphabetical` but within the
    /// "Dismissed" section it reverses the cache's oldest-first order
    /// (most recent dismissal last → matches the acceptance criterion).
    RecentlyDismissed,
}

impl SortKey {
    /// All five variants, in the order shown in the dropdown. Kept as
    /// a slice so tests can iterate without depending on an
    /// `IntoEnumIterator`-style derive.
    pub const ALL: &'static [SortKey] = &[
        SortKey::Impact,
        SortKey::FileCount,
        SortKey::LineCount,
        SortKey::Alphabetical,
        SortKey::RecentlyDismissed,
    ];

    /// Human-readable label used in the sort dropdown.
    pub fn label(self) -> &'static str {
        match self {
            SortKey::Impact => "Impact",
            SortKey::FileCount => "File count",
            SortKey::LineCount => "Line count",
            SortKey::Alphabetical => "Alphabetical",
            SortKey::RecentlyDismissed => "Recently dismissed last",
        }
    }
}

/// Counts shown in the sidebar summary header (issue #23).
///
/// All fields are post-filter — the summary reflects the currently-visible
/// list, so `filter_groups` → `summary` is the usual composition.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SummaryCounts {
    pub groups: usize,
    pub functions: usize,
    pub blocks: usize,
    pub files: usize,
    pub duplicated_lines: usize,
}

impl SummaryCounts {
    /// Render the acceptance-criterion summary string:
    /// `"N groups · N functions · N blocks · N files · N duplicated lines"`.
    pub fn format(self) -> String {
        format!(
            "{g} groups \u{00B7} {fns} functions \u{00B7} {blks} blocks \u{00B7} {files} files \u{00B7} {lines} duplicated lines",
            g = self.groups,
            fns = self.functions,
            blks = self.blocks,
            files = self.files,
            lines = self.duplicated_lines,
        )
    }
}

/// Stable tiebreaker key for a group. Prefers the content hash (unique
/// per normalized block) with the id as a backup for streaming rows.
fn tiebreak_key(g: &GroupView) -> (u64, i64) {
    (g.group_hash.unwrap_or(0), g.id)
}

/// Sort `groups` by `key`, returning a fresh `Vec`. Stable under any
/// tie: groups with equal primary keys fall back to [`tiebreak_key`]
/// (content hash + id) so repeated calls are deterministic.
///
/// This is a pure function — no GPUI types, no `AppState` — so the
/// `cargo test` lane can exercise every sort order without touching
/// the main-thread-only GUI runtime.
pub fn sort_groups(groups: &[GroupView], key: SortKey) -> Vec<GroupView> {
    let mut out: Vec<GroupView> = groups.to_vec();
    match key {
        SortKey::Impact => {
            out.sort_by(|a, b| {
                impact_key(a)
                    .cmp(&impact_key(b))
                    .then_with(|| tiebreak_key(a).cmp(&tiebreak_key(b)))
            });
        }
        SortKey::FileCount => {
            out.sort_by(|a, b| {
                // Descending by file count, then tiebreaker.
                distinct_file_count(b)
                    .cmp(&distinct_file_count(a))
                    .then_with(|| tiebreak_key(a).cmp(&tiebreak_key(b)))
            });
        }
        SortKey::LineCount => {
            out.sort_by(|a, b| {
                total_line_count(b)
                    .cmp(&total_line_count(a))
                    .then_with(|| tiebreak_key(a).cmp(&tiebreak_key(b)))
            });
        }
        SortKey::Alphabetical => {
            out.sort_by(|a, b| {
                a.label
                    .to_lowercase()
                    .cmp(&b.label.to_lowercase())
                    .then_with(|| tiebreak_key(a).cmp(&tiebreak_key(b)))
            });
        }
        SortKey::RecentlyDismissed => {
            // Main list behaves like Alphabetical — "recently dismissed
            // last" is an ordering over the *dismissed* section (handled
            // separately in the view layer via the session-dismissed
            // append order). This keeps the main list in a stable,
            // predictable order when the user picks this sort mode.
            out.sort_by(|a, b| {
                a.label
                    .to_lowercase()
                    .cmp(&b.label.to_lowercase())
                    .then_with(|| tiebreak_key(a).cmp(&tiebreak_key(b)))
            });
        }
    }
    out
}

/// Case-insensitive substring filter over a group's `label`, every
/// `occurrence.path`, and its `language`. An empty query matches
/// everything (returns a clone of `groups`).
pub fn filter_groups(groups: &[GroupView], query: &str) -> Vec<GroupView> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return groups.to_vec();
    }
    groups
        .iter()
        .filter(|g| group_matches(g, &needle))
        .cloned()
        .collect()
}

fn group_matches(g: &GroupView, needle_lower: &str) -> bool {
    if g.label.to_lowercase().contains(needle_lower) {
        return true;
    }
    if let Some(lang) = &g.language
        && lang.to_lowercase().contains(needle_lower)
    {
        return true;
    }
    g.occurrences.iter().any(|o| {
        o.path
            .display()
            .to_string()
            .to_lowercase()
            .contains(needle_lower)
    })
}

/// Compute the summary counts shown above the sidebar.
///
/// Duplicated lines formula: `sum((end_line - start_line + 1) *
/// (occurrence_count - 1))` across every group — i.e. the *removable*
/// line count if deduplication were applied. Each group keeps one copy,
/// so we subtract 1 from the occurrence count. `occurrence_count == 1`
/// contributes 0 (no duplication).
pub fn summary(groups: &[GroupView]) -> SummaryCounts {
    use std::collections::HashSet;
    let mut files: HashSet<PathBuf> = HashSet::new();
    let mut functions = 0usize;
    let mut blocks = 0usize;
    let mut duplicated_lines = 0usize;

    for g in groups {
        match g.tier {
            Tier::B => functions += 1,
            Tier::A => blocks += 1,
        }
        let occ_count = g.occurrences.len();
        // Distinct file paths across all groups.
        for o in &g.occurrences {
            files.insert(o.path.clone());
        }
        if occ_count >= 2 {
            // All occurrences of a group have the same line span, but
            // the cache stores them individually — use the first.
            if let Some(first) = g.occurrences.first() {
                let span = (first.end_line - first.start_line + 1).max(0) as usize;
                duplicated_lines = duplicated_lines.saturating_add(span * (occ_count - 1));
            }
        }
    }

    SummaryCounts {
        groups: groups.len(),
        functions,
        blocks,
        files: files.len(),
        duplicated_lines,
    }
}

fn distinct_file_count(g: &GroupView) -> usize {
    use std::collections::HashSet;
    g.occurrences
        .iter()
        .map(|o| o.path.as_path())
        .collect::<HashSet<_>>()
        .len()
}

fn total_line_count(g: &GroupView) -> i64 {
    g.occurrences
        .iter()
        .map(|o| (o.end_line - o.start_line + 1).max(0))
        .sum()
}

/// Placeholder editor launcher for the `o` keyboard shortcut
/// (issue #23). Logs the paths and returns — the real launcher lands
/// with issue #29.
// TODO(#29): wire editor launcher.
pub fn open_in_editor(paths: &[&Path]) {
    tracing::info!(
        count = paths.len(),
        paths = ?paths,
        "open_in_editor (no-op placeholder — issue #29 wires real launcher)"
    );
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
        // Reset per-folder issue-#23 state so a re-open doesn't carry
        // stale search / selection from the previous folder.
        self.session_dismissed.clear();
        self.selected_group_idx = if self.groups.is_empty() {
            None
        } else {
            Some(0)
        };
        self.search_query.clear();
        // Keep `sort_key` — it's a user preference that should persist.
        self.focused_pane = Pane::Sidebar;
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

    // -----------------------------------------------------------------
    // Issue #23 — sidebar sort / filter / search / keyboard nav.
    // -----------------------------------------------------------------

    /// Current (post-session-dismiss) source list the sidebar filters +
    /// sorts from. Excludes anything the user has dismissed this session
    /// via `x`, mirroring the cache's `suppressed_hashes` filter applied
    /// on folder load.
    pub fn source_groups(&self) -> Vec<GroupView> {
        self.groups
            .iter()
            .filter(|g| match g.group_hash {
                Some(h) => !self.session_dismissed.contains(&h),
                None => true,
            })
            .cloned()
            .collect()
    }

    /// The filtered + sorted sidebar list (issue #23). Composition is
    /// `filter_groups(sort_groups(source_groups, sort_key), search_query)`
    /// so the order-then-filter behaviour is consistent with how the
    /// streaming buffer is rendered.
    pub fn visible_groups(&self) -> Vec<GroupView> {
        let sorted = sort_groups(&self.source_groups(), self.sort_key);
        filter_groups(&sorted, &self.search_query)
    }

    /// Current summary counts — always over the filtered list so the
    /// header updates as the user types (acceptance criterion).
    pub fn summary(&self) -> SummaryCounts {
        summary(&self.visible_groups())
    }

    /// Update the substring search query, recomputing the selection
    /// cursor so it stays in range of the filtered list.
    pub fn set_search_query(&mut self, query: String) {
        self.search_query = query;
        self.reclamp_selection();
    }

    /// Swap the sort key, keeping the selection on the same group-id
    /// when possible (so re-sort doesn't feel like a teleport).
    pub fn set_sort_key(&mut self, key: SortKey) {
        self.sort_key = key;
        self.reclamp_selection();
    }

    /// Move the sidebar cursor forward. Clamps at the bottom of the
    /// list (no wraparound — matches the issue-text choice). Updates
    /// `selected_group` so the detail pane follows.
    pub fn next_group(&mut self) {
        let visible = self.visible_groups();
        if visible.is_empty() {
            self.selected_group_idx = None;
            self.selected_group = None;
            return;
        }
        let next_idx = match self.selected_group_idx {
            None => 0,
            Some(i) if i + 1 < visible.len() => i + 1,
            Some(i) => i, // already at the bottom — clamp.
        };
        self.selected_group_idx = Some(next_idx);
        self.selected_group = visible.get(next_idx).map(|g| g.id);
    }

    /// Move the sidebar cursor backward. Clamps at the top.
    pub fn prev_group(&mut self) {
        let visible = self.visible_groups();
        if visible.is_empty() {
            self.selected_group_idx = None;
            self.selected_group = None;
            return;
        }
        let prev_idx = match self.selected_group_idx {
            None => 0,
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.selected_group_idx = Some(prev_idx);
        self.selected_group = visible.get(prev_idx).map(|g| g.id);
    }

    /// `Enter` handler — focus the detail pane without moving the cursor.
    pub fn activate_group(&mut self) {
        self.focused_pane = Pane::Detail;
    }

    /// `x` handler — dismiss the currently-selected group locally.
    ///
    /// Returns the `group_hash` of the dismissed row (if any) so the
    /// caller can persist it to [`dedup_core::Cache::dismiss_hash`].
    /// Locally we push a fresh [`SuppressionView`] onto `dismissed` and
    /// record the hash in `session_dismissed` so `visible_groups()`
    /// drops the row immediately. Selection clamps down to the new
    /// list length.
    pub fn dismiss_current_group(&mut self) -> Option<(u64, i64)> {
        let visible = self.visible_groups();
        let idx = self.selected_group_idx?;
        let group = visible.get(idx)?.clone();
        let hash = group.group_hash?;
        self.session_dismissed.insert(hash);
        // Append to dismissed so "recently dismissed" ends up last.
        self.dismissed.push(SuppressionView {
            hash_hex: format!("{hash:016x}"),
            last_group_id: Some(group.id),
        });
        self.reclamp_selection();
        Some((hash, group.id))
    }

    /// Change the focused pane (⌘1 / ⌘2).
    pub fn focus_pane(&mut self, pane: Pane) {
        self.focused_pane = pane;
    }

    /// After a filter / sort / dismiss change, snap the selection back
    /// into `[0, visible.len())`. Keeps the cursor on the same group-id
    /// if it's still in the list.
    fn reclamp_selection(&mut self) {
        let visible = self.visible_groups();
        if visible.is_empty() {
            self.selected_group_idx = None;
            self.selected_group = None;
            return;
        }
        // Prefer to track the currently-selected id if it's still in
        // the visible list.
        if let Some(id) = self.selected_group
            && let Some(pos) = visible.iter().position(|g| g.id == id)
        {
            self.selected_group_idx = Some(pos);
            return;
        }
        // Otherwise clamp to the last valid index.
        let new_idx = match self.selected_group_idx {
            Some(i) if i < visible.len() => i,
            Some(_) => visible.len() - 1,
            None => 0,
        };
        self.selected_group_idx = Some(new_idx);
        self.selected_group = visible.get(new_idx).map(|g| g.id);
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
    let language = occurrences
        .first()
        .and_then(|o| language_from_path(&o.path));
    GroupView {
        // Negative sentinel ids keep streaming rows distinguishable from
        // cache-backed rows. The index keeps each streaming id unique
        // within the current scan (same scan can emit many groups).
        id: -1 - index as i64,
        tier: group.tier,
        label,
        occurrences,
        language,
        // Streaming rows don't carry the cache `group_hash` — the post-
        // scan cache reload replaces them with rows that do.
        group_hash: None,
    }
}

/// Best-effort language label from a file extension.
///
/// Covers the languages the PRD / CLI already knows about (Rust + friends
/// from `LanguageProfile`). Returned string is a stable, human-readable
/// label (`"Rust"`, `"Python"`, etc.) — we compare it case-insensitively
/// in [`filter_groups`], but keeping the canonical form here is cheaper
/// than doing it on every keystroke.
///
/// Returns `None` for unknown or missing extensions; filter + summary
/// callers treat `None` the same as `Some("")` for search purposes.
pub fn language_from_path(path: &Path) -> Option<String> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    let label = match ext.as_str() {
        "rs" => "Rust",
        "py" | "pyi" => "Python",
        "js" | "mjs" | "cjs" => "JavaScript",
        "jsx" => "JSX",
        "ts" => "TypeScript",
        "tsx" => "TSX",
        "go" => "Go",
        "java" => "Java",
        "kt" | "kts" => "Kotlin",
        "c" | "h" => "C",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => "C++",
        "cs" => "C#",
        "rb" => "Ruby",
        "swift" => "Swift",
        "php" => "PHP",
        "scala" => "Scala",
        "sh" | "bash" | "zsh" => "Shell",
        _ => return None,
    };
    Some(label.to_string())
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
        let language = occurrences
            .first()
            .and_then(|o| language_from_path(&o.path));

        groups.push(GroupView {
            id: detail.id,
            tier: detail.tier,
            label,
            occurrences,
            language,
            group_hash: hash,
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
                    language: Some("Rust".into()),
                    group_hash: Some(0xAA),
                },
                GroupView {
                    id: 8,
                    tier: Tier::B,
                    label: "b".into(),
                    occurrences: vec![occ("b.rs", 3, 4)],
                    language: Some("Rust".into()),
                    group_hash: Some(0xBB),
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
                language: None,
                group_hash: None,
            },
            GroupView {
                id: 2,
                tier: Tier::B,
                label: "b".into(),
                occurrences: vec![],
                language: None,
                group_hash: None,
            },
            GroupView {
                id: 3,
                tier: Tier::A,
                label: "a2".into(),
                occurrences: vec![],
                language: None,
                group_hash: None,
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
            language: Some("Rust".into()),
            group_hash: None,
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
            language: Some("Rust".into()),
            group_hash: None,
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
            language: Some("Rust".into()),
            group_hash: None,
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
        let language = occurrences
            .first()
            .and_then(|o| language_from_path(&o.path));
        GroupView {
            id,
            tier: Tier::A,
            // Label participates in the tiebreak; using the id makes
            // the ordering deterministic when impact collides.
            label: format!("g{id}"),
            occurrences,
            language,
            group_hash: None,
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

    // -------------------------------------------------------------------
    // Issue #23 — sort / filter / search / summary + keyboard nav.
    //
    // All tests here exercise the pure functions in this module. No
    // GPUI types are constructed; every assertion is on plain data.
    // -------------------------------------------------------------------

    /// Construct a `GroupView` with explicit knobs for the sort/filter
    /// tests. Kept local so the production constructor stays minimal.
    fn mkgroup(
        id: i64,
        tier: Tier,
        label: &str,
        occurrences: Vec<OccurrenceView>,
        group_hash: Option<u64>,
    ) -> GroupView {
        let language = occurrences
            .first()
            .and_then(|o| language_from_path(&o.path));
        GroupView {
            id,
            tier,
            label: label.to_string(),
            occurrences,
            language,
            group_hash,
        }
    }

    #[test]
    fn language_from_path_covers_known_extensions() {
        assert_eq!(
            language_from_path(Path::new("src/a.rs")),
            Some("Rust".into())
        );
        assert_eq!(language_from_path(Path::new("A.PY")), Some("Python".into()));
        assert_eq!(language_from_path(Path::new("foo.tsx")), Some("TSX".into()));
        assert_eq!(language_from_path(Path::new("README")), None);
    }

    #[test]
    fn sort_groups_impact_puts_bigger_groups_first() {
        let small = mkgroup(
            1,
            Tier::A,
            "small",
            vec![occ("a.rs", 1, 5), occ("b.rs", 1, 5)],
            Some(0x11),
        );
        let big = mkgroup(
            2,
            Tier::A,
            "big",
            vec![occ("a.rs", 1, 50), occ("b.rs", 1, 50), occ("c.rs", 1, 50)],
            Some(0x22),
        );
        let out = sort_groups(&[small.clone(), big.clone()], SortKey::Impact);
        assert_eq!(out[0].id, 2, "big impact sorts first");
        assert_eq!(out[1].id, 1);
    }

    #[test]
    fn sort_groups_file_count_descending() {
        let one = mkgroup(
            1,
            Tier::A,
            "one",
            vec![occ("a.rs", 1, 5), occ("a.rs", 10, 14)],
            Some(0x11),
        );
        let three = mkgroup(
            2,
            Tier::A,
            "three",
            vec![occ("a.rs", 1, 5), occ("b.rs", 1, 5), occ("c.rs", 1, 5)],
            Some(0x22),
        );
        let out = sort_groups(&[one, three], SortKey::FileCount);
        let ids: Vec<i64> = out.iter().map(|g| g.id).collect();
        assert_eq!(ids, vec![2, 1]);
    }

    #[test]
    fn sort_groups_line_count_descending() {
        let short = mkgroup(1, Tier::A, "short", vec![occ("a.rs", 1, 4)], Some(0x11));
        let long = mkgroup(2, Tier::A, "long", vec![occ("a.rs", 1, 100)], Some(0x22));
        let out = sort_groups(&[short, long], SortKey::LineCount);
        assert_eq!(out[0].id, 2);
    }

    #[test]
    fn sort_groups_alphabetical_case_insensitive() {
        let z = mkgroup(1, Tier::A, "Zebra", vec![occ("z.rs", 1, 2)], Some(0x11));
        let a = mkgroup(2, Tier::A, "apple", vec![occ("a.rs", 1, 2)], Some(0x22));
        let m = mkgroup(3, Tier::A, "Mango", vec![occ("m.rs", 1, 2)], Some(0x33));
        let out = sort_groups(&[z, a, m], SortKey::Alphabetical);
        let labels: Vec<&str> = out.iter().map(|g| g.label.as_str()).collect();
        assert_eq!(labels, vec!["apple", "Mango", "Zebra"]);
    }

    #[test]
    fn sort_groups_stable_tiebreak_on_hash() {
        // Two groups with identical impact + file count + line count +
        // label. The hash is the final tiebreaker — the smaller hash
        // must come first under any sort key.
        let a = mkgroup(
            1,
            Tier::A,
            "same",
            vec![occ("a.rs", 1, 10)],
            Some(0x0000_00AA),
        );
        let b = mkgroup(
            2,
            Tier::A,
            "same",
            vec![occ("b.rs", 1, 10)],
            Some(0x0000_00BB),
        );
        for key in SortKey::ALL {
            let out = sort_groups(&[b.clone(), a.clone()], *key);
            assert_eq!(
                out[0].id, 1,
                "key {key:?}: tiebreaker must be deterministic on hash"
            );
        }
    }

    #[test]
    fn filter_groups_empty_query_matches_everything() {
        let g = mkgroup(1, Tier::A, "label", vec![occ("a.rs", 1, 2)], Some(0));
        let out = filter_groups(std::slice::from_ref(&g), "");
        assert_eq!(out.len(), 1);
        let out = filter_groups(std::slice::from_ref(&g), "   ");
        assert_eq!(out.len(), 1, "whitespace-only query is also a no-op");
    }

    #[test]
    fn filter_groups_matches_label_case_insensitive() {
        let g = mkgroup(
            1,
            Tier::A,
            "validate_email",
            vec![occ("auth.rs", 1, 2)],
            Some(0),
        );
        assert_eq!(filter_groups(std::slice::from_ref(&g), "VALIDATE").len(), 1);
        assert_eq!(filter_groups(std::slice::from_ref(&g), "nomatch").len(), 0);
    }

    #[test]
    fn filter_groups_matches_path_substring() {
        let g = mkgroup(
            1,
            Tier::A,
            "x",
            vec![occ("src/auth/login.rs", 1, 2)],
            Some(0),
        );
        assert_eq!(filter_groups(std::slice::from_ref(&g), "auth").len(), 1);
        assert_eq!(filter_groups(std::slice::from_ref(&g), "missing/").len(), 0);
    }

    #[test]
    fn filter_groups_matches_language_case_insensitive() {
        let g = mkgroup(1, Tier::A, "x", vec![occ("a.rs", 1, 2)], Some(0));
        assert_eq!(filter_groups(std::slice::from_ref(&g), "rust").len(), 1);
        assert_eq!(filter_groups(std::slice::from_ref(&g), "RUST").len(), 1);
    }

    #[test]
    fn summary_counts_groups_functions_blocks_files_lines() {
        let groups = vec![
            // Tier B (function), 10 lines, 3 occurrences → 10 * 2 = 20
            // duplicated lines. 2 distinct paths (a.rs, b.rs — c.rs too).
            mkgroup(
                1,
                Tier::B,
                "fn f",
                vec![occ("a.rs", 1, 10), occ("b.rs", 1, 10), occ("c.rs", 1, 10)],
                Some(0x1),
            ),
            // Tier A (block), 5 lines, 2 occurrences → 5 * 1 = 5
            // duplicated lines. `b.rs` already counted above — only
            // `d.rs` is new.
            mkgroup(
                2,
                Tier::A,
                "block",
                vec![occ("b.rs", 20, 24), occ("d.rs", 1, 5)],
                Some(0x2),
            ),
            // Singleton — not really a duplicate; 0 duplicated lines.
            mkgroup(3, Tier::A, "lonely", vec![occ("e.rs", 1, 3)], Some(0x3)),
        ];
        let s = summary(&groups);
        assert_eq!(s.groups, 3);
        assert_eq!(s.functions, 1);
        assert_eq!(s.blocks, 2);
        // Distinct paths: a.rs, b.rs, c.rs, d.rs, e.rs = 5.
        assert_eq!(s.files, 5);
        assert_eq!(s.duplicated_lines, 20 + 5);
    }

    #[test]
    fn summary_format_uses_middle_dots() {
        let s = SummaryCounts {
            groups: 3,
            functions: 1,
            blocks: 2,
            files: 5,
            duplicated_lines: 25,
        };
        assert_eq!(
            s.format(),
            "3 groups \u{00B7} 1 functions \u{00B7} 2 blocks \u{00B7} 5 files \u{00B7} 25 duplicated lines"
        );
    }

    // ---- AppState cursor + dismiss -----------------------------------

    fn loaded_state_with(groups: Vec<GroupView>) -> AppState {
        let mut s = AppState::new();
        let result = FolderLoadResult {
            folder: PathBuf::from("/tmp/x"),
            groups,
            dismissed: vec![],
            status: AppStatus::Loaded,
        };
        s.set_folder_result(result);
        s
    }

    #[test]
    fn visible_groups_respects_sort_and_filter() {
        let s = loaded_state_with(vec![
            mkgroup(1, Tier::A, "apple", vec![occ("a.rs", 1, 5)], Some(0x1)),
            mkgroup(2, Tier::A, "banana", vec![occ("b.py", 1, 5)], Some(0x2)),
            mkgroup(3, Tier::A, "cherry", vec![occ("c.rs", 1, 5)], Some(0x3)),
        ]);
        // Default: Impact sort + empty query → all three visible.
        assert_eq!(s.visible_groups().len(), 3);

        // Switch sort + apply a Python-only filter.
        let mut s2 = s.clone();
        s2.set_sort_key(SortKey::Alphabetical);
        s2.set_search_query("python".into());
        let got: Vec<i64> = s2.visible_groups().iter().map(|g| g.id).collect();
        assert_eq!(got, vec![2]);
    }

    #[test]
    fn summary_updates_with_filter() {
        let mut s = loaded_state_with(vec![
            mkgroup(
                1,
                Tier::A,
                "apple",
                vec![occ("a.rs", 1, 5), occ("a2.rs", 1, 5)],
                Some(0x1),
            ),
            mkgroup(
                2,
                Tier::A,
                "banana",
                vec![occ("b.py", 1, 5), occ("b2.py", 1, 5)],
                Some(0x2),
            ),
        ]);
        assert_eq!(s.summary().groups, 2);
        s.set_search_query("python".into());
        assert_eq!(s.summary().groups, 1);
        assert_eq!(s.summary().files, 2);
    }

    #[test]
    fn next_and_prev_group_clamp_at_ends() {
        let mut s = loaded_state_with(vec![
            mkgroup(1, Tier::A, "a", vec![occ("a.rs", 1, 2)], Some(0x1)),
            mkgroup(2, Tier::A, "b", vec![occ("b.rs", 1, 2)], Some(0x2)),
            mkgroup(3, Tier::A, "c", vec![occ("c.rs", 1, 2)], Some(0x3)),
        ]);
        // Default selection = first.
        assert_eq!(s.selected_group_idx, Some(0));
        s.next_group();
        assert_eq!(s.selected_group_idx, Some(1));
        s.next_group();
        assert_eq!(s.selected_group_idx, Some(2));
        // Clamp at bottom — no wraparound.
        s.next_group();
        assert_eq!(s.selected_group_idx, Some(2));
        // Walk backwards.
        s.prev_group();
        s.prev_group();
        s.prev_group();
        assert_eq!(s.selected_group_idx, Some(0), "clamp at top");
    }

    #[test]
    fn activate_group_focuses_detail_pane() {
        let mut s = loaded_state_with(vec![mkgroup(
            1,
            Tier::A,
            "a",
            vec![occ("a.rs", 1, 2)],
            Some(0x1),
        )]);
        assert_eq!(s.focused_pane, Pane::Sidebar);
        s.activate_group();
        assert_eq!(s.focused_pane, Pane::Detail);
    }

    #[test]
    fn dismiss_current_group_moves_it_out_of_visible_list() {
        let mut s = loaded_state_with(vec![
            mkgroup(1, Tier::A, "a", vec![occ("a.rs", 1, 2)], Some(0xAA)),
            mkgroup(2, Tier::A, "b", vec![occ("b.rs", 1, 2)], Some(0xBB)),
        ]);
        // Sort by Alphabetical so order is deterministic.
        s.set_sort_key(SortKey::Alphabetical);
        let before = s.visible_groups();
        assert_eq!(before.len(), 2);
        assert_eq!(s.selected_group, Some(1));

        let out = s.dismiss_current_group();
        assert_eq!(out, Some((0xAA, 1)));

        let after = s.visible_groups();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].id, 2);
        // Dismissed row appended to the dismissed list.
        assert_eq!(s.dismissed.len(), 1);
        assert_eq!(s.dismissed.last().unwrap().last_group_id, Some(1));
        // Selection clamps to the remaining group.
        assert_eq!(s.selected_group, Some(2));
        assert_eq!(s.selected_group_idx, Some(0));
    }

    #[test]
    fn dismiss_current_group_is_noop_when_hash_missing() {
        // Streaming rows have `group_hash = None` and therefore can't
        // be dismissed — the action should return `None` rather than
        // panicking.
        let mut s = loaded_state_with(vec![mkgroup(
            -1,
            Tier::A,
            "streaming",
            vec![occ("a.rs", 1, 2)],
            None,
        )]);
        assert_eq!(s.dismiss_current_group(), None);
        assert_eq!(s.visible_groups().len(), 1);
    }

    #[test]
    fn focus_pane_flips_between_panes() {
        let mut s = AppState::new();
        s.focus_pane(Pane::Detail);
        assert_eq!(s.focused_pane, Pane::Detail);
        s.focus_pane(Pane::Sidebar);
        assert_eq!(s.focused_pane, Pane::Sidebar);
    }
}
