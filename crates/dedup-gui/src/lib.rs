//! GPUI-based macOS frontend for dedup.
//!
//! The crate compiles as empty on non-macOS targets via the `cfg` gate
//! below; the real GPUI integration lands in a later milestone.
//!
//! Issue #16 adds the logging infrastructure the GUI will wire up once
//! the app skeleton (#19) lands. [`init_logging`] configures a layered
//! [`tracing`] subscriber that writes JSON-formatted events to a
//! daily-rolling file under `~/.config/dedup/logs/`. A companion pruning
//! helper ([`prune_old_logs`]) keeps at most 7 files — `tracing-appender`
//! rotates but does not garbage-collect, so the app calls the helper at
//! startup.
#![cfg(target_os = "macos")]

mod logging;

pub use logging::{LogGuard, MAX_LOG_FILES, init_logging, log_dir, prune_old_logs};
