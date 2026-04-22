# Suppressions and dismissals

Groups you don't want to see again can be dismissed. Dismissals live
in the cache (`<repo>/.dedup/cache.sqlite`) and are keyed by the
group's **normalized-block-hash** — the same `group_hash` value
stored on `match_groups`.

Implementation: `crates/dedup-core/src/cache.rs`. CLI entry points
live in `crates/dedup-cli/src/main.rs`.

## Two dismissal granularities

### Group-level (`suppressions` table)

Hides the entire group from future reports. Tables: `suppressions`.

- CLI: `dedup dismiss <ID>`.
- GUI: the sidebar's "x" button or the `x` keyboard shortcut on a
  focused group.

The group disappears from `scan`, `list`, `show` (still prints if
directly addressed), and GUI output.

### Per-occurrence (`occurrence_suppressions` table)

Hides one file's occurrence inside a group. Tables:
`occurrence_suppressions`, keyed by `(group_hash, file_path)`.

- GUI: per-occurrence checkboxes in the detail-pane toolbar.
- CLI: no direct command today (exposed in the GUI only at MVP).

If the per-occurrence suppression leaves the group with fewer than
two remaining occurrences, the whole group drops from the output —
the "duplicate" has no pair left.

## Why key by hash

The hash is computed over the **normalised** token stream — see
[config.md](config.md) for the `conservative` vs. `aggressive`
modes. That means:

- Whitespace-only edits do not change the hash. A dismissed group
  **stays hidden** through cosmetic reformats.
- A meaningful edit (changing an identifier that's `Kept`, a kept
  literal under conservative mode, etc.) changes the hash. The
  group **re-surfaces** — you get to reconsider whether the
  duplicate still matters.

This is the intended contract: dismissals are "I've reviewed this
exact code and accept it", not "never bother me about anything in
this file".

## Inspecting

```sh
dedup suppressions list
```

Text output (one row per dismissal):

```
00000000deadbeef  dismissed_at=1712345678  last_group_id=42
```

With `--format json`, NDJSON (one JSON object per line). See the
**SuppressionJson** shape in [cli.md](cli.md#output-formats).

## Clearing

```sh
dedup suppressions clear
```

Removes every group-level dismissal. Irreversible — previously
hidden groups re-surface on the next scan/list. Prints the number of
rows removed.

Per-occurrence suppressions have a separate
`clear_occurrence_suppressions` API on `Cache` but no CLI entry
point at MVP.

## Reviewing & restoring in the GUI (#54)

The sidebar's **Dismissed** section is interactive:

- Clicking a row selects that dismissed group and opens its
  occurrences in the detail pane. The pane renders a read-only
  toolbar and a banner ("Dismissed on YYYY-MM-DD — [Restore]")
  instead of the usual dismiss / copy / open actions so the user
  can review the suppressed code without accidentally re-mutating
  it.
- The banner enumerates any per-occurrence dismissals still
  attached to the group, each with its own `[Restore]` control.
- Each sidebar row carries an inline `[Restore]` button that
  undoes the group-level dismissal in one click — this is
  equivalent to selecting the row and pressing the banner's
  `[Restore]`.

Restoring a group deletes its suppression row (and any
per-occurrence rows still attached to the same hash), so the group
re-appears in the active sidebar/CLI list on the next list /
reload. The core-layer primitives are
[`Cache::undismiss`](../crates/dedup-core/src/cache.rs) and
[`Cache::undismiss_occurrence`]; the GUI routing lives in
[`dedup-gui::suppressions_view`](../crates/dedup-gui/src/suppressions_view.rs).

## Persistence

All suppressions live in `<repo>/.dedup/cache.sqlite`. Deleting the
cache directory (via `dedup clean`, or manually) clears every
dismissal for that repo. The file is checked into `.gitignore`
automatically by dedup (the `.dedup/` directory is seeded with a
single-line `*` `.gitignore` on first create).

## Last-known group id

The suppressions table stores a `last_group_id` breadcrumb — the id
that was named when `dedup dismiss <ID>` was invoked. It is **not**
a foreign key: subsequent scans may renumber groups. The field is
informational so `dedup suppressions list` has something to echo;
the canonical key is always the hash.
