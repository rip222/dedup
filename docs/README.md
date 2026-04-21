# dedup — documentation

Welcome to the user and contributor documentation for **dedup**, a
duplicate-code detector with a command-line interface and a native
macOS GUI.

This tree documents the behaviour of the current MVP build. Anything
not yet implemented is called out explicitly in the relevant page.

## Table of contents

- [install.md](install.md) — installation (build from source).
- [cli.md](cli.md) — CLI reference: every subcommand, every flag,
  output format schemas, exit codes.
- [gui.md](gui.md) — macOS GUI guide: Open flow, Scan, sidebar
  navigation, stacked view, keyboard shortcuts.
- [config.md](config.md) — layered TOML config schema: every key, its
  default, valid values, layer precedence.
- [editor.md](editor.md) — editor preset table and the `custom`
  escape hatch with `{file}` / `{line}` / `{cmd}` placeholders.
- [ignore.md](ignore.md) — the four-layer ignore-rule stack and its
  precedence rules.
- [suppressions.md](suppressions.md) — group-level and
  per-occurrence dismissal behaviour and persistence.
- [troubleshooting.md](troubleshooting.md) — log locations, common
  errors, and how to reset the cache.
- [performance.md](performance.md) — performance expectations,
  cold/warm-scan scaling, and large-repo guidance.
- [contributing-language-profile.md](contributing-language-profile.md)
  — how to add a new Tier B `LanguageProfile` to `dedup-lang`.

## Quick start

```sh
# Build a release binary.
cargo build --release -p dedup-cli

# Run it on the current directory.
./target/release/dedup scan .

# Or, inspect what was cached by the most recent scan:
./target/release/dedup list
```

See [install.md](install.md) for the full build and install flow.
