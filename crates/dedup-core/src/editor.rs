//! Editor launcher: a **pure** `(preset, targets) -> Vec<CommandSpec>`
//! builder, a thin `std::process` spawn wrapper, and a missing-editor
//! fallback chain (issue #29).
//!
//! This module is GPUI-free and frontend-agnostic on purpose — the CLI
//! might eventually grow a "reveal in editor" flow too. The GUI owns the
//! small amount of GPUI glue (dialog + banner); everything else lives
//! here.
//!
//! ## Preset table
//!
//! | preset      | cmd template                    | terminal |
//! |-------------|---------------------------------|----------|
//! | `nvim`      | `nvim +{line} {file}` (default) | auto     |
//! | `vim`       | `vim +{line} {file}`            | auto     |
//! | `helix`     | `hx {file}:{line}`              | auto     |
//! | `emacs`     | `emacs -nw {file}`              | auto     |
//! | `code`      | `code -g {file}:{line}`         | none     |
//! | `cursor`    | `cursor -g {file}:{line}`       | none     |
//! | `zed`       | `zed {file}:{line}`             | none     |
//! | `sublime`   | `subl {file}:{line}`            | none     |
//! | `jetbrains` | `idea --line {line} {file}`     | none     |
//! | `custom`    | user-supplied template          | user     |
//!
//! ## Multi-file behaviour
//!
//! - **nvim / vim**: a single `CommandSpec` with a `-c` chain. The first
//!   file opens via `edit {path} | {line}`; each subsequent file opens
//!   in a new tab via `tabnew +{line} {path}`. Paths are POSIX
//!   single-quote escaped so embedded spaces / quotes / shell
//!   metacharacters don't break the command string.
//! - **helix / emacs**: one `CommandSpec` with every file as an argv
//!   entry (`hx` and `emacs` both accept multiple file arguments).
//! - **GUI editors** (`code` / `cursor` / `zed` / `sublime` /
//!   `jetbrains`): one `CommandSpec` **per file** so each invocation
//!   re-uses the already-running window.
//! - **custom**: one `CommandSpec` per file, substituting `{file}` and
//!   `{line}` into the user template.
//!
//! ## Terminal modes
//!
//! - `auto` — wrap each spec with `osascript` to open a macOS
//!   `Terminal.app` window running the command.
//! - `none` — run the command directly (GUI editors don't need a TTY).
//! - `custom` — substitute the spec's rendered argv into `{cmd}` inside
//!   `terminal_command`.
//!
//! ## Fallback chain
//!
//! [`resolve_preset`] looks up the preset's primary binary on `PATH`.
//! If the user picked `nvim` but `nvim` isn't installed, the config is
//! rewritten to `vim` and retried. If neither exists,
//! [`EditorError::NoEditor`] is returned so the GUI can surface the
//! "No editor found — run dedup config edit to pick one." toast.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

// -------------------------------------------------------------------------
// Public types
// -------------------------------------------------------------------------

/// One of the ten supported editor presets. The variant names line up
/// with the on-disk `[editor] preset = "..."` string (lowercase, per
/// serde `rename_all`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EditorPreset {
    /// `nvim +{line} {file}` — the default preset.
    #[default]
    Nvim,
    /// `vim +{line} {file}`.
    Vim,
    /// `hx {file}:{line}`.
    Helix,
    /// `emacs -nw {file}`.
    Emacs,
    /// `code -g {file}:{line}` (VS Code).
    Code,
    /// `cursor -g {file}:{line}`.
    Cursor,
    /// `zed {file}:{line}`.
    Zed,
    /// `subl {file}:{line}` (Sublime Text).
    Sublime,
    /// `idea --line {line} {file}` (IntelliJ / JetBrains toolbox).
    Jetbrains,
    /// User-supplied `command` + `terminal_command`. Substitution
    /// tokens: `{file}` and `{line}` in `command`, `{cmd}` in
    /// `terminal_command`.
    Custom,
}

impl EditorPreset {
    /// Parse a lowercase preset name from TOML / CLI input. Returns
    /// `None` for unknown names; the GUI falls back to `Nvim` in that
    /// case.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "nvim" => Self::Nvim,
            "vim" => Self::Vim,
            "helix" => Self::Helix,
            "emacs" => Self::Emacs,
            "code" => Self::Code,
            "cursor" => Self::Cursor,
            "zed" => Self::Zed,
            "sublime" => Self::Sublime,
            "jetbrains" => Self::Jetbrains,
            "custom" => Self::Custom,
            _ => return None,
        })
    }

    /// Canonical lowercase name (matches the TOML value).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Nvim => "nvim",
            Self::Vim => "vim",
            Self::Helix => "helix",
            Self::Emacs => "emacs",
            Self::Code => "code",
            Self::Cursor => "cursor",
            Self::Zed => "zed",
            Self::Sublime => "sublime",
            Self::Jetbrains => "jetbrains",
            Self::Custom => "custom",
        }
    }

    /// Primary binary name for PATH lookup (used by
    /// [`resolve_preset`]). `Custom` has no primary — the caller pulls
    /// it out of `command` instead.
    pub fn primary_binary(&self) -> Option<&'static str> {
        Some(match self {
            Self::Nvim => "nvim",
            Self::Vim => "vim",
            Self::Helix => "hx",
            Self::Emacs => "emacs",
            Self::Code => "code",
            Self::Cursor => "cursor",
            Self::Zed => "zed",
            Self::Sublime => "subl",
            Self::Jetbrains => "idea",
            Self::Custom => return None,
        })
    }

    /// Default terminal mode for the preset. Terminal-native editors
    /// want `Auto` (osascript wrap on macOS); GUI editors want `None`.
    pub fn default_terminal(&self) -> TerminalMode {
        match self {
            Self::Nvim | Self::Vim | Self::Helix | Self::Emacs => TerminalMode::Auto,
            Self::Code | Self::Cursor | Self::Zed | Self::Sublime | Self::Jetbrains => {
                TerminalMode::None
            }
            Self::Custom => TerminalMode::None,
        }
    }
}

impl fmt::Display for EditorPreset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Terminal handling for the spawned command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TerminalMode {
    /// Wrap with `osascript` to open a macOS `Terminal.app` window.
    #[default]
    Auto,
    /// Spawn directly without a TTY (GUI editors).
    None,
    /// Substitute the rendered argv into `{cmd}` inside
    /// `terminal_command` from the config.
    Custom,
}

impl TerminalMode {
    /// Parse a lowercase terminal mode from TOML. Returns `None` for
    /// unknown values.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "auto" => Self::Auto,
            "none" => Self::None,
            "custom" => Self::Custom,
            _ => return None,
        })
    }

    /// Canonical lowercase name.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::None => "none",
            Self::Custom => "custom",
        }
    }
}

/// On-disk / in-memory config for the editor launcher. Populated from
/// the `[editor]` section of `config.toml` (see
/// [`crate::config::Config`]).
///
/// `command` / `terminal` / `terminal_command` are `Option`s so a
/// named preset can leave them unset (they then resolve from the
/// preset's defaults). For `Custom`, `command` is required — an empty
/// `command` with `preset = "custom"` renders zero `CommandSpec`s.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EditorConfig {
    /// Which preset's template to use.
    pub preset: EditorPreset,
    /// Override the preset's `command` template. Required for
    /// [`EditorPreset::Custom`]. Substitution: `{file}`, `{line}`.
    pub command: Option<String>,
    /// Override the preset's terminal mode.
    pub terminal: Option<String>,
    /// For [`TerminalMode::Custom`]: a template with `{cmd}` that
    /// wraps the rendered argv. Example:
    /// `kitty --hold -e sh -c {cmd}`.
    pub terminal_command: Option<String>,
}

impl EditorConfig {
    /// Resolved terminal mode for the config: the explicit `terminal`
    /// field if set (and parseable), otherwise the preset default.
    pub fn resolved_terminal(&self) -> TerminalMode {
        self.terminal
            .as_deref()
            .and_then(TerminalMode::parse)
            .unwrap_or_else(|| self.preset.default_terminal())
    }
}

/// One spawnable command: program + argv, ready for
/// `std::process::Command::new(program).args(args).spawn()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    /// Executable name or absolute path.
    pub program: String,
    /// Argv (excluding argv[0]).
    pub args: Vec<String>,
}

impl CommandSpec {
    /// New spec from a program + argv.
    pub fn new(program: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            program: program.into(),
            args,
        }
    }
}

/// Errors surfaced by [`resolve_preset`] / [`launch`].
#[derive(Debug)]
pub enum EditorError {
    /// Neither the requested preset's binary *nor* the `vim`
    /// fallback is on `PATH`. The GUI surfaces this as the
    /// "No editor found — run dedup config edit to pick one." toast.
    NoEditor,
    /// A [`EditorPreset::Custom`] config is missing its `command`
    /// template.
    CustomWithoutCommand,
    /// A [`TerminalMode::Custom`] config is missing
    /// `terminal_command`.
    CustomTerminalWithoutTemplate,
    /// `spawn()` itself failed for a resolved command.
    Spawn {
        program: String,
        source: std::io::Error,
    },
}

impl fmt::Display for EditorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoEditor => f.write_str("No editor found — run dedup config edit to pick one."),
            Self::CustomWithoutCommand => {
                f.write_str("editor preset `custom` requires `command` to be set")
            }
            Self::CustomTerminalWithoutTemplate => {
                f.write_str("terminal mode `custom` requires `terminal_command` to be set")
            }
            Self::Spawn { program, source } => {
                write!(f, "failed to spawn editor `{program}`: {source}")
            }
        }
    }
}

impl std::error::Error for EditorError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Spawn { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Outcome of [`resolve_preset`]: the preset after fallback + whether
/// a rewrite happened. Callers that want to persist the rewrite do so
/// via the regular config-save flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedEditor {
    /// The preset actually used after any fallback.
    pub preset: EditorPreset,
    /// `true` if the requested preset's binary was missing and we
    /// fell back (today only `Nvim → Vim`).
    pub fell_back: bool,
}

// -------------------------------------------------------------------------
// Pure builder
// -------------------------------------------------------------------------

/// Build the list of [`CommandSpec`]s to spawn for `targets`.
///
/// Pure: no filesystem, no network, no env reads. Every preset is
/// testable against a fixed `(config, targets)` pair.
///
/// `targets` is a slice of `(file, line)` pairs — one per occurrence
/// the user wants to open.
pub fn build_commands(cfg: &EditorConfig, targets: &[(PathBuf, u32)]) -> Vec<CommandSpec> {
    if targets.is_empty() {
        return Vec::new();
    }

    let specs = match cfg.preset {
        EditorPreset::Nvim => build_nvim_like("nvim", targets),
        EditorPreset::Vim => build_nvim_like("vim", targets),
        EditorPreset::Helix => build_helix(targets),
        EditorPreset::Emacs => build_emacs(targets),
        EditorPreset::Code => build_gui_per_file("code", &["-g"], ColonLine, targets),
        EditorPreset::Cursor => build_gui_per_file("cursor", &["-g"], ColonLine, targets),
        EditorPreset::Zed => build_gui_per_file("zed", &[], ColonLine, targets),
        EditorPreset::Sublime => build_gui_per_file("subl", &[], ColonLine, targets),
        EditorPreset::Jetbrains => build_jetbrains(targets),
        EditorPreset::Custom => match cfg.command.as_deref() {
            Some(tmpl) if !tmpl.is_empty() => build_custom(tmpl, targets),
            _ => return Vec::new(),
        },
    };

    // Apply terminal wrapping per the resolved mode.
    let term = cfg.resolved_terminal();
    match term {
        TerminalMode::None => specs,
        TerminalMode::Auto => specs.into_iter().map(wrap_osascript).collect(),
        TerminalMode::Custom => match cfg.terminal_command.as_deref() {
            Some(tmpl) if !tmpl.is_empty() => specs
                .into_iter()
                .map(|s| wrap_terminal_custom(&s, tmpl))
                .collect(),
            _ => specs, // no template — pass through rather than drop.
        },
    }
}

/// Strategy marker for the file-positioning syntax of GUI editors.
/// `code` / `cursor` / `zed` / `subl` all use `{file}:{line}`.
#[derive(Clone, Copy)]
struct ColonLine;

fn build_nvim_like(program: &str, targets: &[(PathBuf, u32)]) -> Vec<CommandSpec> {
    // Single file → `nvim +{line} {file}`. This matches the preset
    // table exactly and keeps the common case readable on the CLI.
    if targets.len() == 1 {
        let (path, line) = &targets[0];
        return vec![CommandSpec::new(
            program,
            vec![format!("+{line}"), path.to_string_lossy().into_owned()],
        )];
    }
    // Multi-file → one `-c` chain.
    //
    // The first file opens via `edit {path} | {line}` (load the buffer,
    // then jump to the line). Each subsequent file opens in a new tab
    // via `tabnew +{line} {path}`. Paths are POSIX single-quote
    // escaped so embedded spaces / quotes / metacharacters don't break
    // the `-c` argument.
    let mut args: Vec<String> = Vec::with_capacity(targets.len() * 2);
    for (i, (path, line)) in targets.iter().enumerate() {
        let escaped = posix_single_quote(&path.to_string_lossy());
        let cmd = if i == 0 {
            format!("edit {escaped} | {line}")
        } else {
            format!("tabnew +{line} {escaped}")
        };
        args.push("-c".to_string());
        args.push(cmd);
    }
    vec![CommandSpec::new(program, args)]
}

fn build_helix(targets: &[(PathBuf, u32)]) -> Vec<CommandSpec> {
    // `hx` accepts multiple `file:line` positional args — one spec.
    let args: Vec<String> = targets
        .iter()
        .map(|(p, l)| format!("{}:{}", p.display(), l))
        .collect();
    vec![CommandSpec::new("hx", args)]
}

fn build_emacs(targets: &[(PathBuf, u32)]) -> Vec<CommandSpec> {
    // `emacs -nw` with N files. Emacs doesn't take a `+line` flag
    // uniformly across versions when multiple files are specified, so
    // we drop line info for the multi-file case. Single file gets
    // `+line` in front (Emacs's documented CLI).
    let mut args: Vec<String> = vec!["-nw".to_string()];
    if targets.len() == 1 {
        let (path, line) = &targets[0];
        args.push(format!("+{line}"));
        args.push(path.to_string_lossy().into_owned());
    } else {
        for (path, _line) in targets {
            args.push(path.to_string_lossy().into_owned());
        }
    }
    vec![CommandSpec::new("emacs", args)]
}

fn build_gui_per_file(
    program: &str,
    pre_args: &[&str],
    _strategy: ColonLine,
    targets: &[(PathBuf, u32)],
) -> Vec<CommandSpec> {
    // GUI editors: one invocation per file so each call re-uses the
    // editor's own window (per AC).
    targets
        .iter()
        .map(|(path, line)| {
            let mut args: Vec<String> = pre_args.iter().map(|s| s.to_string()).collect();
            args.push(format!("{}:{}", path.display(), line));
            CommandSpec::new(program, args)
        })
        .collect()
}

fn build_jetbrains(targets: &[(PathBuf, u32)]) -> Vec<CommandSpec> {
    // `idea --line {line} {file}` — one per file.
    targets
        .iter()
        .map(|(path, line)| {
            CommandSpec::new(
                "idea",
                vec![
                    "--line".to_string(),
                    line.to_string(),
                    path.to_string_lossy().into_owned(),
                ],
            )
        })
        .collect()
}

fn build_custom(template: &str, targets: &[(PathBuf, u32)]) -> Vec<CommandSpec> {
    // Parse the template as `program arg1 arg2 ...` — splitting on
    // ASCII whitespace. This is deliberately simple: users who need
    // quoted args with spaces can reach for [`TerminalMode::Custom`]
    // instead. One spec per file (simplest and most predictable).
    targets
        .iter()
        .map(|(path, line)| {
            let rendered = substitute_file_line(template, path, *line);
            let mut parts = rendered.split_whitespace();
            let program = parts.next().unwrap_or("").to_string();
            let args = parts.map(|s| s.to_string()).collect();
            CommandSpec::new(program, args)
        })
        .collect()
}

fn substitute_file_line(tmpl: &str, file: &Path, line: u32) -> String {
    tmpl.replace("{file}", &file.to_string_lossy())
        .replace("{line}", &line.to_string())
}

// -------------------------------------------------------------------------
// Terminal wrapping
// -------------------------------------------------------------------------

fn render_argv(spec: &CommandSpec) -> String {
    // Render `program arg1 arg2 ...` with POSIX single-quote escaping
    // so a downstream `sh -c` (or `Terminal.app do script`) sees the
    // right tokenization even when paths contain spaces / quotes.
    let mut out = posix_single_quote(&spec.program);
    for a in &spec.args {
        out.push(' ');
        out.push_str(&posix_single_quote(a));
    }
    out
}

fn wrap_osascript(spec: CommandSpec) -> CommandSpec {
    // `osascript -e 'tell application "Terminal" to do script "<cmd>"'`
    //
    // AppleScript strings are double-quoted, so any `"` or `\` in the
    // rendered command string has to be backslash-escaped.
    let rendered = render_argv(&spec);
    let applescript_str = applescript_escape(&rendered);
    let script = format!("tell application \"Terminal\" to do script \"{applescript_str}\"");
    CommandSpec::new("osascript", vec!["-e".to_string(), script])
}

fn wrap_terminal_custom(spec: &CommandSpec, template: &str) -> CommandSpec {
    // Substitute `{cmd}` with the POSIX-escaped rendered argv string,
    // then split the result on whitespace for argv. This mirrors the
    // `Custom` preset builder — good enough for `kitty --hold -e sh
    // -c {cmd}`-style templates.
    let rendered = render_argv(spec);
    let substituted = template.replace("{cmd}", &rendered);
    let mut parts = substituted.split_whitespace();
    let program = parts.next().unwrap_or("").to_string();
    let args = parts.map(|s| s.to_string()).collect();
    CommandSpec::new(program, args)
}

// -------------------------------------------------------------------------
// Shell / AppleScript escaping
// -------------------------------------------------------------------------

/// POSIX single-quote escape: wrap in `'...'` and replace embedded
/// `'` with `'\''` (close, escape literal, reopen). This is the
/// gold-standard minimal escape for `sh`-family shells.
pub fn posix_single_quote(s: &str) -> String {
    // Fast path: empty string → `''`.
    if s.is_empty() {
        return "''".to_string();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            // Close, literal `'`, reopen.
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// AppleScript string-literal escape: backslash-escape `"` and `\`.
/// Everything else (including single quotes and shell metacharacters)
/// passes through literally because AppleScript strings are
/// double-quoted.
fn applescript_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            _ => out.push(c),
        }
    }
    out
}

// -------------------------------------------------------------------------
// Fallback resolution + spawn
// -------------------------------------------------------------------------

/// Trait for PATH lookups. Production uses [`PathLookup::default`]
/// which scans `$PATH`; tests inject a mock that returns a fixed set
/// of "present" binaries.
pub trait PathLookup {
    /// `true` iff `name` is on `PATH` as an executable.
    fn has(&self, name: &str) -> bool;
}

/// Default [`PathLookup`] that scans `$PATH`.
#[derive(Debug, Default, Clone, Copy)]
pub struct EnvPathLookup;

impl PathLookup for EnvPathLookup {
    fn has(&self, name: &str) -> bool {
        let Some(path) = std::env::var_os("PATH") else {
            return false;
        };
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(name);
            if is_executable(&candidate) {
                return true;
            }
        }
        false
    }
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(p) {
        Ok(m) => m.is_file() && (m.permissions().mode() & 0o111 != 0),
        Err(_) => false,
    }
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    p.is_file()
}

/// Resolve `cfg.preset` to a usable editor. If the preset is
/// [`EditorPreset::Nvim`] and `nvim` isn't on `PATH`, fall back to
/// `vim`. If neither exists (or a non-nvim preset's binary is
/// missing), returns [`EditorError::NoEditor`].
pub fn resolve_preset<P: PathLookup>(
    cfg: &EditorConfig,
    lookup: &P,
) -> Result<ResolvedEditor, EditorError> {
    match cfg.preset {
        EditorPreset::Nvim => {
            if lookup.has("nvim") {
                Ok(ResolvedEditor {
                    preset: EditorPreset::Nvim,
                    fell_back: false,
                })
            } else if lookup.has("vim") {
                Ok(ResolvedEditor {
                    preset: EditorPreset::Vim,
                    fell_back: true,
                })
            } else {
                Err(EditorError::NoEditor)
            }
        }
        EditorPreset::Custom => {
            // Pull the primary program out of the custom command
            // template — first whitespace-delimited token. If it's
            // empty or missing, the config is unusable.
            let cmd = cfg
                .command
                .as_deref()
                .ok_or(EditorError::CustomWithoutCommand)?;
            let program = cmd.split_whitespace().next().unwrap_or("");
            if program.is_empty() {
                Err(EditorError::CustomWithoutCommand)
            } else if lookup.has(program) || Path::new(program).is_absolute() {
                Ok(ResolvedEditor {
                    preset: EditorPreset::Custom,
                    fell_back: false,
                })
            } else {
                Err(EditorError::NoEditor)
            }
        }
        other => {
            let bin = other
                .primary_binary()
                .expect("non-custom preset has primary");
            if lookup.has(bin) {
                Ok(ResolvedEditor {
                    preset: other,
                    fell_back: false,
                })
            } else {
                Err(EditorError::NoEditor)
            }
        }
    }
}

/// Spawn the commands built from `(cfg, targets)`, after resolving
/// the preset (with fallback). On any spawn error, returns
/// [`EditorError::Spawn`] with the program name and the underlying
/// I/O error.
///
/// Each [`CommandSpec`] is spawned with `spawn()` (not `status()`) —
/// we don't block the GUI on the child process.
pub fn launch<P: PathLookup>(
    cfg: &EditorConfig,
    targets: &[(PathBuf, u32)],
    lookup: &P,
) -> Result<ResolvedEditor, EditorError> {
    let resolved = resolve_preset(cfg, lookup)?;
    // If we fell back from nvim → vim, build using the fallback
    // preset rather than the stored one. Same terminal mode /
    // `command` overrides carry over.
    let effective = if resolved.fell_back {
        EditorConfig {
            preset: resolved.preset,
            ..cfg.clone()
        }
    } else {
        cfg.clone()
    };
    let specs = build_commands(&effective, targets);
    for spec in specs {
        let mut cmd = std::process::Command::new(&spec.program);
        cmd.args(&spec.args);
        cmd.spawn().map_err(|e| EditorError::Spawn {
            program: spec.program.clone(),
            source: e,
        })?;
    }
    Ok(resolved)
}

// -------------------------------------------------------------------------
// Tests
// -------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    fn cfg(preset: EditorPreset) -> EditorConfig {
        // Default terminal = None so the bare-builder tests don't
        // have to strip the osascript wrapper. The separate terminal
        // tests exercise the Auto / Custom paths explicitly.
        EditorConfig {
            preset,
            command: None,
            terminal: Some("none".to_string()),
            terminal_command: None,
        }
    }

    // ---------------------------------------------------------------
    // Single-file builds — every preset renders correctly.
    // ---------------------------------------------------------------

    #[test]
    fn nvim_single_file_uses_plus_line_syntax() {
        let specs = build_commands(&cfg(EditorPreset::Nvim), &[(p("src/a.rs"), 42)]);
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].program, "nvim");
        assert_eq!(
            specs[0].args,
            vec!["+42".to_string(), "src/a.rs".to_string()]
        );
    }

    #[test]
    fn vim_single_file_uses_plus_line_syntax() {
        let specs = build_commands(&cfg(EditorPreset::Vim), &[(p("x.py"), 7)]);
        assert_eq!(specs[0].program, "vim");
        assert_eq!(specs[0].args, vec!["+7".to_string(), "x.py".to_string()]);
    }

    #[test]
    fn helix_single_file_uses_colon_line_syntax() {
        let specs = build_commands(&cfg(EditorPreset::Helix), &[(p("a.rs"), 10)]);
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].program, "hx");
        assert_eq!(specs[0].args, vec!["a.rs:10".to_string()]);
    }

    #[test]
    fn emacs_single_file_uses_plus_line_and_nw() {
        let specs = build_commands(&cfg(EditorPreset::Emacs), &[(p("a.el"), 5)]);
        assert_eq!(specs[0].program, "emacs");
        assert_eq!(
            specs[0].args,
            vec!["-nw".to_string(), "+5".to_string(), "a.el".to_string()]
        );
    }

    #[test]
    fn code_single_file_uses_dash_g() {
        let specs = build_commands(&cfg(EditorPreset::Code), &[(p("a.ts"), 3)]);
        assert_eq!(specs[0].program, "code");
        assert_eq!(specs[0].args, vec!["-g".to_string(), "a.ts:3".to_string()]);
    }

    #[test]
    fn cursor_single_file_uses_dash_g() {
        let specs = build_commands(&cfg(EditorPreset::Cursor), &[(p("a.ts"), 3)]);
        assert_eq!(specs[0].program, "cursor");
        assert_eq!(specs[0].args, vec!["-g".to_string(), "a.ts:3".to_string()]);
    }

    #[test]
    fn zed_single_file_uses_colon_line_syntax() {
        let specs = build_commands(&cfg(EditorPreset::Zed), &[(p("a.rs"), 9)]);
        assert_eq!(specs[0].program, "zed");
        assert_eq!(specs[0].args, vec!["a.rs:9".to_string()]);
    }

    #[test]
    fn sublime_single_file_uses_subl_binary() {
        let specs = build_commands(&cfg(EditorPreset::Sublime), &[(p("a.rs"), 12)]);
        assert_eq!(specs[0].program, "subl");
        assert_eq!(specs[0].args, vec!["a.rs:12".to_string()]);
    }

    #[test]
    fn jetbrains_single_file_uses_dash_dash_line() {
        let specs = build_commands(&cfg(EditorPreset::Jetbrains), &[(p("Foo.java"), 88)]);
        assert_eq!(specs[0].program, "idea");
        assert_eq!(
            specs[0].args,
            vec![
                "--line".to_string(),
                "88".to_string(),
                "Foo.java".to_string()
            ]
        );
    }

    #[test]
    fn custom_preset_substitutes_file_and_line_tokens() {
        let c = EditorConfig {
            preset: EditorPreset::Custom,
            command: Some("micro {file}:{line}".to_string()),
            terminal: Some("none".to_string()),
            terminal_command: None,
        };
        let specs = build_commands(&c, &[(p("a.rs"), 7)]);
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].program, "micro");
        assert_eq!(specs[0].args, vec!["a.rs:7".to_string()]);
    }

    #[test]
    fn custom_preset_without_command_produces_no_specs() {
        let c = EditorConfig {
            preset: EditorPreset::Custom,
            command: None,
            terminal: Some("none".to_string()),
            terminal_command: None,
        };
        assert!(build_commands(&c, &[(p("a.rs"), 1)]).is_empty());
    }

    #[test]
    fn empty_targets_produces_no_specs() {
        assert!(build_commands(&cfg(EditorPreset::Nvim), &[]).is_empty());
    }

    // ---------------------------------------------------------------
    // Multi-file builds — nvim/vim chain; GUI editors one-per-file.
    // ---------------------------------------------------------------

    #[test]
    fn nvim_multi_file_builds_one_spec_with_c_chain() {
        let specs = build_commands(
            &cfg(EditorPreset::Nvim),
            &[(p("a.rs"), 10), (p("b.rs"), 20), (p("c.rs"), 30)],
        );
        assert_eq!(specs.len(), 1, "nvim multi-file must be a single spec");
        assert_eq!(specs[0].program, "nvim");
        assert_eq!(
            specs[0].args,
            vec![
                "-c".to_string(),
                "edit 'a.rs' | 10".to_string(),
                "-c".to_string(),
                "tabnew +20 'b.rs'".to_string(),
                "-c".to_string(),
                "tabnew +30 'c.rs'".to_string(),
            ]
        );
    }

    #[test]
    fn vim_multi_file_builds_one_spec_with_c_chain() {
        let specs = build_commands(&cfg(EditorPreset::Vim), &[(p("a.rs"), 10), (p("b.rs"), 20)]);
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].program, "vim");
        assert_eq!(
            specs[0].args,
            vec![
                "-c".to_string(),
                "edit 'a.rs' | 10".to_string(),
                "-c".to_string(),
                "tabnew +20 'b.rs'".to_string(),
            ]
        );
    }

    #[test]
    fn nvim_multi_file_shell_escapes_paths_with_spaces_and_quotes() {
        let specs = build_commands(
            &cfg(EditorPreset::Nvim),
            &[(p("with space.rs"), 1), (p("it's.rs"), 2)],
        );
        assert_eq!(specs[0].args[1], "edit 'with space.rs' | 1");
        // "it's.rs" — embedded single quote: `'it'\''s.rs'`.
        assert_eq!(specs[0].args[3], "tabnew +2 'it'\\''s.rs'");
    }

    #[test]
    fn helix_multi_file_is_one_spec_with_multiple_args() {
        let specs = build_commands(&cfg(EditorPreset::Helix), &[(p("a.rs"), 1), (p("b.rs"), 2)]);
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].program, "hx");
        assert_eq!(
            specs[0].args,
            vec!["a.rs:1".to_string(), "b.rs:2".to_string()]
        );
    }

    #[test]
    fn emacs_multi_file_is_one_spec_dropping_line_info() {
        let specs = build_commands(&cfg(EditorPreset::Emacs), &[(p("a.el"), 1), (p("b.el"), 2)]);
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].program, "emacs");
        assert_eq!(
            specs[0].args,
            vec!["-nw".to_string(), "a.el".to_string(), "b.el".to_string()]
        );
    }

    #[test]
    fn code_multi_file_produces_one_spec_per_file() {
        let specs = build_commands(&cfg(EditorPreset::Code), &[(p("a.ts"), 3), (p("b.ts"), 7)]);
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].args, vec!["-g".to_string(), "a.ts:3".to_string()]);
        assert_eq!(specs[1].args, vec!["-g".to_string(), "b.ts:7".to_string()]);
    }

    #[test]
    fn cursor_multi_file_produces_one_spec_per_file() {
        let specs = build_commands(
            &cfg(EditorPreset::Cursor),
            &[(p("a.ts"), 1), (p("b.ts"), 2)],
        );
        assert_eq!(specs.len(), 2);
    }

    #[test]
    fn zed_multi_file_produces_one_spec_per_file() {
        let specs = build_commands(&cfg(EditorPreset::Zed), &[(p("a.rs"), 1), (p("b.rs"), 2)]);
        assert_eq!(specs.len(), 2);
    }

    #[test]
    fn sublime_multi_file_produces_one_spec_per_file() {
        let specs = build_commands(
            &cfg(EditorPreset::Sublime),
            &[(p("a.rs"), 1), (p("b.rs"), 2)],
        );
        assert_eq!(specs.len(), 2);
    }

    #[test]
    fn jetbrains_multi_file_produces_one_spec_per_file() {
        let specs = build_commands(
            &cfg(EditorPreset::Jetbrains),
            &[(p("a.java"), 1), (p("b.java"), 2)],
        );
        assert_eq!(specs.len(), 2);
        assert_eq!(
            specs[1].args,
            vec!["--line".to_string(), "2".to_string(), "b.java".to_string()]
        );
    }

    // ---------------------------------------------------------------
    // Terminal modes.
    // ---------------------------------------------------------------

    #[test]
    fn terminal_auto_wraps_in_osascript() {
        let mut c = cfg(EditorPreset::Nvim);
        c.terminal = Some("auto".to_string());
        let specs = build_commands(&c, &[(p("a.rs"), 7)]);
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].program, "osascript");
        assert_eq!(specs[0].args.len(), 2);
        assert_eq!(specs[0].args[0], "-e");
        // AppleScript string contains the escaped inner argv.
        assert!(
            specs[0].args[1].contains("tell application \"Terminal\" to do script"),
            "osascript `-e` must be a `tell application Terminal` form; got {:?}",
            specs[0].args[1]
        );
        assert!(specs[0].args[1].contains("'nvim'"));
        assert!(specs[0].args[1].contains("+7"));
    }

    #[test]
    fn terminal_auto_escapes_embedded_double_quotes_for_applescript() {
        // A path with a double quote in it would break the AppleScript
        // string literal unless we backslash-escape it.
        let c = EditorConfig {
            preset: EditorPreset::Custom,
            // A fake "editor" whose name contains `"` so the rendered
            // command includes a literal quote — the AppleScript
            // wrapper must escape it as `\"`.
            command: Some("weird\"ed {file}".to_string()),
            terminal: Some("auto".to_string()),
            terminal_command: None,
        };
        let specs = build_commands(&c, &[(p("a.rs"), 1)]);
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].program, "osascript");
        // AppleScript escape of `"` is `\"` (literal backslash + quote).
        assert!(
            specs[0].args[1].contains("\\\""),
            "expected AppleScript-escaped double quotes in {:?}",
            specs[0].args[1]
        );
    }

    #[test]
    fn terminal_none_passes_through() {
        let specs = build_commands(&cfg(EditorPreset::Nvim), &[(p("a.rs"), 1)]);
        assert_eq!(specs[0].program, "nvim");
    }

    #[test]
    fn terminal_custom_substitutes_cmd_placeholder() {
        let c = EditorConfig {
            preset: EditorPreset::Nvim,
            command: None,
            terminal: Some("custom".to_string()),
            terminal_command: Some("kitty -e {cmd}".to_string()),
        };
        let specs = build_commands(&c, &[(p("a.rs"), 1)]);
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].program, "kitty");
        // After substitution and re-split:
        //   kitty -e 'nvim' '+1' 'a.rs'
        // The re-split drops the outer quotes since we're using plain
        // split_whitespace — that's fine for this template shape.
        assert_eq!(specs[0].args[0], "-e");
        assert!(specs[0].args.iter().any(|a| a.contains("nvim")));
    }

    #[test]
    fn terminal_custom_without_template_passes_through() {
        let c = EditorConfig {
            preset: EditorPreset::Nvim,
            command: None,
            terminal: Some("custom".to_string()),
            terminal_command: None,
        };
        let specs = build_commands(&c, &[(p("a.rs"), 1)]);
        // No template → pass through bare `nvim` spec rather than
        // silently dropping.
        assert_eq!(specs[0].program, "nvim");
    }

    #[test]
    fn default_terminal_for_nvim_is_auto() {
        let mut c = cfg(EditorPreset::Nvim);
        c.terminal = None;
        let specs = build_commands(&c, &[(p("a.rs"), 1)]);
        assert_eq!(specs[0].program, "osascript");
    }

    #[test]
    fn default_terminal_for_code_is_none() {
        let mut c = cfg(EditorPreset::Code);
        c.terminal = None;
        let specs = build_commands(&c, &[(p("a.rs"), 1)]);
        assert_eq!(specs[0].program, "code");
    }

    // ---------------------------------------------------------------
    // Shell-escape edge cases.
    // ---------------------------------------------------------------

    #[test]
    fn posix_single_quote_wraps_plain_string() {
        assert_eq!(posix_single_quote("foo"), "'foo'");
    }

    #[test]
    fn posix_single_quote_handles_spaces() {
        assert_eq!(posix_single_quote("foo bar"), "'foo bar'");
    }

    #[test]
    fn posix_single_quote_escapes_embedded_single_quote() {
        assert_eq!(posix_single_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn posix_single_quote_passes_dollar_and_backtick_literally() {
        // `$` and `` ` `` are only dangerous inside double-quoted
        // strings. Inside single quotes they're literal — no escape
        // needed, which is exactly why we use single quotes.
        assert_eq!(posix_single_quote("$(rm -rf /)"), "'$(rm -rf /)'");
        assert_eq!(posix_single_quote("`x`"), "'`x`'");
    }

    #[test]
    fn posix_single_quote_empty_string() {
        assert_eq!(posix_single_quote(""), "''");
    }

    // ---------------------------------------------------------------
    // Fallback resolution.
    // ---------------------------------------------------------------

    struct FakePath(Vec<&'static str>);

    impl PathLookup for FakePath {
        fn has(&self, name: &str) -> bool {
            self.0.contains(&name)
        }
    }

    #[test]
    fn resolve_nvim_present_returns_nvim() {
        let lookup = FakePath(vec!["nvim", "vim"]);
        let r = resolve_preset(&cfg(EditorPreset::Nvim), &lookup).unwrap();
        assert_eq!(r.preset, EditorPreset::Nvim);
        assert!(!r.fell_back);
    }

    #[test]
    fn resolve_nvim_missing_falls_back_to_vim() {
        let lookup = FakePath(vec!["vim"]);
        let r = resolve_preset(&cfg(EditorPreset::Nvim), &lookup).unwrap();
        assert_eq!(r.preset, EditorPreset::Vim);
        assert!(r.fell_back);
    }

    #[test]
    fn resolve_nvim_and_vim_missing_errors_no_editor() {
        let lookup = FakePath(vec![]);
        let err = resolve_preset(&cfg(EditorPreset::Nvim), &lookup).unwrap_err();
        assert!(matches!(err, EditorError::NoEditor));
    }

    #[test]
    fn resolve_non_nvim_missing_errors_no_editor() {
        let lookup = FakePath(vec![]);
        let err = resolve_preset(&cfg(EditorPreset::Code), &lookup).unwrap_err();
        assert!(matches!(err, EditorError::NoEditor));
    }

    #[test]
    fn resolve_custom_uses_first_word_of_command() {
        let c = EditorConfig {
            preset: EditorPreset::Custom,
            command: Some("micro {file}".to_string()),
            terminal: Some("none".to_string()),
            terminal_command: None,
        };
        let lookup = FakePath(vec!["micro"]);
        let r = resolve_preset(&c, &lookup).unwrap();
        assert_eq!(r.preset, EditorPreset::Custom);
    }

    #[test]
    fn resolve_custom_without_command_errors() {
        let c = EditorConfig {
            preset: EditorPreset::Custom,
            command: None,
            terminal: Some("none".to_string()),
            terminal_command: None,
        };
        let lookup = FakePath(vec![]);
        let err = resolve_preset(&c, &lookup).unwrap_err();
        assert!(matches!(err, EditorError::CustomWithoutCommand));
    }

    // ---------------------------------------------------------------
    // Config serialization.
    // ---------------------------------------------------------------

    #[test]
    fn editor_config_round_trips_through_toml() {
        let c = EditorConfig {
            preset: EditorPreset::Code,
            command: Some("code -g {file}:{line}".to_string()),
            terminal: Some("none".to_string()),
            terminal_command: None,
        };
        let rendered = toml::to_string(&c).unwrap();
        let parsed: EditorConfig = toml::from_str(&rendered).unwrap();
        assert_eq!(c, parsed);
    }

    #[test]
    fn editor_preset_parses_all_known_values() {
        for name in [
            "nvim",
            "vim",
            "helix",
            "emacs",
            "code",
            "cursor",
            "zed",
            "sublime",
            "jetbrains",
            "custom",
        ] {
            let p = EditorPreset::parse(name).unwrap();
            assert_eq!(p.as_str(), name);
        }
    }

    #[test]
    fn editor_preset_rejects_unknown_value() {
        assert!(EditorPreset::parse("notepad").is_none());
    }
}
