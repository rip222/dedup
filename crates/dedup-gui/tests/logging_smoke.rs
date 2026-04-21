//! Smoke test for the GUI logging subscriber (issue #16).
//!
//! Verifies:
//!
//! 1. [`init_logging`] materializes the target log directory and produces
//!    at least one `dedup.log.*` file when a `tracing` event is emitted.
//! 2. [`prune_old_logs`] enforces the 7-file retention cap.
//!
//! Runs only on macOS because the GUI crate itself is macOS-gated.

#![cfg(target_os = "macos")]

use std::fs;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use dedup_gui::{MAX_LOG_FILES, init_logging, log_dir, prune_old_logs};

/// Tests in this file mutate the process-wide `HOME`/`XDG_CONFIG_HOME`
/// env vars; cargo runs them in parallel within the same binary by
/// default, so we serialize with a mutex. Combining into one giant
/// `#[test]` would hide which assertion failed, so we pay the mutex.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Install a temp `HOME` for the duration of the test so
/// `~/.config/dedup/logs/` resolves under the tempdir. We also clear
/// `XDG_CONFIG_HOME` so the HOME-relative path wins.
struct HomeGuard {
    _tmp: tempfile::TempDir,
    canonical: std::path::PathBuf,
    prev_home: Option<std::ffi::OsString>,
    prev_xdg: Option<std::ffi::OsString>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl HomeGuard {
    fn install() -> Self {
        let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        // On macOS `tempdir()` returns something like `/var/folders/...`
        // which `fs::canonicalize` resolves to `/private/var/folders/...`.
        // We need the canonicalized form for the `starts_with` assertion
        // because that's what `init_logging` ends up joining against.
        let canonical = fs::canonicalize(tmp.path()).unwrap();
        let prev_home = std::env::var_os("HOME");
        let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
        // SAFETY: the `ENV_LOCK` mutex above ensures at most one
        // `HomeGuard` exists at a time within this test binary. Other
        // test binaries run in their own processes and so can't race.
        unsafe {
            std::env::set_var("HOME", &canonical);
            std::env::remove_var("XDG_CONFIG_HOME");
        }
        Self {
            _tmp: tmp,
            canonical,
            prev_home,
            prev_xdg,
            _lock: lock,
        }
    }

    fn path(&self) -> &Path {
        &self.canonical
    }
}

impl Drop for HomeGuard {
    fn drop(&mut self) {
        unsafe {
            match &self.prev_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            match &self.prev_xdg {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
        }
    }
}

#[test]
fn init_logging_creates_dir_and_writes_a_file() {
    let home = HomeGuard::install();

    let guard = init_logging().expect("init_logging must not panic");

    // Resolve the directory the same way `init_logging` did.
    let dir = log_dir().unwrap();
    assert!(
        dir.starts_with(home.path()),
        "log dir {:?} should live under temp HOME {:?}",
        dir,
        home.path()
    );
    assert!(dir.exists(), "log dir materialized");

    // Emit an event through the global subscriber so the appender has
    // something to flush to disk.
    tracing::info!("smoke test event");

    // Drop the guard → flushes the non-blocking worker.
    drop(guard);

    let files: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("dedup.log"))
        .collect();
    assert!(
        !files.is_empty(),
        "at least one dedup.log.* file should exist after init_logging; saw {files:?}"
    );
}

#[test]
fn prune_retains_seven_most_recent() {
    let home = HomeGuard::install();
    let dir = home.path().join(".config").join("dedup").join("logs");
    fs::create_dir_all(&dir).unwrap();

    // 10 rotated files with strictly-increasing mtimes.
    let now = SystemTime::now();
    for i in 0..10 {
        let name = format!("dedup.log.2026-04-{:02}", i + 1);
        let p = dir.join(&name);
        fs::write(&p, b"").unwrap();
        let mtime = now - Duration::from_secs(((10 - i) * 3600) as u64);
        filetime::set_file_mtime(&p, filetime::FileTime::from_system_time(mtime)).unwrap();
    }

    let removed = prune_old_logs(&dir, MAX_LOG_FILES).unwrap();
    assert_eq!(removed, 3, "should remove the 3 oldest");

    let remaining: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("dedup.log"))
        .collect();
    assert_eq!(remaining.len(), MAX_LOG_FILES);
}
