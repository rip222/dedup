//! GUI logging: daily-rolling JSON log file + pruning helper.
//!
//! The macOS app installs a layered [`tracing`] subscriber that:
//!
//! - resolves `~/.config/dedup/logs/` via [`dirs::config_dir`] (creating
//!   the directory on first run),
//! - writes JSON events into `dedup.log.YYYY-MM-DD` files via
//!   [`tracing_appender::rolling::daily`],
//! - retains at most [`MAX_LOG_FILES`] files (manual prune on startup —
//!   `tracing-appender` doesn't garbage-collect).
//!
//! The subscriber also honors `RUST_LOG` via [`EnvFilter`], defaulting to
//! `info` when unset.
//!
//! This module intentionally does not initialize a subscriber at crate
//! load time; the app's `main` calls [`init_logging`] once. Multiple
//! calls are safe — the underlying `try_init` no-ops on re-install.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt;

/// The log filename prefix used by the rolling appender. Rotated files
/// are named `<prefix>.YYYY-MM-DD`, e.g. `dedup.log.2026-04-21`.
const LOG_FILE_PREFIX: &str = "dedup.log";

/// Maximum number of rotated log files to keep on disk.
pub const MAX_LOG_FILES: usize = 7;

/// Returned from [`init_logging`]. Holding it keeps the non-blocking
/// appender's background worker alive; dropping it flushes remaining
/// events. Apps typically bind it to a top-level variable in `main` that
/// lives for the life of the process.
#[must_use = "LogGuard flushes logs on drop — bind it to a variable in main()"]
pub struct LogGuard {
    _worker: WorkerGuard,
}

/// Resolve the dedup GUI log directory: `$XDG_CONFIG_HOME/dedup/logs/`,
/// or on macOS where [`dirs::config_dir`] returns
/// `$HOME/Library/Application Support`, override to the
/// `~/.config/dedup/logs/` path the PRD specifies.
///
/// We deliberately prefer `$HOME/.config/dedup/logs/` over the macOS
/// default Application Support directory because the PRD calls it out by
/// name and because the GUI shares its log location conventions with the
/// CLI on non-mac developer machines. Tests can redirect by setting
/// `HOME` (or `XDG_CONFIG_HOME`).
pub fn log_dir() -> io::Result<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        return Ok(PathBuf::from(xdg).join("dedup").join("logs"));
    }
    let home = std::env::var_os("HOME").ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "HOME environment variable not set; cannot resolve log directory",
        )
    })?;
    Ok(PathBuf::from(home)
        .join(".config")
        .join("dedup")
        .join("logs"))
}

/// Install the GUI [`tracing`] subscriber.
///
/// Creates the log directory if missing, prunes stale files, installs a
/// daily-rolling JSON-formatting appender, and returns a [`LogGuard`]
/// the caller must keep alive for the lifetime of the process.
pub fn init_logging() -> io::Result<LogGuard> {
    let dir = log_dir()?;
    fs::create_dir_all(&dir)?;

    // Prune on startup so a long-lived machine with daily logs doesn't
    // accumulate indefinitely.
    if let Err(e) = prune_old_logs(&dir, MAX_LOG_FILES) {
        // Pruning is best-effort — a failure here should not block the
        // app from starting. Emit via eprintln since tracing isn't
        // installed yet.
        eprintln!("dedup-gui: log pruning failed: {e}");
    }

    let appender = rolling::daily(&dir, LOG_FILE_PREFIX);
    let (writer, worker) = tracing_appender::non_blocking(appender);

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = fmt()
        .json()
        .with_env_filter(filter)
        .with_writer(writer)
        .try_init();

    Ok(LogGuard { _worker: worker })
}

/// Delete rotated log files beyond the `keep` most recent by mtime.
///
/// Matches files whose name starts with [`LOG_FILE_PREFIX`] and has a
/// date-like suffix — i.e. the filenames `tracing-appender` rotates to.
/// The "current" (undated) base file, if any, is left alone.
///
/// Returns the number of files removed.
pub fn prune_old_logs(dir: &Path, keep: usize) -> io::Result<usize> {
    if !dir.exists() {
        return Ok(0);
    }

    let mut candidates: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !is_rotated_log(&name) {
            continue;
        }
        let meta = entry.metadata()?;
        let mtime = meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        candidates.push((entry.path(), mtime));
    }

    // Newest first.
    candidates.sort_by(|a, b| b.1.cmp(&a.1));

    let mut removed = 0usize;
    for (path, _) in candidates.into_iter().skip(keep) {
        match fs::remove_file(&path) {
            Ok(()) => removed += 1,
            Err(e) => {
                // Surface but continue; a file we couldn't delete this
                // run will be retried on the next app start.
                eprintln!("dedup-gui: failed to delete {}: {e}", path.display());
            }
        }
    }
    Ok(removed)
}

/// Recognize a filename as a rotated dedup log file.
///
/// Matches `dedup.log.<suffix>` where the suffix is non-empty — e.g.
/// `dedup.log.2026-04-21`. The bare `dedup.log` base file (if the
/// appender ever produces one) is excluded so we never prune the
/// currently-active file.
fn is_rotated_log(name: &str) -> bool {
    match name.strip_prefix(LOG_FILE_PREFIX) {
        Some(rest) => rest.starts_with('.') && rest.len() > 1,
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};
    use tempfile::tempdir;

    #[test]
    fn is_rotated_log_matches_dated_files() {
        assert!(is_rotated_log("dedup.log.2026-04-21"));
        assert!(is_rotated_log("dedup.log.2026-01-01"));
    }

    #[test]
    fn is_rotated_log_rejects_unrelated_files() {
        assert!(!is_rotated_log("dedup.log"));
        assert!(!is_rotated_log("other.log.2026-04-21"));
        assert!(!is_rotated_log("dedup.logs"));
        assert!(!is_rotated_log("README.md"));
    }

    #[test]
    fn prune_keeps_newest_n_by_mtime() {
        let dir = tempdir().unwrap();
        // Create 10 rotated files with strictly-increasing mtimes so the
        // prune order is deterministic.
        let now = SystemTime::now();
        let mut expected_kept: Vec<PathBuf> = Vec::new();
        for i in 0..10 {
            let name = format!("dedup.log.2026-04-{:02}", i + 1);
            let p = dir.path().join(&name);
            fs::write(&p, b"").unwrap();
            // Older index → older mtime.
            let mtime = now - Duration::from_secs(((10 - i) * 3600) as u64);
            filetime::set_file_mtime(&p, filetime::FileTime::from_system_time(mtime)).unwrap();
            if i >= 3 {
                expected_kept.push(p);
            }
        }

        let removed = prune_old_logs(dir.path(), MAX_LOG_FILES).unwrap();
        assert_eq!(removed, 3);

        let remaining: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .collect();
        assert_eq!(remaining.len(), MAX_LOG_FILES);
    }

    #[test]
    fn prune_is_noop_when_under_limit() {
        let dir = tempdir().unwrap();
        for i in 0..3 {
            let name = format!("dedup.log.2026-04-{:02}", i + 1);
            fs::write(dir.path().join(&name), b"").unwrap();
        }
        let removed = prune_old_logs(dir.path(), MAX_LOG_FILES).unwrap();
        assert_eq!(removed, 0);
    }

    #[test]
    fn prune_ignores_unrelated_files() {
        let dir = tempdir().unwrap();
        for i in 0..10 {
            let name = format!("dedup.log.2026-04-{:02}", i + 1);
            fs::write(dir.path().join(&name), b"").unwrap();
        }
        fs::write(dir.path().join("unrelated.txt"), b"x").unwrap();

        prune_old_logs(dir.path(), MAX_LOG_FILES).unwrap();
        assert!(dir.path().join("unrelated.txt").exists());
    }

    #[test]
    fn prune_on_missing_dir_returns_zero() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("nope");
        assert_eq!(prune_old_logs(&missing, MAX_LOG_FILES).unwrap(), 0);
    }
}
