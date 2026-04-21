# Installation

At MVP, dedup is installed by building it from source. Pre-built
binaries and package-manager recipes are planned post-MVP.

## Requirements

- **Rust toolchain** — stable, current (`cargo`, `rustc`). Install
  via [rustup](https://rustup.rs/) if you don't already have it.
- **C compiler** — required by the `tree-sitter` crates used by the
  Tier B language profiles (`cc`, `clang`, etc.).
- **SQLite** headers are not required — dedup bundles `rusqlite` in
  bundled-sqlite mode.

The GUI (`dedup-gui`) additionally requires:

- **macOS** — the GUI is macOS-only at this milestone. It depends
  on GPUI and AppKit; it does not build on Linux or Windows.

The CLI (`dedup-cli`) builds and runs on Linux, macOS, and — on a
best-effort basis — Windows.

## Build from source

Clone the repository and build the release binaries:

```sh
git clone https://github.com/rip222/dedup.git
cd dedup

# CLI only.
cargo build --release -p dedup-cli

# GUI (macOS only).
cargo build --release -p dedup-gui
```

The binaries land in `target/release/`:

- `target/release/dedup` — the CLI.
- `target/release/dedup-gui` — the macOS app binary.

Put `target/release/dedup` on your `$PATH` (e.g. copy into
`~/.local/bin/` or `~/bin/`) to invoke it as `dedup` anywhere.

## Sanity checks

```sh
dedup --version          # prints version; exits 0
dedup --help             # prints full usage; exits 0
dedup-gui --smoke-test   # boots GPUI, exits 0 — used in CI
```

## Running the test suite

```sh
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## Uninstall

dedup writes only to:

- `<repo>/.dedup/` — per-repository cache and optional config file.
- `~/.config/dedup/` — global config and GUI logs.

Remove both directories to uninstall completely. The per-repository
cache can be removed with `dedup clean`; the global directory must
be removed manually.
