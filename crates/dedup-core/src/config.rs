//! Layered TOML configuration for the dedup CLI and library.
//!
//! Resolution order (lowest precedence first):
//!
//! 1. Baked-in defaults ([`Config::default`]).
//! 2. Global file at `~/.config/dedup/config.toml` (if present).
//! 3. Project file at `<repo_root>/.dedup/config.toml` (if present).
//!
//! Merging is field-wise: any field set in a higher layer overrides the
//! same field from a lower layer, nested tables flattened one level deep
//! (`[thresholds.tier_a].min_lines` is its own mergeable field).
//!
//! Files are **never** auto-created. The only place a fresh file ever
//! lands on disk is the CLI's `dedup config edit` subcommand — this
//! module is strictly a loader.
//!
//! Future schema bumps: on load, if a file's `schema_version` is greater
//! than [`SCHEMA_VERSION`], [`Config::load`] returns
//! [`ConfigError::SchemaVersionMismatch`] so callers (today the CLI,
//! later possibly a migrator per #17/#30) can decide whether to warn +
//! fall back to defaults or rename the file to `.bak`. At this milestone
//! the CLI just warns.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::editor::EditorConfig;

/// The config schema version this build understands. Absent
/// `schema_version` in a file defaults to 1.
pub const SCHEMA_VERSION: u32 = 1;

/// Relative path of the per-project config inside the repo's `.dedup/`
/// directory.
pub const PROJECT_CONFIG_DIR: &str = ".dedup";
/// File name of the config file inside [`PROJECT_CONFIG_DIR`] and inside
/// `~/.config/dedup/`.
pub const CONFIG_FILE: &str = "config.toml";
/// Directory inside the user's config home where the global config lives.
pub const GLOBAL_CONFIG_SUBDIR: &str = "dedup";

/// Errors that can surface while loading or resolving configuration.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// IO failure while reading a config file we already believed exists.
    #[error("config io error: {0}")]
    Io(#[from] std::io::Error),
    /// The file could not be parsed as valid TOML into our schema.
    #[error("failed to parse config at {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    /// The file declared a `schema_version` greater than [`SCHEMA_VERSION`].
    /// Callers decide whether to fall back to defaults.
    #[error(
        "config at {path} declares schema_version {found} which is newer than supported version {expected}"
    )]
    SchemaVersionMismatch {
        path: PathBuf,
        found: u32,
        expected: u32,
    },
    /// `dirs::home_dir()` returned `None`. Rare, but possible on exotic
    /// environments where there is no home concept.
    #[error("could not locate the current user's home directory")]
    HomeDirNotFound,
}

/// Tier A threshold tunables.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TierAThresholds {
    pub min_lines: usize,
    pub min_tokens: usize,
}

impl Default for TierAThresholds {
    fn default() -> Self {
        Self {
            min_lines: 6,
            min_tokens: 50,
        }
    }
}

/// Tier B threshold tunables. Tier B detection itself lands in #6; these
/// values load through the schema today so later wiring is a no-op.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TierBThresholds {
    pub min_lines: usize,
    pub min_tokens: usize,
}

impl Default for TierBThresholds {
    fn default() -> Self {
        Self {
            min_lines: 3,
            min_tokens: 15,
        }
    }
}

/// Grouped Tier A + Tier B thresholds.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Thresholds {
    pub tier_a: TierAThresholds,
    pub tier_b: TierBThresholds,
}

/// Normalization strategy. `"conservative"` (default) keeps literals
/// distinct. `"aggressive"` (wired in #10) abstracts string / numeric
/// literals for broader matching by rewriting every
/// [`dedup_lang::RenameClass::Literal`] leaf to a stable placeholder
/// (`<LIT>`) before hashing. Both modes alpha-rename locals; only the
/// treatment of literal leaves differs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Normalization {
    #[default]
    Conservative,
    Aggressive,
}

impl From<Normalization> for dedup_lang::NormalizationMode {
    fn from(n: Normalization) -> Self {
        match n {
            Normalization::Conservative => dedup_lang::NormalizationMode::Conservative,
            Normalization::Aggressive => dedup_lang::NormalizationMode::Aggressive,
        }
    }
}

/// Scanner-side knobs: bytes budget per file, symlink + submodule
/// policies. All wired straight through to `ScanConfig`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScanSettings {
    pub max_file_size: u64,
    pub follow_symlinks: bool,
    pub include_submodules: bool,
}

impl Default for ScanSettings {
    fn default() -> Self {
        Self {
            max_file_size: 1_048_576,
            follow_symlinks: false,
            include_submodules: false,
        }
    }
}

/// GUI detail-pane tunables (issue #26). Currently just the number of
/// before/after context lines shown around each duplicate range; further
/// knobs (line-wrap, highlight theme, …) can land here without growing
/// the top-level [`Config`] schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DetailConfig {
    /// Number of dimmed context lines to show above and below each
    /// duplicated range in the GUI detail pane. `0` disables context
    /// entirely. Default: 3.
    pub context_lines: usize,
}

impl Default for DetailConfig {
    fn default() -> Self {
        Self { context_lines: 3 }
    }
}

/// The resolved config — always populated from the layering rules
/// documented on [`Config::load`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub schema_version: u32,
    pub thresholds: Thresholds,
    pub normalization: Normalization,
    pub scan: ScanSettings,
    pub detail: DetailConfig,
    /// Editor launcher preset + terminal handling (issue #29).
    pub editor: EditorConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            thresholds: Thresholds::default(),
            normalization: Normalization::default(),
            scan: ScanSettings::default(),
            detail: DetailConfig::default(),
            editor: EditorConfig::default(),
        }
    }
}

/// Raw deserialization target: every field wrapped in `Option<_>` so we
/// can tell "unset" apart from "set to the default value" during the
/// merge. The on-disk file does not need to specify every field.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct PartialConfig {
    schema_version: Option<u32>,
    thresholds: Option<PartialThresholds>,
    normalization: Option<Normalization>,
    scan: Option<PartialScanSettings>,
    detail: Option<PartialDetailConfig>,
    editor: Option<PartialEditorConfig>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct PartialEditorConfig {
    preset: Option<crate::editor::EditorPreset>,
    command: Option<String>,
    terminal: Option<String>,
    terminal_command: Option<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct PartialDetailConfig {
    context_lines: Option<usize>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct PartialThresholds {
    tier_a: Option<PartialTierAThresholds>,
    tier_b: Option<PartialTierBThresholds>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct PartialTierAThresholds {
    min_lines: Option<usize>,
    min_tokens: Option<usize>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct PartialTierBThresholds {
    min_lines: Option<usize>,
    min_tokens: Option<usize>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct PartialScanSettings {
    max_file_size: Option<u64>,
    follow_symlinks: Option<bool>,
    include_submodules: Option<bool>,
}

impl PartialConfig {
    fn apply_to(self, base: &mut Config) {
        if let Some(v) = self.schema_version {
            base.schema_version = v;
        }
        if let Some(t) = self.thresholds {
            if let Some(a) = t.tier_a {
                if let Some(v) = a.min_lines {
                    base.thresholds.tier_a.min_lines = v;
                }
                if let Some(v) = a.min_tokens {
                    base.thresholds.tier_a.min_tokens = v;
                }
            }
            if let Some(b) = t.tier_b {
                if let Some(v) = b.min_lines {
                    base.thresholds.tier_b.min_lines = v;
                }
                if let Some(v) = b.min_tokens {
                    base.thresholds.tier_b.min_tokens = v;
                }
            }
        }
        if let Some(n) = self.normalization {
            base.normalization = n;
        }
        if let Some(s) = self.scan {
            if let Some(v) = s.max_file_size {
                base.scan.max_file_size = v;
            }
            if let Some(v) = s.follow_symlinks {
                base.scan.follow_symlinks = v;
            }
            if let Some(v) = s.include_submodules {
                base.scan.include_submodules = v;
            }
        }
        if let Some(d) = self.detail
            && let Some(v) = d.context_lines
        {
            base.detail.context_lines = v;
        }
        if let Some(e) = self.editor {
            if let Some(p) = e.preset {
                base.editor.preset = p;
            }
            if let Some(v) = e.command {
                base.editor.command = Some(v);
            }
            if let Some(v) = e.terminal {
                base.editor.terminal = Some(v);
            }
            if let Some(v) = e.terminal_command {
                base.editor.terminal_command = Some(v);
            }
        }
    }
}

impl Config {
    /// Resolve the global config path.
    ///
    /// Per the PRD (GitHub issue #1), dedup stores its global state under
    /// `~/.config/dedup/` on all platforms — this is literal and
    /// load-bearing across the codebase (tracing logs and other future
    /// features follow the same convention). On Unix we honor
    /// `$XDG_CONFIG_HOME` when set, else fall back to
    /// `$HOME/.config/dedup/config.toml`. On Windows (not in the CI
    /// matrix) we fall back to `dirs::config_dir()` for lack of a sane
    /// `~/.config` equivalent.
    pub fn global_path() -> PathBuf {
        global_config_dir()
            .unwrap_or_else(|| PathBuf::from(""))
            .join(GLOBAL_CONFIG_SUBDIR)
            .join(CONFIG_FILE)
    }

    /// Resolve the project config path for `repo_root`:
    /// `<repo_root>/.dedup/config.toml`.
    pub fn project_path(repo_root: &Path) -> PathBuf {
        repo_root.join(PROJECT_CONFIG_DIR).join(CONFIG_FILE)
    }

    /// Load and merge the layered config. `repo_root` is optional: when
    /// `None`, only the global layer is consulted.
    ///
    /// Missing files are silently skipped (defaults-only is a valid and
    /// common case). Parse failures and schema-version mismatches are
    /// surfaced via [`ConfigError`].
    pub fn load(repo_root: Option<&Path>) -> Result<Self, ConfigError> {
        let mut cfg = Config::default();

        let global = Self::global_path();
        if global.exists() {
            let partial = parse_partial(&global)?;
            check_schema_version(&global, partial.schema_version)?;
            partial.apply_to(&mut cfg);
        }

        if let Some(root) = repo_root {
            let project = Self::project_path(root);
            if project.exists() {
                let partial = parse_partial(&project)?;
                check_schema_version(&project, partial.schema_version)?;
                partial.apply_to(&mut cfg);
            }
        }

        Ok(cfg)
    }
}

/// Resolve the directory that contains the `dedup/` subdirectory for the
/// global config. See [`Config::global_path`] for the platform rules.
fn global_config_dir() -> Option<PathBuf> {
    #[cfg(unix)]
    {
        if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
            let xdg = PathBuf::from(xdg);
            if !xdg.as_os_str().is_empty() {
                return Some(xdg);
            }
        }
        dirs::home_dir().map(|h| h.join(".config"))
    }
    #[cfg(not(unix))]
    {
        dirs::config_dir()
    }
}

fn parse_partial(path: &Path) -> Result<PartialConfig, ConfigError> {
    let body = std::fs::read_to_string(path)?;
    toml::from_str::<PartialConfig>(&body).map_err(|source| ConfigError::Parse {
        path: path.to_path_buf(),
        source,
    })
}

fn check_schema_version(path: &Path, found: Option<u32>) -> Result<(), ConfigError> {
    // Absent schema_version in the file is treated as the current version
    // (the spec's default-via-serde behavior). Only an *explicit* newer
    // value triggers the mismatch error.
    if let Some(v) = found
        && v > SCHEMA_VERSION
    {
        return Err(ConfigError::SchemaVersionMismatch {
            path: path.to_path_buf(),
            found: v,
            expected: SCHEMA_VERSION,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::Mutex;
    use tempfile::tempdir;

    /// Serializes tests that mutate `HOME` / `XDG_CONFIG_HOME`. Cargo
    /// runs tests within a single binary in parallel by default, so
    /// without this mutex two tests can race the process-global env
    /// vars (e.g. `global_path_prefers_xdg_config_home_when_set` setting
    /// `XDG_CONFIG_HOME` while
    /// `global_path_falls_back_to_home_dot_config_without_xdg` expects
    /// it cleared). Matches the pattern used in
    /// `crates/dedup-gui/tests/logging_smoke.rs` (issue #16).
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn write_project(root: &Path, body: &str) {
        let dir = root.join(PROJECT_CONFIG_DIR);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(CONFIG_FILE), body).unwrap();
    }

    /// Write a global config under the sandboxed `home`. Resolves the
    /// actual global path (`$HOME/.config/dedup/config.toml` on Unix per
    /// the PRD). The env-override helper below has already set HOME, so
    /// call this inside `with_test_home`'s closure if you want the
    /// resolver and the writer to agree.
    fn write_global_under_home(home: &Path, body: &str) {
        with_test_home(home, || {
            let path = Config::global_path();
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, body).unwrap();
        });
    }

    #[test]
    fn default_round_trips_through_toml() {
        let cfg = Config::default();
        let rendered = toml::to_string(&cfg).unwrap();
        let parsed: Config = toml::from_str(&rendered).unwrap();
        assert_eq!(cfg, parsed);
    }

    #[test]
    fn load_with_no_files_returns_defaults() {
        // Point HOME at an empty tempdir so the global lookup misses.
        let home = tempdir().unwrap();
        let root = tempdir().unwrap();
        let cfg = with_test_home(home.path(), || Config::load(Some(root.path())));
        let cfg = cfg.unwrap();
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn load_respects_project_overrides() {
        let home = tempdir().unwrap();
        let root = tempdir().unwrap();
        write_project(
            root.path(),
            r#"
schema_version = 1

[thresholds.tier_a]
min_lines = 42
min_tokens = 123

[scan]
max_file_size = 2048
follow_symlinks = true
"#,
        );
        let cfg = with_test_home(home.path(), || Config::load(Some(root.path()))).unwrap();

        assert_eq!(cfg.thresholds.tier_a.min_lines, 42);
        assert_eq!(cfg.thresholds.tier_a.min_tokens, 123);
        assert_eq!(cfg.scan.max_file_size, 2048);
        assert!(cfg.scan.follow_symlinks);
        // Fields the user didn't set still fall through to defaults.
        assert_eq!(
            cfg.thresholds.tier_b,
            TierBThresholds::default(),
            "tier_b untouched by project file"
        );
        assert!(!cfg.scan.include_submodules);
    }

    #[test]
    fn project_overrides_global_and_global_overrides_defaults() {
        let home = tempdir().unwrap();
        // Global: bump tier_a.min_lines to 10, tier_b.min_tokens to 99.
        write_global_under_home(
            home.path(),
            r#"
[thresholds.tier_a]
min_lines = 10

[thresholds.tier_b]
min_tokens = 99
"#,
        );

        let root = tempdir().unwrap();
        // Project: override tier_a.min_lines to 20. Leave tier_b alone.
        write_project(
            root.path(),
            r#"
[thresholds.tier_a]
min_lines = 20
"#,
        );

        let cfg = with_test_home(home.path(), || Config::load(Some(root.path()))).unwrap();

        // Project beat global.
        assert_eq!(cfg.thresholds.tier_a.min_lines, 20);
        // Global's tier_a.min_tokens fallthrough is absent, so default.
        assert_eq!(
            cfg.thresholds.tier_a.min_tokens,
            TierAThresholds::default().min_tokens
        );
        // Global applied where project didn't.
        assert_eq!(cfg.thresholds.tier_b.min_tokens, 99);
        // Defaults where neither layer said anything.
        assert_eq!(
            cfg.thresholds.tier_b.min_lines,
            TierBThresholds::default().min_lines
        );
    }

    #[test]
    fn global_only_is_applied_when_no_repo_root() {
        let home = tempdir().unwrap();
        write_global_under_home(
            home.path(),
            r#"
normalization = "aggressive"

[scan]
include_submodules = true
"#,
        );

        let cfg = with_test_home(home.path(), || Config::load(None)).unwrap();
        assert_eq!(cfg.normalization, Normalization::Aggressive);
        assert!(cfg.scan.include_submodules);
    }

    #[test]
    fn invalid_toml_surfaces_parse_error_with_path() {
        let home = tempdir().unwrap();
        let root = tempdir().unwrap();
        write_project(root.path(), "this is = = not valid toml\n");

        let err = with_test_home(home.path(), || Config::load(Some(root.path()))).unwrap_err();
        match err {
            ConfigError::Parse { path, .. } => {
                assert!(path.ends_with(PathBuf::from(PROJECT_CONFIG_DIR).join(CONFIG_FILE)));
            }
            other => panic!("expected Parse, got {other:?}"),
        }
    }

    #[test]
    fn newer_schema_version_is_rejected() {
        let home = tempdir().unwrap();
        let root = tempdir().unwrap();
        write_project(
            root.path(),
            &format!("schema_version = {}\n", SCHEMA_VERSION + 5),
        );
        let err = with_test_home(home.path(), || Config::load(Some(root.path()))).unwrap_err();
        match err {
            ConfigError::SchemaVersionMismatch {
                path,
                found,
                expected,
            } => {
                assert!(path.ends_with(PathBuf::from(PROJECT_CONFIG_DIR).join(CONFIG_FILE)));
                assert_eq!(found, SCHEMA_VERSION + 5);
                assert_eq!(expected, SCHEMA_VERSION);
            }
            other => panic!("expected SchemaVersionMismatch, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn global_path_prefers_xdg_config_home_when_set() {
        let home = tempdir().unwrap();
        let xdg = tempdir().unwrap();
        // Hold the env lock for the whole mutation + read so
        // concurrent tests can't flip `XDG_CONFIG_HOME` underneath us.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev_home = std::env::var_os("HOME");
        let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
        // SAFETY: test-only env mutation; restored below, and serialized
        // by ENV_LOCK against other env-touching tests in this module.
        unsafe {
            std::env::set_var("HOME", home.path());
            std::env::set_var("XDG_CONFIG_HOME", xdg.path());
        }
        let resolved = Config::global_path();
        // Cleanup before assertions so failures don't leak state.
        unsafe {
            match prev_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            match prev_xdg {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
        }
        assert_eq!(
            resolved,
            xdg.path().join(GLOBAL_CONFIG_SUBDIR).join(CONFIG_FILE)
        );
    }

    #[cfg(unix)]
    #[test]
    fn global_path_falls_back_to_home_dot_config_without_xdg() {
        let home = tempdir().unwrap();
        let resolved = with_test_home(home.path(), Config::global_path);
        assert_eq!(
            resolved,
            home.path()
                .join(".config")
                .join(GLOBAL_CONFIG_SUBDIR)
                .join(CONFIG_FILE)
        );
    }

    #[test]
    fn detail_context_lines_parses_from_toml() {
        // Issue #26 — the GUI detail pane reads `detail.context_lines`
        // to decide how many dimmed lines to show around each duplicate
        // range. Verify it parses, overrides the default, and that an
        // absent value falls through to the 3-line default.
        let home = tempdir().unwrap();
        let root = tempdir().unwrap();
        write_project(
            root.path(),
            r#"
[detail]
context_lines = 7
"#,
        );
        let cfg = with_test_home(home.path(), || Config::load(Some(root.path()))).unwrap();
        assert_eq!(cfg.detail.context_lines, 7);

        // Default when unspecified.
        let root2 = tempdir().unwrap();
        let cfg2 = with_test_home(home.path(), || Config::load(Some(root2.path()))).unwrap();
        assert_eq!(cfg2.detail.context_lines, 3);
    }

    #[test]
    fn editor_section_parses_from_toml() {
        // Issue #29 — `[editor]` section loads preset + overrides.
        let home = tempdir().unwrap();
        let root = tempdir().unwrap();
        write_project(
            root.path(),
            r#"
[editor]
preset = "code"
command = "code -g {file}:{line}"
terminal = "none"
"#,
        );
        let cfg = with_test_home(home.path(), || Config::load(Some(root.path()))).unwrap();
        assert_eq!(cfg.editor.preset, crate::editor::EditorPreset::Code);
        assert_eq!(cfg.editor.command.as_deref(), Some("code -g {file}:{line}"));
        assert_eq!(cfg.editor.terminal.as_deref(), Some("none"));
    }

    #[test]
    fn editor_section_defaults_to_nvim_when_absent() {
        let home = tempdir().unwrap();
        let root = tempdir().unwrap();
        let cfg = with_test_home(home.path(), || Config::load(Some(root.path()))).unwrap();
        assert_eq!(cfg.editor.preset, crate::editor::EditorPreset::Nvim);
    }

    #[test]
    fn absent_schema_version_is_treated_as_current() {
        let home = tempdir().unwrap();
        let root = tempdir().unwrap();
        // No schema_version key — must load clean as current.
        write_project(
            root.path(),
            r#"
[thresholds.tier_a]
min_lines = 7
"#,
        );
        let cfg = with_test_home(home.path(), || Config::load(Some(root.path()))).unwrap();
        assert_eq!(cfg.schema_version, SCHEMA_VERSION);
        assert_eq!(cfg.thresholds.tier_a.min_lines, 7);
    }

    /// Execute `f` with `$HOME` set to `home` and `$XDG_CONFIG_HOME`
    /// cleared so [`Config::global_path`] resolves to
    /// `<home>/.config/dedup/config.toml` regardless of the developer's
    /// or CI runner's real environment (GitHub's ubuntu-latest sets
    /// `XDG_CONFIG_HOME`, which would otherwise poison the HOME fallback
    /// test). Restores the environment after.
    ///
    /// Holds [`ENV_LOCK`] for the duration of `f` so concurrent tests
    /// in this module (notably
    /// `global_path_prefers_xdg_config_home_when_set`) can't race our
    /// mutations of the process-global env.
    fn with_test_home<F, R>(home: &Path, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev_home = std::env::var_os("HOME");
        let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
        // Force `dirs` to treat our tempdir as HOME and clear XDG so the
        // fallback path `<home>/.config/` applies uniformly. The test
        // must remove XDG explicitly — CI runners (ubuntu-latest) set
        // `XDG_CONFIG_HOME` by default, which would otherwise override
        // the HOME-fallback path the caller is asserting against.
        // SAFETY: test-only env mutation; serialized by ENV_LOCK and
        // restored at scope exit.
        unsafe {
            std::env::set_var("HOME", home);
            std::env::remove_var("XDG_CONFIG_HOME");
        }
        let out = f();
        unsafe {
            match prev_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            match prev_xdg {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
        }
        out
    }
}
