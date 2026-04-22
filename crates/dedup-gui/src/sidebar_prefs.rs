//! Persisted sidebar width (issue #47).
//!
//! GPUI-free, atomic-write persistence for the draggable sidebar
//! splitter. Mirrors the [`crate::recent::RecentProjects`] pattern:
//! serialize to `sidebar.json` under the same config dir
//! (`$XDG_CONFIG_HOME/dedup/` with `$HOME/.config/dedup/` fallback,
//! matching the macOS "Application Support" convention reuse called
//! out in the issue), write via sibling `.tmp` + `fs::rename` so a
//! crash mid-write never leaves a truncated JSON file behind.
//!
//! Corrupt / missing / unparseable file → the in-memory default
//! (320 px), never a panic and never a launch-blocking error. The GUI
//! treats the file as a pure hint.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::app_state::SortKey;
use crate::recent::config_dir;

/// Default sidebar width in pixels.
///
/// Matches the original fixed layout so an existing install upgrading
/// to #47 sees no visual change until the user drags the splitter.
pub const DEFAULT_SIDEBAR_WIDTH: f32 = 320.0;

/// Lower bound enforced by [`crate::app_state::AppState::set_sidebar_width`].
pub const MIN_SIDEBAR_WIDTH: f32 = 200.0;

/// Upper bound enforced by [`crate::app_state::AppState::set_sidebar_width`].
pub const MAX_SIDEBAR_WIDTH: f32 = 600.0;

/// On-disk filename for the persisted preferences. Lives next to
/// `recent.json` under `$XDG_CONFIG_HOME/dedup/`.
const SIDEBAR_FILE_NAME: &str = "sidebar.json";

/// Serializable shape of the preferences file.
///
/// Wrapped in a struct (rather than a bare `f32`) so future knobs —
/// e.g. "collapse Tier A by default", "show line numbers in detail" —
/// can land additive without bumping the file format. A missing or
/// malformed value snaps back to [`DEFAULT_SIDEBAR_WIDTH`] via
/// [`Self::load_or_default`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SidebarPrefs {
    /// Width in pixels. Clamped to [`MIN_SIDEBAR_WIDTH`] ..=
    /// [`MAX_SIDEBAR_WIDTH`] on read so a hand-edited file that sets
    /// `sidebar_width = 2.0` still renders a usable sidebar.
    #[serde(default = "default_width")]
    pub sidebar_width: f32,
    /// Whether the sidebar is hidden (issue #52). Toggled by ⌘B / the
    /// View → Toggle Sidebar menu item. Persists next to
    /// `sidebar_width` so visibility survives across window close and
    /// reopen. Defaults to `false` (visible) so a fresh install sees
    /// the sidebar.
    #[serde(default)]
    pub sidebar_hidden: bool,
    /// Persisted sidebar sort key (issue #56). `None` means the file
    /// was written before #56 landed — callers treat that as "keep
    /// the user's previous Impact-based ordering" rather than
    /// force-upgrading them to Severity. A fresh install (no file at
    /// all) skips this struct entirely and gets the in-memory default
    /// ([`SortKey::Severity`]) via [`Default::default`].
    #[serde(default)]
    pub sort_key: Option<SortKey>,
    /// Per-folder collapsed state for the dup-LOC sparkline strip
    /// (issue #63). Key = canonical folder path as a display string;
    /// value = `true` when the strip is collapsed. Missing entry → the
    /// strip is collapsed by default (the acceptance criterion). A
    /// legacy file without this field deserializes to an empty map via
    /// `#[serde(default)]`, preserving the collapsed-by-default
    /// behavior.
    #[serde(default)]
    pub sparkline_collapsed: HashMap<String, bool>,
}

fn default_width() -> f32 {
    DEFAULT_SIDEBAR_WIDTH
}

impl Default for SidebarPrefs {
    fn default() -> Self {
        Self {
            sidebar_width: DEFAULT_SIDEBAR_WIDTH,
            sidebar_hidden: false,
            // Issue #56 — fresh install defaults to Severity. The load
            // path that reads an existing file with no `sort_key` field
            // ([`SidebarPrefs::load_from_path`]) swaps this to
            // `Some(SortKey::Impact)` to preserve the legacy ordering
            // for users who already had a saved pref file.
            sort_key: Some(SortKey::Severity),
            sparkline_collapsed: HashMap::new(),
        }
    }
}

impl SidebarPrefs {
    /// Whether the sparkline strip is collapsed for `folder` (#63).
    /// Returns `true` (collapsed) when there is no entry — the strip
    /// defaults to collapsed per the acceptance criteria.
    pub fn is_sparkline_collapsed(&self, folder: &str) -> bool {
        self.sparkline_collapsed
            .get(folder)
            .copied()
            .unwrap_or(true)
    }

    /// Record the collapsed state for `folder` (#63). Caller persists
    /// via [`Self::save_to_disk`].
    pub fn set_sparkline_collapsed(&mut self, folder: &str, collapsed: bool) {
        self.sparkline_collapsed
            .insert(folder.to_string(), collapsed);
    }

    /// Clamp `self.sidebar_width` into the acceptable range. Callers
    /// should invoke this after deserialization — the [`Self::load`] +
    /// [`Self::load_or_default`] entry points already do so.
    pub fn clamp_in_place(&mut self) {
        self.sidebar_width = clamp_width(self.sidebar_width);
    }

    /// Load from the default config path. Missing file / corrupt JSON
    /// / unreadable path all resolve to [`Self::default`] — this helper
    /// never returns `Err` because a failed read must never block
    /// window open.
    pub fn load_or_default() -> Self {
        let Some(path) = sidebar_file_path() else {
            tracing::debug!("dedup-gui: sidebar.json path unresolvable — using default width");
            return Self::default();
        };
        Self::load_from_path(&path)
    }

    /// Lower-level load, parameterized on an explicit path so tests
    /// can point at a tempdir without mutating `$HOME` /
    /// `$XDG_CONFIG_HOME`.
    pub fn load_from_path(path: &Path) -> Self {
        match fs::read_to_string(path) {
            Ok(body) => match serde_json::from_str::<SidebarPrefs>(&body) {
                Ok(mut prefs) => {
                    prefs.clamp_in_place();
                    // Issue #56 — legacy pref files pre-date the
                    // `sort_key` field. A missing field deserializes
                    // as `None`; upgrade that to `Some(Impact)` so
                    // existing users keep their prior "Impact" sort
                    // rather than silently flipping to Severity.
                    if prefs.sort_key.is_none() {
                        prefs.sort_key = Some(SortKey::Impact);
                    }
                    prefs
                }
                Err(e) => {
                    tracing::debug!(
                        path = %path.display(),
                        error = %e,
                        "dedup-gui: sidebar.json malformed — using default width",
                    );
                    Self::default()
                }
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => Self::default(),
            Err(e) => {
                tracing::debug!(
                    path = %path.display(),
                    error = %e,
                    "dedup-gui: sidebar.json unreadable — using default width",
                );
                Self::default()
            }
        }
    }

    /// Serialize + atomically persist to the default config path.
    pub fn save_to_disk(&self) -> io::Result<()> {
        let Some(path) = sidebar_file_path() else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "cannot resolve sidebar.json path (HOME unset?)",
            ));
        };
        self.save_to_path(&path)
    }

    /// Lower-level save, parameterized on an explicit path.
    pub fn save_to_path(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let payload = serde_json::to_vec_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let tmp = tmp_path_for(path);
        fs::write(&tmp, &payload)?;
        fs::rename(&tmp, path)?;
        Ok(())
    }
}

/// Clamp a raw pixel value into the acceptable sidebar-width range.
///
/// Non-finite values (NaN / infinities) snap back to the default so a
/// bogus file never yields an unusable layout.
pub fn clamp_width(w: f32) -> f32 {
    if !w.is_finite() {
        return DEFAULT_SIDEBAR_WIDTH;
    }
    w.clamp(MIN_SIDEBAR_WIDTH, MAX_SIDEBAR_WIDTH)
}

/// Resolve the full path to `sidebar.json` under the shared config
/// dir. `None` when neither `$XDG_CONFIG_HOME` nor `$HOME` is set —
/// callers treat that as "no persistence available".
pub fn sidebar_file_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join(SIDEBAR_FILE_NAME))
}

/// Sibling `.tmp` path used by the atomic-write dance. Lifted so
/// tests can check the shape without writing anything.
fn tmp_path_for(path: &Path) -> PathBuf {
    let mut os = path.as_os_str().to_os_string();
    os.push(".tmp");
    PathBuf::from(os)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn clamp_below_min_returns_min() {
        assert_eq!(clamp_width(50.0), MIN_SIDEBAR_WIDTH);
    }

    #[test]
    fn clamp_above_max_returns_max() {
        assert_eq!(clamp_width(9999.0), MAX_SIDEBAR_WIDTH);
    }

    #[test]
    fn clamp_in_range_returns_value() {
        assert_eq!(clamp_width(400.0), 400.0);
    }

    #[test]
    fn clamp_nan_returns_default() {
        assert_eq!(clamp_width(f32::NAN), DEFAULT_SIDEBAR_WIDTH);
    }

    #[test]
    fn default_is_320() {
        assert_eq!(SidebarPrefs::default().sidebar_width, 320.0);
    }

    #[test]
    fn load_missing_file_returns_default() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nope.json");
        let prefs = SidebarPrefs::load_from_path(&path);
        assert_eq!(prefs, SidebarPrefs::default());
    }

    #[test]
    fn load_malformed_json_returns_default_without_panic() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sidebar.json");
        fs::write(&path, "{ not json").unwrap();
        let prefs = SidebarPrefs::load_from_path(&path);
        assert_eq!(prefs, SidebarPrefs::default());
    }

    #[test]
    fn load_clamps_out_of_range_stored_value() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sidebar.json");
        fs::write(&path, r#"{"sidebar_width": 5000.0}"#).unwrap();
        let prefs = SidebarPrefs::load_from_path(&path);
        assert_eq!(prefs.sidebar_width, MAX_SIDEBAR_WIDTH);
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sidebar.json");
        let prefs = SidebarPrefs {
            sidebar_width: 412.0,
            sidebar_hidden: false,
            sort_key: Some(SortKey::Severity),
            sparkline_collapsed: HashMap::new(),
        };
        prefs.save_to_path(&path).unwrap();
        let loaded = SidebarPrefs::load_from_path(&path);
        assert_eq!(loaded, prefs);
    }

    #[test]
    fn save_creates_parent_directory() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nested").join("deeper").join("sidebar.json");
        SidebarPrefs::default().save_to_path(&path).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn save_atomic_replaces_existing_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sidebar.json");
        fs::write(&path, b"old contents").unwrap();

        let prefs = SidebarPrefs {
            sidebar_width: 500.0,
            sidebar_hidden: false,
            sort_key: Some(SortKey::Severity),
            sparkline_collapsed: HashMap::new(),
        };
        prefs.save_to_path(&path).unwrap();

        let body = fs::read_to_string(&path).unwrap();
        assert!(body.contains("500"));
        assert!(!dir.path().join("sidebar.json.tmp").exists());
    }

    // Issue #52 — sidebar-hidden persistence.
    #[test]
    fn default_sidebar_hidden_is_false() {
        assert!(!SidebarPrefs::default().sidebar_hidden);
    }

    #[test]
    fn save_and_load_roundtrip_preserves_hidden_flag() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sidebar.json");
        let prefs = SidebarPrefs {
            sidebar_width: 320.0,
            sidebar_hidden: true,
            sort_key: Some(SortKey::Severity),
            sparkline_collapsed: HashMap::new(),
        };
        prefs.save_to_path(&path).unwrap();
        let loaded = SidebarPrefs::load_from_path(&path);
        assert!(loaded.sidebar_hidden);
    }

    #[test]
    fn legacy_file_without_hidden_defaults_to_visible() {
        // Pre-#52 sidebar.json only stored sidebar_width. `#[serde(default)]`
        // must let us read it back as sidebar_hidden=false instead of
        // erroring + snapping the whole struct to defaults.
        let dir = tempdir().unwrap();
        let path = dir.path().join("sidebar.json");
        fs::write(&path, r#"{"sidebar_width": 400.0}"#).unwrap();
        let prefs = SidebarPrefs::load_from_path(&path);
        assert_eq!(prefs.sidebar_width, 400.0);
        assert!(!prefs.sidebar_hidden);
    }

    // -----------------------------------------------------------------
    // Issue #56 — persisted sort key.
    // -----------------------------------------------------------------

    #[test]
    fn default_sort_key_is_severity_for_new_install() {
        // `Default::default` is what `load_or_default` returns when the
        // file doesn't exist — i.e. a brand-new install. Issue #56
        // specifies that brand-new installs default to Severity.
        assert_eq!(
            SidebarPrefs::default().sort_key,
            Some(SortKey::Severity)
        );
    }

    #[test]
    fn legacy_file_without_sort_key_upgrades_to_impact() {
        // Pre-#56 sidebar.json doesn't have a `sort_key` field. The
        // load path must interpret that as "keep the user's prior
        // Impact ordering", not force them onto Severity.
        let dir = tempdir().unwrap();
        let path = dir.path().join("sidebar.json");
        fs::write(&path, r#"{"sidebar_width": 320.0}"#).unwrap();
        let prefs = SidebarPrefs::load_from_path(&path);
        assert_eq!(
            prefs.sort_key,
            Some(SortKey::Impact),
            "legacy users keep their persisted Impact sort"
        );
    }

    #[test]
    fn missing_file_returns_severity_for_new_install() {
        // No file on disk = fresh install. Must land on Severity, not
        // Impact (the legacy upgrade path is reserved for files that
        // exist but predate #56).
        let dir = tempdir().unwrap();
        let path = dir.path().join("does-not-exist.json");
        let prefs = SidebarPrefs::load_from_path(&path);
        assert_eq!(prefs.sort_key, Some(SortKey::Severity));
    }

    #[test]
    fn save_and_load_roundtrip_preserves_sort_key() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sidebar.json");
        let prefs = SidebarPrefs {
            sidebar_width: 320.0,
            sidebar_hidden: false,
            sort_key: Some(SortKey::Alphabetical),
            sparkline_collapsed: HashMap::new(),
        };
        prefs.save_to_path(&path).unwrap();
        let loaded = SidebarPrefs::load_from_path(&path);
        assert_eq!(loaded.sort_key, Some(SortKey::Alphabetical));
    }

    // -----------------------------------------------------------------
    // Issue #63 — per-folder sparkline collapsed state.
    // -----------------------------------------------------------------

    #[test]
    fn sparkline_collapsed_defaults_to_true_for_unseen_folder() {
        // Acceptance criterion: "Strip collapsed by default". A folder
        // the user has never touched must therefore report collapsed.
        let prefs = SidebarPrefs::default();
        assert!(prefs.is_sparkline_collapsed("/repo/one"));
    }

    #[test]
    fn sparkline_collapsed_is_per_folder_independent() {
        let mut prefs = SidebarPrefs::default();
        prefs.set_sparkline_collapsed("/repo/one", false);
        assert!(!prefs.is_sparkline_collapsed("/repo/one"));
        // An unrelated folder stays on the default (collapsed).
        assert!(prefs.is_sparkline_collapsed("/repo/two"));
    }

    #[test]
    fn sparkline_collapsed_roundtrips_through_disk() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sidebar.json");
        let mut prefs = SidebarPrefs::default();
        prefs.set_sparkline_collapsed("/repo/one", false);
        prefs.save_to_path(&path).unwrap();
        let loaded = SidebarPrefs::load_from_path(&path);
        assert!(!loaded.is_sparkline_collapsed("/repo/one"));
        assert!(loaded.is_sparkline_collapsed("/repo/two"));
    }

    #[test]
    fn legacy_file_without_sparkline_map_defaults_to_collapsed() {
        // Pre-#63 sidebar.json does not have `sparkline_collapsed`.
        // `#[serde(default)]` must deserialize an empty map so every
        // folder starts out collapsed.
        let dir = tempdir().unwrap();
        let path = dir.path().join("sidebar.json");
        fs::write(&path, r#"{"sidebar_width": 320.0}"#).unwrap();
        let prefs = SidebarPrefs::load_from_path(&path);
        assert!(prefs.is_sparkline_collapsed("/any"));
    }

    #[test]
    fn tmp_path_appends_dot_tmp_suffix() {
        let p = PathBuf::from("/tmp/sidebar.json");
        assert_eq!(tmp_path_for(&p), PathBuf::from("/tmp/sidebar.json.tmp"));
    }
}
