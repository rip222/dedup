//! "Open Recent" MRU list for the File menu (issue #28).
//!
//! This module is intentionally GPUI-free: the data model, persistence,
//! and "is-this-entry-stale?" check are all pure Rust so they're unit-
//! testable off the main thread. The menubar/view layer reads
//! [`RecentProjects`] from [`crate::app_state::AppState`] and rebuilds
//! the NSMenu submenu whenever it mutates.
//!
//! Layout on disk: `$XDG_CONFIG_HOME/dedup/recent.json` (falling back to
//! `$HOME/.config/dedup/recent.json`), matching the log-directory
//! convention from issue #16. The payload is a JSON object with a
//! single top-level key (`entries`) so we can evolve the schema without
//! breaking the file open path:
//!
//! ```json
//! { "entries": [
//!   { "path": "/Users/alice/code/project", "opened_at": 1713618123 }
//! ] }
//! ```
//!
//! MRU semantics (newest first, capped at [`MAX_RECENTS`] = 5):
//! - [`RecentProjects::push`] moves the path to the front (dedup on
//!   re-open) and truncates the tail.
//! - [`RecentProjects::remove`] drops a single entry.
//! - [`RecentProjects::clear`] wipes the list.
//!
//! Corrupt / missing files never panic — [`RecentProjects::load_from_disk`]
//! returns an empty list and logs a debug line. Writes are atomic: we
//! write to a sibling `recent.json.tmp` and `fs::rename` into place so a
//! crash mid-write never leaves a truncated JSON file behind.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Hard cap on the MRU list. Matches the PRD / acceptance criterion.
pub const MAX_RECENTS: usize = 5;

/// The on-disk filename for the recents store. Lives next to
/// `logs/` under `$XDG_CONFIG_HOME/dedup/`.
const RECENT_FILE_NAME: &str = "recent.json";

/// Single MRU entry.
///
/// `opened_at` is unix seconds (UTC) — we only need it for ordering
/// within the list and for a possible future "Last opened N hours ago"
/// subtitle. Stored as `i64` so the JSON is small and we don't have to
/// deal with `SystemTime` serde.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentProject {
    pub path: PathBuf,
    pub opened_at: i64,
}

impl RecentProject {
    /// True iff the path no longer resolves to an existing directory.
    ///
    /// Used by the menubar click handler to decide whether to load the
    /// folder or pop a "this project is missing" banner. Pure filesystem
    /// check — no network, no symlink chasing beyond `Path::is_dir`.
    pub fn is_stale(&self) -> bool {
        !self.path.is_dir()
    }

    /// Short menu label: the path with `$HOME` collapsed to `~` and only
    /// the last two segments visible when it's deep — keeps the File →
    /// Open Recent submenu narrow enough to render on standard-width
    /// screens. Pure function, tested directly.
    pub fn menu_label(&self) -> String {
        menu_label_for(&self.path, std::env::var_os("HOME").map(PathBuf::from))
    }
}

/// MRU collection, newest first. Capped at [`MAX_RECENTS`].
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentProjects {
    #[serde(default)]
    pub entries: Vec<RecentProject>,
}

impl RecentProjects {
    /// An empty MRU. Equivalent to `Default::default()` but more
    /// self-documenting at call sites.
    pub fn empty() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Push `path` to the front of the MRU, deduping any prior copy and
    /// truncating the tail to [`MAX_RECENTS`].
    ///
    /// The timestamp is produced from `SystemTime::now()`; callers that
    /// need determinism in tests can construct entries directly.
    pub fn push(&mut self, path: impl Into<PathBuf>) {
        let path = path.into();
        // Dedup — if the path was already in the list, drop it; the
        // fresh insert below re-adds it at the front with a bumped
        // timestamp (so the on-disk order matches the acceptance
        // criterion "MRU caps at 5; oldest evicted first").
        self.entries.retain(|e| e.path != path);
        self.entries.insert(
            0,
            RecentProject {
                path,
                opened_at: now_unix_secs(),
            },
        );
        if self.entries.len() > MAX_RECENTS {
            self.entries.truncate(MAX_RECENTS);
        }
    }

    /// Drop a single entry by path equality. No-op if the path isn't in
    /// the list. Used by the banner's `[Remove from recents]` button and
    /// by the click-stale-entry flow.
    pub fn remove(&mut self, path: &Path) {
        self.entries.retain(|e| e.path != path);
    }

    /// Wipe every entry. Used by File → Open Recent → Clear Menu.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Load the MRU from `recent.json`.
    ///
    /// Non-existent file → empty MRU (first launch). Corrupt JSON →
    /// empty MRU + a `debug!` log line. Filesystem errors are also
    /// swallowed into an empty MRU; the GUI treats this as "no recents"
    /// rather than blocking startup on a config-dir read error.
    pub fn load_from_disk() -> Self {
        let Some(path) = recent_file_path() else {
            tracing::debug!("dedup-gui: recent.json path unresolvable — empty MRU");
            return Self::empty();
        };
        Self::load_from_path(&path)
    }

    /// Lower-level load, parameterized on an explicit path so tests can
    /// point at a tempdir without mutating `$HOME` / `$XDG_CONFIG_HOME`.
    pub fn load_from_path(path: &Path) -> Self {
        match fs::read_to_string(path) {
            Ok(body) => match serde_json::from_str::<RecentProjects>(&body) {
                Ok(mut list) => {
                    // Enforce the cap even if the file was hand-edited
                    // with more entries than the current build allows.
                    if list.entries.len() > MAX_RECENTS {
                        list.entries.truncate(MAX_RECENTS);
                    }
                    list
                }
                Err(e) => {
                    tracing::debug!(
                        path = %path.display(),
                        error = %e,
                        "dedup-gui: recent.json malformed — starting with empty MRU",
                    );
                    Self::empty()
                }
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => Self::empty(),
            Err(e) => {
                tracing::debug!(
                    path = %path.display(),
                    error = %e,
                    "dedup-gui: recent.json unreadable — starting with empty MRU",
                );
                Self::empty()
            }
        }
    }

    /// Serialize + atomically write to `recent.json`.
    ///
    /// "Atomic" here means: serialize into memory, write to
    /// `recent.json.tmp`, `fs::rename` onto the final path. A crash
    /// between the two steps leaves the previous `recent.json` intact;
    /// a crash before the rename leaves a stray `.tmp` which we'd
    /// happily overwrite on the next save.
    pub fn save_to_disk(&self) -> io::Result<()> {
        let Some(path) = recent_file_path() else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "cannot resolve recent.json path (HOME unset?)",
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

/// Build the sibling `.tmp` path used by the atomic-write dance.
///
/// Lifted into a free function so tests can verify the shape without
/// writing anything, and so the save path stays free of ad-hoc string
/// concatenation.
fn tmp_path_for(path: &Path) -> PathBuf {
    let mut os = path.as_os_str().to_os_string();
    os.push(".tmp");
    PathBuf::from(os)
}

/// Resolve `$XDG_CONFIG_HOME/dedup/recent.json` (falling back to
/// `$HOME/.config/dedup/recent.json`).
///
/// Returns `None` when neither env var is set — callers treat that as
/// "no persistence available" and skip the read / write. Matches the
/// shape of [`crate::logging::log_dir`]; lifted into a shared helper
/// below so the two paths can't drift.
pub fn recent_file_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join(RECENT_FILE_NAME))
}

/// `$XDG_CONFIG_HOME/dedup/` (or `$HOME/.config/dedup/`), same rule as
/// [`crate::logging::log_dir`]. Public so future config-file features
/// can reuse it instead of re-inventing the fallback chain.
pub fn config_dir() -> Option<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        return Some(PathBuf::from(xdg).join("dedup"));
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".config").join("dedup"))
}

/// Current wall-clock in unix seconds, clamped to `i64::MAX` on the
/// (vanishingly unlikely) chance the clock is pre-1970 or past year
/// ~292 billion.
fn now_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs().min(i64::MAX as u64) as i64)
        .unwrap_or(0)
}

/// Shorten `path` for the menu:
/// - If `home_opt` is a prefix of `path`, replace that prefix with `~`.
/// - Otherwise, if the path has more than three components, keep only
///   the last two joined by the platform separator, prefixed with
///   `…/`.
/// - Otherwise, show the path in full.
///
/// Pure function (`home_opt` is injected rather than read from env) so
/// the tests are portable.
fn menu_label_for(path: &Path, home_opt: Option<PathBuf>) -> String {
    // Collapse $HOME → ~ when possible.
    if let Some(home) = home_opt
        && let Ok(stripped) = path.strip_prefix(&home)
    {
        let rel = stripped.display().to_string();
        return if rel.is_empty() {
            "~".to_string()
        } else {
            format!("~/{rel}")
        };
    }
    // Deep paths: show only the last two segments with a leading ellipsis.
    let comps: Vec<_> = path.components().collect();
    if comps.len() > 3 {
        let tail: PathBuf = comps
            .iter()
            .rev()
            .take(2)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .map(|c| c.as_os_str())
            .collect();
        return format!("\u{2026}/{}", tail.display());
    }
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::tempdir;

    // ---------------------------------------------------------------
    // push / remove / clear
    // ---------------------------------------------------------------

    #[test]
    fn push_adds_to_front() {
        let mut list = RecentProjects::empty();
        list.push("/a");
        list.push("/b");
        let paths: Vec<_> = list.entries.iter().map(|e| e.path.clone()).collect();
        assert_eq!(paths, vec![PathBuf::from("/b"), PathBuf::from("/a")]);
    }

    #[test]
    fn push_dedups_existing_entry() {
        let mut list = RecentProjects::empty();
        list.push("/a");
        list.push("/b");
        list.push("/a"); // re-open `/a` — should move it to front, not duplicate.
        let paths: Vec<_> = list.entries.iter().map(|e| e.path.clone()).collect();
        assert_eq!(paths, vec![PathBuf::from("/a"), PathBuf::from("/b")]);
    }

    #[test]
    fn push_caps_at_max_recents_evicting_oldest() {
        let mut list = RecentProjects::empty();
        for i in 0..7 {
            list.push(format!("/p{i}"));
        }
        assert_eq!(list.entries.len(), MAX_RECENTS);
        // Newest (pushed last) at the front → `/p6`, then `/p5`…`/p2`.
        let paths: Vec<_> = list
            .entries
            .iter()
            .map(|e| e.path.display().to_string())
            .collect();
        assert_eq!(paths, vec!["/p6", "/p5", "/p4", "/p3", "/p2"]);
    }

    #[test]
    fn remove_drops_single_entry() {
        let mut list = RecentProjects::empty();
        list.push("/a");
        list.push("/b");
        list.push("/c");
        list.remove(&PathBuf::from("/b"));
        let paths: Vec<_> = list.entries.iter().map(|e| e.path.clone()).collect();
        assert_eq!(paths, vec![PathBuf::from("/c"), PathBuf::from("/a")]);
    }

    #[test]
    fn remove_missing_path_is_noop() {
        let mut list = RecentProjects::empty();
        list.push("/a");
        list.remove(&PathBuf::from("/does-not-exist"));
        assert_eq!(list.entries.len(), 1);
    }

    #[test]
    fn clear_wipes_all() {
        let mut list = RecentProjects::empty();
        list.push("/a");
        list.push("/b");
        list.clear();
        assert!(list.entries.is_empty());
    }

    // ---------------------------------------------------------------
    // Persistence: load / save / roundtrip
    // ---------------------------------------------------------------

    #[test]
    fn load_from_missing_file_returns_empty() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nope.json");
        let list = RecentProjects::load_from_path(&path);
        assert!(list.entries.is_empty());
    }

    #[test]
    fn load_from_malformed_json_returns_empty_without_panic() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("recent.json");
        fs::write(&path, "{ this is not json").unwrap();
        let list = RecentProjects::load_from_path(&path);
        assert!(list.entries.is_empty());
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("recent.json");
        let mut list = RecentProjects::empty();
        list.push("/a");
        list.push("/b");
        list.save_to_path(&path).unwrap();

        let loaded = RecentProjects::load_from_path(&path);
        assert_eq!(loaded, list);
    }

    #[test]
    fn save_creates_parent_directory() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nested").join("deeper").join("recent.json");
        let list = RecentProjects::empty();
        list.save_to_path(&path).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn save_replaces_existing_file_atomically() {
        // Pre-existing file should be replaced in-place without ever
        // leaving a truncated intermediate. We can't observe the rename
        // step directly from a unit test, but we can at least verify the
        // final contents are the new ones and no `.tmp` is left behind.
        let dir = tempdir().unwrap();
        let path = dir.path().join("recent.json");
        fs::write(&path, b"old contents").unwrap();

        let mut list = RecentProjects::empty();
        list.push("/fresh");
        list.save_to_path(&path).unwrap();

        let body = fs::read_to_string(&path).unwrap();
        assert!(body.contains("/fresh"));
        assert!(!dir.path().join("recent.json.tmp").exists());
    }

    #[test]
    fn load_clamps_overlong_files_to_max_recents() {
        // Defensive: if a user hand-edits `recent.json` to include more
        // than MAX_RECENTS entries, we still render only the first 5.
        let dir = tempdir().unwrap();
        let path = dir.path().join("recent.json");
        let entries: Vec<RecentProject> = (0..10)
            .map(|i| RecentProject {
                path: PathBuf::from(format!("/p{i}")),
                opened_at: i as i64,
            })
            .collect();
        let list = RecentProjects { entries };
        fs::write(&path, serde_json::to_string(&list).unwrap()).unwrap();

        let loaded = RecentProjects::load_from_path(&path);
        assert_eq!(loaded.entries.len(), MAX_RECENTS);
    }

    // ---------------------------------------------------------------
    // is_stale
    // ---------------------------------------------------------------

    #[test]
    fn is_stale_true_for_missing_path() {
        let entry = RecentProject {
            path: PathBuf::from("/definitely-not-a-real-directory-xyz-42"),
            opened_at: 0,
        };
        assert!(entry.is_stale());
    }

    #[test]
    fn is_stale_false_for_existing_directory() {
        let dir = tempdir().unwrap();
        let entry = RecentProject {
            path: dir.path().to_path_buf(),
            opened_at: 0,
        };
        assert!(!entry.is_stale());
    }

    // ---------------------------------------------------------------
    // menu_label_for — HOME collapse + deep-path shortening
    // ---------------------------------------------------------------

    #[test]
    fn menu_label_collapses_home_to_tilde() {
        let home = PathBuf::from("/Users/alice");
        let path = PathBuf::from("/Users/alice/code/dedup");
        assert_eq!(menu_label_for(&path, Some(home)), "~/code/dedup");
    }

    #[test]
    fn menu_label_leaves_short_foreign_paths_alone() {
        let path = PathBuf::from("/opt/repo");
        assert_eq!(
            menu_label_for(&path, Some(PathBuf::from("/Users/alice"))),
            "/opt/repo"
        );
    }

    #[test]
    fn menu_label_ellipsises_deep_foreign_paths() {
        let path = PathBuf::from("/opt/vendor/deep/nested/thing");
        let label = menu_label_for(&path, Some(PathBuf::from("/Users/alice")));
        assert_eq!(label, "\u{2026}/nested/thing");
    }

    #[test]
    fn menu_label_for_home_itself_is_tilde() {
        let home = PathBuf::from("/Users/alice");
        assert_eq!(menu_label_for(&home, Some(home.clone())), "~");
    }

    // ---------------------------------------------------------------
    // tmp_path_for — atomic-save helper shape.
    // ---------------------------------------------------------------

    #[test]
    fn tmp_path_appends_dot_tmp_suffix() {
        let p = PathBuf::from("/tmp/recent.json");
        assert_eq!(tmp_path_for(&p), PathBuf::from("/tmp/recent.json.tmp"));
    }
}
