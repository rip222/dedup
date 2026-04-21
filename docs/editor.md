# Editor launcher

dedup can launch external editors positioned at a specific file and
line. The launcher is a pure
`(preset, targets) -> Vec<CommandSpec>` builder defined in
`crates/dedup-core/src/editor.rs`.

Configuration lives in the `[editor]` section of `config.toml`. See
[config.md](config.md#editor) for loader behaviour.

## Preset table

Sourced directly from `crates/dedup-core/src/editor.rs`.

| preset      | command template            | default terminal | binary looked up |
|-------------|-----------------------------|------------------|------------------|
| `nvim`      | `nvim +{line} {file}`       | `auto`           | `nvim`           |
| `vim`       | `vim +{line} {file}`        | `auto`           | `vim`            |
| `helix`     | `hx {file}:{line}`          | `auto`           | `hx`             |
| `emacs`     | `emacs -nw +{line} {file}`  | `auto`           | `emacs`          |
| `code`      | `code -g {file}:{line}`     | `none`           | `code`           |
| `cursor`    | `cursor -g {file}:{line}`   | `none`           | `cursor`         |
| `zed`       | `zed {file}:{line}`         | `none`           | `zed`            |
| `sublime`   | `subl {file}:{line}`        | `none`           | `subl`           |
| `jetbrains` | `idea --line {line} {file}` | `none`           | `idea`           |
| `custom`    | user-supplied template      | `none`           | first word of `command` |

The default preset is `nvim` (see `EditorPreset::default`). If
`nvim` is not on `PATH`, the launcher transparently falls back to
`vim`. If neither is present (or a non-nvim preset's binary is
missing) the GUI surfaces a toast: **"No editor found — run dedup
config edit to pick one."**

## Multi-file behaviour

Per the implementation in `build_commands`:

- **nvim / vim** — a single `CommandSpec` whose argv is a chain of
  `-c` commands. The first file loads via `edit {path} | {line}`;
  each subsequent file opens in a new tab via
  `tabnew +{line} {path}`. Paths are POSIX single-quote escaped.
- **helix / emacs** — one `CommandSpec` with every file as an argv
  entry. Emacs drops per-file line positioning when more than one
  file is passed (no uniform `+line` syntax).
- **GUI editors** (`code`, `cursor`, `zed`, `sublime`, `jetbrains`)
  — one `CommandSpec` **per file** so each call re-uses the
  already-running editor window.
- **custom** — one `CommandSpec` per file. The template is split on
  ASCII whitespace; the first token is the program.

## Terminal modes

`terminal` (or the preset default when unset) controls how the
spawned process is wrapped:

| mode | behaviour |
|---|---|
| `auto` | Wrap each spec with `osascript` to open a macOS Terminal.app window running the command. Terminal-native editors (`nvim`, `vim`, `helix`, `emacs`) default to this. |
| `none` | Run the command directly without a TTY. GUI editors default to this. |
| `custom` | Substitute the rendered argv into `{cmd}` in `terminal_command`. Required when `terminal = "custom"`. |

Example `custom` terminal wrap:

```toml
[editor]
preset = "nvim"
terminal = "custom"
terminal_command = "kitty --hold -e sh -c {cmd}"
```

## Placeholders

Substitutions performed by the builder:

| placeholder | replaced with |
|---|---|
| `{file}` | The target file path. |
| `{line}` | The 1-based line number. |
| `{cmd}` | (inside `terminal_command` only) The POSIX-escaped rendered argv: `program arg1 arg2 …`. |

`{file}` and `{line}` apply inside `command` templates — on the
stock presets (every entry above) and on `preset = "custom"`.

## Custom-preset escape hatch

When the built-in presets don't match your editor of choice, use
`preset = "custom"` together with a `command` template:

```toml
[editor]
preset = "custom"
command = "micro {file}:{line}"
# Defaults to terminal = "none"; override if you want a Terminal.app wrap.
```

Constraints:

- `command` is **required** for `custom`. An empty or missing
  `command` renders zero commands (the launcher no-ops).
- The first whitespace-delimited token must exist on `PATH` (or be
  an absolute path) for `resolve_preset` to succeed.
- `command` is split on ASCII whitespace. If you need quoted
  arguments with embedded spaces, wrap with `terminal = "custom"`
  and construct the shell string via `terminal_command`.

## Error surfaces

The `EditorError` enum in `crates/dedup-core/src/editor.rs`:

- `NoEditor` — primary binary (and `vim` fallback, for `nvim`) is
  missing from `PATH`.
- `CustomWithoutCommand` — `preset = "custom"` with no `command`.
- `CustomTerminalWithoutTemplate` — `terminal = "custom"` with no
  `terminal_command`.
- `Spawn { program, source }` — `std::process::Command::spawn`
  failed for a resolved command.

The GUI surfaces all four as toasts. The CLI does not currently
drive the editor launcher (reserved for a post-MVP "reveal in
editor" flow).
