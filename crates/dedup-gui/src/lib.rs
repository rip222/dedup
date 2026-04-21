//! GPUI-based macOS frontend for dedup.
//!
//! The crate compiles as empty on non-macOS targets via the `cfg` gate
//! below; the real GPUI integration lands in a later milestone.
#![cfg(target_os = "macos")]
