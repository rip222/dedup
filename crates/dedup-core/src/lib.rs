//! Core detection, normalization, and persistence primitives for dedup.
//!
//! This crate is frontend-agnostic; the CLI and GUI both depend on it.
//!
//! At this milestone the public API is a single Tier A scanner:
//!
//! ```no_run
//! use dedup_core::{Scanner, ScanConfig};
//! use std::path::Path;
//!
//! let scanner = Scanner::new(ScanConfig::default());
//! let result = scanner.scan(Path::new(".")).expect("scan");
//! for group in &result.groups {
//!     println!("{} occurrences", group.occurrences.len());
//! }
//! ```
//!
//! The three deep modules (`tokenizer`, `rolling_hash`, `scanner`) are
//! each testable in isolation. Later milestones add cache (#4), tree-sitter
//! Tier B (#6), `ignore`-crate layers (#5), parallelism (#14), and so on.

pub mod cache;
pub mod config;
pub mod editor;
pub mod ignore;
pub mod rolling_hash;
pub mod scanner;
pub mod tokenizer;

pub use cache::{
    Cache, CacheError, CachedBlocks, CachedOccurrence, FileFingerprint, GroupDetail, GroupSummary,
    Suppression,
};
pub use config::{
    Config, ConfigError, DetailConfig, Normalization, SCHEMA_VERSION as CONFIG_SCHEMA_VERSION,
    ScanSettings, Thresholds, TierAThresholds, TierBThresholds,
};
pub use editor::{
    CommandSpec, EditorConfig, EditorError, EditorPreset, EnvPathLookup, PathLookup,
    ResolvedEditor, TerminalMode, build_commands, launch, resolve_preset,
};
pub use ignore::{IgnoreRules, IgnoreRulesOptions};
pub use rolling_hash::{Hash, Span};
pub use scanner::{
    AtomicProgressSink, FileIssue, FileIssueCounts, FileIssueKind, MatchGroup, NoopSink,
    Occurrence, ProgressSink, ScanConfig, ScanError, ScanResult, Scanner, Tier,
    TierAStreamCallback,
};
pub use tokenizer::{Token, TokenKind};
