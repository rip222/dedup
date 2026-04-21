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
//! - Schema versioning (issue #18): the schema version is tracked in
//!   SQLite's `PRAGMA user_version` (an integer stored in the database
//!   file header). A small migration runner walks a `(from, fn)` ladder
//!   on open. A cache whose `user_version` is *newer* than the running
//!   build is left untouched on disk and surfaces as
//!   [`CacheError::NewerSchema`] so the CLI can print a "rescan?" prompt.
//! - Idempotent writes: [`Cache::write_scan_result`] wraps a full replace
//!   of `match_groups` (cascade-deletes occurrences) in a single
//!   transaction so a second write on the same repo yields the second
//!   write's state, not a union. `put_file_entry` and `dismiss_hash` use
//!   `INSERT OR REPLACE`, so content-hash-keyed writes are idempotent
//!   and safe under concurrent writers (WAL auto-retries via
//!   `busy_timeout`).
//!
//! # Warm-scan cache (issue #14)
//!
//! The `file_hashes` table stores, per scanned path, a 64-bit content
//! fingerprint alongside the file's size and mtime (seconds since epoch)
//! at scan time. A companion `file_blocks` table stores the full rolling-
//! hash block-hash list for the file as a length-prefixed sequence of
//! little-endian `u64`s. Together they let the scanner skip the
//! read-tokenize-hash path for unchanged files: on a warm scan, files
//! whose `(size, mtime)` matches the cache and whose freshly-computed
//! content hash matches the persisted one short-circuit straight into
//! the bucket-fill pass with their cached block hashes.
//!
//! The `(size, mtime)` pre-check exists because recomputing the content
//! hash still requires reading the file; `(size, mtime)` is a cheap
//! stat-only probe that filters out the common "nothing changed" case
//! before any I/O. When the probe matches, the content hash is still
//! trusted as the authoritative key — the PRD says "content-hash-keyed",
//! and `(size, mtime)` is an optimization on top of that, not a
//! replacement for it.
//!
//! # Suppressions (issue #11)
//!
//! A `suppressions` table keys dismissed groups by **normalized-block-hash**
//! — the same `group_hash` value stored on `match_groups`. Keying by hash
//! (rather than file path or group id) means cosmetic whitespace changes
//! that leave the normalized token stream untouched stay hidden, while any
//! meaningful edit changes the hash and honestly re-surfaces the group.
//!
//! Dismissals are never used to mutate the `match_groups` rows themselves;
//! filtering happens at report time in the CLI frontend. That preserves
//! the "altered block re-surfaces" property by construction.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use thiserror::Error;
use tracing::{debug, info, warn};

use crate::rolling_hash::Span;
use crate::scanner::{MatchGroup, Occurrence, ScanResult, Tier};

/// Directory name under the repo root where the cache lives.
pub const CACHE_DIR: &str = ".dedup";
/// File name of the SQLite database inside [`CACHE_DIR`].
pub const CACHE_FILE: &str = "cache.sqlite";
/// The schema version this build understands. Bumped when the on-disk
/// schema evolves. The migration runner in [`run_migrations`] walks from
/// whatever `PRAGMA user_version` reports up to this number.
///
/// v1 folds in every table/column that existed in the ad-hoc `probe-and-
/// add` era (issues #4, #6, #11, #14, #17): no real user databases ship
/// v0, so collapsing the history into a single-step bootstrap is safe.
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

/// Backwards-compatible alias for [`CURRENT_SCHEMA_VERSION`]. Kept so
/// `pub use` consumers of this crate don't break on the #18 rename.
pub const SCHEMA_VERSION: i64 = CURRENT_SCHEMA_VERSION as i64;

/// Busy-timeout every [`Cache`] connection runs with. Long enough that a
/// normal scan/list/show contention window (sub-second) auto-retries
/// without surfacing `SQLITE_BUSY` to the caller, short enough that a
/// genuinely stuck writer still fails loudly.
const BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

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

    /// The on-disk cache declares a schema version newer than this build
    /// understands. The file is left untouched — the CLI surfaces a
    /// "rescan?" prompt, the GUI surfaces a toast (issue #30).
    #[error(
        "Cache created by newer Dedup version (schema {found} > supported {supported}). Rescan?"
    )]
    NewerSchema { found: u32, supported: u32 },
}

/// Summary row returned by [`Cache::list_groups`]. One entry per stored
/// match group, ordered for deterministic output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupSummary {
    pub id: i64,
    pub occurrence_count: i64,
    pub total_lines: i64,
    pub total_tokens: i64,
    /// Which detection pass produced the group (Tier A or Tier B).
    pub tier: Tier,
}

/// Detail row returned by [`Cache::get_group`] — the group plus each of
/// its occurrences' spans.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupDetail {
    pub id: i64,
    pub occurrence_count: i64,
    pub total_lines: i64,
    pub total_tokens: i64,
    /// Which detection pass produced the group (Tier A or Tier B).
    pub tier: Tier,
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

/// One dismissed-group entry. Keyed by the normalized-block-hash of the
/// group at dismissal time (the same `group_hash` that lives on
/// `match_groups`). `last_group_id` is informational — it records which
/// group-id the user named when they called `dedup dismiss`, so
/// `dedup suppressions list` can echo it back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suppression {
    /// The normalized-block-hash. Stored on disk as an 8-byte big-endian
    /// blob so it round-trips identically to `match_groups.group_hash`.
    pub hash: crate::rolling_hash::Hash,
    /// Unix-epoch seconds at which the dismissal was recorded.
    pub dismissed_at: i64,
    /// The group id the user named when dismissing, if any. The row
    /// referenced may since have been replaced by a subsequent
    /// `write_scan_result` — this field is a breadcrumb, not a foreign
    /// key.
    pub last_group_id: Option<i64>,
}

/// Per-file content-hash entry used by the warm-scan cache (#14).
///
/// The tuple `(size, mtime)` is a cheap stat-only probe that the scanner
/// checks before trusting `content_hash`: if either differs, the file is
/// re-hashed from disk; if both match, the cached `content_hash` keys a
/// lookup into the block-hash list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileFingerprint {
    /// Stored as an 8-byte big-endian blob so it round-trips identically
    /// to `match_groups.group_hash`.
    pub content_hash: crate::rolling_hash::Hash,
    /// File size in bytes at the time of the last scan.
    pub size: u64,
    /// File mtime in whole seconds since the Unix epoch.
    pub mtime: i64,
}

/// A per-file block-hash list alongside the content hash it was computed
/// under. Fetched on a warm scan so we can skip tokenize + rolling-hash
/// work. The `block_hashes` vector preserves the order produced by the
/// rolling-hash pass, so the scanner can rebuild its per-file bucket
/// index without re-reading the source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedBlocks {
    pub content_hash: crate::rolling_hash::Hash,
    pub block_hashes: Vec<crate::rolling_hash::Hash>,
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
    /// 4. Enables `foreign_keys`, `journal_mode = WAL`, and a 5-second
    ///    `busy_timeout` so concurrent writers auto-retry briefly instead
    ///    of surfacing `SQLITE_BUSY`.
    /// 5. Runs the migration ladder up to [`CURRENT_SCHEMA_VERSION`]. If
    ///    the on-disk `user_version` is *newer* than this build, the file
    ///    is left untouched and [`CacheError::NewerSchema`] is returned.
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
        let mut conn = Connection::open(&db_path)?;
        configure_connection(&conn)?;
        // IMPORTANT: migration runner must run AFTER WAL + busy_timeout
        // are set so any CREATE TABLE contention with a parallel opener
        // is retried rather than erroring out.
        ensure_schema(&mut conn)?;

        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap_or_default();
        info!(
            path = %db_path.display(),
            journal_mode = %mode,
            "cache: opened"
        );

        Ok(Self { conn })
    }

    /// Open the cache read-only for `repo_root`.
    ///
    /// Returns `Ok(None)` if `<repo_root>/.dedup/cache.sqlite` does not
    /// exist. Never creates any files or directories — this is the mode
    /// `dedup list` / `dedup show` use.
    ///
    /// A newer-than-supported schema surfaces as [`CacheError::NewerSchema`];
    /// the on-disk file is not modified in that case.
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
        // Schema check: refuse to operate on a newer schema. We do NOT
        // run migrations here — open_readonly is used by list/show/dismiss
        // and must never mutate a freshly-opened DB's schema. If the file
        // is at an older version than we support we also refuse rather
        // than silently upgrading through a read-only path; the caller
        // should fall back to [`Cache::open`] to trigger migrations.
        let version = read_user_version(&conn)?;
        if version > CURRENT_SCHEMA_VERSION {
            warn!(
                found = version,
                supported = CURRENT_SCHEMA_VERSION,
                "cache: schema newer than supported build"
            );
            return Err(CacheError::NewerSchema {
                found: version,
                supported: CURRENT_SCHEMA_VERSION,
            });
        }

        Ok(Some(Self { conn }))
    }

    /// Read the stored [`FileFingerprint`] for `path`, if any.
    ///
    /// `path` must be a repository-relative path (the same POSIX-form
    /// string the scanner writes on insert). Returns `Ok(None)` when the
    /// cache has no row for this path — the caller treats that as a
    /// cold entry and re-hashes the file.
    pub fn file_fingerprint(&self, path: &Path) -> Result<Option<FileFingerprint>, CacheError> {
        let row = self
            .conn
            .query_row(
                "SELECT content_hash, size, mtime FROM file_hashes WHERE path = ?1",
                params![path_to_posix_str(path)],
                |r| {
                    let bytes: Vec<u8> = r.get(0)?;
                    let size: i64 = r.get(1)?;
                    let mtime: i64 = r.get(2)?;
                    Ok((bytes, size, mtime))
                },
            )
            .optional()?;
        Ok(row.and_then(|(bytes, size, mtime)| {
            blob_to_hash(&bytes).map(|content_hash| FileFingerprint {
                content_hash,
                size: size.max(0) as u64,
                mtime,
            })
        }))
    }

    /// Read the stored [`CachedBlocks`] for `path`, if any.
    ///
    /// Returned iff (a) a `file_blocks` row exists for `path` AND
    /// (b) its `content_hash` equals the caller-provided `expected_hash`.
    /// The scanner always passes the freshly-confirmed content hash to
    /// filter out stale rows where the file was edited and re-hashed but
    /// the block list was never rewritten — the pair is the join key.
    pub fn file_blocks(
        &self,
        path: &Path,
        expected_hash: crate::rolling_hash::Hash,
    ) -> Result<Option<CachedBlocks>, CacheError> {
        let row = self
            .conn
            .query_row(
                "SELECT content_hash, block_hashes FROM file_blocks WHERE path = ?1",
                params![path_to_posix_str(path)],
                |r| {
                    let ch: Vec<u8> = r.get(0)?;
                    let bh: Vec<u8> = r.get(1)?;
                    Ok((ch, bh))
                },
            )
            .optional()?;
        Ok(row.and_then(|(ch, bh)| {
            let stored = blob_to_hash(&ch)?;
            if stored != expected_hash {
                return None;
            }
            Some(CachedBlocks {
                content_hash: stored,
                block_hashes: decode_block_hashes(&bh),
            })
        }))
    }

    /// Upsert the fingerprint + block-hash list for `path`. Used by the
    /// scanner's warm-cache path to refresh rows for files that just got
    /// re-hashed (cold read).
    pub fn put_file_entry(
        &mut self,
        path: &Path,
        fp: &FileFingerprint,
        block_hashes: &[crate::rolling_hash::Hash],
    ) -> Result<(), CacheError> {
        let tx = self.conn.transaction()?;
        {
            let now = now_unix_seconds();
            let hash_bytes = fp.content_hash.to_be_bytes();
            tx.execute(
                "INSERT OR REPLACE INTO file_hashes \
                    (path, content_hash, scanned_at, size, mtime) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    path_to_posix_str(path),
                    &hash_bytes[..],
                    now,
                    fp.size as i64,
                    fp.mtime,
                ],
            )?;
            let blocks_blob = encode_block_hashes(block_hashes);
            tx.execute(
                "INSERT OR REPLACE INTO file_blocks \
                    (path, content_hash, block_hashes) \
                 VALUES (?1, ?2, ?3)",
                params![path_to_posix_str(path), &hash_bytes[..], blocks_blob],
            )?;
        }
        tx.commit()?;
        Ok(())
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
        debug!(
            groups = result.groups.len(),
            files_scanned = result.files_scanned,
            "cache: write_scan_result"
        );
        let tx = self.conn.transaction()?;

        // Fresh state — occurrences cascade from match_groups. The
        // `file_hashes` / `file_blocks` tables are NOT truncated here:
        // those rows are the warm-scan cache, managed per-file by
        // `put_file_entry` during the scan itself.
        tx.execute("DELETE FROM match_groups", [])?;

        {
            let mut group_stmt = tx.prepare(
                "INSERT INTO match_groups \
                    (group_hash, occurrence_count, total_tokens, total_lines, tier) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
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
                    tier_label(group.tier),
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

        tx.commit()?;
        Ok(())
    }

    /// List all persisted group summaries, ordered by tier (A first,
    /// then B), then by the smallest occurrence path (path-asc,
    /// start-line-asc). Mirrors the scanner's output order so CLI
    /// output is stable.
    pub fn list_groups(&self) -> Result<Vec<GroupSummary>, CacheError> {
        let mut stmt = self.conn.prepare(
            "SELECT g.id, g.occurrence_count, g.total_lines, g.total_tokens, g.tier, \
                    MIN(o.path)        AS first_path, \
                    MIN(o.start_line)  AS first_start \
             FROM match_groups g \
             LEFT JOIN occurrences o ON o.group_id = g.id \
             GROUP BY g.id \
             ORDER BY g.tier ASC, first_path ASC, first_start ASC, g.id ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(GroupSummary {
                id: row.get(0)?,
                occurrence_count: row.get(1)?,
                total_lines: row.get(2)?,
                total_tokens: row.get(3)?,
                tier: tier_from_row(row, 4)?,
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
                "SELECT id, occurrence_count, total_lines, total_tokens, tier \
                 FROM match_groups WHERE id = ?1",
                params![id],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        tier_from_row(row, 4)?,
                    ))
                },
            )
            .optional()?;

        let (id, occurrence_count, total_lines, total_tokens, tier) = match group_row {
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
            tier,
            occurrences,
        }))
    }

    /// Look up the normalized-block-hash for a given `match_groups.id`.
    /// Returns `Ok(None)` if no such id exists in the current cache.
    ///
    /// This is what `dedup dismiss <group-id>` uses to translate from the
    /// user-facing group number to the stable hash we actually key
    /// suppressions by.
    pub fn group_hash(&self, id: i64) -> Result<Option<crate::rolling_hash::Hash>, CacheError> {
        let row: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT group_hash FROM match_groups WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(row.and_then(|bytes| blob_to_hash(&bytes)))
    }

    /// Record a dismissal for `hash`. Idempotent: a second call with the
    /// same hash refreshes `dismissed_at` and `last_group_id` via
    /// `INSERT OR REPLACE` rather than erroring.
    pub fn dismiss_hash(
        &mut self,
        hash: crate::rolling_hash::Hash,
        last_group_id: Option<i64>,
    ) -> Result<(), CacheError> {
        let now = now_unix_seconds();
        let bytes = hash.to_be_bytes();
        self.conn.execute(
            "INSERT OR REPLACE INTO suppressions \
                (group_hash, dismissed_at, last_group_id) \
             VALUES (?1, ?2, ?3)",
            params![&bytes[..], now, last_group_id],
        )?;
        Ok(())
    }

    /// List every dismissed hash, sorted by dismissal time (oldest first)
    /// then by hash for a stable tiebreaker.
    pub fn list_suppressions(&self) -> Result<Vec<Suppression>, CacheError> {
        let mut stmt = self.conn.prepare(
            "SELECT group_hash, dismissed_at, last_group_id \
             FROM suppressions \
             ORDER BY dismissed_at ASC, group_hash ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            let bytes: Vec<u8> = row.get(0)?;
            let dismissed_at: i64 = row.get(1)?;
            let last_group_id: Option<i64> = row.get(2)?;
            Ok((bytes, dismissed_at, last_group_id))
        })?;

        let mut out = Vec::new();
        for r in rows {
            let (bytes, dismissed_at, last_group_id) = r?;
            if let Some(hash) = blob_to_hash(&bytes) {
                out.push(Suppression {
                    hash,
                    dismissed_at,
                    last_group_id,
                });
            }
        }
        Ok(out)
    }

    /// Return the set of currently suppressed hashes. Cheap helper for
    /// report-time filtering that doesn't need timestamps.
    pub fn suppressed_hashes(
        &self,
    ) -> Result<std::collections::HashSet<crate::rolling_hash::Hash>, CacheError> {
        let mut stmt = self.conn.prepare("SELECT group_hash FROM suppressions")?;
        let rows = stmt.query_map([], |row| {
            let bytes: Vec<u8> = row.get(0)?;
            Ok(bytes)
        })?;
        let mut out = std::collections::HashSet::new();
        for r in rows {
            if let Some(hash) = blob_to_hash(&r?) {
                out.insert(hash);
            }
        }
        Ok(out)
    }

    /// Truncate the suppressions table. Used by `dedup suppressions clear`.
    pub fn clear_suppressions(&mut self) -> Result<usize, CacheError> {
        let n = self.conn.execute("DELETE FROM suppressions", [])?;
        Ok(n)
    }

    /// The current database schema version, as reported by
    /// `PRAGMA user_version`. Public so tests can assert on it directly;
    /// callers shouldn't normally need it.
    pub fn schema_version(&self) -> Result<i64, CacheError> {
        Ok(read_user_version(&self.conn)? as i64)
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
/// and a reasonable busy timeout so concurrent writers auto-retry instead
/// of surfacing `SQLITE_BUSY` to callers during a normal scan/list/show
/// contention window.
fn configure_connection(conn: &Connection) -> Result<(), CacheError> {
    // Order matters: set WAL first so the `.wal` sidecar exists, then
    // turn on FKs so cascade-delete works for occurrences. The busy
    // timeout is load-bearing for issue #18's concurrent-writer story:
    // WAL lets multiple readers + one writer coexist without blocking,
    // but two *writers* briefly serialize; without a busy timeout the
    // second would surface `SQLITE_BUSY` immediately.
    let _mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
    conn.pragma_update(None, "foreign_keys", true)?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    Ok(())
}

/// A single migration step: given a transaction sitting at `from_version`,
/// apply schema deltas that bring it to `from_version + 1`. The runner
/// sets `PRAGMA user_version` on commit; each step only touches DDL.
type MigrationFn = fn(&rusqlite::Transaction<'_>) -> Result<(), CacheError>;

/// Ordered ladder of schema migrations. Each entry is `(from_version, fn)`:
/// the migration runner runs every entry whose `from_version >= current`
/// and `< CURRENT_SCHEMA_VERSION` in order, bumping the stored version as
/// it goes.
///
/// Today there is exactly one step (`0 → 1`), which bootstraps the entire
/// schema — folding in the ad-hoc columns/tables that earlier issues
/// (#6 `tier`, #11 `suppressions`, #14 `size`/`mtime` + `file_blocks`)
/// added as probe-and-add tweaks to the pre-#18 "stub migration runner".
/// Consolidation is safe because no real user DBs exist yet; the only
/// pre-#18 databases are dev scratchpads regenerated on every run. A
/// future v2 entry would add a new `(1, migrate_v1_to_v2)` row.
const MIGRATIONS: &[(u32, MigrationFn)] = &[(0, migrate_v0_to_v1)];

/// Ensure the cache is at [`CURRENT_SCHEMA_VERSION`].
///
/// Semantics:
/// - `user_version == 0` on a fresh or legacy-stub DB → run all
///   migrations whose `from_version >= 0`.
/// - `user_version < CURRENT_SCHEMA_VERSION` → run the remaining ladder.
/// - `user_version == CURRENT_SCHEMA_VERSION` → no-op, cheap open.
/// - `user_version > CURRENT_SCHEMA_VERSION` → refuse. The file is left
///   untouched (no DDL runs) and we surface `NewerSchema` so the CLI can
///   print a "rescan?" prompt. This is the core acceptance criterion of
///   issue #18.
fn ensure_schema(conn: &mut Connection) -> Result<(), CacheError> {
    let current = read_user_version(conn)?;

    if current > CURRENT_SCHEMA_VERSION {
        warn!(
            found = current,
            supported = CURRENT_SCHEMA_VERSION,
            "cache: schema newer than supported build"
        );
        return Err(CacheError::NewerSchema {
            found: current,
            supported: CURRENT_SCHEMA_VERSION,
        });
    }

    // Handle one legacy edge case before running migrations: the pre-#18
    // stub runner tracked versions in a `schema_version` table and never
    // set `PRAGMA user_version`. If such a DB exists (dev-only — no real
    // users shipped it), read the stub value, stamp `user_version`
    // accordingly, and drop the defunct table. The actual schema DDL is
    // identical to v1 already, so the ladder below is a no-op from here.
    let legacy = legacy_schema_version(conn)?;
    if let Some(v) = legacy
        && current == 0
        && v >= 1
        && v <= CURRENT_SCHEMA_VERSION
    {
        conn.execute("DROP TABLE IF EXISTS schema_version", [])?;
        conn.pragma_update(None, "user_version", v)?;
        return Ok(());
    }

    run_migrations(conn, current, CURRENT_SCHEMA_VERSION)?;
    Ok(())
}

/// Walk the migration ladder from `from` up to (but not past) `to`, each
/// step inside its own transaction. Stamping `user_version` is part of
/// the same transaction so a crash mid-migration can never leave the DB
/// at a version the DDL didn't actually finish.
fn run_migrations(conn: &mut Connection, from: u32, to: u32) -> Result<(), CacheError> {
    let mut current = from;
    while current < to {
        // Look up the step for `current`. If no entry matches, we've
        // reached the top of the declared ladder — log and bail out so a
        // miscounted CURRENT_SCHEMA_VERSION bump is loud instead of
        // silently "succeeding" with an unmigrated DB.
        let step = MIGRATIONS
            .iter()
            .find(|(from_v, _)| *from_v == current)
            .ok_or_else(|| {
                CacheError::Sqlite(rusqlite::Error::ToSqlConversionFailure(
                    format!("no migration registered for schema version {current}").into(),
                ))
            })?;
        let next = current + 1;
        let tx = conn.transaction()?;
        (step.1)(&tx)?;
        tx.pragma_update(None, "user_version", next)?;
        tx.commit()?;
        info!(from = current, to = next, "cache: migration applied");
        current = next;
    }
    Ok(())
}

/// Bootstrap the v1 schema. Folds in every table/column that earlier
/// issues added as ad-hoc tweaks:
///
/// - `match_groups.tier` — issue #6 (Tier B)
/// - `file_hashes.size` / `file_hashes.mtime` — issue #14 (warm cache)
/// - `file_blocks` — issue #14 (warm cache)
/// - `suppressions` — issue #11 (dismiss)
///
/// All are created up-front here rather than as a chain of ALTER TABLEs
/// because no production v1 DBs exist; the consolidation is a one-time
/// clean-up. `CREATE TABLE IF NOT EXISTS` guards make the step idempotent
/// against dev DBs that already had the tables from the pre-#18 stub
/// runner — `ensure_schema` stamps `user_version = 1` on those in place.
fn migrate_v0_to_v1(tx: &rusqlite::Transaction<'_>) -> Result<(), CacheError> {
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS file_hashes (\
            path         TEXT PRIMARY KEY,\
            content_hash BLOB NOT NULL,\
            scanned_at   INTEGER,\
            size         INTEGER NOT NULL DEFAULT 0,\
            mtime        INTEGER NOT NULL DEFAULT 0\
         );\
         CREATE TABLE IF NOT EXISTS file_blocks (\
            path         TEXT PRIMARY KEY,\
            content_hash BLOB NOT NULL,\
            block_hashes BLOB NOT NULL\
         );\
         CREATE TABLE IF NOT EXISTS match_groups (\
            id                INTEGER PRIMARY KEY,\
            group_hash        BLOB NOT NULL,\
            occurrence_count  INTEGER,\
            total_tokens      INTEGER,\
            total_lines       INTEGER,\
            tier              TEXT NOT NULL DEFAULT 'A'\
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
            ON occurrences(group_id);\
         CREATE TABLE IF NOT EXISTS suppressions (\
            group_hash     BLOB PRIMARY KEY,\
            dismissed_at   INTEGER NOT NULL,\
            last_group_id  INTEGER\
         );",
    )?;
    Ok(())
}

/// Read `PRAGMA user_version` as a `u32`. SQLite stores it as a signed
/// 32-bit integer; negative values would be a corrupted-header edge case
/// we'd never write ourselves, so we clamp to 0 rather than surfacing an
/// error.
fn read_user_version(conn: &Connection) -> Result<u32, CacheError> {
    let raw: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    Ok(raw.max(0) as u32)
}

/// Peek at the legacy pre-#18 `schema_version` table, if it exists.
/// Returns `None` when absent (fresh DB or modern DB). Used by
/// [`ensure_schema`] to promote pre-#18 dev DBs (whose schema matches v1
/// already) to `PRAGMA user_version = 1` without re-running DDL.
fn legacy_schema_version(conn: &Connection) -> Result<Option<u32>, CacheError> {
    let exists: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master \
         WHERE type = 'table' AND name = 'schema_version'",
        [],
        |row| row.get(0),
    )?;
    if exists == 0 {
        return Ok(None);
    }
    let v: i64 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_version",
        [],
        |row| row.get(0),
    )?;
    Ok(Some(v.max(0) as u32))
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

/// Decode an 8-byte big-endian blob back into a [`crate::rolling_hash::Hash`].
/// Returns `None` if the blob is the wrong size — callers treat this as
/// "row is malformed, skip it" rather than surfacing an error, so that a
/// legacy row from an earlier build can never crash a list or a filter.
fn blob_to_hash(bytes: &[u8]) -> Option<crate::rolling_hash::Hash> {
    if bytes.len() != 8 {
        return None;
    }
    let mut arr = [0u8; 8];
    arr.copy_from_slice(bytes);
    Some(u64::from_be_bytes(arr))
}

/// Encode a block-hash list as a flat little-endian `u64` byte vector.
/// Length is implicit from `bytes.len() / 8`.
fn encode_block_hashes(hashes: &[crate::rolling_hash::Hash]) -> Vec<u8> {
    let mut out = Vec::with_capacity(hashes.len() * 8);
    for h in hashes {
        out.extend_from_slice(&h.to_le_bytes());
    }
    out
}

/// Inverse of [`encode_block_hashes`]. A trailing partial chunk (not a
/// multiple of 8) is discarded — malformed row, treated as a cache miss.
fn decode_block_hashes(bytes: &[u8]) -> Vec<crate::rolling_hash::Hash> {
    let mut out = Vec::with_capacity(bytes.len() / 8);
    for chunk in bytes.chunks_exact(8) {
        let mut arr = [0u8; 8];
        arr.copy_from_slice(chunk);
        out.push(u64::from_le_bytes(arr));
    }
    out
}

fn now_unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// One-character label used to persist a [`Tier`] as SQLite TEXT.
fn tier_label(tier: Tier) -> &'static str {
    match tier {
        Tier::A => "A",
        Tier::B => "B",
    }
}

/// Parse a persisted `tier` column back into a [`Tier`]. Unknown
/// values default to [`Tier::A`] — this keeps rows written by a very
/// old build (pre-#6) readable without an explicit migration.
fn tier_from_row(row: &rusqlite::Row<'_>, idx: usize) -> rusqlite::Result<Tier> {
    let s: String = row.get(idx)?;
    Ok(match s.as_str() {
        "B" => Tier::B,
        _ => Tier::A,
    })
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
                tier: Tier::A,
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
            issues: Vec::new(),
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
                tier: Tier::A,
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
            issues: Vec::new(),
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
    fn suppressions_roundtrip_by_hash() {
        let dir = tempdir().unwrap();
        let mut cache = Cache::open(dir.path()).unwrap();
        cache.write_scan_result(&synthetic_result()).unwrap();

        // Resolve the hash behind group-id 1, then dismiss by that hash.
        let groups = cache.list_groups().unwrap();
        assert_eq!(groups.len(), 1);
        let gid = groups[0].id;
        let hash = cache.group_hash(gid).unwrap().expect("hash present");
        assert_eq!(hash, 0xdead_beef_cafe_f00d);

        cache.dismiss_hash(hash, Some(gid)).unwrap();

        let entries = cache.list_suppressions().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].hash, hash);
        assert_eq!(entries[0].last_group_id, Some(gid));

        let set = cache.suppressed_hashes().unwrap();
        assert!(set.contains(&hash));
    }

    #[test]
    fn suppression_keyed_by_hash_not_group_id() {
        // Re-scan replaces all match_groups rows, so group_id changes. The
        // hash is stable, so the suppression must still apply.
        let dir = tempdir().unwrap();
        let mut cache = Cache::open(dir.path()).unwrap();
        cache.write_scan_result(&synthetic_result()).unwrap();
        let gid_before = cache.list_groups().unwrap()[0].id;
        let hash = cache.group_hash(gid_before).unwrap().unwrap();
        cache.dismiss_hash(hash, Some(gid_before)).unwrap();

        // Simulate a re-scan: same hash, but write_scan_result truncates
        // and re-inserts, so the id very likely changes.
        cache.write_scan_result(&synthetic_result()).unwrap();
        let gid_after = cache.list_groups().unwrap()[0].id;
        // The suppression is still there, still keyed by hash.
        assert!(cache.suppressed_hashes().unwrap().contains(&hash));
        // And the new group resolves to the same hash — so filtering will
        // still hide it.
        let hash_after = cache.group_hash(gid_after).unwrap().unwrap();
        assert_eq!(hash, hash_after);
    }

    #[test]
    fn altered_block_hash_no_longer_suppressed() {
        // Dismiss one hash; write a "mutated" scan result with a different
        // hash. The suppression should NOT match the new hash.
        let dir = tempdir().unwrap();
        let mut cache = Cache::open(dir.path()).unwrap();
        cache.write_scan_result(&synthetic_result()).unwrap();
        let original = 0xdead_beef_cafe_f00d_u64;
        cache.dismiss_hash(original, Some(1)).unwrap();

        let mutated = ScanResult {
            groups: vec![MatchGroup {
                hash: 0x0000_1111_2222_3333,
                tier: Tier::A,
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
            issues: Vec::new(),
        };
        cache.write_scan_result(&mutated).unwrap();

        let set = cache.suppressed_hashes().unwrap();
        assert!(set.contains(&original));
        assert!(!set.contains(&0x0000_1111_2222_3333));
    }

    #[test]
    fn dismiss_is_idempotent() {
        let dir = tempdir().unwrap();
        let mut cache = Cache::open(dir.path()).unwrap();
        cache.dismiss_hash(0xabcd, Some(42)).unwrap();
        cache.dismiss_hash(0xabcd, Some(43)).unwrap();
        let entries = cache.list_suppressions().unwrap();
        assert_eq!(entries.len(), 1);
        // Second write overwrites last_group_id.
        assert_eq!(entries[0].last_group_id, Some(43));
    }

    #[test]
    fn clear_suppressions_truncates() {
        let dir = tempdir().unwrap();
        let mut cache = Cache::open(dir.path()).unwrap();
        cache.dismiss_hash(0x1111, None).unwrap();
        cache.dismiss_hash(0x2222, None).unwrap();
        assert_eq!(cache.list_suppressions().unwrap().len(), 2);

        let removed = cache.clear_suppressions().unwrap();
        assert_eq!(removed, 2);
        assert!(cache.list_suppressions().unwrap().is_empty());
    }

    #[test]
    fn group_hash_returns_none_for_unknown_id() {
        let dir = tempdir().unwrap();
        let cache = Cache::open(dir.path()).unwrap();
        assert!(cache.group_hash(9_999).unwrap().is_none());
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
            issues: Vec::new(),
        };
        cache.write_scan_result(&empty).unwrap();
        assert!(cache.list_groups().unwrap().is_empty());
    }

    // --- Warm-scan cache (issue #14) -----------------------------------

    #[test]
    fn file_fingerprint_roundtrip() {
        let dir = tempdir().unwrap();
        let mut cache = Cache::open(dir.path()).unwrap();
        let path = PathBuf::from("src/foo.rs");
        let fp = FileFingerprint {
            content_hash: 0xabcd_1234_abcd_1234,
            size: 512,
            mtime: 1_700_000_000,
        };
        let blocks = vec![0x11, 0x22, 0x33];
        cache.put_file_entry(&path, &fp, &blocks).unwrap();

        let loaded = cache.file_fingerprint(&path).unwrap().expect("present");
        assert_eq!(loaded, fp);

        let loaded_blocks = cache
            .file_blocks(&path, fp.content_hash)
            .unwrap()
            .expect("present");
        assert_eq!(loaded_blocks.content_hash, fp.content_hash);
        assert_eq!(loaded_blocks.block_hashes, blocks);
    }

    #[test]
    fn file_blocks_miss_on_hash_mismatch() {
        // Blocks list is keyed by (path, content_hash). A query with a
        // different content hash must miss, so the scanner never
        // rehydrates stale blocks after a file edit.
        let dir = tempdir().unwrap();
        let mut cache = Cache::open(dir.path()).unwrap();
        let path = PathBuf::from("a.rs");
        let fp = FileFingerprint {
            content_hash: 0xaaaa,
            size: 10,
            mtime: 1,
        };
        cache.put_file_entry(&path, &fp, &[1, 2, 3]).unwrap();

        // Same path, different hash → cache miss.
        assert!(cache.file_blocks(&path, 0xbbbb).unwrap().is_none());
        // Matching hash → hit.
        assert!(cache.file_blocks(&path, 0xaaaa).unwrap().is_some());
    }

    #[test]
    fn file_fingerprint_missing_returns_none() {
        let dir = tempdir().unwrap();
        let cache = Cache::open(dir.path()).unwrap();
        assert!(
            cache
                .file_fingerprint(&PathBuf::from("nope.rs"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn put_file_entry_replaces_on_reinsert() {
        let dir = tempdir().unwrap();
        let mut cache = Cache::open(dir.path()).unwrap();
        let path = PathBuf::from("x.rs");

        let fp1 = FileFingerprint {
            content_hash: 0x1111,
            size: 100,
            mtime: 10,
        };
        cache.put_file_entry(&path, &fp1, &[1, 2]).unwrap();

        // Simulate a file edit: different content hash + blocks.
        let fp2 = FileFingerprint {
            content_hash: 0x2222,
            size: 200,
            mtime: 20,
        };
        cache.put_file_entry(&path, &fp2, &[9, 8, 7]).unwrap();

        let loaded = cache.file_fingerprint(&path).unwrap().expect("present");
        assert_eq!(loaded, fp2);
        assert!(cache.file_blocks(&path, 0x1111).unwrap().is_none());
        let blocks = cache.file_blocks(&path, 0x2222).unwrap().expect("present");
        assert_eq!(blocks.block_hashes, vec![9, 8, 7]);
    }

    #[test]
    fn write_scan_result_preserves_file_cache() {
        // Writing scan results must not clobber the warm-scan tables —
        // that's the whole point of keeping them separate from
        // match_groups.
        let dir = tempdir().unwrap();
        let mut cache = Cache::open(dir.path()).unwrap();
        let path = PathBuf::from("preserved.rs");
        let fp = FileFingerprint {
            content_hash: 0xdeadbeef,
            size: 1,
            mtime: 1,
        };
        cache.put_file_entry(&path, &fp, &[42]).unwrap();

        cache.write_scan_result(&synthetic_result()).unwrap();

        // Row still present after the scan write.
        assert!(cache.file_fingerprint(&path).unwrap().is_some());
    }

    // --- Schema versioning (issue #18) --------------------------------

    #[test]
    fn user_version_stamped_on_fresh_open() {
        // A fresh DB must be stamped with `PRAGMA user_version` =
        // CURRENT_SCHEMA_VERSION so re-opens recognize it as current and
        // skip the migration ladder.
        let dir = tempdir().unwrap();
        let cache = Cache::open(dir.path()).unwrap();
        let raw: i64 = cache
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(raw, CURRENT_SCHEMA_VERSION as i64);
    }

    #[test]
    fn newer_schema_is_preserved_and_surfaces_error() {
        // Core acceptance criterion of #18: a DB whose user_version is
        // *newer* than this build must (a) surface NewerSchema and (b)
        // leave the file bytes untouched.
        let dir = tempdir().unwrap();
        {
            let _ = Cache::open(dir.path()).unwrap();
        }
        let db_path = dir.path().join(CACHE_DIR).join(CACHE_FILE);

        // Bump user_version out-of-band to a far-future value.
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.pragma_update(None, "user_version", 999_u32).unwrap();
        }

        let bytes_before = std::fs::read(&db_path).unwrap();

        // open() must refuse and leave the file untouched.
        match Cache::open(dir.path()) {
            Err(CacheError::NewerSchema { found, supported }) => {
                assert_eq!(found, 999);
                assert_eq!(supported, CURRENT_SCHEMA_VERSION);
            }
            Err(other) => panic!("expected NewerSchema, got {other:?}"),
            Ok(_) => panic!("expected NewerSchema error, got success"),
        }

        // open_readonly must refuse the same way.
        match Cache::open_readonly(dir.path()) {
            Err(CacheError::NewerSchema { found: 999, .. }) => {}
            Err(other) => panic!("expected NewerSchema, got {other:?}"),
            Ok(_) => panic!("expected NewerSchema error, got success"),
        }

        let bytes_after = std::fs::read(&db_path).unwrap();
        assert_eq!(
            bytes_before, bytes_after,
            "cache file must be preserved byte-for-byte when newer schema refused"
        );
    }

    #[test]
    fn legacy_stub_db_promoted_to_user_version() {
        // Pre-#18 dev DBs recorded the version in a `schema_version`
        // table and left PRAGMA user_version = 0. ensure_schema should
        // promote those in place (no DDL re-run, no data loss) by
        // stamping user_version and dropping the defunct table.
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(CACHE_DIR)).unwrap();
        let db_path = dir.path().join(CACHE_DIR).join(CACHE_FILE);

        {
            // Hand-roll the pre-#18 layout: full v1 schema + legacy
            // `schema_version` table with value 1. user_version stays 0.
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE schema_version (version INTEGER NOT NULL PRIMARY KEY);\
                 INSERT INTO schema_version (version) VALUES (1);\
                 CREATE TABLE match_groups (\
                    id INTEGER PRIMARY KEY, group_hash BLOB NOT NULL,\
                    occurrence_count INTEGER, total_tokens INTEGER,\
                    total_lines INTEGER, tier TEXT NOT NULL DEFAULT 'A');\
                 CREATE TABLE occurrences (\
                    id INTEGER PRIMARY KEY,\
                    group_id INTEGER NOT NULL REFERENCES match_groups(id) \
                        ON DELETE CASCADE,\
                    path TEXT NOT NULL, start_line INTEGER, end_line INTEGER,\
                    start_byte INTEGER, end_byte INTEGER);\
                 CREATE TABLE file_hashes (\
                    path TEXT PRIMARY KEY, content_hash BLOB NOT NULL,\
                    scanned_at INTEGER, size INTEGER NOT NULL DEFAULT 0,\
                    mtime INTEGER NOT NULL DEFAULT 0);\
                 CREATE TABLE file_blocks (\
                    path TEXT PRIMARY KEY, content_hash BLOB NOT NULL,\
                    block_hashes BLOB NOT NULL);\
                 CREATE TABLE suppressions (\
                    group_hash BLOB PRIMARY KEY, dismissed_at INTEGER NOT NULL,\
                    last_group_id INTEGER);",
            )
            .unwrap();
        }

        let cache = Cache::open(dir.path()).expect("legacy DB should be promoted");
        assert_eq!(cache.schema_version().unwrap(), 1);

        // The legacy table should be gone after promotion.
        let exists: i64 = cache
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type = 'table' AND name = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(exists, 0, "legacy schema_version table should be dropped");
    }

    #[test]
    fn put_file_entry_is_idempotent_on_same_hash() {
        // Content-hash-keyed writes: calling put_file_entry twice with
        // the same hash must converge (row count unchanged, no error).
        let dir = tempdir().unwrap();
        let mut cache = Cache::open(dir.path()).unwrap();
        let path = PathBuf::from("idempotent.rs");
        let fp = FileFingerprint {
            content_hash: 0xfeed_face_cafe_0001,
            size: 42,
            mtime: 1_700_000_000,
        };
        let blocks = vec![0xaa, 0xbb, 0xcc];

        cache.put_file_entry(&path, &fp, &blocks).unwrap();
        cache.put_file_entry(&path, &fp, &blocks).unwrap();
        cache.put_file_entry(&path, &fp, &blocks).unwrap();

        let hash_count: i64 = cache
            .conn
            .query_row(
                "SELECT COUNT(*) FROM file_hashes WHERE path = ?1",
                params![path_to_posix_str(&path)],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(hash_count, 1, "duplicate writes should not add rows");
        let blocks_count: i64 = cache
            .conn
            .query_row(
                "SELECT COUNT(*) FROM file_blocks WHERE path = ?1",
                params![path_to_posix_str(&path)],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(blocks_count, 1);
    }

    #[test]
    fn concurrent_writers_on_same_db_converge() {
        // Two threads, each with their own `Connection` to the same DB,
        // writing overlapping `put_file_entry` calls. WAL + busy_timeout
        // must let them both land without corruption and with idempotent
        // convergence on shared keys.
        use std::sync::Arc;
        use std::thread;

        let dir = tempdir().unwrap();
        // Materialize the schema up front so the two writer threads
        // don't both try to run migrations concurrently.
        {
            let _ = Cache::open(dir.path()).unwrap();
        }
        let root: Arc<PathBuf> = Arc::new(dir.path().to_path_buf());

        let writer = |thread_id: usize, root: Arc<PathBuf>| {
            let mut cache = Cache::open(&root).unwrap();
            for i in 0..50 {
                // Interleave writes: half of the keys are thread-unique,
                // half are shared so both writers target the same row.
                let shared = i % 2 == 0;
                let path = if shared {
                    PathBuf::from(format!("shared-{i}.rs"))
                } else {
                    PathBuf::from(format!("t{thread_id}-{i}.rs"))
                };
                let fp = FileFingerprint {
                    content_hash: if shared {
                        0x1111_2222_3333_4444
                    } else {
                        0xdead_0000_0000_0000 | (thread_id as u64) << 32 | (i as u64)
                    },
                    size: i as u64,
                    mtime: i as i64,
                };
                cache.put_file_entry(&path, &fp, &[i as u64]).unwrap();
            }
        };

        let h1 = {
            let r = Arc::clone(&root);
            thread::spawn(move || writer(1, r))
        };
        let h2 = {
            let r = Arc::clone(&root);
            thread::spawn(move || writer(2, r))
        };
        h1.join().unwrap();
        h2.join().unwrap();

        // Verify final state: every shared key is present exactly once
        // (idempotent convergence); every thread-unique key is present.
        let cache = Cache::open(dir.path()).unwrap();
        let shared_count: i64 = cache
            .conn
            .query_row(
                "SELECT COUNT(*) FROM file_hashes WHERE path LIKE 'shared-%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(shared_count, 25, "25 shared keys, one row each");
        let unique_count: i64 = cache
            .conn
            .query_row(
                "SELECT COUNT(*) FROM file_hashes WHERE path LIKE 't%-%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(unique_count, 50, "25 unique keys per thread, 2 threads");
    }
}
