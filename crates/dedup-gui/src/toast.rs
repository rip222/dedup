//! Toast subsystem (issue #30).
//!
//! Pure-data structures for the three toast tiers (Error / Warning / Info)
//! plus a [`ToastStack`] that owns the live queue. All state transitions
//! are GPUI-free so the unit-test lane can exercise them directly; the
//! render and auto-dismiss ticker live in [`crate::project_view`].
//!
//! Durations:
//!
//! - [`ToastKind::Error`]: no auto-dismiss. Surface stays up until the
//!   user clicks `[×]` (or an action button that implicitly dismisses).
//! - [`ToastKind::Warning`]: 5s auto-dismiss.
//! - [`ToastKind::Info`]: 3s auto-dismiss.
//!
//! Action routing: each toast can carry one optional [`ToastAction`]. The
//! action's `action_name` is a stable string key (e.g.
//! `"cache.delete_and_rescan"`) that [`crate::project_view`] dispatches on
//! when the toast's button is clicked. Keeping the action as a string
//! rather than a typed GPUI `Action` keeps this module GPUI-free — the
//! view layer owns the `action_name -> handler` mapping.
//!
//! The classifiers at the bottom of this file turn raw
//! [`dedup_core::CacheError`] / [`dedup_core::ConfigError`] variants into
//! [`ToastClass`] decisions so the view layer just matches on the enum
//! without re-parsing error strings.
//!
//! Post-scan issues dialog: [`format_issues_clipboard`] produces the
//! GitHub-issue-ready markdown block the "Copy details" button writes to
//! the clipboard.

use std::time::{Duration, Instant};

use dedup_core::{CacheError, FileIssue, FileIssueKind};

/// Auto-dismiss timer for warning toasts. Matches the PRD (5 seconds).
pub const WARNING_TTL: Duration = Duration::from_secs(5);
/// Auto-dismiss timer for info toasts. Matches the PRD (3 seconds).
pub const INFO_TTL: Duration = Duration::from_secs(3);

/// Tier of a toast. Drives colour, icon, and auto-dismiss behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    /// Red border + persistent. Requires dismissal (or an action button
    /// click that dismisses implicitly).
    Error,
    /// Yellow border, 5s auto-dismiss.
    Warning,
    /// Neutral palette, 3s auto-dismiss.
    Info,
}

impl ToastKind {
    /// Auto-dismiss TTL for this tier, or `None` if the toast is
    /// persistent. [`ToastStack::push`] uses this to seed
    /// [`Toast::auto_dismiss_at`].
    pub fn auto_dismiss(self) -> Option<Duration> {
        match self {
            ToastKind::Error => None,
            ToastKind::Warning => Some(WARNING_TTL),
            ToastKind::Info => Some(INFO_TTL),
        }
    }
}

/// Optional action button attached to a toast (e.g. the "Delete .dedup/
/// and rescan" button on a cache-corruption toast).
///
/// `action_name` is a free-form key routed by the project view's action
/// dispatcher (`ProjectView::dispatch_toast_action`). Keeping it as a
/// string rather than a typed GPUI `Action` lets this module stay free
/// of the GPUI dependency so the test lane can cover the toast state
/// machine without spinning up a GPUI runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToastAction {
    pub label: String,
    pub action_name: &'static str,
}

/// A single live toast on the [`ToastStack`].
///
/// `auto_dismiss_at = None` means "persistent" (error toasts). Warning
/// and info toasts seed this at construction time via
/// [`ToastKind::auto_dismiss`]; [`ToastStack::tick`] drops any toast
/// whose deadline has passed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toast {
    pub id: u64,
    pub kind: ToastKind,
    pub title: String,
    pub body: Option<String>,
    pub action: Option<ToastAction>,
    pub auto_dismiss_at: Option<Instant>,
    pub created_at: Instant,
}

/// Queue of live toasts plus the next-id counter.
///
/// Cheap to clone; the project view owns one as a plain field. All
/// mutations go through the methods here so `next_id` never skips.
#[derive(Debug, Default, Clone)]
pub struct ToastStack {
    next_id: u64,
    pub toasts: Vec<Toast>,
}

impl ToastStack {
    /// Fresh empty stack.
    pub fn new() -> Self {
        Self::default()
    }

    /// Push a new toast and return its id so callers can dismiss
    /// programmatically if needed. Auto-dismiss deadline is seeded from
    /// [`ToastKind::auto_dismiss`] relative to [`Instant::now`].
    pub fn push(&mut self, kind: ToastKind, title: impl Into<String>) -> u64 {
        self.push_full(kind, title.into(), None, None)
    }

    /// Push a toast with every field populated. Convenience constructor
    /// for the cache/config/error flows that need an action button.
    pub fn push_full(
        &mut self,
        kind: ToastKind,
        title: String,
        body: Option<String>,
        action: Option<ToastAction>,
    ) -> u64 {
        let now = Instant::now();
        let auto_dismiss_at = kind.auto_dismiss().map(|d| now + d);
        let id = self.next_id;
        self.next_id += 1;
        self.toasts.push(Toast {
            id,
            kind,
            title,
            body,
            action,
            auto_dismiss_at,
            created_at: now,
        });
        id
    }

    /// Remove the toast with the given id. No-op if the id is missing
    /// (e.g. already auto-dismissed). Returns `true` if a toast was
    /// removed.
    pub fn dismiss(&mut self, id: u64) -> bool {
        let before = self.toasts.len();
        self.toasts.retain(|t| t.id != id);
        before != self.toasts.len()
    }

    /// Drop every toast whose auto-dismiss deadline has passed.
    ///
    /// Called on a 500ms timer from the project view. `now` is injected
    /// so tests can drive the clock without `std::thread::sleep`.
    pub fn tick(&mut self, now: Instant) {
        self.toasts.retain(|t| match t.auto_dismiss_at {
            Some(deadline) => now < deadline,
            None => true,
        });
    }

    /// True if the stack currently has no live toasts.
    pub fn is_empty(&self) -> bool {
        self.toasts.is_empty()
    }

    /// Number of live toasts.
    pub fn len(&self) -> usize {
        self.toasts.len()
    }
}

// ---------------------------------------------------------------------------
// Action-name registry — the stable string keys the project view routes on.
// ---------------------------------------------------------------------------
/// Cache-corruption toast's "Delete .dedup/ and rescan" button.
pub const ACTION_CACHE_DELETE_AND_RESCAN: &str = "cache.delete_and_rescan";
/// Cache-newer-schema toast's "Rescan (overwrites cache)" button.
pub const ACTION_CACHE_RESCAN: &str = "cache.rescan";
/// Stale-recent toast's "Remove from recents" button.
pub const ACTION_REMOVE_STALE_RECENT: &str = "recents.remove_stale";
/// Invalid-config startup modal's "Fix config" button (opens the config
/// file in the user's editor).
pub const ACTION_CONFIG_FIX: &str = "config.fix";
/// Invalid-config startup modal's "Reset to defaults" button (writes a
/// defaults-only TOML and retries the load).
pub const ACTION_CONFIG_RESET: &str = "config.reset";
/// Post-scan issues link → opens the issues dialog.
pub const ACTION_SHOW_ISSUES: &str = "scan.show_issues";

// ---------------------------------------------------------------------------
// Error classification
// ---------------------------------------------------------------------------

/// Result of classifying a [`CacheError`] into the GUI-facing bucket.
///
/// The GUI surfaces each bucket with a different toast / modal; this
/// enum lets the decision happen in pure code that the test lane can
/// drive directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheErrorClass {
    /// Schema is newer than this build understands. Surface the
    /// "Rescan (overwrites cache)" toast.
    NewerSchema,
    /// SQLite reports on-disk corruption, or the error chain otherwise
    /// looks like data corruption (serde decode failure on a stored
    /// blob, etc.). Surface the "Delete .dedup/ and rescan" toast.
    Corrupted,
    /// Everything else — generic I/O, SQLite busy, etc. Surface as a
    /// plain error toast with the raw message.
    Other,
}

/// Classify a [`CacheError`] for toast surfacing.
///
/// `NewerSchema` is matched structurally; "corrupted" is matched
/// heuristically — we check the error chain for the SQLite "corrupt"
/// substring so any `rusqlite` path that surfaces a `SQLITE_CORRUPT`
/// code (or its extended variants) classifies the same way regardless
/// of the precise error shape.
pub fn classify_cache_error(err: &CacheError) -> CacheErrorClass {
    if matches!(err, CacheError::NewerSchema { .. }) {
        return CacheErrorClass::NewerSchema;
    }
    if error_chain_looks_corrupted(err) {
        return CacheErrorClass::Corrupted;
    }
    CacheErrorClass::Other
}

/// Heuristic: walk the [`std::error::Error`] source chain and look for
/// the word "corrupt" (SQLite's signal) or a `malformed` code. We
/// deliberately keep this conservative so a spurious false-positive
/// surfaces the safer "delete cache" toast rather than silently
/// corrupting a working cache.
fn error_chain_looks_corrupted(err: &(dyn std::error::Error + 'static)) -> bool {
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = cur {
        let msg = e.to_string().to_ascii_lowercase();
        if msg.contains("corrupt") || msg.contains("database disk image is malformed") {
            return true;
        }
        cur = e.source();
    }
    false
}

// ---------------------------------------------------------------------------
// Background-thread panic propagation
// ---------------------------------------------------------------------------

/// Extract a human-readable message from a panic payload returned by
/// [`std::panic::catch_unwind`].
///
/// Duplicated from `dedup_core::scanner::panic_message` (it's private
/// there); same three-branch logic: `&'static str`, `String`, otherwise
/// a generic label. Lives in this module so the GUI's background-thread
/// catch_unwind can produce the same postmortem text without reaching
/// into the core crate's internals.
pub fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "panic payload was not a string".to_string()
    }
}

// ---------------------------------------------------------------------------
// Post-scan issues clipboard formatter
// ---------------------------------------------------------------------------

/// Format a [`FileIssue`] list as a GitHub-issue-ready markdown block.
///
/// Output shape (matches the issue body):
///
/// ```text
/// ### dedup scan issues
/// - `path/to/file.rs`: ReadError — permission denied
/// - `path/to/other.ts`: TierBParse — <detail>
/// ```
///
/// Paths are rendered verbatim (the scanner already normalises them to
/// forward-slash form). Empty `message` strings collapse to just the
/// kind label so the dash isn't dangling.
pub fn format_issues_clipboard(issues: &[FileIssue]) -> String {
    let mut out = String::from("### dedup scan issues\n");
    for issue in issues {
        let kind = kind_camel_label(issue.kind);
        let path = issue.path.display();
        if issue.message.is_empty() {
            out.push_str(&format!("- `{path}`: {kind}\n"));
        } else {
            out.push_str(&format!(
                "- `{path}`: {kind} \u{2014} {msg}\n",
                msg = issue.message
            ));
        }
    }
    out
}

/// CamelCase display label for a [`FileIssueKind`] — what shows up in
/// the clipboard block and the issues-dialog rows. The core crate's
/// `FileIssueKind::label()` returns snake_case for JSON/CLI output;
/// this function is the GUI-side display variant.
fn kind_camel_label(kind: FileIssueKind) -> &'static str {
    match kind {
        FileIssueKind::ReadError => "ReadError",
        FileIssueKind::Utf8 => "Utf8",
        FileIssueKind::TierBParse => "TierBParse",
        FileIssueKind::TierBPanic => "TierBPanic",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::thread::sleep;
    use std::time::Duration;

    fn issue(path: &str, kind: FileIssueKind, msg: &str) -> FileIssue {
        FileIssue {
            path: PathBuf::from(path),
            kind,
            message: msg.to_string(),
        }
    }

    // -----------------------------------------------------------------
    // ToastStack lifecycle.
    // -----------------------------------------------------------------

    #[test]
    fn push_info_auto_dismisses_after_3s() {
        let mut stack = ToastStack::new();
        let t0 = Instant::now();
        let _id = stack.push(ToastKind::Info, "hi");
        assert_eq!(stack.len(), 1);
        // Tick before the 3s deadline — still present.
        stack.tick(t0 + Duration::from_secs(2));
        assert_eq!(stack.len(), 1);
        // Tick past the 3s deadline — gone.
        stack.tick(t0 + Duration::from_secs(4));
        assert_eq!(stack.len(), 0);
    }

    #[test]
    fn push_warning_auto_dismisses_after_5s() {
        let mut stack = ToastStack::new();
        let t0 = Instant::now();
        stack.push(ToastKind::Warning, "warn");
        stack.tick(t0 + Duration::from_secs(4));
        assert_eq!(stack.len(), 1);
        stack.tick(t0 + Duration::from_secs(6));
        assert_eq!(stack.len(), 0);
    }

    #[test]
    fn error_is_persistent_until_dismissed() {
        let mut stack = ToastStack::new();
        let id = stack.push(ToastKind::Error, "boom");
        // Tick far into the future — error toast still present.
        stack.tick(Instant::now() + Duration::from_secs(3600));
        assert_eq!(stack.len(), 1);
        assert!(stack.dismiss(id));
        assert_eq!(stack.len(), 0);
    }

    #[test]
    fn dismiss_unknown_id_is_noop() {
        let mut stack = ToastStack::new();
        stack.push(ToastKind::Error, "a");
        assert!(!stack.dismiss(9999));
        assert_eq!(stack.len(), 1);
    }

    #[test]
    fn dismiss_removes_only_matching_toast() {
        let mut stack = ToastStack::new();
        let a = stack.push(ToastKind::Error, "a");
        let b = stack.push(ToastKind::Error, "b");
        let c = stack.push(ToastKind::Error, "c");
        assert!(stack.dismiss(b));
        let remaining: Vec<u64> = stack.toasts.iter().map(|t| t.id).collect();
        assert_eq!(remaining, vec![a, c]);
    }

    #[test]
    fn auto_dismiss_ttls_match_prd() {
        // Guardrails: if someone nudges these constants, the test goes
        // red and we re-review the PRD.
        assert_eq!(ToastKind::Warning.auto_dismiss(), Some(WARNING_TTL));
        assert_eq!(ToastKind::Info.auto_dismiss(), Some(INFO_TTL));
        assert_eq!(ToastKind::Error.auto_dismiss(), None);
        assert_eq!(WARNING_TTL, Duration::from_secs(5));
        assert_eq!(INFO_TTL, Duration::from_secs(3));
    }

    #[test]
    fn push_returns_unique_incrementing_ids() {
        let mut stack = ToastStack::new();
        let a = stack.push(ToastKind::Info, "a");
        let b = stack.push(ToastKind::Info, "b");
        let c = stack.push(ToastKind::Error, "c");
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert!(a < b && b < c);
    }

    #[test]
    fn push_full_attaches_body_and_action() {
        let mut stack = ToastStack::new();
        let id = stack.push_full(
            ToastKind::Error,
            "Cache is corrupted".to_string(),
            Some("SQLite reports corruption.".to_string()),
            Some(ToastAction {
                label: "Delete .dedup/ and rescan".to_string(),
                action_name: ACTION_CACHE_DELETE_AND_RESCAN,
            }),
        );
        let t = stack.toasts.iter().find(|t| t.id == id).unwrap();
        assert_eq!(t.kind, ToastKind::Error);
        assert_eq!(t.body.as_deref(), Some("SQLite reports corruption."));
        assert_eq!(
            t.action.as_ref().map(|a| a.action_name),
            Some(ACTION_CACHE_DELETE_AND_RESCAN)
        );
    }

    #[test]
    fn tick_drops_expired_but_keeps_persistent() {
        let mut stack = ToastStack::new();
        let t0 = Instant::now();
        stack.push(ToastKind::Info, "info");
        let err_id = stack.push(ToastKind::Error, "err");
        stack.push(ToastKind::Warning, "warn");
        stack.tick(t0 + Duration::from_secs(10));
        // Only the error toast remains (no auto-dismiss).
        assert_eq!(stack.len(), 1);
        assert_eq!(stack.toasts[0].id, err_id);
    }

    #[test]
    fn real_time_auto_dismiss_is_consistent_with_tick() {
        // Exercises the real `Instant::now()` path — `push` seeds
        // `auto_dismiss_at`, then `tick(now)` consults it. A short
        // sleep keeps the test fast while still proving the deadline
        // wiring isn't inverted.
        let mut stack = ToastStack::new();
        stack.push_full(ToastKind::Info, "quick".to_string(), None, None);
        // Immediately after push — still present.
        stack.tick(Instant::now());
        assert_eq!(stack.len(), 1);
        // Fast-forward simulated by injecting a future `now` (real
        // sleep only used to ensure Instant monotonicity on this OS).
        sleep(Duration::from_millis(1));
        stack.tick(Instant::now() + Duration::from_secs(5));
        assert_eq!(stack.len(), 0);
    }

    // -----------------------------------------------------------------
    // CacheError classification.
    // -----------------------------------------------------------------

    #[test]
    fn classify_newer_schema_is_newer_schema() {
        let err = CacheError::NewerSchema {
            found: 99,
            supported: 3,
        };
        assert_eq!(classify_cache_error(&err), CacheErrorClass::NewerSchema);
    }

    #[test]
    fn classify_io_error_with_corrupt_message_is_corrupted() {
        // The classifier is heuristic on message content — if the
        // message chain contains "corrupt" (SQLite's signal) we
        // classify as Corrupted regardless of the variant shape.
        // Exercising via the Io variant keeps the test free of a
        // rusqlite dev-dep while still covering the heuristic path.
        let io = std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "database disk image is malformed",
        );
        let err = CacheError::Io {
            path: PathBuf::from("/tmp/x"),
            source: io,
        };
        assert_eq!(classify_cache_error(&err), CacheErrorClass::Corrupted);
    }

    #[test]
    fn classify_plain_io_error_is_other() {
        // A benign permission-denied shape should not trip the
        // corruption heuristic.
        let io = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let err = CacheError::Io {
            path: PathBuf::from("/tmp/x"),
            source: io,
        };
        assert_eq!(classify_cache_error(&err), CacheErrorClass::Other);
    }

    #[test]
    fn classify_corrupt_keyword_case_insensitive() {
        // Upper-case SQLite message shouldn't escape the classifier.
        let io = std::io::Error::other("SQLITE_CORRUPT: index rewind");
        let err = CacheError::Io {
            path: PathBuf::from("/tmp/x"),
            source: io,
        };
        assert_eq!(classify_cache_error(&err), CacheErrorClass::Corrupted);
    }

    // -----------------------------------------------------------------
    // Panic message extractor.
    // -----------------------------------------------------------------

    #[test]
    fn panic_message_extracts_literal() {
        let payload: Box<dyn std::any::Any + Send> = Box::new("literal boom");
        assert_eq!(panic_message(&payload), "literal boom");
    }

    #[test]
    fn panic_message_extracts_owned_string() {
        let payload: Box<dyn std::any::Any + Send> = Box::new(String::from("formatted boom"));
        assert_eq!(panic_message(&payload), "formatted boom");
    }

    #[test]
    fn panic_message_falls_back_for_typed_payload() {
        let payload: Box<dyn std::any::Any + Send> = Box::new(42u32);
        assert!(!panic_message(&payload).is_empty());
    }

    #[test]
    fn background_panic_closure_yields_message() {
        // Simulates the scan-worker closure: drive a closure through
        // `catch_unwind`, convert the payload to a string, assert the
        // round-trip contains the panic message. No real threads —
        // this proves the message-extraction path end-to-end.
        use std::panic::{AssertUnwindSafe, catch_unwind};
        let result = catch_unwind(AssertUnwindSafe(|| {
            panic!("scan worker kaboom");
        }));
        let payload = match result {
            Ok(()) => panic!("closure should have panicked"),
            Err(p) => p,
        };
        let msg = panic_message(&payload);
        assert!(
            msg.contains("scan worker kaboom"),
            "expected message to contain panic literal, got {msg:?}"
        );
    }

    // -----------------------------------------------------------------
    // Post-scan issues clipboard formatter.
    // -----------------------------------------------------------------

    #[test]
    fn clipboard_block_has_heading_and_rows() {
        let issues = vec![
            issue("src/a.rs", FileIssueKind::ReadError, "permission denied"),
            issue("src/b.ts", FileIssueKind::TierBParse, "unexpected token"),
        ];
        let got = format_issues_clipboard(&issues);
        let expected = "\
### dedup scan issues
- `src/a.rs`: ReadError \u{2014} permission denied
- `src/b.ts`: TierBParse \u{2014} unexpected token
";
        assert_eq!(got, expected);
    }

    #[test]
    fn clipboard_block_handles_empty_message() {
        let issues = vec![issue("src/c.rs", FileIssueKind::Utf8, "")];
        let got = format_issues_clipboard(&issues);
        let expected = "\
### dedup scan issues
- `src/c.rs`: Utf8
";
        assert_eq!(got, expected);
    }

    #[test]
    fn clipboard_block_with_zero_issues_is_heading_only() {
        // Edge case — a caller that invokes the formatter with an
        // empty slice still gets a valid markdown block. Keeps the
        // post-scan dialog's "Copy details" idempotent regardless of
        // how the surrounding flow hands issues in.
        let got = format_issues_clipboard(&[]);
        assert_eq!(got, "### dedup scan issues\n");
    }

    #[test]
    fn clipboard_renders_each_kind_camelcase() {
        // Spot-check every FileIssueKind produces the CamelCase label
        // the GUI documents (vs. the snake_case label the CLI emits).
        let issues = vec![
            issue("a", FileIssueKind::ReadError, "x"),
            issue("b", FileIssueKind::Utf8, "x"),
            issue("c", FileIssueKind::TierBParse, "x"),
            issue("d", FileIssueKind::TierBPanic, "x"),
        ];
        let got = format_issues_clipboard(&issues);
        assert!(got.contains("ReadError"));
        assert!(got.contains("Utf8"));
        assert!(got.contains("TierBParse"));
        assert!(got.contains("TierBPanic"));
    }
}
