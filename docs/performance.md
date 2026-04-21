# Performance expectations

This page describes what to expect from a dedup scan at different
repository sizes and how to tune it.

Sources of truth:

- `crates/dedup-core/benches/` — four criterion benches:
  tokenize + rolling-hash on a 1k-line synthetic Rust file,
  tree-sitter parse + subtree hash, bucket-fill on 100k hash pairs,
  and a full fixture scan (issue #15).
- `crates/dedup-core/src/scanner.rs` — rayon-parallel scan pipeline
  with content-hash warm cache (issue #14).
- The `bench` and `dogfood` CI jobs (`.github/workflows/ci.yml`).

## Scan phases

A single `dedup scan` run has these phases:

1. **Walk** — `ignore::WalkBuilder` enumerates files. Serial, but
   metadata-cheap. Dominated by directory I/O.
2. **Read / tokenize / hash** — each file is read, decoded as
   UTF-8, tokenised, and rolling-hashed into 64-bit block hashes.
   **Parallel** via a scoped rayon pool, bounded by `--jobs N`.
3. **Bucket-fill** — two-pass bucket map: count first, then
   materialise only non-singleton hashes. Keeps peak memory bounded
   on scans dominated by unique blocks.
4. **Group emit** — collected duplicate blocks become `MatchGroup`s
   tagged `Tier::A`.
5. **Tier B** — tree-sitter parse + syntactic-unit extraction per
   registered `LanguageProfile`. Currently Rust, TypeScript/TSX,
   Python.
6. **Persist** — one transactional replace of `match_groups`,
   occurrences, `file_hashes`, and `file_blocks`.

## Warm vs. cold

Each file's content hash (plus `(size, mtime)` as a cheap probe)
is cached in SQLite. On a second scan of an unchanged tree:

- Files whose `(size, mtime)` match are candidates for the fast
  path.
- Their content hash is still recomputed to confirm. If it
  matches, the cached block-hash list is used directly — no
  tokenise, no hash.
- The bucket-fill and emit phases still run.

As a result, warm scans on tree where nothing changed are **much**
cheaper than cold scans — the expensive per-file work is entirely
skipped for unchanged files. Projects with a small working-set
(typical developer flow: touch a handful of files and rescan) see
near-instant warm scans regardless of total repo size.

## Measured — this repository (dogfood job)

The CI `dogfood` job runs `dedup scan .` against this repository
and asserts exit 0. **This repository** is a small-to-medium
workspace; the scan completes well under a second on modern
hardware. Specific absolute numbers are not pinned in the tree —
the `bench` job publishes full criterion output with each PR.

To reproduce locally:

```sh
cargo build --release -p dedup-cli

# Cold scan (delete the cache first).
rm -rf .dedup && time ./target/release/dedup scan .

# Warm scan (no file changes).
time ./target/release/dedup scan .
```

## Projected — repository size bands

The numbers below are **projected** from the scan-phase complexity
(linear in total source bytes for phases 2–4) and the current
benchmark results. They are intended as order-of-magnitude
guidance, not hard guarantees. They assume a recent developer
laptop (M-series Mac or 8-core Linux x86_64), an SSD, and the
scanner's default `--jobs` (= `num_cpus`).

| band | lines of source | projected cold scan | projected warm scan (no changes) |
|---|---|---|---|
| small | < 10k | well under a second | sub-second |
| medium | 10k – 100k | a few seconds | sub-second |
| large | 100k – 1M | tens of seconds | a few seconds |
| very large | > 1M | minutes | seconds |

Projections. Actual numbers depend on language mix (tree-sitter
parsing dominates Tier B time), average file size, and disk
speed. Treat as upper-bound intuition; run the `dogfood` flow on
your own repo for real numbers.

## Large-repo guidance

For repositories larger than ~100k lines:

- **Commit a `.dedup/` directory** (or symlink to it) between runs
  to preserve the warm-scan cache. The directory is git-ignored by
  default (auto-seeded `*` gitignore).
- **Exclude vendored / generated trees** via `.dedupignore` — the
  layer-3 defaults already skip a lot, but project-specific trees
  are worth adding. See [ignore.md](ignore.md).
- **Tune `scan.max_file_size`** upward only if you have specific
  large hand-written files. The default 1 MiB handles almost every
  hand-authored file; larger files are usually minified /
  generated / vendored.
- **Run with `--tier a`** if you only need the language-oblivious
  pass. Tier B is more expensive per file (tree-sitter parse).
- **Tune `--jobs`** if you are I/O-bound (try `--jobs 1` or `2` on
  spinning disks) or if you want to leave cores for other work.

## When to run

- **CI**: the `bench` job compares the PR against a base ref with a
  ±10% regression threshold (reported, not enforced) — see
  `.github/scripts/bench_compare.py`. The `dogfood` job asserts
  `dedup scan .` stays exit-0 on this repo.
- **Pre-commit / pre-push hooks**: the warm-scan path is fast
  enough for a pre-push hook on a small/medium repo. For larger
  repos, consider a nightly job instead.

## Known limitations

- The walk phase is serial. On repos with very many tiny
  directories, directory-traversal dominates — this is an area
  marked for future work but not on the MVP.
- Tier B languages at MVP: Rust, TypeScript/TSX, Python. Other
  files go through Tier A only.
- Memory is dominated by the bucket-fill map. Very large repos
  (millions of blocks) can spike memory during phase 3. The
  two-pass design keeps this bounded in practice but is not a hard
  ceiling.
