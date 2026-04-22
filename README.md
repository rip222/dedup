# dedup

**dedup** finds duplicate code across a directory tree. It ships as a
cross-platform CLI (`dedup`) and a native macOS GUI (`dedup-gui`),
combining a language-oblivious rolling-hash pass (Tier A) with
per-language tree-sitter matching (Tier B) for Rust, TypeScript/TSX,
and Python.

## Install

At MVP, dedup is installed by building it from source. See
[`docs/install.md`](docs/install.md) for requirements, platform notes,
and sanity checks.

```sh
git clone https://github.com/rip222/dedup.git
cd dedup

# CLI (Linux, macOS, best-effort Windows).
cargo build --release -p dedup-cli

# GUI (macOS only).
cargo build --release -p dedup-gui
```

Binaries land in `target/release/`. Put `target/release/dedup` on your
`$PATH` to invoke it as `dedup` anywhere.

## 60-second CLI quickstart

```sh
# Scan the current directory — walks the tree, runs Tier A + Tier B,
# persists results to .dedup/cache.sqlite, and prints duplicate groups.
dedup scan .
```

Follow-up commands: `dedup list` re-streams the cached groups without
rescanning; `dedup show <ID>` prints full detail for one group;
`dedup dismiss <ID>` hides a group from future reports. Full reference
in [`docs/cli.md`](docs/cli.md).

## 60-second GUI quickstart

1. Launch the app: `cargo run --release -p dedup-gui` (or run the
   built `./target/release/dedup-gui`).
2. **File → Open…** (⌘O) and pick a folder. Any prior cached scan for
   that folder appears immediately.
3. **Scan → Start Scan** (⌘R). Groups stream into the sidebar as they
   are found. Click a group to review its occurrences side-by-side in
   the stacked detail pane.

Full guide and keyboard shortcuts in [`docs/gui.md`](docs/gui.md).

## Documentation

The full user and contributor documentation lives under
[`docs/`](docs/):

- [`docs/install.md`](docs/install.md) — build from source,
  requirements, sanity checks, uninstall.
- [`docs/cli.md`](docs/cli.md) — CLI reference: every subcommand,
  every flag, output formats, exit codes.
- [`docs/config.md`](docs/config.md) — layered TOML config schema.
- [`docs/ignore.md`](docs/ignore.md) — the four-layer ignore-rule
  stack and its precedence rules.
- [`docs/suppressions.md`](docs/suppressions.md) — group-level and
  per-occurrence dismissal behaviour.
- [`docs/editor.md`](docs/editor.md) — editor preset table and the
  `custom` escape hatch.
- [`docs/gui.md`](docs/gui.md) — macOS GUI guide and keyboard
  shortcuts.
- [`docs/performance.md`](docs/performance.md) — performance
  expectations and large-repo guidance.
- [`docs/troubleshooting.md`](docs/troubleshooting.md) — log
  locations, common errors, cache reset.
- [`docs/contributing-language-profile.md`](docs/contributing-language-profile.md)
  — how to add a new Tier B `LanguageProfile` to `dedup-lang`.
- [`docs/dogfood-triage.md`](docs/dogfood-triage.md) — triage guide
  for running dedup against its own source tree.

## Contributing

The most common way to contribute is to add a new Tier B language
profile. See
[`docs/contributing-language-profile.md`](docs/contributing-language-profile.md)
for the full walkthrough.

## License

Dual-licensed under either of:

- [Apache License, Version 2.0](LICENSE-APACHE)
- [MIT License](LICENSE-MIT)

at your option.
