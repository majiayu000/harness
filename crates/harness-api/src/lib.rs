//! Stable public API surface for Harness.
//!
//! This crate provides a single import point for consumers of the Harness
//! ecosystem. It re-exports stable interfaces from the underlying crates,
//! shielding consumers from internal reorganization.
//!
//! # Modules
//!
//! - [`core`] — domain types, agent traits, ID types, error types
//! - [`protocol`] — JSON-RPC wire types and codec (all RPC methods)
//! - [`sandbox`] — sandboxing types and `wrap_command`
//! - [`exec`] — execution plan types for spec-driven workflows

pub mod core {}

pub mod protocol {}

pub mod sandbox {}

pub mod exec {}
