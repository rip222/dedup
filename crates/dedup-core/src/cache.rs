//! SQLite-backed persistence for scan results.
//!
//! The cache lives at `<repo_root>/.dedup/cache.sqlite`. It is opened in
//! WAL mode so subsequent reads (from `dedup list` / `dedup show`) don't
//! block writes and the database file survives process restarts cleanly.
//!
//! Concerns at this milestone:
//!
//! - Lazy directory: `.dedup/` is only materialized when [`Cache::open`]
//!   succeeds. [`Cache::open_readonly`] never creates the directory; it
//!   returns `Ok(None)` when the cache file is absent.
//! - Auto-`.gitignore`: on fresh `.dedup/` creation we write a single-line
//!   `*` `.gitignore`. If the user has customized the file, we leave it
//!   alone (idempotent create).
//! - Schema v1: tracked in a `schema_version` metadata table so later
//!   issues (#18) can extend with real migrations. The migration runner
//!   here is a stub: opening at v1 is a no-op.
//! - Idempotent writes: [`Cache::write_scan_result`] wraps a full replace
//!   of `match_groups` (cascade-deletes occurrences) in a single
//!   transaction so a second write on the same repo yields the second
//!   write's state, not a union.
//!
//! Out of scope here (punted to later issues per the PRD / issue #4 spec):
//!
//! - Content-hash-keyed warm-scan skip (→ #14).
//! - Concurrent-writer testing and real schema bumps (→ #18).
//! - Dismissal tracking (→ #11).

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use thiserror::Error;

use crate::rolling_hash::Span;
use crate::scanner::{MatchGroup, Occurrence, ScanResult};

/// Directory name under the repo root where the cache lives.
pub const CACHE_DIR: &str = ".dedup";
/// File name of the SQLite database inside [`CACHE_DIR`].
pub const CACHE_FILE: &str = "cache.sqlite";
/// The schema version this build understands. Bumped in #18 when the
/// schema evolves.
pub const SCHEMA_VERSION: i64 = 1;

/// Errors the cache layer can surface. Most are thin wrappers around
/// `rusqlite::Error` / `std::io::Error` so callers can `?` through.
#[derive(Debug, Error)]
pub enum CacheError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error(
        "cache schema version {found} is newer than supported version {supported}; upgrade dedup"
    )]
    FutureSchema { found: i64, supported: i64 },
}

/// Summary row returned by [`Cache::list_groups`]. One entry per stored
/// match group, ordered for deterministic output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupSummary {
    pub id: i64,
    pub occurrence_count: i64,
    pub total_lines: i64,
    pub total_tokens: i64,
}

/// Detail row returned by [`Cache::get_group`] — the group plus each of
/// its occurrences' spans.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupDetail {
    pub id: i64,
    pub occurrence_count: i64,
    pub total_lines: i64,
    pub total_tokens: i64,
    pub occurrences: Vec<CachedOccurrence>,
}

/// A single persisted occurrence: a file path and the span within it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedOccurrence {
    pub path: PathBuf,
    pub start_line: i64,
    pub end_line: i64,
    pub start_byte: i64,
    pub end_byte: i64,
}

/// Owning handle to the SQLite cache.
///
/// Cheap to construct beyond the `open`-time pragmas. Holds the underlying
/// connection, which is reused for all subsequent operations.
pub struct Cache {
    conn: Connection,
}

impl Cache {
    /// Open (creating if needed) the cache for `repo_root`.
    ///
    /// Side effects, in order:
    /// 1. Creates `<repo_root>/.dedup/` if missing.
    /// 2. Writes `<repo_root>/.dedup/.gitignore` with `*` if missing.
    /// 3. Opens/creates `<repo_root>/.dedup/cache.sqlite`.
    /// 4. Enables `foreign_keys` and `journal_mode = WAL`.
    /// 5. Creates or verifies schema v1.
    pub fn open(repo_root: &Path) -> Result<Self, CacheError> {
        let dir = repo_root.join(CACHE_DIR);
        if !dir.exists() {
            std::fs::create_dir_all(&dir).map_err(|source| CacheError::Io {
                path: dir.clone(),
                source,
            })?;
        }

        // Idempotent auto-.gitignore: only write if the file isn't already
        // there. That lets users customize without us clobbering them on
        // subsequent scans.
        let gitignore = dir.join(".gitignore");
        if !gitignore.exists() {
            std::fs::write(&gitignore, "*\n").map_err(|source| CacheError::Io {
                path: gitignore.clone(),
                source,
            })?;
        }

        let db_path = dir.join(CACHE_FILE);
        let conn = Connection::open(&db_path)?;
        configure_connection(&conn)?;
        ensure_schema(&conn)?;

        Ok(Self { conn })
    }

    /// Open the cache read-only for `repo_root`.
    ///
    /// Returns `Ok(None)` if `<repo_root>/.dedup/cache.sqlite` does not
    /// exist. Never creates any files or directories — this is the mode
    /// `dedup list` / `dedup show` use.
    pub fn open_readonly(repo_root: &Path) -> Result<Option<Self>, CacheError> {
        let db_path = repo_root.join(CACHE_DIR).join(CACHE_FILE);
        if !db_path.exists() {
            return Ok(None);
        }

        let conn = Connection::open_with_flags(
            &db_path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_URI,
        )?;
        // Even in "read-only-conceptually" mode we want WAL + FK pragmas
        // applied so subsequent list/show calls use the same connection
        // semantics. WAL mode sticks to the database file itself, so
        // setting it here is a cheap no-op if already WAL.
        configure_connection(&conn)?;
        // Schema check: refuse to operate on a future schema. Upgrading is
        // #18's job.
        let version = read_schema_version(&conn)?;
        if version > SCHEMA_VERSION {
            return Err(CacheError::FutureSchema {
                found: version,
                supported: SCHEMA_VERSION,
            });
        }

        Ok(Some(Self { conn }))
    }

    /// Replace all persisted match groups with those from `result`.
    ///
    /// Runs as a single transaction. `occurrences` rows cascade-delete
    /// when their parent `match_groups` row goes away, so a truncate of
    /// `match_groups` is sufficient to reset state.
    ///
    /// Also refreshes `file_hashes` with the set of scanned paths; the
    /// content-hash column is populated with a placeholder byte string
    /// for now. #14 will wire real per-file content hashes into the
    /// warm-scan skip path; this issue only needs the row to exist.
    pub fn write_scan_result(&mut self, result: &ScanResult) -> Result<(), CacheError> {
        let tx = self.conn.transaction()?;

        // Fresh state — occurrences cascade from match_groups.
        tx.execute("DELETE FROM match_groups", [])?;
        tx.execute("DELETE FROM file_hashes", [])?;

        {
            let mut group_stmt = tx.prepare(
                "INSERT INTO match_groups \
                    (group_hash, occurrence_count, total_tokens, total_lines) \
                 VALUES (?1, ?2, ?3, ?4)",
            )?;
            let mut occ_stmt = tx.prepare(
                "INSERT INTO occurrences \
                    (group_id, path, start_line, end_line, start_byte, end_byte) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;

            for group in &result.groups {
                let (total_lines, total_tokens) = group_totals(group);
                let group_hash_bytes = group.hash.to_be_bytes();
                group_stmt.execute(params![
                    &group_hash_bytes[..],
                    group.occurrences.len() as i64,
                    total_tokens as i64,
                    total_lines as i64,
                ])?;
                let group_id = tx.last_insert_rowid();

                for occ in &group.occurrences {
                    occ_stmt.execute(params![
                        group_id,
                        path_to_posix_str(&occ.path),
                        occ.span.start_line as i64,
                        occ.span.end_line as i64,
                        occ.span.start_byte as i64,
                        occ.span.end_byte as i64,
                    ])?;
                }
            }
        }

        // Populate file_hashes with scanned paths. Real content hashes
        // land in #14; for now we store an empty blob so the row key
        // (path) exists for future warm-scan logic.
        {
            let mut file_paths: Vec<&std::path::Path> = Vec::new();
            for group in &result.groups {
                for occ in &group.occurrences {
                    file_paths.push(&occ.path);
                }
            }
            file_paths.sort();
            file_paths.dedup();

            let now = now_unix_seconds();
            let mut stmt = tx.prepare(
                "INSERT OR REPLACE INTO file_hashes \
                    (path, content_hash, scanned_at) \
                 VALUES (?1, ?2, ?3)",
            )?;
            for p in file_paths {
                stmt.execute(params![path_to_posix_str(p), &[][..], now])?;
            }
        }

        tx.commit()?;
        Ok(())
    }

    /// List all persisted group summaries, ordered by the smallest
    /// occurrence path (path-asc, then start-line-asc) — matches the
    /// ordering `Scanner::scan` uses so CLI output is stable.
    pub fn list_groups(&self) -> Result<Vec<GroupSummary>, CacheError> {
        // Sort groups by (min_path, min_start_line) across their
        // occurrences, which mirrors the scanner's output order.
        let mut stmt = self.conn.prepare(
            "SELECT g.id, g.occurrence_count, g.total_lines, g.total_tokens, \
                    MIN(o.path)        AS first_path, \
                    MIN(o.start_line)  AS first_start \
             FROM match_groups g \
             LEFT JOIN occurrences o ON o.group_id = g.id \
             GROUP BY g.id \
             ORDER BY first_path ASC, first_start ASC, g.id ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(GroupSummary {
                id: row.get(0)?,
                occurrence_count: row.get(1)?,
                total_lines: row.get(2)?,
                total_tokens: row.get(3)?,
            })
        })?;

        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Fetch a single group's full detail, or `None` if no such id.
    pub fn get_group(&self, id: i64) -> Result<Option<GroupDetail>, CacheError> {
        let group_row = self
            .conn
            .query_row(
                "SELECT id, occurrence_count, total_lines, total_tokens \
                 FROM match_groups WHERE id = ?1",
                params![id],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()?;

        let (id, occurrence_count, total_lines, total_tokens) = match group_row {
            Some(t) => t,
            None => return Ok(None),
        };

        let mut stmt = self.conn.prepare(
            "SELECT path, start_line, end_line, start_byte, end_byte \
             FROM occurrences \
             WHERE group_id = ?1 \
             ORDER BY path ASC, start_line ASC",
        )?;
        let rows = stmt.query_map(params![id], |row| {
            let path: String = row.get(0)?;
            Ok(CachedOccurrence {
                path: PathBuf::from(path),
                start_line: row.get(1)?,
                end_line: row.get(2)?,
                start_byte: row.get(3)?,
                end_byte: row.get(4)?,
            })
        })?;

        let mut occurrences = Vec::new();
        for r in rows {
            occurrences.push(r?);
        }

        Ok(Some(GroupDetail {
            id,
            occurrence_count,
            total_lines,
            total_tokens,
            occurrences,
        }))
    }

    /// The current database schema version. Public so tests can assert
    /// on it directly; callers shouldn't normally need it.
    pub fn schema_version(&self) -> Result<i64, CacheError> {
        read_schema_version(&self.conn)
    }

    /// The active journal mode. Useful for tests that assert WAL is
    /// actually enabled.
    pub fn journal_mode(&self) -> Result<String, CacheError> {
        let mode: String = self
            .conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
        Ok(mode)
    }
}

/// Apply the pragmas every connection needs: WAL journal, foreign keys,
/// and a reasonable busy timeout for the common case of a second process
/// (list/show) briefly contending with a write (scan).
fn configure_connection(conn: &Connection) -> Result<(), CacheError> {
    // Order matters: set WAL first so the `.wal` sidecar exists, then
    // turn on FKs so cascade-delete works for occurrences.
    let _mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
    conn.pragma_update(None, "foreign_keys", true)?;
    // 5s is plenty for tiny repos; #18 will revisit.
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(())
}

/// Create schema v1 if missing, or run the (trivial) migration sequence
/// to bring an older version up to v1. This is the migration-runner stub
/// the issue calls for.
fn ensure_schema(conn: &Connection) -> Result<(), CacheError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_version (\
            version INTEGER NOT NULL PRIMARY KEY\
         );",
    )?;

    let current: i64 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_version",
        [],
        |row| row.get(0),
    )?;

    if current > SCHEMA_VERSION {
        return Err(CacheError::FutureSchema {
            found: current,
            supported: SCHEMA_VERSION,
        });
    }

    // Walk the migration ladder. Today there is exactly one step, from 0
    // to 1. #18 extends this with more entries.
    if current < 1 {
        migrate_v0_to_v1(conn)?;
        conn.execute(
            "INSERT INTO schema_version (version) VALUES (?1)",
            params![1_i64],
        )?;
    }

    // Opening an already-current cache is a no-op: the loop above simply
    // falls through. This keeps `Cache::open` cheap on every run.

    Ok(())
}

/// Create the initial v1 schema.
fn migrate_v0_to_v1(conn: &Connection) -> Result<(), CacheError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS file_hashes (\
            path         TEXT PRIMARY KEY,\
            content_hash BLOB NOT NULL,\
            scanned_at   INTEGER\
         );\
         CREATE TABLE IF NOT EXISTS match_groups (\
            id                INTEGER PRIMARY KEY,\
            group_hash        BLOB NOT NULL,\
            occurrence_count  INTEGER,\
            total_tokens      INTEGER,\
            total_lines       INTEGER\
         );\
         CREATE TABLE IF NOT EXISTS occurrences (\
            id          INTEGER PRIMARY KEY,\
            group_id    INTEGER NOT NULL REFERENCES match_groups(id) ON DELETE CASCADE,\
            path        TEXT NOT NULL,\
            start_line  INTEGER,\
            end_line    INTEGER,\
            start_byte  INTEGER,\
            end_byte    INTEGER\
         );\
         CREATE INDEX IF NOT EXISTS occurrences_group_idx \
            ON occurrences(group_id);",
    )?;
    Ok(())
}

fn read_schema_version(conn: &Connection) -> Result<i64, CacheError> {
    // Be defensive: if the metadata table doesn't exist, report 0 rather
    // than erroring. This lets `open_readonly` gracefully handle the
    // "file exists but schema never ran" edge case.
    let exists: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master \
         WHERE type = 'table' AND name = 'schema_version'",
        [],
        |row| row.get(0),
    )?;
    if exists == 0 {
        return Ok(0);
    }
    let version: i64 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_version",
        [],
        |row| row.get(0),
    )?;
    Ok(version)
}

/// Sum up `(total_lines, total_tokens)` across a group's occurrences.
/// Tokens aren't stored on the `Occurrence` struct at this milestone, so
/// we approximate with `end_byte - start_byte` as a rough proxy. This is
/// good enough for metadata; it is NOT used to re-derive matches.
fn group_totals(group: &MatchGroup) -> (usize, usize) {
    let mut total_lines = 0usize;
    let mut total_tokens = 0usize;
    for occ in &group.occurrences {
        let lines = occ.span.end_line.saturating_sub(occ.span.start_line) + 1;
        let bytes = occ.span.end_byte.saturating_sub(occ.span.start_byte);
        total_lines = total_lines.saturating_add(lines);
        total_tokens = total_tokens.saturating_add(bytes);
    }
    (total_lines, total_tokens)
}

/// Normalize a path to a POSIX-style forward-slashed string for storage
/// so the cache is cross-platform-portable.
fn path_to_posix_str(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

fn now_unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl From<&Occurrence> for CachedOccurrence {
    fn from(occ: &Occurrence) -> Self {
        CachedOccurrence {
            path: occ.path.clone(),
            start_line: occ.span.start_line as i64,
            end_line: occ.span.end_line as i64,
            start_byte: occ.span.start_byte as i64,
            end_byte: occ.span.end_byte as i64,
        }
    }
}

impl CachedOccurrence {
    /// Shape-convert to a [`Span`] for callers that want to reuse the
    /// scanner's types.
    pub fn span(&self) -> Span {
        Span {
            start_line: self.start_line as usize,
            end_line: self.end_line as usize,
            start_byte: self.start_byte as usize,
            end_byte: self.end_byte as usize,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rolling_hash::Span;
    use crate::scanner::{MatchGroup, Occurrence, ScanResult};
    use tempfile::tempdir;

    fn synthetic_result() -> ScanResult {
        ScanResult {
            groups: vec![MatchGroup {
                hash: 0xdead_beef_cafe_f00d,
                occurrences: vec![
                    Occurrence {
                        path: PathBuf::from("a.rs"),
                        span: Span {
                            start_line: 1,
                            end_line: 10,
                            start_byte: 0,
                            end_byte: 100,
                        },
                    },
                    Occurrence {
                        path: PathBuf::from("b.rs"),
                        span: Span {
                            start_line: 5,
                            end_line: 14,
                            start_byte: 40,
                            end_byte: 140,
                        },
                    },
                ],
            }],
            files_scanned: 2,
        }
    }

    #[test]
    fn open_creates_dir_and_gitignore() {
        let dir = tempdir().unwrap();
        let _cache = Cache::open(dir.path()).unwrap();

        let gi = dir.path().join(CACHE_DIR).join(".gitignore");
        assert!(gi.exists());
        let contents = std::fs::read_to_string(&gi).unwrap();
        assert_eq!(contents, "*\n");
    }

    #[test]
    fn open_does_not_clobber_custom_gitignore() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(CACHE_DIR)).unwrap();
        let gi = dir.path().join(CACHE_DIR).join(".gitignore");
        std::fs::write(&gi, "custom\n").unwrap();

        let _cache = Cache::open(dir.path()).unwrap();
        let contents = std::fs::read_to_string(&gi).unwrap();
        assert_eq!(contents, "custom\n");
    }

    #[test]
    fn open_readonly_returns_none_when_missing() {
        let dir = tempdir().unwrap();
        let res = Cache::open_readonly(dir.path()).unwrap();
        assert!(res.is_none());
        // And .dedup/ was never created.
        assert!(!dir.path().join(CACHE_DIR).exists());
    }

    #[test]
    fn wal_mode_enabled() {
        let dir = tempdir().unwrap();
        let cache = Cache::open(dir.path()).unwrap();
        let mode = cache.journal_mode().unwrap();
        assert_eq!(mode.to_ascii_lowercase(), "wal");
    }

    #[test]
    fn schema_version_is_one() {
        let dir = tempdir().unwrap();
        let cache = Cache::open(dir.path()).unwrap();
        assert_eq!(cache.schema_version().unwrap(), 1);
    }

    #[test]
    fn roundtrip_write_then_list_and_get() {
        let dir = tempdir().unwrap();
        let mut cache = Cache::open(dir.path()).unwrap();
        let result = synthetic_result();
        cache.write_scan_result(&result).unwrap();

        let summaries = cache.list_groups().unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].occurrence_count, 2);

        let detail = cache.get_group(summaries[0].id).unwrap().unwrap();
        assert_eq!(detail.occurrences.len(), 2);
        // Occurrences come back ordered by path asc.
        assert_eq!(detail.occurrences[0].path, PathBuf::from("a.rs"));
        assert_eq!(detail.occurrences[1].path, PathBuf::from("b.rs"));
    }

    #[test]
    fn write_is_idempotent_replace() {
        let dir = tempdir().unwrap();
        let mut cache = Cache::open(dir.path()).unwrap();

        // First write: two occurrences.
        cache.write_scan_result(&synthetic_result()).unwrap();
        let before = cache.list_groups().unwrap();
        assert_eq!(before.len(), 1);

        // Second write: single occurrence, different hash. After the
        // replace, list must reflect the second write only.
        let second = ScanResult {
            groups: vec![MatchGroup {
                hash: 0x1111_2222_3333_4444,
                occurrences: vec![
                    Occurrence {
                        path: PathBuf::from("x.rs"),
                        span: Span {
                            start_line: 1,
                            end_line: 6,
                            start_byte: 0,
                            end_byte: 50,
                        },
                    },
                    Occurrence {
                        path: PathBuf::from("y.rs"),
                        span: Span {
                            start_line: 2,
                            end_line: 7,
                            start_byte: 10,
                            end_byte: 60,
                        },
                    },
                ],
            }],
            files_scanned: 2,
        };
        cache.write_scan_result(&second).unwrap();

        let after = cache.list_groups().unwrap();
        assert_eq!(after.len(), 1);
        let detail = cache.get_group(after[0].id).unwrap().unwrap();
        let paths: Vec<_> = detail.occurrences.iter().map(|o| o.path.clone()).collect();
        assert_eq!(paths, vec![PathBuf::from("x.rs"), PathBuf::from("y.rs")]);
    }

    #[test]
    fn get_group_returns_none_for_unknown_id() {
        let dir = tempdir().unwrap();
        let cache = Cache::open(dir.path()).unwrap();
        assert!(cache.get_group(9_999).unwrap().is_none());
    }

    #[test]
    fn empty_scan_result_clears_existing_rows() {
        let dir = tempdir().unwrap();
        let mut cache = Cache::open(dir.path()).unwrap();
        cache.write_scan_result(&synthetic_result()).unwrap();
        assert_eq!(cache.list_groups().unwrap().len(), 1);

        let empty = ScanResult {
            groups: vec![],
            files_scanned: 0,
        };
        cache.write_scan_result(&empty).unwrap();
        assert!(cache.list_groups().unwrap().is_empty());
    }
}
