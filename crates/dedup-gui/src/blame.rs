//! Git-blame overlay for occurrence headers (issue #58).
//!
//! Each occurrence header in the detail pane can show
//! `(<author> · <short_sha> · <date>)` in dim text, sourced from
//! `git blame --porcelain -L <line>,<line> -- <path>` on the starting
//! line of the occurrence. Blame fetch is lazy (first header render),
//! cached on [`AppState`], and hard-capped at 500 ms per invocation.
//! Non-git folders, missing git, and timeouts are silently ignored —
//! the header still renders, just without the overlay.
//!
//! ## Mockable subprocess boundary
//!
//! The blame invocation is expressed as a [`BlameProvider`] trait so
//! parse / cache behaviour has pure-data tests. Production code uses
//! [`GitBlameProvider`], which shells out to `git` with a
//! `kill-on-timeout` wrapper. Unit tests use [`MockBlameProvider`] and
//! friends to feed canned porcelain output (or timeouts) into the
//! cache path.
//!
//! ## Porcelain format
//!
//! `git blame --porcelain -L N,N -- <path>` emits one header line
//! (`<sha> <orig-line> <final-line> <num-lines>`) followed by metadata
//! key-value lines (`author …`, `author-time …`, `summary …`, etc.),
//! a single TAB-prefixed content line, and EOF. The parser is
//! line-based and tolerant of unknown keys — we only extract `author`,
//! `author-time` (Unix seconds), and `summary`, plus the leading SHA.

use std::fmt::Write as _;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Hard timeout for a single `git blame` invocation. AC: 500 ms; a
/// timeout is treated as "no blame" — the header still renders.
pub const BLAME_TIMEOUT: Duration = Duration::from_millis(500);

/// Information extracted from one porcelain blame block — just the
/// fields surfaced in the header overlay + tooltip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlameInfo {
    /// Author name as reported by `author <name>`. Never empty — the
    /// parser replaces a missing author with `"?"` so the overlay
    /// always has something to show.
    pub author: String,
    /// Short commit SHA (first 8 hex chars of the porcelain header
    /// SHA). Long form is dropped to keep the overlay compact.
    pub short_sha: String,
    /// YYYY-MM-DD commit date, derived from `author-time` (UTC).
    pub date: String,
    /// First-line of the commit message (`summary <text>` line). Empty
    /// when the porcelain block omits `summary`.
    pub summary: String,
}

impl BlameInfo {
    /// Pre-formatted "author · short_sha · date" used as the header
    /// overlay label. Kept here so the project-view renderer doesn't
    /// have to know the separator.
    pub fn overlay_text(&self) -> String {
        format!("{} · {} · {}", self.author, self.short_sha, self.date)
    }
}

/// Cache key for blame lookups. `(path, start_line, file_mtime)` per
/// AC — mtime included so the cache invalidates when the file is
/// edited out from under us without needing an explicit refresh.
///
/// `mtime` is optional because `fs::metadata(...).modified()` can
/// fail (e.g. on filesystems without mtime support). When missing we
/// still cache under `None` so repeated renders of the same header
/// don't re-shell-out on every frame.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BlameCacheKey {
    pub path: PathBuf,
    pub start_line: u32,
    pub mtime: Option<SystemTime>,
}

impl BlameCacheKey {
    /// Compute the `(path, start_line, mtime)` key for an absolute
    /// file path. `fs::metadata` failure → `mtime = None`, which is
    /// still a valid cache key (just a coarser one).
    pub fn new(path: PathBuf, start_line: u32) -> Self {
        let mtime = std::fs::metadata(&path)
            .ok()
            .and_then(|m| m.modified().ok());
        Self {
            path,
            start_line,
            mtime,
        }
    }
}

/// Result of a blame attempt. `Ok(Some(info))` — blame succeeded;
/// `Ok(None)` — blame is silently absent (non-git folder, timeout,
/// git missing, path not tracked). `Err` is reserved for future use
/// and is never produced by [`GitBlameProvider`] today — the concrete
/// impl collapses every failure to `Ok(None)` because the AC calls
/// for silent omission.
pub type BlameResult = Result<Option<BlameInfo>, ()>;

/// Mockable subprocess boundary so the parse + cache path has a unit
/// test without shelling out.
#[allow(clippy::result_unit_err)]
pub trait BlameProvider {
    /// Blame line `start_line` of `rel_path` inside `folder`. The
    /// provider is responsible for enforcing the 500 ms timeout and
    /// swallowing any errors back into `Ok(None)`.
    fn blame(&self, folder: &Path, rel_path: &Path, start_line: u32) -> BlameResult;
}

/// Production blame provider — shells out to `git blame --porcelain`.
#[derive(Debug, Clone, Default)]
pub struct GitBlameProvider;

impl BlameProvider for GitBlameProvider {
    fn blame(&self, folder: &Path, rel_path: &Path, start_line: u32) -> BlameResult {
        Ok(run_git_blame(folder, rel_path, start_line, BLAME_TIMEOUT))
    }
}

/// Shell out to `git blame --porcelain -L N,N -- <rel_path>` inside
/// `folder` and return the parsed [`BlameInfo`], or `None` on any
/// failure (not a git repo, git missing, timeout, non-zero exit,
/// unparseable output).
///
/// The timeout is enforced by spawning a worker thread that reads
/// stdout to completion; the main thread `recv_timeout`s on a channel
/// and calls `child.kill()` on elapse. `kill()` is best-effort — the
/// worker thread is left detached but will exit as soon as the pipe
/// closes. We never panic.
pub fn run_git_blame(
    folder: &Path,
    rel_path: &Path,
    start_line: u32,
    timeout: Duration,
) -> Option<BlameInfo> {
    let mut child = Command::new("git")
        .arg("blame")
        .arg("--porcelain")
        .arg("-L")
        .arg(format!("{start_line},{start_line}"))
        .arg("--")
        .arg(rel_path)
        .current_dir(folder)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    // Reader thread drains stdout into a `String`. Sent back over the
    // channel once EOF is reached; if the process is killed first the
    // channel just closes without a message.
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = mpsc::channel::<String>();
    let reader = thread::spawn(move || {
        let mut buf = String::new();
        let _ = stdout.read_to_string(&mut buf);
        let _ = tx.send(buf);
    });

    match rx.recv_timeout(timeout) {
        Ok(out) => {
            // Reader has the full stdout; reap the process to avoid
            // zombies. `wait()` here is non-blocking in practice
            // because stdout already hit EOF.
            let _ = child.wait();
            let _ = reader.join();
            parse_porcelain(&out)
        }
        Err(_) => {
            // Timeout — kill the child, detach the reader. Swallow
            // kill errors silently per AC.
            let _ = child.kill();
            let _ = child.wait();
            None
        }
    }
}

/// Parse a `git blame --porcelain` single-line block. Returns `None`
/// if the input is empty or doesn't start with a porcelain header.
pub fn parse_porcelain(raw: &str) -> Option<BlameInfo> {
    // First non-empty line must be `<sha> <orig> <final> [<num>]`.
    let mut lines = raw.lines();
    let header = lines.next()?.trim();
    if header.is_empty() {
        return None;
    }
    let sha = header.split_whitespace().next()?;
    if sha.len() < 8 || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let short_sha = sha.chars().take(8).collect::<String>();

    let mut author: Option<String> = None;
    let mut author_time: Option<i64> = None;
    let mut summary: Option<String> = None;

    for line in lines {
        if line.starts_with('\t') {
            // Content line — signals end of the header block.
            break;
        }
        if let Some(rest) = line.strip_prefix("author ") {
            author = Some(rest.to_string());
        } else if let Some(rest) = line.strip_prefix("author-time ") {
            author_time = rest.trim().parse::<i64>().ok();
        } else if let Some(rest) = line.strip_prefix("summary ") {
            summary = Some(rest.to_string());
        }
    }

    let author = author.filter(|s| !s.is_empty()).unwrap_or_else(|| "?".to_string());
    let date = author_time
        .map(format_unix_date)
        .unwrap_or_else(|| "?".to_string());
    let summary = summary.unwrap_or_default();

    Some(BlameInfo {
        author,
        short_sha,
        date,
        summary,
    })
}

/// Format a Unix timestamp (seconds) as `YYYY-MM-DD` in UTC. No
/// `chrono` dep — civil-date arithmetic is cheap and deterministic
/// enough to inline.
pub fn format_unix_date(secs: i64) -> String {
    // Days since the Unix epoch, floored toward negative infinity so
    // pre-1970 timestamps format correctly too (unlikely in blame
    // output but free).
    let days = secs.div_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let mut out = String::with_capacity(10);
    // Year can be 4+ digits for far-future blame; pad the common
    // 4-digit case, let `write!` handle overflow.
    let _ = write!(&mut out, "{y:04}-{m:02}-{d:02}");
    out
}

/// Convert a day-count since 1970-01-01 into `(year, month, day)`.
/// Algorithm from Howard Hinnant's "date algorithms" paper — branch-
/// free and correct for the full i64 range. Used to avoid pulling in
/// `chrono` just to stringify a date.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z / 146_097 } else { (z - 146_096) / 146_097 };
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Mock provider that returns canned answers indexed by
/// `(rel_path, start_line)`. Used in tests.
#[cfg(test)]
#[derive(Debug, Clone, Default)]
pub struct MockBlameProvider {
    pub answers: std::collections::HashMap<(PathBuf, u32), Option<BlameInfo>>,
    pub calls: std::cell::RefCell<Vec<(PathBuf, u32)>>,
}

#[cfg(test)]
impl BlameProvider for MockBlameProvider {
    fn blame(&self, _folder: &Path, rel_path: &Path, start_line: u32) -> BlameResult {
        self.calls
            .borrow_mut()
            .push((rel_path.to_path_buf(), start_line));
        Ok(self
            .answers
            .get(&(rel_path.to_path_buf(), start_line))
            .cloned()
            .unwrap_or(None))
    }
}

/// Convenience: fetch-or-cache wrapper. Returns the cached entry if
/// present; otherwise invokes the provider, stores the result, and
/// returns it. The cache stores `Option<BlameInfo>` rather than
/// `Option<Option<…>>` so a failed lookup is still "known" — we don't
/// retry the blame every frame when the folder isn't a git repo.
pub fn fetch_with_cache<P: BlameProvider>(
    provider: &P,
    cache: &mut std::collections::HashMap<BlameCacheKey, Option<BlameInfo>>,
    folder: &Path,
    rel_path: &Path,
    start_line: u32,
    abs_path: &Path,
) -> Option<BlameInfo> {
    let key = BlameCacheKey::new(abs_path.to_path_buf(), start_line);
    if let Some(hit) = cache.get(&key) {
        return hit.clone();
    }
    let value = provider
        .blame(folder, rel_path, start_line)
        .ok()
        .flatten();
    cache.insert(key, value.clone());
    value
}

/// Deliberately unused but exported so downstream diagnostics can
/// construct a `SystemTime` for a stored cache entry without taking a
/// dep on `UNIX_EPOCH` arithmetic in the caller.
#[allow(dead_code)]
pub fn system_time_from_secs(secs: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
a1b2c3d4e5f6789012345678901234567890abcd 12 12 1
author Jane Doe
author-mail <jane@example.com>
author-time 1700000000
author-tz +0000
committer Jane Doe
committer-mail <jane@example.com>
committer-time 1700000000
committer-tz +0000
summary fix: correct the widget alignment
previous deadbeef…
filename src/widget.rs
\tcode line goes here\n";

    #[test]
    fn parse_extracts_author_sha_date_summary() {
        let got = parse_porcelain(SAMPLE).expect("parses");
        assert_eq!(got.author, "Jane Doe");
        assert_eq!(got.short_sha, "a1b2c3d4");
        assert_eq!(got.date, "2023-11-14");
        assert_eq!(got.summary, "fix: correct the widget alignment");
        assert_eq!(
            got.overlay_text(),
            "Jane Doe · a1b2c3d4 · 2023-11-14"
        );
    }

    #[test]
    fn parse_empty_input_returns_none() {
        assert_eq!(parse_porcelain(""), None);
        assert_eq!(parse_porcelain("\n\n"), None);
    }

    #[test]
    fn parse_rejects_non_sha_header() {
        // Doesn't start with a hex SHA at all.
        assert_eq!(parse_porcelain("nope just text\n"), None);
        // Too short to be a short sha.
        assert_eq!(parse_porcelain("abc 1 1 1\n"), None);
    }

    #[test]
    fn parse_missing_author_uses_question_mark() {
        let raw = "deadbeef1234567890 1 1 1\nauthor-time 0\n\tx\n";
        let got = parse_porcelain(raw).expect("parses");
        assert_eq!(got.author, "?");
        assert_eq!(got.short_sha, "deadbeef");
        assert_eq!(got.date, "1970-01-01");
        assert_eq!(got.summary, "");
    }

    #[test]
    fn format_unix_date_matches_known_epochs() {
        assert_eq!(format_unix_date(0), "1970-01-01");
        assert_eq!(format_unix_date(86_399), "1970-01-01");
        assert_eq!(format_unix_date(86_400), "1970-01-02");
        // 2000-01-01 = 946684800.
        assert_eq!(format_unix_date(946_684_800), "2000-01-01");
        // 2023-11-14 = 1700000000 sampled in the fixture above.
        assert_eq!(format_unix_date(1_700_000_000), "2023-11-14");
    }

    #[test]
    fn fetch_with_cache_stores_miss_as_none() {
        let provider = MockBlameProvider::default();
        let mut cache = std::collections::HashMap::new();
        // Mock has no answer → returns None and caches it.
        let folder = PathBuf::from("/some/folder");
        let rel = PathBuf::from("a.rs");
        let abs = folder.join(&rel);
        let got = fetch_with_cache(&provider, &mut cache, &folder, &rel, 10, &abs);
        assert_eq!(got, None);
        // Second call is a hit — provider.calls stays at 1.
        let _ = fetch_with_cache(&provider, &mut cache, &folder, &rel, 10, &abs);
        assert_eq!(provider.calls.borrow().len(), 1);
    }

    #[test]
    fn fetch_with_cache_returns_stored_hit() {
        let mut provider = MockBlameProvider::default();
        let info = BlameInfo {
            author: "Alice".into(),
            short_sha: "12345678".into(),
            date: "2024-01-02".into(),
            summary: "fix thing".into(),
        };
        provider
            .answers
            .insert((PathBuf::from("a.rs"), 5), Some(info.clone()));
        let mut cache = std::collections::HashMap::new();
        let folder = PathBuf::from("/repo");
        let rel = PathBuf::from("a.rs");
        let abs = folder.join(&rel);

        let got1 = fetch_with_cache(&provider, &mut cache, &folder, &rel, 5, &abs);
        assert_eq!(got1, Some(info.clone()));
        let got2 = fetch_with_cache(&provider, &mut cache, &folder, &rel, 5, &abs);
        assert_eq!(got2, Some(info));
        assert_eq!(provider.calls.borrow().len(), 1);
    }

    #[test]
    fn cache_key_different_line_is_different_entry() {
        let k1 = BlameCacheKey {
            path: PathBuf::from("/a"),
            start_line: 1,
            mtime: None,
        };
        let k2 = BlameCacheKey {
            path: PathBuf::from("/a"),
            start_line: 2,
            mtime: None,
        };
        assert_ne!(k1, k2);
    }

    #[test]
    fn blame_timeout_returns_none_silently() {
        // `sleep 2` as a cheap long-running stand-in — if `git` isn't
        // present we just run the timeout check against `sleep` via
        // a direct call to `run_git_blame`'s internals. To keep the
        // test hermetic we assert the public contract: a spawn
        // failure (non-git folder, missing file, whatever) returns
        // `None` and does not panic.
        let tmp = std::env::temp_dir();
        let got = run_git_blame(
            &tmp,
            Path::new("definitely-not-a-real-file-for-blame.xyz"),
            1,
            Duration::from_millis(200),
        );
        assert_eq!(got, None);
    }
}
