//! Entry point for the `dedup-gui` binary.
//!
//! On macOS, delegates to [`dedup_gui::run`] which opens the empty-state
//! window and installs the NSMenu menubar. On other platforms, prints a
//! message and exits 0 so the workspace build stays green on Linux CI
//! (the whole lib crate is `cfg(target_os = "macos")`-gated).
//!
//! Logging (#16) is initialized here in `main` rather than inside
//! [`dedup_gui::run`] because [`dedup_gui::LogGuard`] must live for the
//! full lifetime of the process: dropping it flushes the non-blocking
//! appender's background worker. Binding it in `main` keeps it alive
//! until the GPUI runloop returns.

#[cfg(target_os = "macos")]
fn main() {
    // `--smoke-test` builds the GPUI `Application` and returns without
    // entering the runloop — used by CI to verify link / init without a
    // display. See `dedup_gui::smoke_test`. Skip logging setup so CI
    // doesn't touch `$HOME/.config/dedup/logs/`.
    //
    // The smoke path deliberately skips `human_panic::setup_panic!()`
    // too — the macro installs a process-wide panic hook, which isn't
    // needed for a link-only assertion and keeps the smoke run a pure
    // function of GPUI init. Any crash during smoke surfaces via the
    // default libstd panic hook, which is exactly what CI wants.
    if std::env::args().any(|a| a == "--smoke-test") {
        dedup_gui::smoke_test();
        return;
    }

    // Issue #30 — install the human-panic postmortem before any work
    // that might panic. Mirrors the CLI's `dedup-cli::main`: metadata
    // points at the same `~/.config/dedup/logs/` directory so users see
    // a consistent breadcrumb regardless of which binary crashed.
    let log_dir = resolve_log_dir_for_panic();
    human_panic::setup_panic!(human_panic::metadata!().support(format!(
        "- See the dedup log directory at: {log_dir}\n\
         - Open an issue at https://github.com/rip222/dedup/issues"
    )));

    // Install the GUI tracing subscriber. Errors are reported to stderr
    // but non-fatal — the app still launches, just without file logs.
    let _log_guard = match dedup_gui::init_logging() {
        Ok(g) => Some(g),
        Err(err) => {
            eprintln!("dedup-gui: failed to initialize logging: {err}");
            None
        }
    };

    dedup_gui::run();
}

/// Resolve the directory panic postmortems should point readers at.
///
/// Mirrors `dedup-cli::main::resolve_log_dir_for_panic` and
/// `dedup_gui::log_dir()`: prefer `$XDG_CONFIG_HOME/dedup/logs`, then
/// `$HOME/.config/dedup/logs`, and fall back to the default spelling
/// when neither env var is set. Returns a [`String`] so `format!`
/// inside [`human_panic::setup_panic!`] stays simple.
#[cfg(target_os = "macos")]
fn resolve_log_dir_for_panic() -> String {
    use std::path::PathBuf;
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        return PathBuf::from(xdg)
            .join("dedup")
            .join("logs")
            .display()
            .to_string();
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home)
            .join(".config")
            .join("dedup")
            .join("logs")
            .display()
            .to_string();
    }
    "~/.config/dedup/logs/".to_string()
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("dedup-gui is only supported on macOS");
}
