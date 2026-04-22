# Dogfood triage (issue #32)

Running `dedup` on its own tree surfaced 114 groups after the
refactor commits (bootstrap helper, `toast_action_button`, `mk_occ`).
This document records which of those are legitimately by-design and
have been dismissed, and which are deferred for human review.

- Scan date: 2026-04-21 (post-fix, from a cold `.dedup/` rebuild).
- Total groups reported: 114.
- Dismissed as by-design: 28.
- Remaining for review: 86.

## How to reproduce the dismissals

`.dedup/cache.sqlite` is local-only — the repo's `.dedup/.gitignore`
carries `*`, so suppressions do not travel with a fresh clone. Rerun
the block below from the repo root to rebuild the same dismissed
set:

```sh
# cold scan
rm -rf .dedup
cargo build --release -p dedup-cli
./target/release/dedup scan . --format json > /tmp/dedup-post-fix.ndjson

# Dismissals are keyed by ordinal (scan-local) but the underlying
# cache stores the hash, so once you've run the commands above in
# the exact state of this commit the ordinals below will match.
for id in 69 70 71 72 73 74 75 76 77 78 79 80 \
         81 82 83 84 103 104 105 106 107 108 \
         109 110 111 112 113 114; do
  ./target/release/dedup dismiss "$id" .
done
```

If the tree drifts, ordinals shift; re-identify by the short hashes
in the table below (all 16-char hashes live in the NDJSON output)
and pass the fresh ordinals to `dedup dismiss`.

## Dismissed groups

Rationale buckets:

- **language-profile test fixtures** — source text inside
  `crates/dedup-lang/tests/{python,rust,typescript}_profile.rs`
  that intentionally duplicates across language tests so the same
  shape exercises each profile.
- **fixture corpus** — `fixtures/**` paths that exist specifically
  to be duplicated; touching them would defeat the golden-scan
  assertions.

| Hash (short)    | Tier | Occs | ~Lines | Representative path                                  | Rationale                         |
| --------------- | ---- | ---- | ------ | ---------------------------------------------------- | --------------------------------- |
| `354be6d79e43`  | A    | 2    | ~15L   | crates/dedup-lang/tests/python_profile.rs (+1 more)  | language-profile test fixtures    |
| `f68cab978208`  | A    | 2    | ~32L   | crates/dedup-lang/tests/python_profile.rs (+1 more)  | language-profile test fixtures    |
| `a3fc3601a5df`  | A    | 3    | ~14L   | crates/dedup-lang/tests/python_profile.rs (+2 more)  | language-profile test fixtures    |
| `4d18d2515325`  | A    | 2    | ~21L   | crates/dedup-lang/tests/python_profile.rs (+1 more)  | language-profile test fixtures    |
| `5b8f1b0199e7`  | A    | 3    | ~12L   | crates/dedup-lang/tests/python_profile.rs (+2 more)  | language-profile test fixtures    |
| `ff67d29ab60d`  | A    | 2    | ~7L    | crates/dedup-lang/tests/python_profile.rs (+1 more)  | language-profile test fixtures    |
| `dbe302fe93b3`  | A    | 2    | ~22L   | crates/dedup-lang/tests/python_profile.rs (+1 more)  | language-profile test fixtures    |
| `c3591eee0666`  | A    | 3    | ~14L   | crates/dedup-lang/tests/python_profile.rs (+2 more)  | language-profile test fixtures    |
| `6b547110a3a1`  | A    | 2    | ~13L   | crates/dedup-lang/tests/python_profile.rs (+1 more)  | language-profile test fixtures    |
| `977948f8854c`  | A    | 2    | ~41L   | crates/dedup-lang/tests/python_profile.rs (+1 more)  | language-profile test fixtures    |
| `797ff4a06918`  | A    | 3    | ~36L   | crates/dedup-lang/tests/python_profile.rs (+2 more)  | language-profile test fixtures    |
| `7f03a85b4f1e`  | A    | 2    | ~20L   | crates/dedup-lang/tests/typescript_profile.rs        | language-profile test fixtures    |
| `8c5f7281a8ff`  | A    | 2    | ~21L   | fixtures/python/alpha.py (+1 more)                   | fixture corpus                    |
| `83a3494ae3d0`  | A    | 2    | ~14L   | fixtures/rust/alpha.rs (+1 more)                     | fixture corpus                    |
| `3b27256149fb`  | A    | 3    | ~19L   | fixtures/tier_a_basic/alpha.rs (+2 more)             | fixture corpus                    |
| `25290b53ecb6`  | A    | 2    | ~18L   | fixtures/typescript/alpha.ts (+1 more)               | fixture corpus                    |
| `43a429b3d1b5`  | B    | 2    | ~5L    | crates/dedup-lang/tests/python_profile.rs (+1 more)  | language-profile test fixtures    |
| `fe271215e554`  | B    | 2    | ~7L    | crates/dedup-lang/tests/python_profile.rs (+1 more)  | language-profile test fixtures    |
| `6315a03c8d0d`  | B    | 3    | ~7L    | crates/dedup-lang/tests/python_profile.rs (+2 more)  | language-profile test fixtures    |
| `3efef4f3ea70`  | B    | 3    | ~23L   | crates/dedup-lang/tests/python_profile.rs (+2 more)  | language-profile test fixtures    |
| `d6982bafa248`  | B    | 3    | ~7L    | crates/dedup-lang/tests/python_profile.rs (+2 more)  | language-profile test fixtures    |
| `3f4ec3910fbd`  | B    | 2    | ~8L    | fixtures/python/alpha.py (+1 more)                   | fixture corpus                    |
| `4981949b85de`  | B    | 2    | ~8L    | fixtures/python/alpha.py (+1 more)                   | fixture corpus                    |
| `b5def0ba3600`  | B    | 2    | ~10L   | fixtures/rust/alpha.rs (+1 more)                     | fixture corpus                    |
| `17b913055ab7`  | B    | 2    | ~10L   | fixtures/rust/alpha.rs (+1 more)                     | fixture corpus                    |
| `42b30bd8cf0f`  | B    | 3    | ~12L   | fixtures/tier_a_basic/alpha.rs (+2 more)             | fixture corpus                    |
| `fd8b1f6c5b4e`  | B    | 2    | ~10L   | fixtures/typescript/alpha.ts (+1 more)               | fixture corpus                    |
| `18650c71df33`  | B    | 2    | ~10L   | fixtures/typescript/alpha.ts (+1 more)               | fixture corpus                    |

## Deferred for human review

The following groups survive the dismissal pass and are listed here
as one-line dispositions for a follow-up decision. None are fixed
automatically because each has a reason that only a human can
evaluate (divergent detail that a refactor would obscure, binary
boundary between `dedup` and `dedup-gui`, etc.).

- **Group 1** (`8343bf41`, A, 2×): `crates/dedup-cli/src/main.rs:600-620` vs
  `crates/dedup-gui/src/main.rs:64-85`. The `human-panic` setup block is
  near-identical but lives in two separate binaries. Consolidating would
  mean promoting a shared `panic_setup()` into a helper crate or
  `dedup-core`; worth doing once we own more than two binaries.
- **Group 3** (`ba701460`, A, 3×): three cache-open boilerplate blocks in
  `run_list`, `run_show`, and the clean subcommand in
  `crates/dedup-cli/src/main.rs`. Collapsing is straightforward
  (`fn open_cache_or_exit(path: &Path) -> Result<Option<Cache>>`), but
  each call site formats the "No cached scan found. Run `dedup scan`
  first." message slightly differently in adjacent lines — a careless
  refactor drops a nuance.
- **Group 37** (`5d183aa8`, A, 2×):
  `crates/dedup-core/src/cache.rs:1866-1880` vs
  `crates/dedup-core/tests/cache.rs:211-233`. The production copy is a
  `#[cfg(test)]` helper inside `cache.rs`; the integration-test copy
  lives in `tests/cache.rs`. Moving the helper to a `#[doc(hidden)]
  pub` fn in `cache.rs` is the obvious fix, but this also involves
  tempdir setup and thread choreography that diverges subtly between
  the two scenarios; best reviewed by the cache owner.
- **Group 75**: dismissed above (`dbe302fe93b3`) — listed as a
  deferred candidate in the original issue #32 because the first-pass
  triage couldn't tell whether it was test-only; post-fix it's
  clearly language-profile fixtures, so it ships in the dismissal
  table.
