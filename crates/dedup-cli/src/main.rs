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
//! - `config`: inspect (`path`) or open (`edit`) the resolved config
//!   file(s). Never auto-creates: the only place a config file is ever
//!   materialized is `dedup config edit`, on explicit user action.
//!
//! # Global flags
//!
//! Flags live on [`GlobalArgs`], flattened into the top-level [`Cli`].
//! Subcommand handlers read them via `cli.globals`. The bulk of the
//! surface (see issue #13) wires the flag shape — the tier/lang filters
//! apply post-scan, `--strict` controls the exit code, progress is
//! driven through the core's `ProgressSink` trait, and verbose / color /
//! jobs flags are stored on the config for downstream issues (`#6` Tier
//! B, `#14` parallelism) to pick up.
//!
//! # Exit codes
//!
//! - `0` success (with or without findings; git-style default).
//! - `1` findings present AND `--strict` was passed.
//! - `2` config / usage / parse error (including clap parse errors and
//!   invalid config files — the `main` function remaps clap's error
//!   kind to `2` before exit).
//! - `101` Rust panic (the default `panic = "unwind"` behavior; we do
//!   not override it here).
//!
//! # Logging (issue #16)
//!
//! Library crates emit `tracing` events; this CLI installs a
//! [`tracing_subscriber`] that writes to **stderr** with the pretty
//! formatter. The filter is built from `RUST_LOG` if set, otherwise it
//! defaults to `warn`. The `--verbose` / `-v` flag (owned by
//! [`GlobalArgs`] per #13) lowers the default to `dedup=debug`, still
//! overridable by `RUST_LOG`. Frontend errors use `anyhow::Result` with
//! `.context(...)` for ergonomic propagation; library errors keep their
//! `thiserror` enums per the PRD.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, ExitCode};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};
use dedup_core::{
    Cache, Config, ConfigError, GroupDetail, MatchGroup, ProgressSink, ScanConfig, Scanner, Tier,
};
use indicatif::{ProgressBar, ProgressStyle};
use tracing_subscriber::{EnvFilter, fmt};

/// Root CLI parser. Subcommands live under [`Command`]; shared flags live
/// on [`GlobalArgs`] and are flattened into this struct so every
/// subcommand sees them uniformly.
#[derive(Parser, Debug)]
#[command(
    name = "dedup",
    about = "Find duplicate code across a directory tree",
    version,
    // Ensure clap parse / usage errors surface as exit code 2 (PRD: usage
    // error). clap's default is 2 for UsageError but 1 for some value
    // errors — normalizing here keeps the contract stable.
    next_help_heading = "Global options"
)]
struct Cli {
    #[command(flatten)]
    globals: GlobalArgs,

    #[command(subcommand)]
    command: Command,
}

/// Flags accepted by every subcommand.
///
/// Some of these flags (`--no-gitignore`, `--jobs`) are deliberate
/// stubs at this milestone — the upstream features they gate (`#5`
/// ignore layers, `#14` parallelism) land in later PRs. They are parsed
/// and stored so the surface is stable and downstream PRs can wire them
/// without re-breaking the CLI.
#[derive(Args, Debug, Clone)]
pub struct GlobalArgs {
    /// Disable the gitignore layer. Parsed and stored; full wiring lands
    /// with the `ignore` crate integration in #5.
    #[arg(long, global = true)]
    pub no_gitignore: bool,

    /// Restrict detection tier. Tier A is the language-oblivious
    /// rolling-hash scan; Tier B is per-language tree-sitter matching.
    /// At MVP Tier B isn't emitted yet (lands in #6), so `b` simply
    /// filters everything out and `both` behaves like `a`.
    #[arg(long, value_enum, default_value_t = TierFilter::Both, global = true)]
    pub tier: TierFilter,

    /// Restrict Tier B languages (comma-separated list, e.g.
    /// `rust,ts,python`). Parsed and stored; only applied to Tier B
    /// groups, which don't exist yet — see #6.
    #[arg(long, value_delimiter = ',', global = true)]
    pub lang: Vec<Language>,

    /// Parallelism for the Tier A scanner. Parsed and stored; full
    /// wiring lands with rayon integration in #14. `0` falls through to
    /// `num_cpus`.
    #[arg(long, global = true)]
    pub jobs: Option<usize>,

    /// Suppress the progress spinner. Exit codes / stdout content are
    /// unaffected.
    #[arg(long, short = 'q', global = true, conflicts_with = "verbose")]
    pub quiet: bool,

    /// Enable debug logging. Lowers the default `tracing` filter to
    /// `dedup=debug`. `RUST_LOG` (when set) still wins.
    #[arg(long, short = 'v', action = ArgAction::SetTrue, global = true)]
    pub verbose: bool,

    /// Control ANSI color output. `auto` (default) disables color when
    /// stdout is not a TTY.
    #[arg(long, value_enum, default_value_t = ColorMode::Auto, global = true)]
    pub color: ColorMode,

    /// Exit 1 when findings are present. Default is git-style exit 0
    /// regardless of findings.
    #[arg(long, global = true)]
    pub strict: bool,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum TierFilter {
    A,
    B,
    Both,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Rust,
    Ts,
    Python,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMode {
    Always,
    Never,
    Auto,
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
    /// Inspect or edit the layered dedup config.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
}

#[derive(Subcommand, Debug)]
enum ConfigAction {
    /// Print the resolved config paths (global + project) with a
    /// presence indicator for each layer.
    Path {
        /// Directory whose project config should be resolved. Defaults
        /// to the current directory.
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Open the resolved config file in `$EDITOR` (falling back to
    /// `$VISUAL`, then `vi`). If no config file exists, an empty one is
    /// created at the project path — this is the one place a config
    /// file is ever materialized.
    Edit {
        /// Directory whose project config should be edited. Defaults
        /// to the current directory.
        #[arg(default_value = ".")]
        path: PathBuf,
    },
}

/// Program entry point.
///
/// clap's [`Parser::parse`] calls `exit(2)` on a parse error by default
/// (which matches the PRD), so we don't need to remap error codes
/// manually — any `UsageError` / parse failure bypasses this function's
/// `ExitCode` entirely.
fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(c) => c,
        Err(e) => {
            // Print the usage/error text clap prepared, then exit with
            // the PRD-mandated code. We re-map non-display errors to 2
            // so parse errors never leak out as 1.
            let code = match e.kind() {
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion => {
                    let _ = e.print();
                    return ExitCode::SUCCESS;
                }
                _ => 2,
            };
            let _ = e.print();
            return ExitCode::from(code);
        }
    };

    // Install the tracing subscriber as early as possible so every
    // downstream call (including the config load below) can emit.
    init_logging(cli.globals.verbose);

    let result = match cli.command {
        Command::Scan { ref path } => run_scan(path, &cli.globals),
        Command::List { ref path } => run_list(path, &cli.globals),
        Command::Show { id, ref path } => run_show(id, path, &cli.globals),
        Command::Config { action } => match action {
            ConfigAction::Path { path } => Ok(run_config_path(&path)),
            ConfigAction::Edit { path } => Ok(run_config_edit(&path)),
        },
    };

    match result {
        Ok(code) => code,
        Err(err) => {
            // `{:#}` prints the full anyhow chain on one line. Context
            // strings added via `.context(...)` at call sites show up as
            // the leading segment.
            eprintln!("dedup: {err:#}");
            ExitCode::from(2)
        }
    }
}

/// Install the process-wide [`tracing`] subscriber.
///
/// - Writes to stderr so scan output on stdout stays clean and pipeable.
/// - Pretty formatter for human-readable dev output.
/// - `EnvFilter` built from `RUST_LOG` when set; otherwise defaults to
///   `warn` (or `dedup=debug` if `--verbose` was passed).
///
/// Idempotent enough in practice: `try_init` silently no-ops if a
/// subscriber was already installed, which keeps integration tests that
/// invoke the binary multiple times from panicking.
fn init_logging(verbose: bool) {
    let default = if verbose { "dedup=debug" } else { "warn" };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default));
    let _ = fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .pretty()
        .try_init();
}

fn run_scan(path: &Path, globals: &GlobalArgs) -> Result<ExitCode> {
    // Load layered config before scanning. Parse errors are fatal; a
    // newer-schema file is treated as a warning and falls back to
    // defaults (the `.bak` migration flow is deferred — see #9 spec).
    let config = match Config::load(Some(path)) {
        Ok(c) => c,
        Err(ConfigError::SchemaVersionMismatch {
            path: p,
            found,
            expected,
        }) => {
            eprintln!(
                "dedup: warning: config at {} declares schema_version {found} which is newer than supported version {expected}; using defaults",
                p.display()
            );
            Config::default()
        }
        Err(e) => {
            eprintln!("dedup: config error: {e}");
            return Ok(ExitCode::from(2));
        }
    };

    let scanner = Scanner::new(ScanConfig::from(&config));

    // Build the progress sink. Spinner is suppressed when:
    // - stdout is not a TTY (piped output), OR
    // - `--quiet` is set.
    // `--color never` also suppresses color on the spinner. We keep the
    // spinner on stderr so stdout stays pipe-clean regardless.
    let use_progress = !globals.quiet && std::io::stderr().is_terminal();
    let sink: Box<dyn ProgressSink> = if use_progress {
        Box::new(IndicatifSink::new(color_enabled_for_stderr(globals)))
    } else {
        Box::new(dedup_core::NoopSink)
    };

    let result = scanner
        .scan_with_progress(path, sink.as_ref())
        .with_context(|| format!("scan failed for {}", path.display()))?;

    // Finalize the spinner before emitting any group text so progress
    // output doesn't collide with stdout lines.
    drop(sink);

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

    // Apply the tier / lang filters to produce the user-visible group
    // slice. Tier A groups pass the lang filter unconditionally
    // (they're language-oblivious); Tier B groups must additionally
    // clear the `--lang` filter when one is specified.
    let visible: Vec<&MatchGroup> = result
        .groups
        .iter()
        .filter(|g| match g.tier {
            Tier::A => tier_allows_a(globals),
            Tier::B => tier_allows_b(globals) && lang_allows(globals, g),
        })
        .collect();

    let had_findings = !visible.is_empty();

    print_scan_groups(&visible, &mut std::io::stdout()).ok();

    if had_findings && globals.strict {
        return Ok(ExitCode::from(1));
    }
    Ok(ExitCode::SUCCESS)
}

fn run_list(path: &Path, globals: &GlobalArgs) -> Result<ExitCode> {
    let cache = match Cache::open_readonly(path)
        .with_context(|| format!("failed to open cache at {}", path.display()))?
    {
        Some(c) => c,
        None => {
            eprintln!("dedup: No cached scan found. Run `dedup scan` first.");
            return Ok(ExitCode::from(2));
        }
    };

    let groups = cache.list_groups().context("failed to read cache")?;

    let allow_a = tier_allows_a(globals);
    let allow_b = tier_allows_b(globals);

    // Fetch full details so we can print occurrences alongside the
    // summary header — matches `dedup scan` output exactly.
    let mut stdout = std::io::stdout();
    let mut emitted = 0usize;
    let mut ordinal = 0usize;
    for summary in groups.iter() {
        let tier_ok = match summary.tier {
            Tier::A => allow_a,
            Tier::B => allow_b,
        };
        if !tier_ok {
            continue;
        }
        let detail = match cache
            .get_group(summary.id)
            .with_context(|| format!("failed to read group {}", summary.id))?
        {
            Some(d) => d,
            None => continue, // group vanished mid-read; skip.
        };
        // `--lang` only applies to Tier B (Tier A is language-oblivious).
        if detail.tier == Tier::B && !lang_allows_cached(globals, &detail) {
            continue;
        }
        ordinal += 1;
        if print_cached_group_full(ordinal, &detail, &mut stdout).is_err() {
            // Broken pipe / closed stdout — treat as clean exit.
            return Ok(ExitCode::SUCCESS);
        }
        emitted += 1;
    }

    if emitted > 0 && globals.strict {
        return Ok(ExitCode::from(1));
    }
    Ok(ExitCode::SUCCESS)
}

fn run_show(id: i64, path: &Path, _globals: &GlobalArgs) -> Result<ExitCode> {
    let cache = match Cache::open_readonly(path)
        .with_context(|| format!("failed to open cache at {}", path.display()))?
    {
        Some(c) => c,
        None => {
            eprintln!("dedup: No cached scan found. Run `dedup scan` first.");
            return Ok(ExitCode::from(2));
        }
    };

    let detail = match cache
        .get_group(id)
        .with_context(|| format!("failed to read group {id}"))?
    {
        Some(d) => d,
        None => {
            eprintln!("dedup: no group with id {id}");
            return Ok(ExitCode::from(2));
        }
    };

    let mut stdout = std::io::stdout();
    print_cached_group_show(&detail, &mut stdout).ok();

    Ok(ExitCode::SUCCESS)
}

/// `dedup config path` — print one line per config layer with a
/// presence indicator. Never creates files.
fn run_config_path(path: &Path) -> ExitCode {
    let global = Config::global_path();
    let project = Config::project_path(path);
    let mut stdout = std::io::stdout();
    let _ = writeln!(stdout, "global: {} {}", global.display(), presence(&global));
    let _ = writeln!(
        stdout,
        "project: {} {}",
        project.display(),
        presence(&project)
    );
    ExitCode::SUCCESS
}

/// `dedup config edit` — resolve to the preferred layer (project if the
/// repo has a `.dedup/` directory, else global), create an empty file
/// there if neither layer has one, then launch `$EDITOR`.
fn run_config_edit(path: &Path) -> ExitCode {
    let project = Config::project_path(path);
    let global = Config::global_path();

    // Prefer the project layer if the `.dedup/` directory already
    // exists. Otherwise prefer whichever file actually exists. If
    // neither exists, materialize an empty project-scoped file — this
    // is the documented "one place" a config file is ever created.
    let target = if path.join(".dedup").is_dir() || project.exists() {
        project.clone()
    } else if global.exists() {
        global.clone()
    } else {
        project.clone()
    };

    if !target.exists() {
        if let Some(parent) = target.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            eprintln!(
                "dedup: failed to create config dir {}: {e}",
                parent.display()
            );
            return ExitCode::from(2);
        }
        if let Err(e) = std::fs::write(&target, "") {
            eprintln!(
                "dedup: failed to create config file {}: {e}",
                target.display()
            );
            return ExitCode::from(2);
        }
    }

    let editor = resolve_editor();
    let status = ProcessCommand::new(&editor).arg(&target).status();
    match status {
        Ok(s) if s.success() => ExitCode::SUCCESS,
        Ok(s) => {
            eprintln!(
                "dedup: editor {} exited with status {}",
                editor,
                s.code()
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "?".into())
            );
            ExitCode::from(2)
        }
        Err(e) => {
            eprintln!("dedup: failed to launch editor {editor}: {e}");
            ExitCode::from(2)
        }
    }
}

/// Resolve the editor command: `$EDITOR`, then `$VISUAL`, then `vi`.
fn resolve_editor() -> String {
    if let Ok(v) = std::env::var("EDITOR")
        && !v.is_empty()
    {
        return v;
    }
    if let Ok(v) = std::env::var("VISUAL")
        && !v.is_empty()
    {
        return v;
    }
    "vi".to_string()
}

fn presence(p: &Path) -> &'static str {
    if p.exists() {
        "(present)"
    } else {
        "(not present)"
    }
}

/// Return true iff Tier A groups should be emitted given `--tier`.
fn tier_allows_a(globals: &GlobalArgs) -> bool {
    matches!(globals.tier, TierFilter::A | TierFilter::Both)
}

/// Return true iff Tier B groups should be emitted given `--tier`.
fn tier_allows_b(globals: &GlobalArgs) -> bool {
    matches!(globals.tier, TierFilter::B | TierFilter::Both)
}

/// Return true iff a Tier B group's language passes the `--lang` filter.
///
/// Tier A groups are language-oblivious and always pass — this helper is
/// only consulted for Tier B. An empty `--lang` accepts every language.
fn lang_allows(globals: &GlobalArgs, group: &MatchGroup) -> bool {
    if globals.lang.is_empty() {
        return true;
    }
    // Infer the language from the first occurrence's extension. All
    // occurrences in a Tier B group come from the same language profile,
    // so inspecting one is sufficient.
    let ext = group
        .occurrences
        .first()
        .and_then(|o| o.path.extension())
        .and_then(|s| s.to_str())
        .unwrap_or("");
    ext_matches_lang_filter(&globals.lang, ext)
}

/// Same shape as [`lang_allows`], but against a cached [`GroupDetail`].
/// Used by `dedup list` where we read persisted rows rather than live
/// [`MatchGroup`]s.
fn lang_allows_cached(globals: &GlobalArgs, detail: &GroupDetail) -> bool {
    if globals.lang.is_empty() {
        return true;
    }
    let ext = detail
        .occurrences
        .first()
        .and_then(|o| o.path.extension())
        .and_then(|s| s.to_str())
        .unwrap_or("");
    ext_matches_lang_filter(&globals.lang, ext)
}

fn ext_matches_lang_filter(filter: &[Language], ext: &str) -> bool {
    let actual = match ext {
        "rs" => Some(Language::Rust),
        "ts" | "tsx" => Some(Language::Ts),
        "py" => Some(Language::Python),
        _ => None,
    };
    match actual {
        Some(lang) => filter.contains(&lang),
        None => false,
    }
}

/// Compute whether ANSI color should be used on stderr. Used by the
/// spinner style; stdout color for groups lands when we add colored
/// output in #12.
fn color_enabled_for_stderr(globals: &GlobalArgs) -> bool {
    match globals.color {
        ColorMode::Always => true,
        ColorMode::Never => false,
        ColorMode::Auto => std::io::stderr().is_terminal(),
    }
}

/// Print a slice of [`MatchGroup`] references in the canonical dedup
/// text format. Taking `&[&MatchGroup]` rather than `&ScanResult`
/// mirrors the post-filter path so callers can drop out groups cheaply.
///
/// Each group header carries a `[A]` / `[B]` prefix so the tier is
/// visible at a glance. Occurrences keep the `path:start-end` shape
/// that downstream tools (e.g. `xargs -o nvim`) depend on.
fn print_scan_groups<W: Write>(groups: &[&MatchGroup], out: &mut W) -> std::io::Result<()> {
    for (i, group) in groups.iter().enumerate() {
        writeln!(
            out,
            "--- [{}] group {} ({} occurrences) ---",
            group.tier.label(),
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
fn print_cached_group_full<W: Write>(
    ordinal: usize,
    detail: &GroupDetail,
    out: &mut W,
) -> std::io::Result<()> {
    writeln!(
        out,
        "--- [{}] group {} ({} occurrences) ---",
        detail.tier.label(),
        ordinal,
        detail.occurrence_count
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
fn print_cached_group_show<W: Write>(detail: &GroupDetail, out: &mut W) -> std::io::Result<()> {
    writeln!(
        out,
        "--- [{}] group {} ({} occurrences) ---",
        detail.tier.label(),
        detail.id,
        detail.occurrence_count
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

// --- Progress sink --------------------------------------------------------

/// `indicatif`-backed progress sink. Owns a `ProgressBar` and some
/// interior-mutable counters that the scanner ticks on each callback.
///
/// Two counters are tracked:
///
/// - `files`: incremented once per `on_file_processed`.
/// - `groups`: incremented once per `on_match_group`.
///
/// The message is refreshed from these counters on each callback, which
/// is cheap (no IO happens until `indicatif` decides to redraw at its
/// configured 10 Hz steady-tick rate).
///
/// Dropping the sink calls `finish_and_clear` so the spinner disappears
/// before stdout is flushed.
struct IndicatifSink {
    bar: ProgressBar,
    files: std::sync::atomic::AtomicUsize,
    groups: std::sync::atomic::AtomicUsize,
}

impl IndicatifSink {
    fn new(color: bool) -> Self {
        let bar = ProgressBar::new_spinner();
        let template = if color {
            "{spinner:.cyan} {elapsed_precise} {msg}"
        } else {
            "{spinner} {elapsed_precise} {msg}"
        };
        bar.set_style(
            ProgressStyle::with_template(template)
                .expect("template")
                .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ "),
        );
        // ~10 Hz steady tick (per PRD).
        bar.enable_steady_tick(Duration::from_millis(100));
        bar.set_message("scanning…");
        Self {
            bar,
            files: std::sync::atomic::AtomicUsize::new(0),
            groups: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn refresh_message(&self) {
        use std::sync::atomic::Ordering;
        let files = self.files.load(Ordering::Relaxed);
        let groups = self.groups.load(Ordering::Relaxed);
        self.bar
            .set_message(format!("{files} files · {groups} groups"));
    }
}

impl ProgressSink for IndicatifSink {
    fn on_file_processed(&self, _path: &Path) {
        self.files
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.refresh_message();
    }

    fn on_match_group(&self, _group: &MatchGroup) {
        self.groups
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.refresh_message();
    }
}

impl Drop for IndicatifSink {
    fn drop(&mut self) {
        self.bar.finish_and_clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn clap_config_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn defaults_are_permissive() {
        let cli = Cli::parse_from(["dedup", "scan"]);
        assert!(matches!(cli.globals.tier, TierFilter::Both));
        assert!(matches!(cli.globals.color, ColorMode::Auto));
        assert!(!cli.globals.strict);
        assert!(!cli.globals.quiet);
        assert!(!cli.globals.verbose);
        assert!(cli.globals.lang.is_empty());
        assert_eq!(cli.globals.jobs, None);
        assert!(!cli.globals.no_gitignore);
    }

    #[test]
    fn tier_filter_parses() {
        let cli = Cli::parse_from(["dedup", "--tier", "a", "scan"]);
        assert!(matches!(cli.globals.tier, TierFilter::A));
        let cli = Cli::parse_from(["dedup", "--tier", "b", "scan"]);
        assert!(matches!(cli.globals.tier, TierFilter::B));
    }

    #[test]
    fn lang_accepts_comma_separated() {
        let cli = Cli::parse_from(["dedup", "--lang", "rust,ts,python", "scan"]);
        assert_eq!(
            cli.globals.lang,
            vec![Language::Rust, Language::Ts, Language::Python]
        );
    }

    #[test]
    fn verbose_and_quiet_are_mutually_exclusive() {
        let r = Cli::try_parse_from(["dedup", "-q", "-v", "scan"]);
        assert!(r.is_err());
    }

    #[test]
    fn short_flags_parse() {
        let cli = Cli::parse_from(["dedup", "-q", "scan"]);
        assert!(cli.globals.quiet);
        let cli = Cli::parse_from(["dedup", "-v", "scan"]);
        assert!(cli.globals.verbose);
    }

    #[test]
    fn color_never_disables_stderr_color() {
        let cli = Cli::parse_from(["dedup", "--color", "never", "scan"]);
        assert!(!color_enabled_for_stderr(&cli.globals));

        let cli = Cli::parse_from(["dedup", "--color", "always", "scan"]);
        assert!(color_enabled_for_stderr(&cli.globals));
    }

    #[test]
    fn tier_allows_a_matches_filter() {
        let cli = Cli::parse_from(["dedup", "--tier", "a", "scan"]);
        assert!(tier_allows_a(&cli.globals));
        let cli = Cli::parse_from(["dedup", "--tier", "both", "scan"]);
        assert!(tier_allows_a(&cli.globals));
        let cli = Cli::parse_from(["dedup", "--tier", "b", "scan"]);
        assert!(!tier_allows_a(&cli.globals));
    }

    #[test]
    fn unknown_flag_errors() {
        let r = Cli::try_parse_from(["dedup", "scan", "--not-a-flag"]);
        assert!(r.is_err());
        assert_ne!(
            r.unwrap_err().kind(),
            clap::error::ErrorKind::DisplayHelp,
            "unknown flag should not be help"
        );
    }

    #[test]
    fn config_subcommand_parses() {
        let cli = Cli::parse_from(["dedup", "config", "path"]);
        assert!(matches!(
            cli.command,
            Command::Config {
                action: ConfigAction::Path { .. }
            }
        ));
        let cli = Cli::parse_from(["dedup", "config", "edit"]);
        assert!(matches!(
            cli.command,
            Command::Config {
                action: ConfigAction::Edit { .. }
            }
        ));
    }
}
