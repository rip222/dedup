# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- M1 · Cargo workspace, license files, and CI skeleton (#2)
- M1 · Tokenizer + Tier A detection + `dedup scan` (#3)
- M1 · SQLite-backed cache with `dedup list` and `dedup show` (#4)
- M1 · Full ignore-rule stack with `--no-gitignore` / `--all` flags (#5)
- M1 · Layered TOML config loader and `dedup config` subcommand (#9)
- M1 · Conservative and aggressive normalization modes (#10)
- M1 · Suppressions: `dedup dismiss` / `suppressions` / `clean` (#11)
- M1 · tracing + logging infrastructure (#16)
- M1 · Per-file error graceful degradation + human-panic integration (#17)
- M1 · Cache schema versioning + WAL concurrency guardrails (#18)
- M1 · rayon-parallel scanner with content-hash warm cache (#14)
- M1 · criterion benchmark suite + CI bench/dogfood jobs (#15)
- M2 · `LanguageProfile` trait + Rust Tier B detection (#6)
- M3 · Tier B TypeScript profile with JSX-aware rename (#7)
- M3 · Tier B Python profile with decorator-aware rename (#8)
- M4 · macOS GUI skeleton with NSMenu menubar (#19)
- M4 · GUI open folder + render cached scan results (#20)
- M4 · GUI scan button with progress bar + completion banner (#21)
- M4 · GUI Open Recent menu with MRU 5 and stale-entry toast (#28)
- M5 · GUI cancel + Tier A streaming with stable Impact sort (#22)
- M5 · GUI sidebar sort / filter / search / summary + keyboard nav (#23)
- M5 · GUI detail syntax highlighting via tree-sitter + syntect (#24)
- M5 · GUI Tier B alpha-rename diff tinting (#25)
- M5 · GUI detail view polish: context lines, gutter, horizontal scroll (#26)
- M5 · GUI toast system + error modals + background panic catch (#30)
- M6 · GUI group toolbar + per-occurrence selection/dismissal (#27)
- M6 · Editor launcher with preset table + fallback chain + preferences dialog (#29)
- M7 · CLI global flags, progress spinner, and `--strict` exit codes (#13)
- M7 · CLI structured output: JSON + SARIF + NDJSON with TTY auto-select (#12)
- M8 · User and contributor documentation under `docs/` (#31)
