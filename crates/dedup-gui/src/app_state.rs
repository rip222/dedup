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

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::blame::{BlameCacheKey, BlameInfo};
use crate::detail_rows::DetailRowsCache;

use dedup_core::editor::{EditorConfig, EditorError, EnvPathLookup};
use dedup_core::{
    AtomicProgressSink, Cache, CacheError, Config, ConfigError, DetailConfig, FileIssue,
    MatchGroup, Tier,
};

use crate::toast::{
    ACTION_CACHE_DELETE_AND_RESCAN, ACTION_CACHE_RESCAN, ACTION_REMOVE_STALE_RECENT,
    ACTION_SHOW_ISSUES, ACTION_UNDO_DISMISS, CacheErrorClass, ToastAction, ToastKind, ToastStack,
    UNDO_TTL, classify_cache_error,
};

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

impl GroupView {
    /// Severity (issue #56) — `occurrence_count × total_lines_of_code`
    /// summed across occurrences. Used by [`SortKey::Severity`] to
    /// surface the highest-leverage duplicates at the top of the
    /// sidebar. `saturating_*` everywhere so a pathological group
    /// (millions of 1-line occurrences) never panics in debug builds.
    ///
    /// Exposed on `GroupView` (not just a private helper) so the CLI
    /// + future UI surfaces can render the same number.
    pub fn severity(&self) -> u64 {
        let total_lines: u64 = self
            .occurrences
            .iter()
            .map(|o| {
                (o.end_line
                    .saturating_sub(o.start_line)
                    .saturating_add(1))
                .max(0) as u64
            })
            .sum();
        (self.occurrences.len() as u64).saturating_mul(total_lines)
    }

    /// First occurrence path, lowercased, for the Severity sort's
    /// alphabetical tiebreaker (#56 acceptance criterion). Empty
    /// string for a groupless row so the comparison is total.
    fn first_path_lower(&self) -> String {
        self.occurrences
            .first()
            .map(|o| o.path.to_string_lossy().to_lowercase())
            .unwrap_or_default()
    }
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
    /// Per-occurrence alpha-rename spans for Tier B tint overlay
    /// (issue #25). Empty for Tier A occurrences; Tier B occurrences
    /// carry one entry per alpha-renamed identifier leaf:
    /// `(start_byte, end_byte, placeholder_idx)` in absolute file
    /// bytes. Same `placeholder_idx` across occurrences of a group
    /// refers to the same logical local — the GUI paints matching
    /// indices the same color.
    pub alpha_rename_spans: Vec<(usize, usize, u32)>,
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
/// last-group-id); the original source content is not recoverable as
/// of the dismissal, so the sidebar label falls back to
/// `Dismissed block (hash <12-hex>…)` when we cannot resolve any live
/// `match_groups` row for the `last_group_id`. When the group IS still
/// in the cache (the common case — dismissals don't delete rows, only
/// hide them), `occurrences` carries the materialized occurrence list
/// so the detail pane can show the same code body it would show for a
/// live group, with a read-only banner on top (#54).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuppressionView {
    /// Normalized-block-hash — the stable key the cache uses. Kept as a
    /// `u64` so GUI handlers can pass it straight back to
    /// `Cache::undismiss` without reparsing a hex string.
    pub hash: u64,
    /// Pre-formatted 16-char hex rendering of `hash`. Cached so the
    /// sidebar label is a cheap slice rather than a fresh format on
    /// every frame.
    pub hash_hex: String,
    /// The `match_groups.id` the user last saw this group as. Doubles
    /// as the selection key when the user clicks the dismissed row —
    /// [`AppState::selected_group`] always stores a group-id, and
    /// dismissed rows plug the cache's live id in here so the selection
    /// hops straight to the read-only detail view (#54).
    pub last_group_id: Option<i64>,
    /// Unix-epoch seconds at which the group was dismissed. `0` when we
    /// couldn't read the cache timestamp (legacy in-memory dismissals
    /// recorded via [`AppState::dismiss_group`] fall through to
    /// [`unix_now`] via the core `dismiss_hash` call — see the
    /// doc-comment on that field).
    pub dismissed_at: i64,
    /// Materialized occurrence list (if the underlying `match_groups`
    /// row is still in the cache). Empty for rows where the cache has
    /// been rewritten since the dismissal and the hash no longer
    /// resolves — the detail pane falls back to a short "(source no
    /// longer in cache)" notice in that case.
    pub occurrences: Vec<OccurrenceView>,
    /// Per-occurrence dismissals still attached to this group (#54).
    /// Each entry carries `(path, dismissed_at)` so the banner can
    /// enumerate them with their own `[Restore]` controls. Only
    /// populated at folder-load time; restores mutate this list
    /// in place.
    pub occurrence_dismissals: Vec<OccurrenceDismissal>,
    /// Tier the underlying group was detected at (#61). Drives the
    /// sidebar A/B badge on dismissed rows so the visual parity with
    /// live rows holds. Falls back to `Tier::A` when the cache row
    /// backing the dismissed hash can no longer be resolved — the
    /// badge is a hint, not ground truth, and Tier A is the safer
    /// default (it doesn't imply alpha-rename behaviour).
    pub tier: Tier,
}

/// One per-occurrence dismissal still tied to a dismissed group (#54).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OccurrenceDismissal {
    pub path: PathBuf,
    pub dismissed_at: i64,
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

/// Unix-epoch seconds as `i64`, saturating on clock skew / pre-1970
/// clocks. Used for in-memory dismissal timestamps — the on-disk
/// value comes from the cache's own `now_unix_seconds` and is what
/// the banner shows after a reload.
pub fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
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
#[derive(Debug, Clone)]
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
    /// Sidebar sort key (issue #23, #56). Defaults to [`SortKey::Severity`]
    /// for fresh installs; a loaded pref file can override to any persisted
    /// variant (see [`crate::sidebar_prefs::SidebarPrefs::sort_key`]).
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
    /// Per-occurrence (group_hash, relative_path) pairs dismissed this
    /// session (#27). Mirrors `session_dismissed` but at occurrence
    /// granularity. The UI filters `selected_occurrences()` against
    /// this set so a row disappears immediately on click; the real
    /// write is [`dedup_core::Cache::dismiss_occurrence`] (invoked by
    /// the `[×]` button handler in `project_view`).
    pub session_occurrence_dismissed: HashSet<(u64, PathBuf)>,
    /// Multi-select checkbox state for the group toolbar (#27). Maps
    /// `group_id` → set of *indices* within that group's
    /// `visible_occurrences` list. Indices are the GUI's position
    /// order (cache path-asc, start-line-asc) rather than cache row
    /// ids because occurrences don't expose their own id to the view
    /// model.
    pub selected_occurrence_indices: HashMap<i64, HashSet<usize>>,
    /// Occurrences whose code body is collapsed (#45). Keyed by
    /// `(group_id, occ_idx)` where `occ_idx` is the visible-list
    /// position within that group. Default is "none collapsed".
    /// `collapse_all_in_active_group` populates this with every
    /// occurrence of the active group; `expand_all_in_active_group`
    /// clears those entries; per-header clicks toggle a single pair.
    ///
    /// Per-occurrence (rather than per-group) so each card collapses
    /// independently — the old group-level model hid every row of
    /// every occurrence at once, including the headers.
    pub collapsed_occurrences: HashSet<(i64, usize)>,
    /// GUI detail-pane tunables (issue #26). Cached on folder open —
    /// not reloaded per frame. Currently carries just
    /// [`DetailConfig::context_lines`] (number of dimmed before/after
    /// context lines).
    pub detail_config: DetailConfig,
    /// File → Open Recent MRU list (issue #28). Loaded from
    /// `recent.json` at app startup via [`AppState::load_from_disk`]; the
    /// menubar renders entries directly off this field. Every successful
    /// folder open pushes the folder to the front and persists.
    pub recent_projects: crate::recent::RecentProjects,
    /// Transient "stale-entry" banner (issue #28). Non-`None` when the
    /// user clicked an Open Recent entry whose path no longer exists;
    /// the banner exposes a `[Remove from recents]` action that calls
    /// [`AppState::remove_recent`] and dismisses itself.
    ///
    // TODO(#30): promote to toast — #28 uses an inline banner because
    // the real toast system lands in #30.
    pub recent_banner: Option<RecentBanner>,
    /// Inline banner surfacing editor-launch failures (issue #29). The
    /// only variant today is "no editor found on PATH" — the message
    /// matches the AC verbatim. Promoted to toast alongside #30.
    pub editor_banner: Option<EditorBanner>,
    /// The active editor config. Default is `nvim` + `auto` terminal.
    /// Populated from `[editor]` in `config.toml` at project load time
    /// (see [`AppState::set_editor_config`]).
    pub editor_config: EditorConfig,
    /// Whether the Preferences dialog is open (issue #29). The dialog
    /// is an inline modal overlay rather than a native window — see
    /// the PR body for the GPUI-primitives compromise.
    pub preferences_open: bool,
    /// Whether the sidebar sort-dropdown popup is open (issue #46).
    /// Toggled by clicking the `Sort: <key>` button; auto-closed by
    /// `set_sort_key` and by a click on the full-window scrim.
    pub sort_popup_open: bool,
    /// Live toast queue (issue #30). Pushed to by error classifiers,
    /// the scan-complete flow, and background-panic propagation. The
    /// 500ms tick timer in `ProjectView::start_toast_ticker` drops
    /// expired auto-dismiss toasts.
    pub toasts: ToastStack,
    /// Startup-error modal state (issue #30). Non-`None` when
    /// `Config::load` failed at launch: the app runs in "degraded"
    /// mode with a modal offering `[Fix config]` / `[Reset to
    /// defaults]`. Cleared on successful retry.
    pub startup_error: Option<StartupError>,
    /// Issues from the most recent scan (issue #30). Mirrors
    /// `ScanResult::issues` so the post-scan sidebar link has
    /// something to render a count off, and the issues dialog has
    /// detail rows to populate.
    pub scan_issues: Vec<FileIssue>,
    /// Whether the post-scan issues dialog is open (issue #30). Toggled
    /// by the sidebar "N files had issues" link.
    pub scan_issues_open: bool,
    /// Current sidebar width in pixels (issue #47). Mutated through
    /// [`AppState::set_sidebar_width`], which clamps into
    /// [`crate::sidebar_prefs::MIN_SIDEBAR_WIDTH`] ..=
    /// [`crate::sidebar_prefs::MAX_SIDEBAR_WIDTH`] and returns early on
    /// no-op writes so the drag-move handler doesn't churn the view.
    /// Persistence lives in [`crate::sidebar_prefs::SidebarPrefs`] —
    /// loaded at startup (see [`crate::lib`]) and written on drag end
    /// via [`AppState::persist_sidebar_prefs`].
    pub sidebar_width: f32,
    /// Whether the sidebar is hidden (issue #52). Toggled by ⌘B / the
    /// View → Toggle Sidebar menu item via
    /// [`AppState::toggle_sidebar_visible`]. Persists alongside
    /// `sidebar_width` in `sidebar.json` so visibility survives across
    /// window close and reopen.
    pub sidebar_hidden: bool,
    /// Cached detail-pane rows (issue #49). Populated on the first
    /// `render_detail` call after the state mutates; invalidated when
    /// the cache-key fingerprint changes (group selection, per-
    /// occurrence collapse, occurrence list, selection set, session-
    /// dismissed set, or `DetailConfig::context_lines`).
    ///
    /// `RefCell` because `render_detail` takes `&AppState` and must
    /// populate the cache on miss without forcing every caller to hold
    /// `&mut AppState`. Not thread-shared (the GUI view-model lives on
    /// the main thread), so `RefCell` is sufficient.
    pub detail_rows_cache: RefCell<Option<DetailRowsCache>>,
    /// Git-blame overlay cache (issue #58). Keyed by
    /// `(abs_path, start_line, file_mtime)` so edits invalidate
    /// automatically. `Option<BlameInfo>` — `None` is still a cached
    /// answer (non-git folder, timeout, blame failure) so we don't
    /// re-shell-out on every frame. Interior mutability: the header
    /// renderer runs under `&AppState` and populates on miss.
    pub blame_cache: RefCell<HashMap<BlameCacheKey, Option<BlameInfo>>>,
    /// Pending undo payloads keyed by the live toast id (issue #60).
    /// Populated by [`AppState::push_undo_toast_for_group`] /
    /// [`AppState::push_undo_toast_for_occurrence`] at dismiss time and
    /// drained by [`AppState::take_pending_undo`] when the user clicks
    /// the toast's `[Undo]` button. [`AppState::tick_toasts`] GCs
    /// entries whose toast auto-dismissed without the user acting — a
    /// stale entry is otherwise harmless (the `toast_id` never recycles
    /// because `ToastStack::next_id` is monotonic) but keeping the map
    /// tight makes the test assertions cleaner.
    pub pending_undos: HashMap<u64, UndoKind>,
}

/// What to restore when the user clicks `[Undo]` on a dismiss toast
/// (issue #60). Stored in [`AppState::pending_undos`] keyed by toast
/// id; the view layer's action dispatcher looks the entry up and
/// routes to `restore_group_click` / `restore_occurrence_click`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UndoKind {
    /// Undo a whole-group dismissal. Restores the row via
    /// `Cache::undismiss(hash)`.
    Group { hash: u64 },
    /// Undo a per-occurrence dismissal. Restores via
    /// `Cache::undismiss_occurrence(hash, path)`.
    Occurrence { hash: u64, path: PathBuf },
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            current_folder: None,
            groups: Vec::new(),
            dismissed: Vec::new(),
            selected_group: None,
            status: AppStatus::default(),
            dismissed_expanded: false,
            scan_state: ScanState::default(),
            groups_streaming: Vec::new(),
            search_query: String::new(),
            sort_key: SortKey::default(),
            focused_pane: Pane::default(),
            selected_group_idx: None,
            session_dismissed: std::collections::HashSet::new(),
            session_occurrence_dismissed: HashSet::new(),
            selected_occurrence_indices: HashMap::new(),
            collapsed_occurrences: HashSet::new(),
            detail_config: DetailConfig::default(),
            recent_projects: crate::recent::RecentProjects::default(),
            recent_banner: None,
            editor_banner: None,
            editor_config: EditorConfig::default(),
            preferences_open: false,
            sort_popup_open: false,
            toasts: ToastStack::default(),
            startup_error: None,
            scan_issues: Vec::new(),
            scan_issues_open: false,
            sidebar_width: crate::sidebar_prefs::DEFAULT_SIDEBAR_WIDTH,
            sidebar_hidden: false,
            detail_rows_cache: RefCell::new(None),
            blame_cache: RefCell::new(HashMap::new()),
            pending_undos: HashMap::new(),
        }
    }
}

/// Startup-error payload surfaced when `Config::load` fails at launch.
///
/// The GUI cannot proceed normally without a config, but we also don't
/// want to crash — instead the window paints an inline modal with
/// `[Fix config]` (opens the offending file in `$EDITOR`) and
/// `[Reset to defaults]` (overwrites the file with a defaults-only
/// TOML and retries). `path` points at the file that failed to load
/// (for "Fix config"); `message` is the human-readable error string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupError {
    pub path: PathBuf,
    pub message: String,
}

impl StartupError {
    /// Build a [`StartupError`] from a [`ConfigError`]. Picks the best
    /// `path` the variant carries (falling back to the global config
    /// path) so the "Fix config" button always has a file to open.
    pub fn from_config_error(err: &ConfigError) -> Self {
        let path = match err {
            ConfigError::Parse { path, .. } => path.clone(),
            ConfigError::SchemaVersionMismatch { path, .. } => path.clone(),
            _ => Config::global_path(),
        };
        Self {
            path,
            message: err.to_string(),
        }
    }
}

/// Inline banner used to surface a stale Open Recent entry. The
/// `[Remove from recents]` button wipes `path` from the MRU and
/// dismisses the banner. Not a full toast — that's issue #30.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentBanner {
    /// The missing/moved path the user clicked on.
    pub path: PathBuf,
}

/// Inline banner for editor-launch failures (issue #29). Holds the
/// human-readable message; the banner's button dispatches
/// `DismissEditorBanner` to close it. Not a full toast — that's issue
/// #30.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorBanner {
    pub message: String,
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

/// Sidebar sort key (issue #23, #56). Default is [`SortKey::Severity`] —
/// `occurrence_count × total_lines_of_code` summed across occurrences
/// so the highest-leverage dupes surface at the top (#56).
///
/// All variants use the group's `group_hash` (or `id` for streaming rows)
/// as the deterministic tiebreaker so `sort_groups` is stable for equal
/// keys across runs (acceptance criterion). For Severity specifically
/// ties fall back to path-alphabetical first, then the hash/id tiebreaker.
#[derive(
    Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize,
)]
pub enum SortKey {
    /// Occurrence count × total duplicated LOC (issue #56). Higher
    /// severity sorts first. Surfaces the highest-leverage duplicates
    /// at the top of the sidebar.
    #[default]
    Severity,
    /// Match size × occurrence count. Higher impact sorts first.
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
    /// All variants, in the order shown in the dropdown. Kept as
    /// a slice so tests can iterate without depending on an
    /// `IntoEnumIterator`-style derive.
    pub const ALL: &'static [SortKey] = &[
        SortKey::Severity,
        SortKey::Impact,
        SortKey::FileCount,
        SortKey::LineCount,
        SortKey::Alphabetical,
        SortKey::RecentlyDismissed,
    ];

    /// Human-readable label used in the sort dropdown.
    pub fn label(self) -> &'static str {
        match self {
            SortKey::Severity => "Severity",
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
        SortKey::Severity => {
            out.sort_by(|a, b| {
                // Descending by severity — higher-leverage dupes first.
                // Ties fall back to path-alphabetical for stability
                // (acceptance criterion #56), then the content-hash /
                // id tiebreaker for determinism across runs.
                b.severity()
                    .cmp(&a.severity())
                    .then_with(|| a.first_path_lower().cmp(&b.first_path_lower()))
                    .then_with(|| tiebreak_key(a).cmp(&tiebreak_key(b)))
            });
        }
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

/// Back-compat shim for the `o` keyboard shortcut (issue #23). Opens
/// every path at line 1 using the default editor config (nvim with
/// `vim` fallback). Kept for callers that don't have line info handy.
///
/// Prefer [`launch_editor`] when the caller has `(path, line)` pairs
/// — it routes line numbers through to the preset's template (issue
/// #29).
pub fn open_in_editor(paths: &[&Path]) {
    let targets: Vec<(PathBuf, u32)> = paths.iter().map(|p| (p.to_path_buf(), 1_u32)).collect();
    let _ = launch_editor(&EditorConfig::default(), &targets);
}

/// Real launcher entry point (issue #29). Resolves the configured
/// preset (with `nvim → vim` fallback) and spawns the built
/// [`dedup_core::editor::CommandSpec`]s. Errors are logged but never
/// panic; the caller surfaces the "No editor found" banner when the
/// returned `Result` is `Err(EditorError::NoEditor)`.
pub fn launch_editor(cfg: &EditorConfig, targets: &[(PathBuf, u32)]) -> Result<(), EditorError> {
    if targets.is_empty() {
        return Ok(());
    }
    match dedup_core::editor::launch(cfg, targets, &EnvPathLookup) {
        Ok(resolved) => {
            if resolved.fell_back {
                tracing::info!(
                    preset = %resolved.preset,
                    "editor: fell back to vim (nvim not on PATH)",
                );
            } else {
                tracing::debug!(preset = %resolved.preset, "editor: launched");
            }
            Ok(())
        }
        Err(e) => {
            tracing::warn!(error = %e, "editor: launch failed");
            Err(e)
        }
    }
}

impl AppState {
    /// Fresh state — no folder open, start-here empty view.
    pub fn new() -> Self {
        Self::default()
    }

    /// Fresh state with recents hydrated from `~/.config/dedup/recent.json`.
    ///
    /// This is the entry point `ProjectView::new` uses at app startup.
    /// Unit tests prefer [`AppState::new`] so they don't touch real
    /// `$HOME`; the MRU-specific tests round-trip through explicit
    /// paths in a tempdir instead.
    pub fn with_recents_from_disk() -> Self {
        Self {
            recent_projects: crate::recent::RecentProjects::load_from_disk(),
            ..Self::default()
        }
    }

    // -----------------------------------------------------------------
    // Issue #28 — Open Recent MRU helpers.
    //
    // Mutations are split across "bump the list" + "persist to disk".
    // Persist failures are logged at debug level and swallowed; the
    // in-memory MRU is the source of truth for the current session, and
    // the menubar already handles an out-of-date `recent.json` (reload
    // returns empty on parse errors).
    // -----------------------------------------------------------------

    /// Push `path` to the front of the recents MRU and persist.
    ///
    /// Called after a successful `File → Open…` (or click on a recent
    /// entry) — i.e. anywhere [`Self::set_folder_result`] just applied a
    /// non-[`AppStatus::Error`] result. Errors are never pushed (see the
    /// caller in `project_view`) so a failed open doesn't pollute the
    /// list.
    pub fn push_recent(&mut self, path: impl Into<PathBuf>) {
        self.recent_projects.push(path);
        self.persist_recents();
    }

    /// Drop a single recent entry + persist. Used by the banner's
    /// `[Remove from recents]` button.
    pub fn remove_recent(&mut self, path: &Path) {
        self.recent_projects.remove(path);
        self.persist_recents();
    }

    /// Wipe every recent + persist. Used by File → Open Recent → Clear
    /// Menu.
    pub fn clear_recents(&mut self) {
        self.recent_projects.clear();
        self.persist_recents();
    }

    // -----------------------------------------------------------------
    // Issue #30 — toast + startup-error helpers.
    //
    // All GPUI-free so the test lane can drive the state machine
    // without spinning up the runloop. The project-view layer owns
    // rendering + the 500ms tick timer.
    // -----------------------------------------------------------------

    /// Push an Error toast with a title + action button. Used by the
    /// cache-corruption / cache-newer-schema / editor-launch-failure
    /// flows. Returns the id so callers can dismiss on action.
    pub fn push_error_toast(
        &mut self,
        title: impl Into<String>,
        body: Option<String>,
        action: Option<ToastAction>,
    ) -> u64 {
        self.toasts
            .push_full(ToastKind::Error, title.into(), body, action)
    }

    /// Push a Warning toast (yellow, 5s auto-dismiss).
    pub fn push_warning_toast(&mut self, title: impl Into<String>) -> u64 {
        self.toasts.push(ToastKind::Warning, title)
    }

    /// Push an Info toast (neutral, 3s auto-dismiss). Used by the
    /// scan-complete flow to surface the "Scan complete — …" message
    /// that previously lived in the green inline banner.
    pub fn push_info_toast(&mut self, title: impl Into<String>) -> u64 {
        self.toasts.push(ToastKind::Info, title)
    }

    /// Push an Info toast with a body and optional action. Used by the
    /// post-scan "N files had issues" surface.
    pub fn push_info_toast_full(
        &mut self,
        title: impl Into<String>,
        body: Option<String>,
        action: Option<ToastAction>,
    ) -> u64 {
        self.toasts
            .push_full(ToastKind::Info, title.into(), body, action)
    }

    /// Push a whole-group undo toast (#60) and register the restore
    /// payload under the resulting toast id. `label` is the
    /// human-readable group name (usually `GroupView::label`). Returns
    /// the toast id so tests can round-trip through
    /// [`Self::take_pending_undo`].
    pub fn push_undo_toast_for_group(&mut self, hash: u64, label: impl Into<String>) -> u64 {
        let title = format!("Dismissed \u{2018}{}\u{2019}", label.into());
        let id = self.toasts.push_full_with_ttl(
            ToastKind::Info,
            title,
            None,
            Some(ToastAction {
                label: "Undo".to_string(),
                action_name: ACTION_UNDO_DISMISS,
            }),
            UNDO_TTL,
        );
        self.pending_undos.insert(id, UndoKind::Group { hash });
        id
    }

    /// Push a per-occurrence undo toast (#60). `path_label` is rendered
    /// verbatim in the toast title — callers typically pass
    /// `PathBuf::display().to_string()` or a pre-shortened display
    /// label.
    pub fn push_undo_toast_for_occurrence(
        &mut self,
        hash: u64,
        path: PathBuf,
        path_label: impl Into<String>,
    ) -> u64 {
        let title = format!("Dismissed occurrence \u{2018}{}\u{2019}", path_label.into());
        let id = self.toasts.push_full_with_ttl(
            ToastKind::Info,
            title,
            None,
            Some(ToastAction {
                label: "Undo".to_string(),
                action_name: ACTION_UNDO_DISMISS,
            }),
            UNDO_TTL,
        );
        self.pending_undos
            .insert(id, UndoKind::Occurrence { hash, path });
        id
    }

    /// Remove and return the pending-undo entry for `toast_id`, if
    /// any. Called by the view layer's action dispatcher when the user
    /// clicks `[Undo]`; the dispatcher then routes the returned
    /// [`UndoKind`] through the matching `restore_*_click` handler so
    /// the in-memory state + on-disk cache both roll back.
    pub fn take_pending_undo(&mut self, toast_id: u64) -> Option<UndoKind> {
        self.pending_undos.remove(&toast_id)
    }

    /// Advance the toast auto-dismiss clock. Called by the project
    /// view's 500ms timer; wraps [`ToastStack::tick`] and GCs any
    /// pending-undo entries whose underlying toast auto-dismissed
    /// without the user clicking `[Undo]` — per #60 that's the
    /// "dismissal stays persisted" branch.
    pub fn tick_toasts(&mut self, now: Instant) {
        self.toasts.tick(now);
        self.pending_undos
            .retain(|toast_id, _| self.toasts.contains(*toast_id));
    }

    /// Dismiss a specific toast by id. Also drops any matching entry
    /// in [`Self::pending_undos`] — closing the toast manually
    /// (via the `[×]` button) means the user consciously passed on
    /// the undo, so the dismissal stays persisted (#60).
    pub fn dismiss_toast(&mut self, id: u64) {
        self.toasts.dismiss(id);
        self.pending_undos.remove(&id);
    }

    /// Classify `err` and push the matching toast for the newer-schema
    /// / corrupted / other buckets. Kept as a single entry point so
    /// `load_folder` and `start_scan` fall into the same code path.
    ///
    /// Returns the classification for callers that still need to
    /// influence other state (e.g. `load_folder` keeps the
    /// `AppStatus::NewerCache` view alongside the toast).
    pub fn surface_cache_error(&mut self, err: &CacheError) -> CacheErrorClass {
        let class = classify_cache_error(err);
        match class {
            CacheErrorClass::NewerSchema => {
                self.push_error_toast(
                    "Cache created by newer Dedup version. Rescan?",
                    Some(err.to_string()),
                    Some(ToastAction {
                        label: "Rescan (overwrites cache)".to_string(),
                        action_name: ACTION_CACHE_RESCAN,
                    }),
                );
            }
            CacheErrorClass::Corrupted => {
                self.push_error_toast(
                    "Cache is corrupted. Delete .dedup/ and rescan?",
                    Some(err.to_string()),
                    Some(ToastAction {
                        label: "Delete .dedup/ and rescan".to_string(),
                        action_name: ACTION_CACHE_DELETE_AND_RESCAN,
                    }),
                );
            }
            CacheErrorClass::Other => {
                self.push_error_toast("Cache error", Some(err.to_string()), None);
            }
        }
        class
    }

    /// Enter "startup error" mode — the GUI shell renders a modal
    /// asking the user to fix or reset the invalid config. Called by
    /// [`crate::run`] before any folder can be opened.
    pub fn set_startup_error(&mut self, err: &ConfigError) {
        self.startup_error = Some(StartupError::from_config_error(err));
    }

    /// Clear the startup-error modal (e.g. after Reset-to-defaults
    /// succeeds).
    pub fn clear_startup_error(&mut self) {
        self.startup_error = None;
    }

    /// Open the post-scan issues dialog. No-op when the issues list is
    /// empty (defensive — the sidebar link only renders with
    /// `scan_issues.len() > 0`).
    pub fn open_scan_issues(&mut self) {
        if !self.scan_issues.is_empty() {
            self.scan_issues_open = true;
        }
    }

    /// Close the post-scan issues dialog.
    pub fn close_scan_issues(&mut self) {
        self.scan_issues_open = false;
    }

    /// Replace the stored scan-issues list. Called by the project view
    /// after a scan completes so the sidebar link + dialog have data
    /// to render.
    pub fn set_scan_issues(&mut self, issues: Vec<FileIssue>) {
        self.scan_issues = issues;
    }

    /// Push a stale-recents Error toast with the "Remove from recents"
    /// action (replaces the inline banner from #28).
    pub fn push_stale_recent_toast(&mut self, path: &Path) {
        self.push_error_toast(
            format!(
                "Couldn\u{2019}t open {} \u{2014} it may have been \
                 moved or deleted.",
                path.display()
            ),
            None,
            Some(ToastAction {
                label: "Remove from recents".to_string(),
                action_name: ACTION_REMOVE_STALE_RECENT,
            }),
        );
    }

    /// Push a post-scan info toast with a "View issues" button. The
    /// action opens the issues dialog via [`Self::open_scan_issues`].
    /// No-op when no issues were recorded.
    pub fn push_post_scan_issues_toast(&mut self) {
        if self.scan_issues.is_empty() {
            return;
        }
        let count = self.scan_issues.len();
        let title = format!(
            "{count} file{s} had issues during the scan",
            s = if count == 1 { "" } else { "s" }
        );
        self.push_info_toast_full(
            title,
            Some("Click View issues for details and a GitHub-ready block.".to_string()),
            Some(ToastAction {
                label: "View issues".to_string(),
                action_name: ACTION_SHOW_ISSUES,
            }),
        );
    }

    /// Surface a banner with `message`. Replaces any existing editor
    /// banner. Used by the editor launcher (#29) when `nvim` and
    /// `vim` are both missing and by custom-preset config errors.
    pub fn surface_editor_banner(&mut self, message: impl Into<String>) {
        self.editor_banner = Some(EditorBanner {
            message: message.into(),
        });
    }

    /// Dismiss the editor-launch banner.
    pub fn dismiss_editor_banner(&mut self) {
        self.editor_banner = None;
    }

    /// Replace the stored editor config with `cfg`. Called after
    /// loading a folder (folder's layered config) and after the
    /// Preferences dialog saves.
    pub fn set_editor_config(&mut self, cfg: EditorConfig) {
        self.editor_config = cfg;
    }

    /// Open the Preferences dialog (issue #29).
    pub fn open_preferences(&mut self) {
        self.preferences_open = true;
    }

    /// Close the Preferences dialog.
    pub fn close_preferences(&mut self) {
        self.preferences_open = false;
    }

    /// Attach a stale-entry banner (user clicked a moved / missing
    /// recent). Replaces any existing banner — we only ever show one
    /// stale-entry banner at a time.
    pub fn surface_recent_banner(&mut self, path: PathBuf) {
        self.recent_banner = Some(RecentBanner { path });
    }

    /// Dismiss the stale-entry banner without touching the MRU.
    pub fn dismiss_recent_banner(&mut self) {
        self.recent_banner = None;
    }

    fn persist_recents(&self) {
        if let Err(e) = self.recent_projects.save_to_disk() {
            tracing::debug!(
                error = %e,
                "dedup-gui: failed to persist recent.json — keeping in-memory MRU",
            );
        }
    }

    // -----------------------------------------------------------------
    // Issue #47 — resizable sidebar helpers.
    //
    // Width lives in-state as a plain `f32` so the render path reads it
    // without allocation. Persistence is punted to drag-end (via
    // [`Self::persist_sidebar_prefs`]) rather than every move event —
    // per-frame disk writes would be wasteful, and the acceptance
    // criteria only require the value to persist across sessions.
    // -----------------------------------------------------------------

    /// Set the sidebar width in pixels, clamped into
    /// [`crate::sidebar_prefs::MIN_SIDEBAR_WIDTH`] ..=
    /// [`crate::sidebar_prefs::MAX_SIDEBAR_WIDTH`]. Called per
    /// mouse-move event from the splitter's drag-move handler. This is
    /// the only sanctioned write path for `sidebar_width`; direct field
    /// writes would bypass the clamp.
    pub fn set_sidebar_width(&mut self, width: f32) {
        self.sidebar_width = crate::sidebar_prefs::clamp_width(width);
    }

    /// Flip `sidebar_hidden` (issue #52). ⌘B / View → Toggle Sidebar
    /// route here through [`crate::project_view::ProjectView::toggle_sidebar`].
    /// Callers should follow up with [`Self::persist_sidebar_prefs`]
    /// so the new visibility survives a restart.
    pub fn toggle_sidebar_visible(&mut self) {
        self.sidebar_hidden = !self.sidebar_hidden;
    }

    /// Persist the current `sidebar_width` + `sidebar_hidden` to
    /// `sidebar.json`. Called on splitter drag end (mouse up) and on
    /// ⌘B toggle so the value survives restarts. Errors are swallowed
    /// with a `debug!` line — persistence is a hint, not a requirement
    /// for the session.
    pub fn persist_sidebar_prefs(&self) {
        let prefs = crate::sidebar_prefs::SidebarPrefs {
            sidebar_width: self.sidebar_width,
            sidebar_hidden: self.sidebar_hidden,
            // Issue #56 — persist the chosen sort key alongside the
            // width / visibility. Writing `Some(_)` means a later
            // load sees the exact variant and skips the legacy
            // "missing field → Impact" upgrade.
            sort_key: Some(self.sort_key),
        };
        if let Err(e) = prefs.save_to_disk() {
            tracing::debug!(
                error = %e,
                "dedup-gui: failed to persist sidebar.json — keeping in-memory width",
            );
        }
    }

    /// Apply a [`FolderLoadResult`] (from [`load_folder`]) to this state.
    ///
    /// Resets `selected_group` to the first group if any, so the detail
    /// pane isn't blank on open.
    pub fn set_folder_result(&mut self, result: FolderLoadResult) {
        // Issue #26 — pick up `detail.context_lines` (and any future
        // detail-pane knobs) from the project's config before we blow
        // away `current_folder`. Config layering errors fall back to
        // defaults so a malformed TOML doesn't break the UI; the
        // dedicated `dedup config` subcommand already warns loudly.
        let loaded = Config::load(Some(&result.folder)).ok();
        self.detail_config = loaded.as_ref().map(|c| c.detail).unwrap_or_default();
        // Issue #29 — pick up `[editor]` so the launcher uses the
        // project's preset on `o` / "Open in editor".
        self.editor_config = loaded.map(|c| c.editor).unwrap_or_default();
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
        self.session_occurrence_dismissed.clear();
        self.selected_occurrence_indices.clear();
        self.collapsed_occurrences.clear();
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
    ///
    /// Applies the per-occurrence session-dismiss filter (#27) — any
    /// occurrence the user dismissed via the per-row `[×]` this session
    /// is dropped before the slice reaches the detail pane. Because
    /// this function returns a `Vec` rather than a slice (the filter
    /// requires a copy), callers hold the result for the render frame.
    pub fn selected_occurrences(&self) -> Vec<OccurrenceView> {
        match self.selected_group {
            Some(id) => {
                if let Some(g) = self.groups.iter().find(|g| g.id == id) {
                    return self.visible_occurrences_of(g);
                }
                // #54 — dismissed-group selection falls through here.
                // We return the materialized occurrence list untouched
                // by the per-occurrence session-dismiss filter: the
                // banner already surfaces per-occurrence dismissals
                // individually, and the detail pane is read-only
                // anyway.
                if let Some(s) = self
                    .dismissed
                    .iter()
                    .find(|s| s.last_group_id == Some(id))
                {
                    return s.occurrences.clone();
                }
                Vec::new()
            }
            None => Vec::new(),
        }
    }

    /// Return the occurrences of `group` that survive the session-
    /// dismiss filter. Isolates the filter so both [`Self::selected_occurrences`]
    /// and the detail-toolbar helpers share one definition.
    pub fn visible_occurrences_of(&self, group: &GroupView) -> Vec<OccurrenceView> {
        let Some(hash) = group.group_hash else {
            // Streaming rows have no hash — can't be per-occurrence
            // dismissed. Return all occurrences untouched.
            return group.occurrences.clone();
        };
        group
            .occurrences
            .iter()
            .filter(|o| {
                !self
                    .session_occurrence_dismissed
                    .contains(&(hash, o.path.clone()))
            })
            .cloned()
            .collect()
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
    ///
    /// Also drops groups whose *visible* occurrence count (after the
    /// per-occurrence session dismiss filter from #27) falls below the
    /// 2-member floor — a group with one remaining occurrence isn't
    /// really a duplicate anymore. Groups that already had fewer than
    /// two occurrences in the source data (synthetic / streaming rows)
    /// survive the filter unchanged — the floor only engages when
    /// session dismissals have actually reduced the count.
    pub fn source_groups(&self) -> Vec<GroupView> {
        self.groups
            .iter()
            .filter(|g| match g.group_hash {
                Some(h) => !self.session_dismissed.contains(&h),
                None => true,
            })
            .filter(|g| {
                let visible = self.visible_occurrences_of(g).len();
                // Only enforce the floor when the original had >= 2 AND
                // the session dismiss reduced that number below 2.
                visible == g.occurrences.len() || visible >= 2
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
    ///
    /// Also closes the sort-dropdown popup (issue #46) — selecting
    /// any key dismisses the menu.
    pub fn set_sort_key(&mut self, key: SortKey) {
        self.sort_key = key;
        self.sort_popup_open = false;
        self.reclamp_selection();
        // Note: persistence of the new key is performed by
        // [`crate::project_view::ProjectView::set_sort_key`] after
        // this call returns. Keeping disk I/O out of `AppState` keeps
        // the unit-test path (which runs without a home dir) pure.
    }

    /// Toggle the sort-dropdown popup (issue #46). Bound to the
    /// `Sort: <key>` button in the sidebar.
    pub fn toggle_sort_popup(&mut self) {
        self.sort_popup_open = !self.sort_popup_open;
    }

    /// Close the sort-dropdown popup without changing the key
    /// (issue #46). Wired to the click-outside scrim.
    pub fn close_sort_popup(&mut self) {
        self.sort_popup_open = false;
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
        let now = unix_now();
        // Append to dismissed so "recently dismissed" ends up last.
        self.dismissed.push(SuppressionView {
            hash,
            hash_hex: format!("{hash:016x}"),
            last_group_id: Some(group.id),
            dismissed_at: now,
            occurrences: group.occurrences.clone(),
            occurrence_dismissals: Vec::new(),
            tier: group.tier,
        });
        self.reclamp_selection();
        Some((hash, group.id))
    }

    /// Change the focused pane (⌘1 / ⌘2).
    pub fn focus_pane(&mut self, pane: Pane) {
        self.focused_pane = pane;
    }

    // -----------------------------------------------------------------
    // Issue #27 — group toolbar + per-occurrence selection / dismissal.
    //
    // All methods below are GPUI-free so the test lane covers them
    // directly. Clipboard writes + cache writes live in the GPUI layer
    // (`project_view`); these helpers only mutate pure state.
    // -----------------------------------------------------------------

    /// Toggle whether the `occ_idx`-th occurrence of `group_id` is
    /// checked. `occ_idx` is the position inside the group's
    /// *visible* (post-per-occurrence-dismiss) occurrence list — the
    /// same order the detail view renders them in.
    pub fn toggle_occurrence(&mut self, group_id: i64, occ_idx: usize) {
        let set = self
            .selected_occurrence_indices
            .entry(group_id)
            .or_default();
        if !set.insert(occ_idx) {
            set.remove(&occ_idx);
        }
        if set.is_empty() {
            self.selected_occurrence_indices.remove(&group_id);
        }
    }

    /// True iff the given `(group_id, occ_idx)` checkbox is checked.
    pub fn is_occurrence_selected(&self, group_id: i64, occ_idx: usize) -> bool {
        self.selected_occurrence_indices
            .get(&group_id)
            .is_some_and(|s| s.contains(&occ_idx))
    }

    /// `(path, first_line)` pairs for every occurrence covered by
    /// [`Self::copy_paths_for_group`] — i.e. checked occurrences when
    /// any are checked, otherwise every visible occurrence. Used by
    /// the "Open in editor" toolbar button (#29) so the launcher has
    /// line info for presets that take `+line`.
    pub fn open_targets_for_group(&self, group_id: i64) -> Vec<(PathBuf, u32)> {
        let Some(group) = self.groups.iter().find(|g| g.id == group_id) else {
            return Vec::new();
        };
        let occurrences = self.visible_occurrences_of(group);
        let checked = self.selected_occurrence_indices.get(&group_id);
        let line_of = |o: &OccurrenceView| o.start_line.max(1) as u32;
        match checked {
            Some(set) if !set.is_empty() => occurrences
                .iter()
                .enumerate()
                .filter(|(i, _)| set.contains(i))
                .map(|(_, o)| (o.path.clone(), line_of(o)))
                .collect(),
            _ => occurrences
                .iter()
                .map(|o| (o.path.clone(), line_of(o)))
                .collect(),
        }
    }

    /// Paths to copy / open for `group_id` given the current checkbox
    /// state. Returns the checked paths when any are checked; returns
    /// every visible occurrence's path when none are checked (the
    /// "whole group, no selection" default per issue copy).
    pub fn copy_paths_for_group(&self, group_id: i64) -> Vec<PathBuf> {
        let Some(group) = self.groups.iter().find(|g| g.id == group_id) else {
            return Vec::new();
        };
        let occurrences = self.visible_occurrences_of(group);
        let checked = self.selected_occurrence_indices.get(&group_id);
        match checked {
            Some(set) if !set.is_empty() => occurrences
                .iter()
                .enumerate()
                .filter(|(i, _)| set.contains(i))
                .map(|(_, o)| o.path.clone())
                .collect(),
            _ => occurrences.iter().map(|o| o.path.clone()).collect(),
        }
    }

    /// Dismiss the entire group identified by `group_id` regardless of
    /// checkbox state (per issue #27 "Dismiss group ignores checkboxes").
    /// Updates `session_dismissed` + appends a row to `dismissed` so
    /// `visible_groups()` drops it immediately. The caller persists
    /// to the cache via [`dedup_core::Cache::dismiss_hash`].
    ///
    /// Returns the `(hash, group_id)` pair on success, or `None` if
    /// the group is missing or its hash is unresolvable (streaming
    /// rows). Clears any checkbox / collapse state tied to the id.
    pub fn dismiss_group(&mut self, group_id: i64) -> Option<(u64, i64)> {
        let group = self.groups.iter().find(|g| g.id == group_id)?.clone();
        let hash = group.group_hash?;
        self.session_dismissed.insert(hash);
        let now = unix_now();
        self.dismissed.push(SuppressionView {
            hash,
            hash_hex: format!("{hash:016x}"),
            last_group_id: Some(group.id),
            dismissed_at: now,
            occurrences: group.occurrences.clone(),
            occurrence_dismissals: Vec::new(),
            tier: group.tier,
        });
        self.selected_occurrence_indices.remove(&group_id);
        self.collapsed_occurrences
            .retain(|(gid, _)| *gid != group_id);
        if self.selected_group == Some(group_id) {
            self.selected_group = None;
        }
        self.reclamp_selection();
        Some((hash, group.id))
    }

    /// Dismiss a single occurrence of `group_id` (per issue #27 "Dismiss
    /// this occurrence preserves rest of group"). `occ_idx` is the
    /// visible-list index; the corresponding path is tracked in
    /// `session_occurrence_dismissed` so future `visible_occurrences_of`
    /// calls skip it. The caller persists to the cache via
    /// [`dedup_core::Cache::dismiss_occurrence`].
    ///
    /// Returns `(hash, path)` on success. `None` when the group or
    /// occurrence is missing, or when the group lacks a stable hash
    /// (streaming rows — can't durably persist the dismissal). Also
    /// clears `selected_occurrence_indices[group_id][occ_idx]` and
    /// shifts any higher-index selections down so the indices stay
    /// valid after the remove.
    ///
    /// Groups whose visible occurrence count falls below 2 after the
    /// dismissal are *not* mutated here — the count falls out of
    /// [`Self::visible_occurrences_of`] naturally; the sidebar's
    /// `visible_groups()` filter continues to surface a singleton
    /// group until the filter hides it organically.
    pub fn dismiss_occurrence(&mut self, group_id: i64, occ_idx: usize) -> Option<(u64, PathBuf)> {
        let group = self.groups.iter().find(|g| g.id == group_id)?.clone();
        let hash = group.group_hash?;
        let visible = self.visible_occurrences_of(&group);
        let occ = visible.get(occ_idx)?.clone();
        self.session_occurrence_dismissed
            .insert((hash, occ.path.clone()));
        // Remove that index from the selection set and shift any
        // higher indices down — indices are into the *post-dismiss*
        // visible list, so a dismiss at index k means all indices > k
        // now point one slot to the left.
        if let Some(set) = self.selected_occurrence_indices.get_mut(&group_id) {
            let mut updated: HashSet<usize> = HashSet::new();
            for i in set.iter() {
                if *i == occ_idx {
                    continue;
                }
                if *i > occ_idx {
                    updated.insert(*i - 1);
                } else {
                    updated.insert(*i);
                }
            }
            if updated.is_empty() {
                self.selected_occurrence_indices.remove(&group_id);
            } else {
                *set = updated;
            }
        }
        Some((hash, occ.path))
    }

    /// Toggle whether a single occurrence's code body is collapsed.
    /// Headers always stay visible; only the `CodeLine` / `Gap` /
    /// `Unavailable` rows are suppressed when collapsed.
    pub fn toggle_occurrence_collapse(&mut self, group_id: i64, occ_idx: usize) {
        let key = (group_id, occ_idx);
        if !self.collapsed_occurrences.insert(key) {
            self.collapsed_occurrences.remove(&key);
        }
    }

    /// Whether a single occurrence's code body is currently collapsed.
    pub fn is_occurrence_collapsed(&self, group_id: i64, occ_idx: usize) -> bool {
        self.collapsed_occurrences.contains(&(group_id, occ_idx))
    }

    /// Collapse every occurrence in the currently-selected group only.
    /// Other groups' collapse state is left untouched — per #45 the
    /// button scope is the active group, not the whole project.
    pub fn collapse_all_in_active_group(&mut self) {
        let Some(group_id) = self.selected_group else {
            return;
        };
        let Some(group) = self.groups.iter().find(|g| g.id == group_id).cloned() else {
            return;
        };
        let count = self.visible_occurrences_of(&group).len();
        for i in 0..count {
            self.collapsed_occurrences.insert((group_id, i));
        }
    }

    /// Expand every occurrence in the currently-selected group only —
    /// drops every `(active_group_id, _)` entry.
    pub fn expand_all_in_active_group(&mut self) {
        let Some(group_id) = self.selected_group else {
            return;
        };
        self.collapsed_occurrences
            .retain(|(gid, _)| *gid != group_id);
    }

    /// Close the group-detail pane — clears the selection. Reached by
    /// the toolbar's `[×]` close button.
    pub fn close_group_detail(&mut self) {
        self.selected_group = None;
        self.selected_group_idx = None;
        self.focused_pane = Pane::Sidebar;
    }

    // -----------------------------------------------------------------
    // Issue #54 — dismissed-group click-to-review + restore helpers.
    //
    // Dismissed groups are not in `self.groups` (they're filtered out
    // at materialization time). `selected_group` is still a plain
    // group-id — dismissed rows plug `SuppressionView::last_group_id`
    // in as that id, so both active and dismissed selection run
    // through the same `select_group` path.
    // -----------------------------------------------------------------

    /// Select a dismissed group by its cache row id. No-op when no
    /// dismissed row carries that id. Moves focus to the detail pane
    /// so the banner + code body are immediately visible.
    pub fn select_dismissed(&mut self, group_id: i64) {
        if self
            .dismissed
            .iter()
            .any(|s| s.last_group_id == Some(group_id))
        {
            self.selected_group = Some(group_id);
            self.focused_pane = Pane::Detail;
        }
    }

    /// Return the [`SuppressionView`] matching the currently selected
    /// group id, if any. The banner + read-only detail pane key off
    /// this.
    pub fn selected_dismissed(&self) -> Option<&SuppressionView> {
        let id = self.selected_group?;
        self.dismissed
            .iter()
            .find(|s| s.last_group_id == Some(id))
    }

    /// Look up a dismissed row by its group-hash. Used by the restore
    /// handlers — the GUI layer resolves `(hash, path)` from a
    /// `SuppressionView` clone, then calls back here on the updated
    /// state to locate the matching row.
    pub fn find_dismissed_by_hash(&self, hash: u64) -> Option<&SuppressionView> {
        self.dismissed.iter().find(|s| s.hash == hash)
    }

    /// Restore a previously-dismissed group (#54). Drops the matching
    /// `SuppressionView` from `self.dismissed`, removes the hash from
    /// `session_dismissed` (so `visible_groups()` no longer hides the
    /// live row if it's still in the cache), and — if the live row is
    /// still in `self.groups` via `last_group_id` — snaps the sidebar
    /// selection to it.
    ///
    /// Returns the `(hash, last_group_id)` pair for the restored row
    /// so the caller can round-trip the undismiss to the cache. Returns
    /// `None` when no dismissed row carries `hash`.
    pub fn restore_group(&mut self, hash: u64) -> Option<(u64, Option<i64>)> {
        let pos = self.dismissed.iter().position(|s| s.hash == hash)?;
        let row = self.dismissed.remove(pos);
        self.session_dismissed.remove(&hash);
        // If the active `groups` list has a row with the same hash
        // (the common case — dismissals never delete rows), select it
        // so the user lands on the now-restored detail view.
        if let Some(gid) = row.last_group_id
            && self.groups.iter().any(|g| g.id == gid)
        {
            self.selected_group = Some(gid);
            self.focused_pane = Pane::Detail;
            self.reclamp_selection();
        } else {
            // Row no longer in active cache (stale breadcrumb). Clear
            // the selection so the detail pane falls back to the
            // empty-state copy.
            if self.selected_group == row.last_group_id {
                self.selected_group = None;
            }
        }
        Some((hash, row.last_group_id))
    }

    /// Restore a single per-occurrence dismissal (#54). Locates the
    /// matching entry in `SuppressionView::occurrence_dismissals` and
    /// drops it; also drops any matching session-level entry.
    /// Returns `(hash, path)` on success.
    pub fn restore_occurrence(&mut self, hash: u64, path: &Path) -> Option<(u64, PathBuf)> {
        let row = self.dismissed.iter_mut().find(|s| s.hash == hash)?;
        let before = row.occurrence_dismissals.len();
        row.occurrence_dismissals.retain(|od| od.path != path);
        let after = row.occurrence_dismissals.len();
        self.session_occurrence_dismissed
            .remove(&(hash, path.to_path_buf()));
        if after == before {
            // Even if no banner entry matched, scrubbing the session
            // set is harmless; but report success only when we
            // actually changed state the UI needs to see.
            return None;
        }
        Some((hash, path.to_path_buf()))
    }

    /// Counts used by the group toolbar's
    /// `[N files · N duplicated lines]` info label (#27).
    /// `files` counts distinct paths across visible occurrences;
    /// `duplicated_lines` mirrors `summary()` — lines per occurrence
    /// times `(count - 1)`, i.e. the removable line count if the
    /// duplicates were deduplicated to one copy.
    pub fn group_toolbar_counts(&self, group_id: i64) -> (usize, usize) {
        let Some(group) = self.groups.iter().find(|g| g.id == group_id) else {
            return (0, 0);
        };
        let visible = self.visible_occurrences_of(group);
        let files: HashSet<&Path> = visible.iter().map(|o| o.path.as_path()).collect();
        let duplicated_lines = if visible.len() >= 2 {
            let first = &visible[0];
            let span = (first.end_line - first.start_line + 1).max(0) as usize;
            span.saturating_mul(visible.len() - 1)
        } else {
            0
        };
        (files.len(), duplicated_lines)
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
            // Streaming path: forward alpha-rename spans verbatim from
            // the scanner's `Occurrence`. Only Tier B occurrences carry
            // them; Tier A's vector is always empty so the tint overlay
            // in the detail view silently stays off.
            alpha_rename_spans: o.alpha_rename_spans.clone(),
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

    // Per-occurrence suppressions (#27) are applied alongside the
    // group-level set: dismiss any (group_hash, path) pair that's in
    // the table, and drop the whole group if the remaining count
    // falls below 2.
    let occurrence_suppressed = match cache.suppressed_occurrences() {
        Ok(s) => s,
        Err(e) => {
            return FolderLoadResult {
                folder,
                groups: Vec::new(),
                dismissed: Vec::new(),
                status: AppStatus::Error(format!("failed to read occurrence suppressions: {e}")),
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
            .filter(|o| match hash {
                Some(h) => !occurrence_suppressed.contains(&(h, o.path.clone())),
                None => true,
            })
            .map(|o| OccurrenceView {
                path: o.path.clone(),
                start_line: o.start_line,
                end_line: o.end_line,
                // Alpha-rename spans come back from the cache as
                // (i64, i64, u32). Narrow to usize for the renderer's
                // byte-range API; negative / out-of-range rows are
                // treated as empty (cache invariant keeps this
                // well-behaved, but guard anyway so a corrupted row
                // can never panic the sidebar).
                alpha_rename_spans: o
                    .alpha_rename_spans
                    .iter()
                    .filter_map(|(s, e, idx)| {
                        if *s < 0 || *e < 0 || *e < *s {
                            None
                        } else {
                            Some((*s as usize, *e as usize, *idx))
                        }
                    })
                    .collect(),
            })
            .collect();

        // #27 — drop groups whose remaining occurrences fall below 2.
        if occurrences.len() < 2 {
            continue;
        }

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

    // Per-occurrence dismissals we already read above power the banner
    // enumeration for each dismissed group (#54). Build a per-hash
    // lookup now — `list_occurrence_suppressions` is the authoritative
    // source (it also carries timestamps, which the banner shows).
    let occurrence_dismissal_rows =
        match cache.list_occurrence_suppressions() {
            Ok(v) => v,
            Err(e) => {
                return FolderLoadResult {
                    folder,
                    groups: Vec::new(),
                    dismissed: Vec::new(),
                    status: AppStatus::Error(format!(
                        "failed to read occurrence suppressions: {e}"
                    )),
                };
            }
        };
    let mut occ_dismissals_by_hash: std::collections::HashMap<u64, Vec<OccurrenceDismissal>> =
        std::collections::HashMap::new();
    for (h, path, ts) in occurrence_dismissal_rows {
        occ_dismissals_by_hash
            .entry(h)
            .or_default()
            .push(OccurrenceDismissal {
                path,
                dismissed_at: ts,
            });
    }

    // Build a hash → (group_id, occurrences) lookup by scanning
    // `list_groups` once more. The cache still carries the rows (we
    // only *hide* dismissed groups from the active list, we never
    // delete them). This lets the detail pane render the dismissed
    // group's actual code body rather than a stub.
    let mut occurrences_by_hash: std::collections::HashMap<u64, (i64, Tier, Vec<OccurrenceView>)> =
        std::collections::HashMap::new();
    let all_summaries = cache.list_groups().unwrap_or_default();
    for summary in all_summaries {
        let Ok(Some(h)) = cache.group_hash(summary.id) else {
            continue;
        };
        if !suppressed.contains(&h) {
            continue;
        }
        let Ok(Some(detail)) = cache.get_group(summary.id) else {
            continue;
        };
        let occs: Vec<OccurrenceView> = detail
            .occurrences
            .iter()
            .map(|o| OccurrenceView {
                path: o.path.clone(),
                start_line: o.start_line,
                end_line: o.end_line,
                alpha_rename_spans: o
                    .alpha_rename_spans
                    .iter()
                    .filter_map(|(s, e, idx)| {
                        if *s < 0 || *e < 0 || *e < *s {
                            None
                        } else {
                            Some((*s as usize, *e as usize, *idx))
                        }
                    })
                    .collect(),
            })
            .collect();
        occurrences_by_hash.insert(h, (detail.id, detail.tier, occs));
    }

    let dismissed: Vec<SuppressionView> = match cache.list_suppressions() {
        Ok(s) => s
            .into_iter()
            .map(|sup| {
                let (live_id, tier, occurrences) = occurrences_by_hash
                    .remove(&sup.hash)
                    .map(|(id, tier, occs)| (Some(id), tier, occs))
                    .unwrap_or((None, Tier::A, Vec::new()));
                let occurrence_dismissals =
                    occ_dismissals_by_hash.remove(&sup.hash).unwrap_or_default();
                SuppressionView {
                    hash: sup.hash,
                    hash_hex: format!("{:016x}", sup.hash),
                    // Prefer the *current* cache row id over the stored
                    // breadcrumb — re-scans renumber rows and the UI
                    // needs the id the active cache knows about so
                    // `select_group` can round-trip to `match_groups`.
                    last_group_id: live_id.or(sup.last_group_id),
                    dismissed_at: sup.dismissed_at,
                    occurrences,
                    occurrence_dismissals,
                    tier,
                }
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
    use crate::sidebar_prefs::{
        DEFAULT_SIDEBAR_WIDTH, MAX_SIDEBAR_WIDTH, MIN_SIDEBAR_WIDTH,
    };
    use std::path::PathBuf;

    // -----------------------------------------------------------------
    // Issue #47 — set_sidebar_width clamping.
    // -----------------------------------------------------------------

    #[test]
    fn set_sidebar_width_clamps_below_min() {
        let mut s = AppState::new();
        s.set_sidebar_width(10.0);
        assert_eq!(s.sidebar_width, MIN_SIDEBAR_WIDTH);
    }

    #[test]
    fn set_sidebar_width_clamps_at_min_boundary() {
        let mut s = AppState::new();
        s.set_sidebar_width(MIN_SIDEBAR_WIDTH);
        assert_eq!(s.sidebar_width, MIN_SIDEBAR_WIDTH);
    }

    #[test]
    fn set_sidebar_width_clamps_above_max() {
        let mut s = AppState::new();
        s.set_sidebar_width(5_000.0);
        assert_eq!(s.sidebar_width, MAX_SIDEBAR_WIDTH);
    }

    #[test]
    fn set_sidebar_width_clamps_at_max_boundary() {
        let mut s = AppState::new();
        s.set_sidebar_width(MAX_SIDEBAR_WIDTH);
        assert_eq!(s.sidebar_width, MAX_SIDEBAR_WIDTH);
    }

    #[test]
    fn set_sidebar_width_accepts_in_range_value() {
        let mut s = AppState::new();
        s.set_sidebar_width(400.0);
        assert_eq!(s.sidebar_width, 400.0);
    }

    #[test]
    fn default_sidebar_width_matches_preset() {
        let s = AppState::new();
        assert_eq!(s.sidebar_width, DEFAULT_SIDEBAR_WIDTH);
    }

    // Issue #52 — sidebar visibility toggle.
    #[test]
    fn default_sidebar_is_visible() {
        let s = AppState::new();
        assert!(!s.sidebar_hidden);
    }

    #[test]
    fn toggle_sidebar_visible_flips_flag() {
        let mut s = AppState::new();
        s.toggle_sidebar_visible();
        assert!(s.sidebar_hidden);
        s.toggle_sidebar_visible();
        assert!(!s.sidebar_hidden);
    }

    fn occ(path: &str, s: i64, e: i64) -> OccurrenceView {
        OccurrenceView {
            path: PathBuf::from(path),
            start_line: s,
            end_line: e,
            alpha_rename_spans: Vec::new(),
        }
    }

    /// Build a `dedup_core::Occurrence` for the load-folder test fixtures.
    ///
    /// All call sites in this module use the same 1..=10 / 0..100 span,
    /// so the helper takes only the path. Collapses the duplicated
    /// struct literals surfaced by #32 dogfood scan (group 56).
    fn mk_occ(path: &str) -> dedup_core::Occurrence {
        dedup_core::Occurrence {
            path: PathBuf::from(path),
            span: dedup_core::rolling_hash::Span {
                start_line: 1,
                end_line: 10,
                start_byte: 0,
                end_byte: 100,
            },
            alpha_rename_spans: Vec::new(),
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
    fn restore_group_drops_row_and_clears_session_dismiss() {
        // Dismiss then restore — the dismissed list should shrink, the
        // session set should clear, and (because the live row is still
        // in `self.groups`) the selection should snap to it.
        let mut s = loaded_with_multi_occ();
        let (hash, gid) = s.dismiss_group(1).unwrap();
        assert!(s.session_dismissed.contains(&hash));
        assert_eq!(s.dismissed.len(), 1);

        let out = s.restore_group(hash);
        assert_eq!(out, Some((hash, Some(gid))));
        assert!(s.dismissed.is_empty());
        assert!(!s.session_dismissed.contains(&hash));
        // Group 1 was still present in `self.groups` (dismiss_group
        // only appends to `dismissed`; it does not remove from
        // `groups`) so selection should track it.
        assert_eq!(s.selected_group, Some(1));
    }

    #[test]
    fn restore_group_noop_on_unknown_hash() {
        let mut s = loaded_with_multi_occ();
        assert_eq!(s.restore_group(0xdead), None);
    }

    #[test]
    fn select_dismissed_routes_focus_and_selection() {
        let mut s = loaded_with_multi_occ();
        let (_, gid) = s.dismiss_group(1).unwrap();
        s.selected_group = None;
        s.select_dismissed(gid);
        assert_eq!(s.selected_group, Some(gid));
        assert_eq!(s.focused_pane, Pane::Detail);
    }

    #[test]
    fn selected_occurrences_returns_dismissed_rows() {
        let mut s = loaded_with_multi_occ();
        let (_, gid) = s.dismiss_group(1).unwrap();
        s.selected_group = Some(gid);
        // dismissed.occurrences were populated at dismiss time.
        let occs = s.selected_occurrences();
        assert_eq!(occs.len(), 3);
    }

    #[test]
    fn restore_occurrence_drops_banner_entry_and_session_row() {
        let mut s = loaded_with_multi_occ();
        let (hash, _gid) = s.dismiss_group(1).unwrap();
        // Simulate a per-occurrence dismissal landing in the banner.
        let row = s.dismissed.iter_mut().find(|d| d.hash == hash).unwrap();
        row.occurrence_dismissals.push(OccurrenceDismissal {
            path: PathBuf::from("a.rs"),
            dismissed_at: 123,
        });
        s.session_occurrence_dismissed
            .insert((hash, PathBuf::from("a.rs")));

        let out = s.restore_occurrence(hash, &PathBuf::from("a.rs"));
        assert_eq!(out, Some((hash, PathBuf::from("a.rs"))));
        let row = s.dismissed.iter().find(|d| d.hash == hash).unwrap();
        assert!(row.occurrence_dismissals.is_empty());
        assert!(!s
            .session_occurrence_dismissed
            .contains(&(hash, PathBuf::from("a.rs"))));
    }

    #[test]
    fn suppression_label_truncates_hash() {
        let s = SuppressionView {
            hash: 0xabcdef0123456789,
            hash_hex: "abcdef0123456789".to_string(),
            last_group_id: Some(3),
            dismissed_at: 0,
            occurrences: Vec::new(),
            occurrence_dismissals: Vec::new(),
            tier: Tier::A,
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

    // -----------------------------------------------------------------
    // Issue #56 — Severity sort option.
    // -----------------------------------------------------------------

    #[test]
    fn severity_is_occurrence_count_times_total_lines() {
        // Two occurrences, 5 lines each (lines 1..=5 inclusive = 5).
        // Total lines = 10. Severity = 2 * 10 = 20.
        let g = mkgroup(
            1,
            Tier::A,
            "x",
            vec![occ("a.rs", 1, 5), occ("b.rs", 1, 5)],
            Some(0x11),
        );
        assert_eq!(g.severity(), 20);
    }

    #[test]
    fn severity_zero_for_empty_occurrences() {
        let g = mkgroup(1, Tier::A, "x", vec![], Some(0x11));
        assert_eq!(g.severity(), 0);
    }

    #[test]
    fn sort_groups_severity_puts_highest_leverage_first() {
        // low:  2 occs × 5 lines = 10
        // mid:  2 occs × 50 lines = 100
        // high: 5 occs × 20 lines = 500
        let low = mkgroup(
            1,
            Tier::A,
            "low",
            vec![occ("a.rs", 1, 5), occ("b.rs", 1, 5)],
            Some(0x11),
        );
        let mid = mkgroup(
            2,
            Tier::A,
            "mid",
            vec![occ("c.rs", 1, 50), occ("d.rs", 1, 50)],
            Some(0x22),
        );
        let high = mkgroup(
            3,
            Tier::A,
            "high",
            vec![
                occ("e.rs", 1, 20),
                occ("f.rs", 1, 20),
                occ("g.rs", 1, 20),
                occ("h.rs", 1, 20),
                occ("i.rs", 1, 20),
            ],
            Some(0x33),
        );
        let out = sort_groups(
            &[low.clone(), mid.clone(), high.clone()],
            SortKey::Severity,
        );
        let ids: Vec<i64> = out.iter().map(|g| g.id).collect();
        assert_eq!(ids, vec![3, 2, 1], "severity descending");
    }

    #[test]
    fn sort_groups_severity_tiebreaks_on_path_alphabetical() {
        // Same severity (1 occ × 10 lines = 10) — path alphabetical
        // must break the tie so ordering is stable.
        let z = mkgroup(
            1,
            Tier::A,
            "z",
            vec![occ("zeta.rs", 1, 10)],
            Some(0xFFFF),
        );
        let a = mkgroup(
            2,
            Tier::A,
            "a",
            vec![occ("alpha.rs", 1, 10)],
            Some(0x0001),
        );
        // Feed in reverse of the expected order so a naive stable
        // sort can't accidentally pass the test.
        let out = sort_groups(&[z, a], SortKey::Severity);
        let ids: Vec<i64> = out.iter().map(|g| g.id).collect();
        assert_eq!(
            ids,
            vec![2, 1],
            "alpha.rs wins the path-alphabetical tiebreaker over zeta.rs"
        );
    }

    #[test]
    fn sort_groups_severity_is_default() {
        assert_eq!(SortKey::default(), SortKey::Severity);
    }

    #[test]
    fn sort_key_all_lists_severity_first() {
        assert_eq!(
            SortKey::ALL.first().copied(),
            Some(SortKey::Severity),
            "dropdown must surface Severity as the top option"
        );
    }

    #[test]
    fn sort_key_label_severity() {
        assert_eq!(SortKey::Severity.label(), "Severity");
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
    fn live_typing_filters_and_clearing_restores_all(
    ) {
        // Issue #50 — simulate the character-by-character typing flow
        // the real `on_key_down` handler runs: each keystroke pushes a
        // char, `set_search_query` reclamps the selection, and
        // `visible_groups` must re-filter on the fly. Clearing the
        // query (equivalent to `escape`) returns the full list
        // without a re-scan.
        let mut s = loaded_state_with(vec![
            mkgroup(1, Tier::A, "apple", vec![occ("a.rs", 1, 5)], Some(0x1)),
            mkgroup(2, Tier::A, "banana", vec![occ("b.py", 1, 5)], Some(0x2)),
            mkgroup(3, Tier::A, "apricot", vec![occ("c.rs", 1, 5)], Some(0x3)),
        ]);
        assert_eq!(s.visible_groups().len(), 3);

        // Type "a"  → still matches all three.
        let mut q = String::new();
        q.push('a');
        s.set_search_query(q.clone());
        assert_eq!(s.visible_groups().len(), 3);

        // Type "p"  → "a" + "p" → apple + apricot.
        q.push('p');
        s.set_search_query(q.clone());
        let ids: Vec<i64> = s.visible_groups().iter().map(|g| g.id).collect();
        assert_eq!(ids, vec![1, 3]);

        // Type "ri" → "apri" → apricot only.
        q.push('r');
        q.push('i');
        s.set_search_query(q.clone());
        let ids: Vec<i64> = s.visible_groups().iter().map(|g| g.id).collect();
        assert_eq!(ids, vec![3]);

        // Simulate `escape` → clear the query. Full list restored.
        s.set_search_query(String::new());
        assert_eq!(s.visible_groups().len(), 3);

        // Simulate `backspace` after re-typing: the handler pops the
        // last char and re-calls `set_search_query`. The underlying
        // `source_groups` list must not be mutated — only the filter
        // view changes — which is what the call-count-invariant below
        // encodes (three calls, three different filtered lengths, no
        // side effect on `groups`).
        let original_len = s.groups.len();
        s.set_search_query("apri".into());
        assert_eq!(s.visible_groups().len(), 1);
        s.set_search_query("apr".into());
        assert_eq!(s.visible_groups().len(), 1);
        s.set_search_query("ap".into());
        assert_eq!(s.visible_groups().len(), 2);
        assert_eq!(s.groups.len(), original_len);
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

    // -------------------------------------------------------------------
    // Issue #27 — group toolbar + per-occurrence selection / dismissal.
    // All pure-state assertions; no GPUI types are constructed.
    // -------------------------------------------------------------------

    fn loaded_with_multi_occ() -> AppState {
        // Two groups, the first with three occurrences (so per-occurrence
        // dismiss can drop one without falling below the 2-member floor).
        loaded_state_with(vec![
            mkgroup(
                1,
                Tier::A,
                "three-occs",
                vec![occ("a.rs", 1, 5), occ("b.rs", 1, 5), occ("c.rs", 1, 5)],
                Some(0xAA),
            ),
            mkgroup(
                2,
                Tier::A,
                "two-occs",
                vec![occ("x.rs", 1, 5), occ("y.rs", 1, 5)],
                Some(0xBB),
            ),
        ])
    }

    #[test]
    fn toggle_occurrence_adds_and_removes() {
        let mut s = loaded_with_multi_occ();
        assert!(!s.is_occurrence_selected(1, 0));
        s.toggle_occurrence(1, 0);
        assert!(s.is_occurrence_selected(1, 0));
        s.toggle_occurrence(1, 0);
        assert!(!s.is_occurrence_selected(1, 0));
        // Empty set is cleaned up so the HashMap doesn't grow unbounded.
        assert!(!s.selected_occurrence_indices.contains_key(&1));
    }

    #[test]
    fn copy_paths_returns_checked_when_any_checked() {
        let mut s = loaded_with_multi_occ();
        s.toggle_occurrence(1, 0);
        s.toggle_occurrence(1, 2);
        let paths = s.copy_paths_for_group(1);
        let set: std::collections::HashSet<_> = paths.iter().cloned().collect();
        let expected: std::collections::HashSet<_> =
            vec![PathBuf::from("a.rs"), PathBuf::from("c.rs")]
                .into_iter()
                .collect();
        assert_eq!(set, expected);
    }

    #[test]
    fn copy_paths_falls_back_to_all_when_none_checked() {
        let s = loaded_with_multi_occ();
        let paths = s.copy_paths_for_group(1);
        assert_eq!(paths.len(), 3);
    }

    #[test]
    fn dismiss_group_removes_from_visible_and_records_hash() {
        let mut s = loaded_with_multi_occ();
        let out = s.dismiss_group(1);
        assert_eq!(out, Some((0xAA, 1)));
        assert!(s.session_dismissed.contains(&0xAA));
        let ids: Vec<i64> = s.visible_groups().iter().map(|g| g.id).collect();
        assert_eq!(ids, vec![2]);
        // Dismissed row appended for the sidebar's dismissed section.
        assert_eq!(s.dismissed.len(), 1);
    }

    #[test]
    fn dismiss_group_ignores_checkboxes() {
        // Even with one checkbox selected, "Dismiss group" drops the
        // whole group — the per-acceptance-criterion invariant.
        let mut s = loaded_with_multi_occ();
        s.toggle_occurrence(1, 0);
        let out = s.dismiss_group(1);
        assert!(out.is_some());
        assert!(!s.selected_occurrence_indices.contains_key(&1));
    }

    #[test]
    fn dismiss_occurrence_preserves_rest_of_group() {
        let mut s = loaded_with_multi_occ();
        // Group 1 has 3 occurrences. Dismiss the first — 2 should remain
        // and the group should stay in the visible list.
        let out = s.dismiss_occurrence(1, 0);
        assert_eq!(out, Some((0xAA, PathBuf::from("a.rs"))));
        // Group 1 still surfaces; remaining occurrences are b.rs + c.rs.
        let visible: Vec<i64> = s.visible_groups().iter().map(|g| g.id).collect();
        assert_eq!(visible, vec![1, 2]);
        let g1 = s.visible_groups().into_iter().find(|g| g.id == 1).unwrap();
        let remaining_paths: Vec<PathBuf> = s
            .visible_occurrences_of(&g1)
            .into_iter()
            .map(|o| o.path)
            .collect();
        assert_eq!(
            remaining_paths,
            vec![PathBuf::from("b.rs"), PathBuf::from("c.rs")]
        );
    }

    #[test]
    fn dismiss_occurrence_drops_group_when_count_below_two() {
        // Group 2 has 2 occurrences. Dismissing one leaves a singleton,
        // so the group should disappear from visible_groups.
        let mut s = loaded_with_multi_occ();
        let _ = s.dismiss_occurrence(2, 0);
        let visible: Vec<i64> = s.visible_groups().iter().map(|g| g.id).collect();
        assert_eq!(visible, vec![1]);
    }

    #[test]
    fn dismiss_occurrence_is_noop_when_hash_missing() {
        // Streaming rows have `group_hash = None`; a dismiss on them
        // must not crash and must return None.
        let mut s = loaded_state_with(vec![mkgroup(
            -1,
            Tier::A,
            "streaming",
            vec![occ("a.rs", 1, 2), occ("b.rs", 1, 2)],
            None,
        )]);
        assert_eq!(s.dismiss_occurrence(-1, 0), None);
    }

    #[test]
    fn toggle_occurrence_collapse_is_per_pair() {
        let mut s = loaded_with_multi_occ();
        // Independent toggles — collapsing (1, 0) leaves (1, 1) and
        // (2, 0) untouched.
        s.toggle_occurrence_collapse(1, 0);
        assert!(s.is_occurrence_collapsed(1, 0));
        assert!(!s.is_occurrence_collapsed(1, 1));
        assert!(!s.is_occurrence_collapsed(2, 0));
        // Second call flips off.
        s.toggle_occurrence_collapse(1, 0);
        assert!(!s.is_occurrence_collapsed(1, 0));
    }

    #[test]
    fn collapse_all_in_active_group_only_touches_active_group() {
        let mut s = loaded_with_multi_occ();
        // Active group is 1 (three occurrences); group 2 should stay
        // fully expanded.
        assert_eq!(s.selected_group, Some(1));
        s.collapse_all_in_active_group();
        assert!(s.is_occurrence_collapsed(1, 0));
        assert!(s.is_occurrence_collapsed(1, 1));
        assert!(s.is_occurrence_collapsed(1, 2));
        assert!(!s.is_occurrence_collapsed(2, 0));
        assert!(!s.is_occurrence_collapsed(2, 1));
    }

    #[test]
    fn expand_all_in_active_group_only_clears_active_group() {
        let mut s = loaded_with_multi_occ();
        // Seed collapse state across both groups, then swap the active
        // group and expand — the inactive group's collapse must stick.
        s.toggle_occurrence_collapse(1, 0);
        s.toggle_occurrence_collapse(2, 0);
        s.selected_group = Some(2);
        s.expand_all_in_active_group();
        assert!(s.is_occurrence_collapsed(1, 0));
        assert!(!s.is_occurrence_collapsed(2, 0));
    }

    #[test]
    fn close_group_detail_clears_selection() {
        let mut s = loaded_with_multi_occ();
        assert_eq!(s.selected_group, Some(1));
        s.close_group_detail();
        assert!(s.selected_group.is_none());
    }

    #[test]
    fn group_toolbar_counts_reflect_visible_occurrences() {
        let mut s = loaded_with_multi_occ();
        // Before any dismiss: 3 files · (5 - 5 + 1) * (3 - 1) = 5 * 2 = 10
        // lines. (occurrences are occ("a.rs", 1, 5): 5 lines.)
        let (files, lines) = s.group_toolbar_counts(1);
        assert_eq!(files, 3);
        assert_eq!(lines, 10);
        // After one dismiss: 2 files · 5 lines (5 * 1).
        let _ = s.dismiss_occurrence(1, 0);
        let (files, lines) = s.group_toolbar_counts(1);
        assert_eq!(files, 2);
        assert_eq!(lines, 5);
    }

    #[test]
    fn selected_occurrences_filters_out_dismissed() {
        let mut s = loaded_with_multi_occ();
        assert_eq!(s.selected_occurrences().len(), 3);
        let _ = s.dismiss_occurrence(1, 0);
        let occs = s.selected_occurrences();
        assert_eq!(occs.len(), 2);
        let paths: Vec<_> = occs.iter().map(|o| o.path.clone()).collect();
        assert_eq!(paths, vec![PathBuf::from("b.rs"), PathBuf::from("c.rs")]);
    }

    #[test]
    fn dismiss_occurrence_shifts_higher_checkbox_indices_down() {
        // Indices reference the *post-dismiss* visible list, so if the
        // user had occurrence 2 checked and dismisses occurrence 0,
        // the old index-2 is now index-1 and must remain checked.
        let mut s = loaded_with_multi_occ();
        s.toggle_occurrence(1, 2);
        let _ = s.dismiss_occurrence(1, 0);
        assert!(
            s.is_occurrence_selected(1, 1),
            "higher checkbox index must shift down after dismiss"
        );
        assert!(!s.is_occurrence_selected(1, 2));
    }

    #[test]
    fn load_folder_applies_occurrence_suppressions() {
        // Integration with cache: write a scan, dismiss one occurrence,
        // reload via `load_folder`. The dismissed occurrence must not
        // reach the GroupView.
        use dedup_core::{MatchGroup as CoreGroup, ScanResult, Tier};

        let tmp = tempfile::tempdir().unwrap();
        let scan = ScanResult {
            groups: vec![CoreGroup {
                hash: 0xfeed_u64,
                tier: Tier::A,
                occurrences: vec![mk_occ("a.rs"), mk_occ("b.rs"), mk_occ("c.rs")],
            }],
            files_scanned: 3,
            issues: Vec::new(),
        };
        let mut cache = dedup_core::Cache::open(tmp.path()).unwrap();
        cache.write_scan_result(&scan).unwrap();
        cache
            .dismiss_occurrence(0xfeed_u64, &PathBuf::from("a.rs"))
            .unwrap();
        drop(cache);

        let r = load_folder(tmp.path());
        assert_eq!(r.status, AppStatus::Loaded);
        assert_eq!(r.groups.len(), 1);
        let paths: Vec<_> = r.groups[0]
            .occurrences
            .iter()
            .map(|o| o.path.clone())
            .collect();
        assert_eq!(paths, vec![PathBuf::from("b.rs"), PathBuf::from("c.rs")]);
    }

    #[test]
    fn load_folder_drops_group_when_occ_suppressions_bring_below_two() {
        use dedup_core::{MatchGroup as CoreGroup, ScanResult, Tier};

        let tmp = tempfile::tempdir().unwrap();
        let scan = ScanResult {
            groups: vec![CoreGroup {
                hash: 0xfeed_u64,
                tier: Tier::A,
                occurrences: vec![mk_occ("a.rs"), mk_occ("b.rs")],
            }],
            files_scanned: 2,
            issues: Vec::new(),
        };
        let mut cache = dedup_core::Cache::open(tmp.path()).unwrap();
        cache.write_scan_result(&scan).unwrap();
        cache
            .dismiss_occurrence(0xfeed_u64, &PathBuf::from("a.rs"))
            .unwrap();
        drop(cache);

        let r = load_folder(tmp.path());
        // Only one occurrence remains → group dropped entirely.
        assert!(r.groups.is_empty());
        assert_eq!(r.status, AppStatus::NoDuplicates);
    }

    // -----------------------------------------------------------------
    // Issue #28 — Open Recent MRU state methods on AppState.
    //
    // The persistence side of things (load/save) is covered by the
    // tests in `recent.rs`; these tests exercise the GPUI-free
    // mutations the view layer calls (push/remove/clear + the banner
    // helpers). We deliberately use `AppState::new` — not
    // `with_recents_from_disk` — so tests never touch the real
    // `$HOME/.config/dedup/recent.json`.
    //
    // `push_recent` / `remove_recent` / `clear_recents` call
    // `save_to_disk()`. To avoid polluting the developer's real
    // `~/.config/dedup/recent.json` (or, on CI, making two tests race
    // over it), every test below points `XDG_CONFIG_HOME` at a fresh
    // tempdir before mutating. The `_guard` keeps the dir alive for
    // the duration of the test.
    // -----------------------------------------------------------------

    /// Per-test guard: holds a unique tempdir + serializes over a
    /// process-wide `Mutex` so two recent-MRU tests don't stomp on
    /// each other's `XDG_CONFIG_HOME` (the env var is process-global
    /// and `cargo test` runs tests in parallel by default). Dropping
    /// the guard restores the previous env value.
    struct ConfigDirGuard {
        _dir: tempfile::TempDir,
        _lock: std::sync::MutexGuard<'static, ()>,
        prev: Option<std::ffi::OsString>,
    }

    impl Drop for ConfigDirGuard {
        fn drop(&mut self) {
            // SAFETY: env mutation is unsafe in Rust 2024 due to
            // POSIX's pthreads / signal-handler quirks, but we've
            // serialized all env touches in this module via the
            // mutex above.
            unsafe {
                match &self.prev {
                    Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                    None => std::env::remove_var("XDG_CONFIG_HOME"),
                }
            }
        }
    }

    fn redirect_config_dir() -> ConfigDirGuard {
        use std::sync::{Mutex, OnceLock};
        static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let lock = ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|p| p.into_inner());

        let dir = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("XDG_CONFIG_HOME");
        // SAFETY: `ENV_LOCK` above serializes every env mutation in
        // this test module so no two threads touch `set_var`
        // concurrently. See `ConfigDirGuard::drop` for the matching
        // restore path.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", dir.path());
        }
        ConfigDirGuard {
            _dir: dir,
            _lock: lock,
            prev,
        }
    }

    #[test]
    fn push_recent_updates_mru_front() {
        let _guard = redirect_config_dir();
        let mut s = AppState::new();
        s.push_recent(PathBuf::from("/a"));
        s.push_recent(PathBuf::from("/b"));
        let paths: Vec<_> = s
            .recent_projects
            .entries
            .iter()
            .map(|e| e.path.clone())
            .collect();
        assert_eq!(paths, vec![PathBuf::from("/b"), PathBuf::from("/a")]);
    }

    #[test]
    fn push_recent_dedupes_existing() {
        let _guard = redirect_config_dir();
        let mut s = AppState::new();
        s.push_recent(PathBuf::from("/a"));
        s.push_recent(PathBuf::from("/b"));
        s.push_recent(PathBuf::from("/a"));
        let paths: Vec<_> = s
            .recent_projects
            .entries
            .iter()
            .map(|e| e.path.clone())
            .collect();
        assert_eq!(paths, vec![PathBuf::from("/a"), PathBuf::from("/b")]);
    }

    #[test]
    fn push_recent_evicts_oldest_beyond_cap() {
        let _guard = redirect_config_dir();
        let mut s = AppState::new();
        for i in 0..6 {
            s.push_recent(PathBuf::from(format!("/p{i}")));
        }
        assert_eq!(s.recent_projects.entries.len(), crate::recent::MAX_RECENTS);
        // Oldest entry (`/p0`) must have been evicted.
        assert!(
            !s.recent_projects
                .entries
                .iter()
                .any(|e| e.path.as_path() == Path::new("/p0"))
        );
    }

    #[test]
    fn remove_recent_drops_entry() {
        let _guard = redirect_config_dir();
        let mut s = AppState::new();
        s.push_recent(PathBuf::from("/a"));
        s.push_recent(PathBuf::from("/b"));
        s.remove_recent(&PathBuf::from("/a"));
        assert_eq!(s.recent_projects.entries.len(), 1);
        assert_eq!(s.recent_projects.entries[0].path, PathBuf::from("/b"));
    }

    #[test]
    fn clear_recents_wipes_list() {
        let _guard = redirect_config_dir();
        let mut s = AppState::new();
        s.push_recent(PathBuf::from("/a"));
        s.push_recent(PathBuf::from("/b"));
        s.clear_recents();
        assert!(s.recent_projects.entries.is_empty());
    }

    #[test]
    fn recent_banner_surface_and_dismiss() {
        let mut s = AppState::new();
        assert!(s.recent_banner.is_none());
        s.surface_recent_banner(PathBuf::from("/missing"));
        assert_eq!(
            s.recent_banner.as_ref().map(|b| b.path.clone()),
            Some(PathBuf::from("/missing"))
        );
        s.dismiss_recent_banner();
        assert!(s.recent_banner.is_none());
    }

    // -----------------------------------------------------------------
    // Issue #30 — AppState-level toast / startup-error / issues tests.
    // -----------------------------------------------------------------

    #[test]
    fn surface_cache_error_newer_schema_uses_rescan_action() {
        let mut s = AppState::new();
        let err = CacheError::NewerSchema {
            found: 9,
            supported: 3,
        };
        let class = s.surface_cache_error(&err);
        assert_eq!(class, CacheErrorClass::NewerSchema);
        assert_eq!(s.toasts.len(), 1);
        let t = &s.toasts.toasts[0];
        assert_eq!(t.kind, ToastKind::Error);
        assert_eq!(
            t.action.as_ref().map(|a| a.action_name),
            Some(ACTION_CACHE_RESCAN)
        );
    }

    #[test]
    fn surface_cache_error_corrupted_uses_delete_action() {
        let mut s = AppState::new();
        let io = std::io::Error::other("database disk image is malformed");
        let err = CacheError::Io {
            path: PathBuf::from("/tmp/x"),
            source: io,
        };
        let class = s.surface_cache_error(&err);
        assert_eq!(class, CacheErrorClass::Corrupted);
        assert_eq!(
            s.toasts.toasts[0].action.as_ref().map(|a| a.action_name),
            Some(ACTION_CACHE_DELETE_AND_RESCAN)
        );
    }

    #[test]
    fn set_startup_error_captures_path_and_message() {
        // Route through the real loader — it's the production path
        // that produces `ConfigError::Parse` and we don't need a
        // hand-constructed `toml::de::Error` (which would add a
        // dev-dep). `parse_malformed_toml_classifies_as_startup_error`
        // covers the same code path end-to-end.
        let tmp = tempfile::tempdir().unwrap();
        let project_cfg = tmp
            .path()
            .join(dedup_core::config::PROJECT_CONFIG_DIR)
            .join(dedup_core::config::CONFIG_FILE);
        std::fs::create_dir_all(project_cfg.parent().unwrap()).unwrap();
        std::fs::write(&project_cfg, "nope[ malformed").unwrap();
        let cfg_err = Config::load(Some(tmp.path())).expect_err("malformed toml must fail");
        let mut s = AppState::new();
        s.set_startup_error(&cfg_err);
        let se = s.startup_error.as_ref().unwrap();
        assert_eq!(se.path, project_cfg);
        assert!(!se.message.is_empty());
        s.clear_startup_error();
        assert!(s.startup_error.is_none());
    }

    #[test]
    fn parse_malformed_toml_classifies_as_startup_error() {
        // Drive the same route `run()` takes: `Config::load` on
        // malformed TOML returns a `Parse` error, which the app wraps
        // into a `StartupError`. No GPUI here — the classification is
        // pure `ConfigError -> StartupError`.
        let tmp = tempfile::tempdir().unwrap();
        let project_cfg = tmp
            .path()
            .join(dedup_core::config::PROJECT_CONFIG_DIR)
            .join(dedup_core::config::CONFIG_FILE);
        std::fs::create_dir_all(project_cfg.parent().unwrap()).unwrap();
        std::fs::write(&project_cfg, "this is [[[ not valid toml").unwrap();
        let err = Config::load(Some(tmp.path())).expect_err("malformed toml must fail");
        let se = StartupError::from_config_error(&err);
        assert_eq!(se.path, project_cfg);
        assert!(!se.message.is_empty());
    }

    #[test]
    fn open_scan_issues_requires_non_empty_list() {
        let mut s = AppState::new();
        s.open_scan_issues();
        assert!(!s.scan_issues_open);
        s.set_scan_issues(vec![FileIssue {
            path: PathBuf::from("src/a.rs"),
            kind: dedup_core::FileIssueKind::ReadError,
            message: "denied".to_string(),
        }]);
        s.open_scan_issues();
        assert!(s.scan_issues_open);
        s.close_scan_issues();
        assert!(!s.scan_issues_open);
    }

    #[test]
    fn push_post_scan_issues_toast_is_noop_when_empty() {
        let mut s = AppState::new();
        s.push_post_scan_issues_toast();
        assert_eq!(s.toasts.len(), 0);
    }

    #[test]
    fn push_post_scan_issues_toast_includes_view_action() {
        let mut s = AppState::new();
        s.set_scan_issues(vec![FileIssue {
            path: PathBuf::from("src/a.rs"),
            kind: dedup_core::FileIssueKind::ReadError,
            message: "denied".to_string(),
        }]);
        s.push_post_scan_issues_toast();
        assert_eq!(s.toasts.len(), 1);
        let t = &s.toasts.toasts[0];
        assert_eq!(t.kind, ToastKind::Info);
        assert_eq!(
            t.action.as_ref().map(|a| a.action_name),
            Some(ACTION_SHOW_ISSUES)
        );
    }

    // -------------------------------------------------------------------
    // Issue #46 — sidebar sort-dropdown popup state helpers.
    // -------------------------------------------------------------------

    #[test]
    fn sort_popup_defaults_closed() {
        let s = AppState::new();
        assert!(!s.sort_popup_open);
    }

    #[test]
    fn toggle_sort_popup_flips_flag() {
        let mut s = AppState::new();
        s.toggle_sort_popup();
        assert!(s.sort_popup_open);
        s.toggle_sort_popup();
        assert!(!s.sort_popup_open);
    }

    #[test]
    fn close_sort_popup_is_idempotent() {
        let mut s = AppState::new();
        s.close_sort_popup();
        assert!(!s.sort_popup_open);
        s.toggle_sort_popup();
        assert!(s.sort_popup_open);
        s.close_sort_popup();
        assert!(!s.sort_popup_open);
        s.close_sort_popup();
        assert!(!s.sort_popup_open);
    }

    #[test]
    fn set_sort_key_closes_popup_and_swaps_key() {
        let mut s = AppState::new();
        s.toggle_sort_popup();
        assert!(s.sort_popup_open);
        assert_eq!(s.sort_key, SortKey::Severity);
        s.set_sort_key(SortKey::Alphabetical);
        assert_eq!(s.sort_key, SortKey::Alphabetical);
        assert!(
            !s.sort_popup_open,
            "selecting a key should dismiss the popup"
        );
    }

    #[test]
    fn set_sort_key_closes_popup_even_when_key_unchanged() {
        // Clicking the currently-selected key should still close the
        // popup (matches "selecting a key closes the popup" AC).
        let mut s = AppState::new();
        s.toggle_sort_popup();
        s.set_sort_key(s.sort_key);
        assert!(!s.sort_popup_open);
    }

    // -------------------------------------------------------------------
    // Issue #60 — undo toast after dismiss.
    //
    // Covers: push-undo-toast registers a `pending_undos` entry keyed
    // by the toast id; `take_pending_undo` drains it; `tick_toasts`
    // GCs stale entries when the toast auto-dismisses without action;
    // manual `dismiss_toast` also drops the pending entry (so `[×]`
    // on the undo toast commits to the dismissal).
    // -------------------------------------------------------------------

    #[test]
    fn push_undo_toast_for_group_registers_pending_payload() {
        let mut s = AppState::new();
        let id = s.push_undo_toast_for_group(0xAA, "my-group");
        assert_eq!(s.toasts.len(), 1);
        let toast = &s.toasts.toasts[0];
        assert!(toast.title.contains("my-group"));
        assert_eq!(
            toast.action.as_ref().map(|a| a.action_name),
            Some(ACTION_UNDO_DISMISS)
        );
        assert_eq!(
            s.pending_undos.get(&id).cloned(),
            Some(UndoKind::Group { hash: 0xAA })
        );
    }

    #[test]
    fn push_undo_toast_for_occurrence_registers_pending_payload() {
        let mut s = AppState::new();
        let id = s.push_undo_toast_for_occurrence(
            0xBB,
            PathBuf::from("src/a.rs"),
            "a.rs",
        );
        let toast = &s.toasts.toasts[0];
        assert!(toast.title.contains("a.rs"));
        assert_eq!(
            s.pending_undos.get(&id).cloned(),
            Some(UndoKind::Occurrence {
                hash: 0xBB,
                path: PathBuf::from("src/a.rs"),
            })
        );
    }

    #[test]
    fn take_pending_undo_drains_entry_on_click() {
        let mut s = AppState::new();
        let id = s.push_undo_toast_for_group(0xAA, "g");
        let taken = s.take_pending_undo(id);
        assert_eq!(taken, Some(UndoKind::Group { hash: 0xAA }));
        // Second drain is None — payload has moved.
        assert_eq!(s.take_pending_undo(id), None);
    }

    #[test]
    fn tick_toasts_gcs_pending_undo_when_toast_auto_dismisses() {
        // 6s UNDO_TTL — tick past that and both the toast and the
        // pending-undo entry should be gone. Simulates the "user
        // walked away" branch: the dismissal stays persisted because
        // no undo was ever fired.
        let mut s = AppState::new();
        let t0 = Instant::now();
        let id = s.push_undo_toast_for_group(0xAA, "g");
        assert!(s.pending_undos.contains_key(&id));
        s.tick_toasts(t0 + Duration::from_secs(7));
        assert!(!s.pending_undos.contains_key(&id));
        assert_eq!(s.toasts.len(), 0);
    }

    #[test]
    fn dismiss_toast_also_drops_pending_undo() {
        // Clicking the `[×]` on an undo toast should *not* leave a
        // stale payload behind — the user consciously passed on undo.
        let mut s = AppState::new();
        let id = s.push_undo_toast_for_group(0xAA, "g");
        s.dismiss_toast(id);
        assert!(!s.pending_undos.contains_key(&id));
    }

    #[test]
    fn multiple_undo_toasts_queue_independently() {
        // Rapid-fire dismissals must not clobber older undo payloads —
        // the pending-undos map is keyed by toast id, so three
        // concurrent toasts yield three live entries that each
        // `take_pending_undo` drains independently.
        let mut s = AppState::new();
        let a = s.push_undo_toast_for_group(0x01, "a");
        let b = s.push_undo_toast_for_group(0x02, "b");
        let c = s.push_undo_toast_for_occurrence(
            0x03,
            PathBuf::from("c.rs"),
            "c.rs",
        );
        assert_eq!(s.toasts.len(), 3);
        assert_eq!(s.pending_undos.len(), 3);
        assert_eq!(
            s.take_pending_undo(b),
            Some(UndoKind::Group { hash: 0x02 })
        );
        // Other two still intact.
        assert_eq!(
            s.take_pending_undo(a),
            Some(UndoKind::Group { hash: 0x01 })
        );
        assert_eq!(
            s.take_pending_undo(c),
            Some(UndoKind::Occurrence {
                hash: 0x03,
                path: PathBuf::from("c.rs"),
            })
        );
    }

    #[test]
    fn undo_toast_ttl_is_six_seconds() {
        // Guardrail: Info default is 3s, undo override is 6s.
        let mut s = AppState::new();
        let t0 = Instant::now();
        s.push_undo_toast_for_group(0xAA, "g");
        // Past the default Info TTL — still live.
        s.tick_toasts(t0 + Duration::from_secs(4));
        assert_eq!(s.toasts.len(), 1);
        // Past the undo TTL — auto-dismissed.
        s.tick_toasts(t0 + Duration::from_secs(7));
        assert_eq!(s.toasts.len(), 0);
    }
}
