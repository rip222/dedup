# CLI reference

The `dedup` binary is implemented in `crates/dedup-cli`. Every
subcommand and flag listed here is verified against
`crates/dedup-cli/src/main.rs` and `crates/dedup-cli/src/output.rs`.

## Invocation

```
dedup [GLOBAL FLAGS] <subcommand> [ARGS]
```

## Global flags

Accepted by every subcommand (flattened onto the root parser). See
`GlobalArgs` in `crates/dedup-cli/src/main.rs`.

| Flag | Values | Default | Description |
|---|---|---|---|
| `--no-gitignore` | bool | off | Disable layer 2 of the ignore stack (`.gitignore`, `.git/info/exclude`, git global excludes). Layers 1, 3, 4 still apply. See [ignore.md](ignore.md). |
| `--all` | bool | off | Disable layers 1–3 of the ignore stack (binary sniff, size cap, `.git/`, built-in defaults). Layer 4 (`.dedupignore`) still applies. |
| `--tier` | `a` \| `b` \| `both` | `both` | Restrict detection tier. Tier A is the language-oblivious rolling-hash scan; Tier B is per-language tree-sitter matching (Rust, TypeScript/TSX, Python at MVP). |
| `--lang` | comma-separated list of `rust,ts,python` | empty | Restrict Tier B languages. Tier A groups are language-oblivious and always pass this filter. |
| `--jobs` | integer | unset | Parallelism for the read/tokenize/hash pipeline. `0` and unset both fall through to `num_cpus`. `1` forces single-threaded. |
| `--quiet`, `-q` | bool | off | Suppress the progress spinner. Exit codes and stdout are unaffected. Mutually exclusive with `--verbose`. |
| `--verbose`, `-v` | bool | off | Lower the default `tracing` filter to `dedup=debug`. `RUST_LOG` still wins when set. Mutually exclusive with `--quiet`. |
| `--color` | `auto` \| `always` \| `never` | `auto` | Control ANSI color on stderr. `auto` disables color when stderr is not a TTY. |
| `--strict` | bool | off | Exit 1 when findings are present. Default is git-style: exit 0 regardless of findings. |
| `--format` | `text` \| `json` \| `sarif` | auto | Output format for stdout. Default: `text` on a TTY, `json` when stdout is piped. `sarif` is meaningful on `scan` / `list` / `show`; `suppressions list` falls back to text for `sarif`. |

## Subcommands

### `dedup scan [PATH]`

Walk the directory tree rooted at `PATH` (default `.`), run Tier A
(and Tier B for supported languages), persist results to
`<PATH>/.dedup/cache.sqlite`, and print the groups to stdout.

- Cache dir `<PATH>/.dedup/` is created on first write and seeded
  with a single-line `*` `.gitignore`.
- If an existing cache declares a `user_version` greater than the
  running build's schema, the file is left untouched and the CLI
  prints a "rescan?" prompt on stderr (exit 2). See
  [troubleshooting.md](troubleshooting.md).
- Progress spinner (indicatif) on stderr. Suppressed when stderr is
  not a TTY or `--quiet` is set.
- After the scan a per-file issue summary is printed on stderr when
  any files produced issues:
  `dedup: <N> files scanned, <M> issues (R read, U utf8, P tier-b-parse, X tier-b-panic)`.

### `dedup list [PATH]`

Read persisted groups from `<PATH>/.dedup/cache.sqlite` and stream
them in the same format as `scan`. No re-scan.

Fails with exit 2 if no cached scan is found.

### `dedup show <ID> [PATH]`

Print full detail (every occurrence span) for one persisted group
identified by the `id` printed by `dedup list` / `dedup scan`.

Fails with exit 2 if no cached scan is found, or if `<ID>` is
unknown.

### `dedup config <action>`

Inspect or edit the layered config. See [config.md](config.md) for
the schema.

- `dedup config path [PATH]` — print one line per layer (`global:`,
  `project:`) with a `(present)` / `(not present)` indicator.
  Never creates files.
- `dedup config edit [PATH]` — launch `$EDITOR` (falling back to
  `$VISUAL`, then `vi`) on the resolved config file. If no config
  file exists, an empty one is created at the project layer — this
  is the only place a config file is ever materialised by dedup.

### `dedup dismiss <ID> [PATH]`

Suppress a group from future reports. The dismissal is keyed by the
group's normalized-block-hash, so cosmetic whitespace changes leave
it hidden but any meaningful edit re-surfaces it. See
[suppressions.md](suppressions.md).

### `dedup suppressions <action>`

Manage the set of currently dismissed groups.

- `dedup suppressions list [PATH]` — print every dismissed entry
  (hex hash, timestamp, last-known group id). In `--format json`
  mode emits one JSON object per line (see **Schemas** below).
- `dedup suppressions clear [PATH]` — truncate the suppressions
  table. Prints the row count. Irreversible — previously hidden
  groups re-surface on the next scan/list.

### `dedup clean [PATH] [--yes|-y]`

Delete the entire `<PATH>/.dedup/` directory. Prompting policy:

- If `--yes` / `-y` is passed, no prompt.
- Otherwise, when stdin is a TTY, prompt `[y/N]` (default: No).
- Otherwise (non-TTY without `--yes`): refuse with exit 2 so scripts
  don't hang on stdin.

If `.dedup/` does not exist, the command is a no-op and prints a
friendly "nothing to clean" message.

## Exit codes

Source of truth: `crates/dedup-cli/src/main.rs` (`ExitCode::from(..)`
call sites and the module-level rustdoc).

| Code | Meaning |
|---|---|
| `0` | Success. Findings may be present — git-style. |
| `1` | Findings present **and** `--strict` was passed. |
| `2` | Usage, parse, or user-actionable error: clap parse failure; invalid config file; schema-version mismatch on cache or config; no cached scan found; unknown group id; refusal to delete without `--yes`; editor launch failure. |
| `101` | Rust panic (default `panic = "unwind"` behaviour; dedup does not override it). |

## Output formats

`--format` selects among three renderers. Shapes are stable contracts.

### Text (default on a TTY)

Byte-for-byte identical to the pre-#12 layout. `scan` and `list`
emit one header line per group followed by one line per occurrence:

```
--- [A] group 3 (4 occurrences) ---
crates/foo/src/lib.rs:10-42
crates/bar/src/helper.rs:5-37
```

`show` uses the cached group id as the header number rather than
the ordinal. `suppressions list` uses a hash / timestamp /
last-group-id row format.

### JSON (default off a TTY)

Stream-friendly NDJSON for `scan` / `list` / `suppressions list`;
one self-describing JSON object for `show`. Shapes are defined in
`crates/dedup-cli/src/output.rs`.

**`GroupJson` shape:**

```json
{
  "id": 42,
  "ordinal": 1,
  "tier": "A",
  "hash": "00000000deadbeef",
  "occurrence_count": 3,
  "occurrences": [
    {
      "path": "src/a.rs",
      "start_line": 10,
      "end_line": 42,
      "start_byte": 120,
      "end_byte": 980
    }
  ]
}
```

Notes:

- `id` is present only for cache-backed groups (`list` / `show`);
  live `scan` output omits it.
- `ordinal` is a 1-based index within the current stream.
- `hash` is a zero-padded 16-char lowercase hex string.
- `start_byte` / `end_byte` are omitted (via
  `skip_serializing_if = "Option::is_none"`) when unavailable.
- Paths use forward slashes for cross-platform stability.

**`SuppressionJson` shape:**

```json
{
  "hash": "00000000deadbeef",
  "dismissed_at": 1712345678,
  "last_group_id": 7
}
```

- `dismissed_at` is Unix-epoch seconds.
- `last_group_id` may be absent when the referenced group has been
  replaced by a subsequent scan.

### SARIF

SARIF 2.1.0. `scan`, `list`, and `show` emit a single SARIF log (one
`results[]` entry per duplicate group). Consumable by GitHub Code
Scanning and any other SARIF ingestor. The vendored schema used by
the test suite lives at
`crates/dedup-cli/tests/fixtures/sarif-2.1.0.json` — that file is
the canonical shape dedup's output is validated against.

## Environment variables

- `RUST_LOG` — standard `tracing_subscriber` filter. When set,
  overrides both the default (`warn`) and `--verbose`
  (`dedup=debug`). Writes to stderr.
- `EDITOR`, `VISUAL` — consulted by `dedup config edit` (in that
  order). Falls back to `vi`.
- `XDG_CONFIG_HOME`, `HOME` — control global-config resolution:
  `$XDG_CONFIG_HOME/dedup/config.toml` when set, otherwise
  `$HOME/.config/dedup/config.toml` on Unix. See [config.md](config.md).
