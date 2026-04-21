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
    if std::env::args().any(|a| a == "--smoke-test") {
        dedup_gui::smoke_test();
        return;
    }

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

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("dedup-gui is only supported on macOS");
}
