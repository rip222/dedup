//! `dedup` CLI entry point.
//!
//! Only the `scan` subcommand exists at this milestone. Other subcommands
//! (`list`, `show`, `dismiss`, `config`, ...) land in later issues.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use dedup_core::{ScanConfig, Scanner};

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
    /// Scan a directory tree for duplicate code and print match groups.
    Scan {
        /// Directory to scan. Defaults to the current directory.
        #[arg(default_value = ".")]
        path: PathBuf,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Scan { path } => run_scan(&path),
    }
}

fn run_scan(path: &std::path::Path) -> ExitCode {
    let scanner = Scanner::new(ScanConfig::default());
    let result = match scanner.scan(path) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("dedup: scan failed: {e}");
            return ExitCode::from(2);
        }
    };

    print_groups(&result, &mut std::io::stdout()).ok();

    // Exit code 0 whether duplicates are found or not (git-style default,
    // per the PRD). `--strict` lands in #13.
    ExitCode::SUCCESS
}

fn print_groups<W: std::io::Write>(
    result: &dedup_core::ScanResult,
    out: &mut W,
) -> std::io::Result<()> {
    for (i, group) in result.groups.iter().enumerate() {
        writeln!(
            out,
            "--- group {} ({} occurrences) ---",
            i + 1,
            group.occurrences.len()
        )?;
        for occ in &group.occurrences {
            // Use forward-slashed display so snapshots are stable across
            // platforms. `Path::display()` uses the native separator on
            // Windows (we don't target Windows in this milestone, but
            // being safe is cheap).
            let path = occ.path.to_string_lossy().replace('\\', "/");
            writeln!(
                out,
                "{}:{}-{}",
                path, occ.span.start_line, occ.span.end_line
            )?;
        }
    }
    Ok(())
}
