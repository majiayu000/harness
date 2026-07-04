//! Runtime-owned eval primitives.
//!
//! This module is additive groundwork for GH-1447. Eval execution will dispatch
//! through the normal workflow runtime; this module only owns manifest parsing
//! and deterministic scoring primitives.

pub mod manifest;
pub mod model;
pub mod scoring;

pub use manifest::{
    parse_benchmark_manifest_str, EvalBenchmarkCase, EvalBenchmarkManifest, ManifestError,
    DEFAULT_CASE_TIMEOUT_SECS,
};
pub use scoring::{score_pr_repair_eval, ScoringError};
