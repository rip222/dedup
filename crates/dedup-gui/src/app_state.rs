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

use dedup_core::{Cache, CacheError, Tier};

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
}
