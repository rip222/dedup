//! `dedup` CLI entry point.
//!
//! Subcommands at this milestone:
//!
//! - `scan`: walk a directory, detect Tier A duplicates, persist them to
//!   `<repo>/.dedup/cache.sqlite`, and print the groups to stdout.
//! - `list`: read the persisted groups from the cache and print them in
//!   the same format as `scan` — no re-scan.
//! - `show <id>`: print full detail (all occurrence spans) for one
//!   persisted group.
//!
//! Exit codes:
//! - `0` success (even when there are no duplicates / no cache hits).
//! - `2` usage/error (no cache to list/show, unknown group id, scan I/O
//!   failure).

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use dedup_core::{Cache, GroupDetail, ScanConfig, ScanResult, Scanner};

#[derive(Parser, Debug)]
#[command(
    name = "dedup",
    about = "Find duplicate code across a directory tree",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Scan a directory tree for duplicate code and persist the groups.
    Scan {
        /// Directory to scan. Defaults to the current directory.
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Print the groups from the most recent cached scan.
    List {
        /// Directory whose `.dedup/cache.sqlite` should be read. Defaults
        /// to the current directory.
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Print full detail for one cached group by id.
    Show {
        /// The group id (as printed by `dedup list`).
        id: i64,
        /// Directory whose `.dedup/cache.sqlite` should be read. Defaults
        /// to the current directory.
        #[arg(default_value = ".")]
        path: PathBuf,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Scan { path } => run_scan(&path),
        Command::List { path } => run_list(&path),
        Command::Show { id, path } => run_show(id, &path),
    }
}

fn run_scan(path: &Path) -> ExitCode {
    let scanner = Scanner::new(ScanConfig::default());
    let result = match scanner.scan(path) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("dedup: scan failed: {e}");
            return ExitCode::from(2);
        }
    };

    // Persist before printing so the cache reflects stdout exactly.
    // Failure to persist is surfaced but does NOT suppress the print:
    // losing the cache is recoverable, losing the scan output isn't.
    match Cache::open(path) {
        Ok(mut cache) => {
            if let Err(e) = cache.write_scan_result(&result) {
                eprintln!("dedup: warning: failed to persist scan: {e}");
            }
        }
        Err(e) => {
            eprintln!("dedup: warning: failed to open cache: {e}");
        }
    }

    print_scan_groups(&result, &mut std::io::stdout()).ok();

    // Exit code 0 whether duplicates are found or not (git-style default,
    // per the PRD). `--strict` lands in #13.
    ExitCode::SUCCESS
}

fn run_list(path: &Path) -> ExitCode {
    let cache = match Cache::open_readonly(path) {
        Ok(Some(c)) => c,
        Ok(None) => {
            eprintln!("dedup: No cached scan found. Run `dedup scan` first.");
            return ExitCode::from(2);
        }
        Err(e) => {
            eprintln!("dedup: failed to open cache: {e}");
            return ExitCode::from(2);
        }
    };

    let groups = match cache.list_groups() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("dedup: failed to read cache: {e}");
            return ExitCode::from(2);
        }
    };

    // Fetch full details so we can print occurrences alongside the
    // summary header — matches `dedup scan` output exactly.
    let mut stdout = std::io::stdout();
    for (ord, summary) in groups.iter().enumerate() {
        let detail = match cache.get_group(summary.id) {
            Ok(Some(d)) => d,
            Ok(None) => continue, // group vanished mid-read; skip.
            Err(e) => {
                eprintln!("dedup: failed to read group {}: {e}", summary.id);
                return ExitCode::from(2);
            }
        };
        if print_cached_group_full(ord + 1, &detail, &mut stdout).is_err() {
            // Broken pipe / closed stdout — treat as clean exit.
            return ExitCode::SUCCESS;
        }
    }

    ExitCode::SUCCESS
}

fn run_show(id: i64, path: &Path) -> ExitCode {
    let cache = match Cache::open_readonly(path) {
        Ok(Some(c)) => c,
        Ok(None) => {
            eprintln!("dedup: No cached scan found. Run `dedup scan` first.");
            return ExitCode::from(2);
        }
        Err(e) => {
            eprintln!("dedup: failed to open cache: {e}");
            return ExitCode::from(2);
        }
    };

    let detail = match cache.get_group(id) {
        Ok(Some(d)) => d,
        Ok(None) => {
            eprintln!("dedup: no group with id {id}");
            return ExitCode::from(2);
        }
        Err(e) => {
            eprintln!("dedup: failed to read cache: {e}");
            return ExitCode::from(2);
        }
    };

    let mut stdout = std::io::stdout();
    print_cached_group_show(&detail, &mut stdout).ok();

    ExitCode::SUCCESS
}

/// Print a [`ScanResult`] in the canonical dedup text format.
fn print_scan_groups<W: std::io::Write>(result: &ScanResult, out: &mut W) -> std::io::Result<()> {
    for (i, group) in result.groups.iter().enumerate() {
        writeln!(
            out,
            "--- group {} ({} occurrences) ---",
            i + 1,
            group.occurrences.len()
        )?;
        for occ in &group.occurrences {
            let path = path_display(&occ.path);
            writeln!(
                out,
                "{}:{}-{}",
                path, occ.span.start_line, occ.span.end_line
            )?;
        }
    }
    Ok(())
}

/// Print one cached group in the same format as `scan`, but using the
/// given ordinal (1-based) as the `group N` header number — callers are
/// expected to enumerate in the same order `list_groups` returned.
fn print_cached_group_full<W: std::io::Write>(
    ordinal: usize,
    detail: &GroupDetail,
    out: &mut W,
) -> std::io::Result<()> {
    writeln!(
        out,
        "--- group {} ({} occurrences) ---",
        ordinal, detail.occurrence_count
    )?;
    for occ in &detail.occurrences {
        let path = path_display(&occ.path);
        writeln!(out, "{}:{}-{}", path, occ.start_line, occ.end_line)?;
    }
    Ok(())
}

/// `show` emits a single group; the header uses the persisted id so it
/// is stable across invocations. Follows with one `path:start-end` line
/// per occurrence, indented to match the visual weight of `list`.
fn print_cached_group_show<W: std::io::Write>(
    detail: &GroupDetail,
    out: &mut W,
) -> std::io::Result<()> {
    writeln!(
        out,
        "--- group {} ({} occurrences) ---",
        detail.id, detail.occurrence_count
    )?;
    for occ in &detail.occurrences {
        let path = path_display(&occ.path);
        writeln!(out, "{}:{}-{}", path, occ.start_line, occ.end_line)?;
    }
    Ok(())
}

/// Forward-slash a path for stable cross-platform output.
fn path_display(p: &std::path::Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}
