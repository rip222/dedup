# Contributing a new `LanguageProfile`

Tier B (language-aware) detection is driven by implementations of
the `LanguageProfile` trait in
`crates/dedup-lang/src/profile.rs`. This page is the contributor
entry point for adding support for a new language.

Concrete references in this crate:

- `crates/dedup-lang/src/rust.rs` — the Rust profile.
- `crates/dedup-lang/src/typescript.rs` — the TypeScript + TSX
  profile, demonstrating context-aware classification via
  `classify_node`.
- `crates/dedup-lang/src/python.rs` — the Python profile, with
  decorator-aware rename handling.

## Trait shape

Full rustdoc lives on the trait itself
(`crates/dedup-lang/src/profile.rs`). The required methods are:

| method | purpose |
|---|---|
| `name() -> &'static str` | Human-readable name for logs and errors. |
| `extensions() -> &[&'static str]` | File extensions claimed, without a leading dot (e.g. `["rs"]`). Matched case-sensitively. |
| `tree_sitter_language() -> tree_sitter::Language` | The `LANGUAGE` constant from the grammar crate. |
| `syntactic_units() -> &[&'static str]` | Tree-sitter node kinds that count as candidate subtrees (e.g. `"function_item"`, `"impl_item"`). |
| `rename_class(node_kind: &str) -> RenameClass` | Per-kind classification — see below. |
| `classify_node(node: &Node<'_>) -> RenameClass` (default) | Context-aware override for languages where the same leaf kind appears in must-keep and must-rename positions (e.g. TSX `identifier`). |

## The `RenameClass` policy

Every leaf node is classified as one of:

- `Local` — alpha-renamed to a stable alias (`v1`, `v2`, …) based
  on order of first occurrence within the unit. This is how Tier B
  becomes rename-resilient: two functions differing only in local
  variable names normalise to the same token stream.
- `Kept` — preserved verbatim. Function names, type names, imported
  items, macro names — anything whose identity is load-bearing.
- `Literal` — string, numeric, or other literal. Under
  `normalization = "conservative"` literals are kept verbatim;
  under `"aggressive"` they are replaced with the
  `AGGRESSIVE_LITERAL_PLACEHOLDER` (`<LIT>`) token.

When in doubt, return `RenameClass::Kept`. Over-keeping is safe
(fewer false positives); over-renaming can produce false positives
that are hard to explain to users.

## Checklist for a new profile

1. **Add the grammar** — a `tree-sitter-<lang>` crate to
   `crates/dedup-lang/Cargo.toml`.
2. **Create `crates/dedup-lang/src/<lang>.rs`** with:
   - A unit struct implementing `LanguageProfile`.
   - A `pub static <LANG>_PROFILE: <Lang>Profile` constant.
3. **Register the profile** by appending `&<LANG>_PROFILE` to the
   `all_profiles()` vector in `crates/dedup-lang/src/lib.rs`.
4. **Re-export** from `lib.rs` (`pub use <lang>::{…, <LANG>_PROFILE}`).
5. **Add the CLI filter variant** — extend the `Language` enum in
   `crates/dedup-cli/src/main.rs` and the `ext_matches_lang_filter`
   table so `--lang <new>` works.
6. **Fixtures + snapshots** — ship at least one small source file
   under `crates/dedup-lang/tests/fixtures/<lang>/` with an
   insta-snapshot test that extracts syntactic units and asserts
   their normalised form. The existing `rust`, `typescript`, and
   `python` fixtures are the reference layout.
7. **Wire `scanner.rs` if your language needs custom filtering** —
   in most cases the generic Tier B path in
   `crates/dedup-core/src/scanner.rs` handles new profiles
   automatically via `profile_for_extension`.

## Testing

Run the relevant targeted tests before opening a PR:

```sh
cargo test -p dedup-lang
cargo test -p dedup-core --test normalization_modes
```

The snapshot diffs should be reviewed by hand; `cargo insta review`
is the conventional UI.

## Examples to study

- **Rust (`rust.rs`)** — simple: Rust's grammar uses distinct leaf
  kinds for bindings vs. call targets, so the default
  `classify_node` (falling through to `rename_class`) is enough.
- **TypeScript (`typescript.rs`)** — more complex: plain
  `identifier` appears as both a binding and a JSX tag name. The
  profile overrides `classify_node` to walk the parent chain and
  disambiguate.
- **Python (`python.rs`)** — decorator-aware: decorators are
  treated as `Kept` so `@staticmethod` doesn't collapse with
  `@classmethod`.

## Performance notes

Profiles are `Send + Sync` and singleton (`&'static dyn
LanguageProfile`). The scanner shares one reference across all
workers. Avoid interior mutability; avoid locking. If you need
per-scan state, thread it through the scanner's own state machine
rather than adding methods to the trait.
