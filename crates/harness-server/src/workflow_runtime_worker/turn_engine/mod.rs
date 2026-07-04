//! Workflow-runtime owned agent turn engine.
//!
//! This module contains the live per-turn execution path used by runtime jobs
//! and interactive turns. The legacy task path keeps its own helper copy until
//! GH-1434 removes that path.

pub(crate) mod helpers;
pub(crate) mod turn_lifecycle;

#[cfg(test)]
mod stall_tests;

#[cfg(test)]
mod terminal_error_tests;
