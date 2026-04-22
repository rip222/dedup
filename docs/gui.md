# GUI guide

The `dedup-gui` app is a native macOS application built on GPUI. It
wraps the same scan pipeline as the CLI. The sources of truth for
this page are:

- `crates/dedup-gui/src/menubar.rs` — menu items, actions, and
  keyboard shortcuts.
- `crates/dedup-gui/src/project_view.rs` — sidebar, detail, and
  toolbar behaviour.
- `crates/dedup-gui/src/app_state.rs` — scan state machine and data
  model.

macOS-only at MVP.

## Launching

```sh
cargo run --release -p dedup-gui
# or, after a release build:
./target/release/dedup-gui
```

`dedup-gui --smoke-test` boots the GPUI `Application`, then exits 0
without opening a window. This is what CI runs; it does not require
a display.

## Opening a project

**File → Open… (⌘O)** presents the native NSOpenPanel for
directory selection. Selecting a folder loads it as the active
project — any cached groups from a prior scan appear immediately in
the sidebar. The project root is remembered in the MRU list (up to
five entries, cap at `MAX_RECENTS` in `crates/dedup-gui/src/recent.rs`).

**File → Open Recent →** shows up to five entries plus a "Clear
Menu" item. A stale entry (the directory no longer exists) surfaces
an inline banner with "Remove from recents" / "Dismiss" actions.

## Scanning

**Scan → Start Scan (⌘R)** kicks off a scan on the active project.
The sidebar populates with Tier A groups as they stream in; Tier B
groups follow. A progress bar shows the file count + group count
during the run.

**Scan → Stop Scan (⌘.)** cooperatively cancels an in-flight scan.
Partial results remain visible. The cache is not updated on cancel.

On completion the sidebar shows a summary banner. If any files
produced issues during the scan, a "N files had issues" link opens
a dialog with the breakdown and a "Copy details" button that writes
a GitHub-issue-ready markdown block to the clipboard.

## Layout

The main window is split into two resizable panes.

### Sidebar (left)

- Search box at the top (⌘F focuses it).
- Group list sorted by the selected sort key (see "Sort options"
  below). Default is **Severity** for fresh installs; a pref file
  from a prior version keeps its previously-persisted key.
- Each row shows: tier badge, occurrence count, summary of paths,
  and a dismiss button.
- Per-run filter controls let you hide Tier A, Tier B, or dismissed
  groups.

#### Sort options

The `Sort: <key>` button in the sidebar opens a dropdown (issue #46)
that lists every variant of
[`SortKey`](../crates/dedup-gui/src/app_state.rs). Selecting an option
re-sorts the list in place and persists the choice to `sidebar.json`
so it survives a window reopen.

| Key | Meaning |
|---|---|
| Severity (default) | `occurrence_count × total_duplicated_LOC` summed across occurrences — surfaces the highest-leverage dupes first. Ties fall back to path-alphabetical for stability. |
| Impact | Alias-style ordering preserved from the streaming buffer (match size × occurrence count). Kept for users who already rely on it. |
| File count | Descending by distinct file paths across the group's occurrences. |
| Line count | Descending by total duplicated line count. |
| Alphabetical | A → Z by the row label. |
| Recently dismissed last | Main list behaves like Alphabetical; the Dismissed section puts the most recent dismissal last. |

Existing users upgrading from a pre-#56 build keep their previous
"Impact" ordering — the load path treats a missing `sort_key` field
in `sidebar.json` as "stay on Impact" rather than force-flipping them
to Severity. Brand-new installs (no `sidebar.json` yet) start on
Severity.

### Detail pane (right — "stacked view")

The detail pane stacks every occurrence of the selected group
vertically. Each occurrence renders as a syntax-highlighted snippet
with:

- A file-path header that opens in the configured editor on click.
- Configurable context lines above and below the duplicated span
  (`detail.context_lines` in config; default 3).
- Per-occurrence checkboxes + a dismiss button on the toolbar for
  dismissing single occurrences without suppressing the whole group.
- Tier B groups show alpha-rename diff tinting on normalised
  identifiers (issue #25).

Click the toolbar's "Open in editor" button (or press `o` with a
group focused) to launch the editor configured by the
`[editor]` section of `config.toml`. See [editor.md](editor.md).

## Keyboard shortcuts

Sourced directly from the `SHORTCUTS` table in
`crates/dedup-gui/src/menubar.rs`.

### Application

| Shortcut | Action |
|---|---|
| ⌘, | Preferences… |
| ⌘Q | Quit Dedup |
| ⌘W | Close Window |

### File

| Shortcut | Action |
|---|---|
| ⌘O | Open… (folder picker) |

### Scan

| Shortcut | Action |
|---|---|
| ⌘R | Start Scan |
| ⌘. | Stop Scan |

### View / Sidebar

| Shortcut | Action |
|---|---|
| ⌘B | Toggle Sidebar — hide / show the sidebar; detail pane expands to fill the window while hidden. Visibility persists across window close + reopen (stored in `sidebar.json` next to the resizable width). |
| ⌘1 | Focus Sidebar |
| ⌘2 | Focus Detail |
| ⌘F | Find in Sidebar (focus search box) |

### Sidebar keyboard navigation

These fire while the sidebar has focus. Bound at app level but
consumed by the sidebar wrapper so they don't swallow keys inside
text inputs elsewhere.

| Shortcut | Action |
|---|---|
| `j` or ↓ | Next group |
| `k` or ↑ | Previous group |
| Enter | Activate — focus the detail pane |
| `x` | Dismiss the currently-selected group |
| `o` | Open the selected group's checked files in the editor |

### Detail-pane toolbar

Not keybound globally — surfaced via mouse/touch through the toolbar
chrome. Includes "Collapse all" / "Expand all" buttons (issue #27).

### Toast overlay

| Shortcut | Action |
|---|---|
| Esc | Dismiss the top-most toast (no-op if none) |

## Tooltips on icon buttons

Every icon button in the GUI — the per-occurrence `×`, the per-group
`Dismiss group` / `Copy paths` / close `×` row, the sort dropdown,
the per-toast `×` — carries a short delayed-hover tooltip that names
the exact action. Tooltips appear after ~500 ms of hover (GPUI's
built-in delay; issue #53) and disappear as soon as the cursor
leaves. Examples:

- Per-occurrence `×` → "Dismiss this occurrence"
- Per-group toolbar `×` → "Close detail pane"
- Per-group `Dismiss group` → "Dismiss this group (hide from sidebar)"
- `Copy path` → "Copy path to clipboard"
- Sort dropdown → "Sort groups"
- Toast `×` → "Dismiss notification"

Source: `crates/dedup-gui/src/tooltip.rs` exposes a single
`with_tooltip(element, text)` wrapper that any `.id(...)`-ed element
can opt into. Convention: whenever a new icon button lands, wrap it
in `with_tooltip` so the user never has to guess which `×` does
what.

## Preferences dialog (⌘,)

The Preferences overlay surfaces an "Edit config file…" action that
opens the resolved config TOML in `$EDITOR`. The full preset picker
is deferred; for now this is the documented flow to change the
editor preset or any other config key. See [config.md](config.md).

## Logs

The GUI installs a daily-rolling JSON log appender that writes to
`~/.config/dedup/logs/dedup.log.YYYY-MM-DD`. Rotation keeps the
seven most recent files. See [troubleshooting.md](troubleshooting.md).
