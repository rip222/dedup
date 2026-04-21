# dedup

Find duplicate code across a directory tree. Ships as a CLI
(`dedup`) and a native macOS GUI (`dedup-gui`).

## Quick start

```sh
# Build from source.
cargo build --release -p dedup-cli

# Scan the current directory.
./target/release/dedup scan .

# Re-inspect the cached results without re-scanning.
./target/release/dedup list
```

## Documentation

See [`docs/`](docs/) for the full user and contributor
documentation:

- [`docs/install.md`](docs/install.md) — build from source.
- [`docs/cli.md`](docs/cli.md) — CLI reference (every subcommand,
  every flag, output formats, exit codes).
- [`docs/gui.md`](docs/gui.md) — macOS GUI guide and keyboard
  shortcuts.
- [`docs/config.md`](docs/config.md) — layered TOML config schema.
- [`docs/editor.md`](docs/editor.md) — editor preset table.
- [`docs/ignore.md`](docs/ignore.md) — ignore-rule layers.
- [`docs/suppressions.md`](docs/suppressions.md) — dismissal
  behaviour.
- [`docs/troubleshooting.md`](docs/troubleshooting.md) — logs,
  common errors, cache reset.
- [`docs/performance.md`](docs/performance.md) — performance
  expectations and large-repo guidance.
- [`docs/contributing-language-profile.md`](docs/contributing-language-profile.md)
  — add a new Tier B language.

## License

Dual-licensed under [MIT](LICENSE-MIT) or
[Apache-2.0](LICENSE-APACHE).
