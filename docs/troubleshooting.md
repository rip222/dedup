# Troubleshooting

## Where are the logs?

### CLI

The CLI installs a `tracing_subscriber` that writes to **stderr**.

- Default filter: `warn`.
- With `--verbose` / `-v`: `dedup=debug`.
- `RUST_LOG` always wins when set (standard env-filter syntax).

No rolling file appender is installed for the CLI. Redirect stderr
to capture:

```sh
dedup scan . 2> scan.log
RUST_LOG=dedup=debug dedup scan . 2> scan.log
```

### GUI

The GUI installs a daily-rolling JSON log appender. Files live at
`~/.config/dedup/logs/dedup.log.YYYY-MM-DD`. Rotation keeps the
seven most recent files (older files are pruned on startup).

Path resolution (in
`crates/dedup-gui/src/logging.rs::log_dir`):

1. `$XDG_CONFIG_HOME/dedup/logs/` when set.
2. `$HOME/.config/dedup/logs/` otherwise.

The default `tracing` filter is `info`; `RUST_LOG` overrides.

## Common errors

### "Cache created by newer Dedup version (schema N > supported M). Rescan?"

Another build of dedup wrote the cache. The file is left untouched
so you don't lose anything. Options:

- Upgrade dedup so it understands the newer schema.
- Discard the cache: `dedup clean --yes` then rescan.

Exit code: 2.

### "dedup: config error: …"

The loader could not parse a config file. The error message
includes the file path. See [config.md](config.md) for the schema.
Fix the file (or move it aside) and retry.

The GUI surfaces the same error via the startup-error modal, with
"Fix config" (opens the file in `$EDITOR`) and "Reset to defaults"
(overwrites with a minimal defaults-only TOML) buttons.

Exit code: 2.

### "config at <path> declares schema_version N which is newer than supported version M"

You have a config from a newer dedup. The CLI warns and falls back
to defaults; the GUI surfaces the startup-error modal.

### "No cached scan found. Run `dedup scan` first."

You ran `list`, `show`, `dismiss`, or `suppressions` but there is
no cache at `<PATH>/.dedup/cache.sqlite`. Run `dedup scan <PATH>`
first.

Exit code: 2.

### "no group with id <N>"

The group id is unknown to the cache. `list` shows all ids.

Exit code: 2.

### "No editor found — run dedup config edit to pick one."

The editor preset's primary binary (and `vim`, for `nvim`) is not
on `PATH`. Pick a different preset in [config.md](config.md) or
install the binary. See [editor.md](editor.md).

### "refusing to delete <…> without --yes (stdin is not a TTY)"

`dedup clean` running from a script/CI needs `--yes` to skip the
interactive prompt.

Exit code: 2.

## Resetting the cache

```sh
dedup clean       # interactive prompt
dedup clean --yes # force
```

Removes the entire `<PATH>/.dedup/` directory. This wipes:

- The scan results (`cache.sqlite`).
- All dismissals (group-level and per-occurrence).
- The warm-scan file fingerprints (`file_hashes`, `file_blocks`).
- The auto-generated `.gitignore` inside `.dedup/`.

The next scan will recreate the directory from scratch.

## Resetting the global config

```sh
rm -rf "${XDG_CONFIG_HOME:-$HOME/.config}/dedup"
```

Removes the global `config.toml` and all GUI logs.

## Panics

The CLI installs [human-panic](https://crates.io/crates/human-panic)
at startup. A panic prints a human-readable postmortem to stderr
pointing at `~/.config/dedup/logs/` (shared with the GUI).

The GUI catches panics in the scan worker (via `catch_unwind`) and
surfaces them as toasts rather than tearing down the app. Main-thread
panics still use `human-panic`.

Panic exit code: 101 (default Rust behaviour; dedup does not
override it).

## Getting help

Open an issue: https://github.com/rip222/dedup/issues

The GUI's post-scan "Copy details" button (for scan-time issues)
produces a pre-formatted markdown block suitable for pasting
directly into an issue.
