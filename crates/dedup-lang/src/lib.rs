//! Language profiles for dedup.
//!
//! Houses the generic Tier A profile and per-language Tier B profiles
//! (Rust, TypeScript, Python at MVP) as modules.
//!
//! The public surface is the [`LanguageProfile`] trait plus a small
//! registry of `&'static dyn LanguageProfile` entries accessible via
//! [`all_profiles`] and [`profile_for_extension`].
//!
//! ```no_run
//! use dedup_lang::{all_profiles, profile_for_extension};
//!
//! for p in all_profiles() {
//!     println!("{}: {:?}", p.name(), p.extensions());
//! }
//!
//! if let Some(p) = profile_for_extension("rs") {
//!     assert_eq!(p.name(), "rust");
//! }
//! ```
//!
//! Adding a new language means implementing the trait in its own
//! module and inserting a reference into [`all_profiles`]. The
//! Scanner in `dedup-core` picks up new profiles automatically.

pub mod normalize;
pub mod profile;
pub mod python;
pub mod rust;
pub mod typescript;

pub use normalize::{NormalizedToken, SyntacticUnit, extract_units, hash_tokens, normalize};
pub use profile::{LanguageProfile, RenameClass};
pub use python::{PYTHON_PROFILE, PythonProfile};
pub use rust::{RUST_PROFILE, RustProfile};
pub use typescript::{TSX_PROFILE, TYPESCRIPT_PROFILE, TsxProfile, TypeScriptProfile};

/// Every profile shipped with this build. Order is stable but
/// arbitrary; callers should not rely on it.
///
/// The returned slice holds `'static` references so it can be cached
/// by the caller. Adding a new profile only requires appending to this
/// function.
pub fn all_profiles() -> Vec<&'static dyn LanguageProfile> {
    vec![
        &RUST_PROFILE,
        &TYPESCRIPT_PROFILE,
        &TSX_PROFILE,
        &PYTHON_PROFILE,
    ]
}

/// Look up the profile that claims `ext` (the extension without a
/// leading dot, e.g. `"rs"`), or `None` if no profile matches.
///
/// Matching is case-sensitive and checks each profile in turn; the
/// first hit wins. With only one registered profile today this is
/// trivial; if we ever grow to a dozen we can switch to a map. Keep
/// the interface stable so callers don't break.
pub fn profile_for_extension(ext: &str) -> Option<&'static dyn LanguageProfile> {
    all_profiles()
        .into_iter()
        .find(|profile| profile.extensions().contains(&ext))
}
