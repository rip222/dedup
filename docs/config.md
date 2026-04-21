# Configuration

dedup reads a layered TOML config. The loader and all defaults live
in `crates/dedup-core/src/config.rs`. Files are **never** auto-created;
the only place a fresh file is ever written is the CLI's
`dedup config edit` subcommand.

## Layer precedence

Lowest precedence first:

1. **Baked-in defaults** (`Config::default()` in
   `crates/dedup-core/src/config.rs`).
2. **Global file** at `$XDG_CONFIG_HOME/dedup/config.toml` (when
   set), else `$HOME/.config/dedup/config.toml` on Unix.
3. **Project file** at `<repo_root>/.dedup/config.toml`.

Higher layers override lower layers field-by-field. A field absent
in a higher layer falls through to the next lower layer's value.

CLI flags (`--no-gitignore`, `--all`, `--jobs`, `--tier`, `--lang`,
`--strict`, `--format`, `--color`, `--quiet`, `--verbose`) are
applied on top of the resolved config per-invocation but are **not**
persisted back to disk.

## Schema version

Every file may declare `schema_version = N` at the top level. The
current version is `1`. Absent values are treated as the current
version. A file with a `schema_version` **greater** than what the
running build supports is rejected:

- The CLI prints a warning and falls back to defaults for the
  affected layer.
- The GUI surfaces a startup-error modal with "Fix config" / "Reset
  to defaults" actions (issue #30).

## Full schema

All fields are optional in the on-disk file; omitted fields use the
defaults below.

### `[thresholds]`

Tier-specific detection minima.

```toml
[thresholds.tier_a]
min_lines = 6       # default: 6
min_tokens = 50     # default: 50

[thresholds.tier_b]
min_lines = 3       # default: 3
min_tokens = 15     # default: 15
```

Both values must be positive integers. Units of measurement:

- `min_lines` — whole lines covered by the span.
- `min_tokens` — normalised tokens within the span.

### `normalization`

Top-level key, `"conservative"` or `"aggressive"`.

```toml
normalization = "conservative"   # default
```

- `"conservative"` — alpha-renames local identifiers, leaves literal
  values verbatim. Two functions differing only in variable names
  hash the same; two functions differing in a string literal do not.
- `"aggressive"` — additionally abstracts literal leaves to a
  stable placeholder (`<LIT>`) before hashing. Broader matching;
  more false positives on intentionally different constants.

### `[scan]`

Scanner-side knobs.

```toml
[scan]
max_file_size = 1048576   # default: 1 MiB (1_048_576 bytes)
follow_symlinks = false   # default: false
include_submodules = false  # default: false
```

- `max_file_size` — files strictly larger are skipped at ignore
  layer 1.
- `follow_symlinks` — when false (default), symlinks are not
  traversed by the walker.
- `include_submodules` — when false (default), directories
  containing a `.git` file/dir (i.e. submodules) are skipped.

### `[detail]`

GUI detail-pane tunables (issue #26). Currently a single knob; more
may land without a schema bump.

```toml
[detail]
context_lines = 3   # default: 3
```

- `context_lines` — number of dimmed context lines shown above and
  below each duplicate range. `0` disables context entirely.

### `[editor]`

Editor launcher configuration. See [editor.md](editor.md) for the
full preset table, multi-file behaviour, and terminal modes.

```toml
[editor]
preset = "nvim"                          # default: "nvim"
command = "nvim +{line} {file}"          # override the preset's template
terminal = "auto"                        # "auto" | "none" | "custom"
terminal_command = "kitty --hold -e sh -c {cmd}"  # required when terminal = "custom"
```

Fields:

- `preset` — one of `nvim`, `vim`, `helix`, `emacs`, `code`,
  `cursor`, `zed`, `sublime`, `jetbrains`, `custom`.
- `command` — overrides the preset's template. Required for
  `preset = "custom"`.
- `terminal` — terminal-wrapping mode. Defaults per-preset
  (terminal-native editors → `auto`; GUI editors → `none`).
- `terminal_command` — required when `terminal = "custom"`.
  Substitutes `{cmd}` with the POSIX-escaped rendered argv.

## Strictness

The TOML loader sets `#[serde(deny_unknown_fields)]` on every table
so typos surface as parse errors with the file path — not silent
no-ops.

## Where dedup looks

```sh
dedup config path     # shows global: … (present|not present) and project: …
```

This never creates a file; it is a pure inspection command.

`dedup config edit` will create the project-layer file
(`<repo>/.dedup/config.toml`) if neither layer exists and no
`.dedup/` directory is present yet. This is the documented "one
place" a file is ever materialised.
