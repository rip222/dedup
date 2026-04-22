//! Shared bootstrap helpers for `dedup-cli` integration tests.
//!
//! These helpers are copied verbatim (aside from rustdoc) from what used
//! to live at the top of every single test file under
//! `crates/dedup-cli/tests/`. Collapsing them into one place was flagged
//! by dedup's own dogfood scan (issue #32).
//!
//! Each test binary in `tests/` is compiled independently, so a helper
//! used by only a subset of binaries will look "dead" to rustc/clippy in
//! the others. `#[allow(dead_code)]` per item is the standard workaround.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

use assert_cmd::Command;

/// The workspace root, i.e. the directory that contains `Cargo.toml`
/// (the virtual-manifest one), `crates/`, and `fixtures/`.
pub fn workspace_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); // crates/dedup-cli -> crates
    p.pop(); // crates           -> workspace root
    p
}

/// Recursively copy `src` to `dst`.
pub fn copy_tree(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let ty = entry.file_type().unwrap();
        let target = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_tree(&entry.path(), &target);
        } else if ty.is_file() {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

/// Build an `assert_cmd::Command` wired to a sandboxed HOME and empty
/// XDG_CONFIG_HOME so `Config::global_path()` resolves to
/// `<home>/.config/dedup/config.toml`.
pub fn dedup_with_home(home: &Path) -> Command {
    let mut cmd = Command::cargo_bin("dedup").unwrap();
    cmd.env("HOME", home).env_remove("XDG_CONFIG_HOME");
    cmd
}
