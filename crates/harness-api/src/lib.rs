//! Stable public API surface for Harness.
//!
//! This crate provides a single import point for consumers of the Harness
//! ecosystem. It re-exports stable interfaces from the underlying crates,
//! shielding consumers from internal reorganization.
//!
//! # Example
//!
//! ```
//! use std::path::Path;
//!
//! use harness_api::core::SessionId;
//! use harness_api::exec::plan::ExecPlan;
//!
//! let _session = SessionId::new();
//! let _plan = ExecPlan::from_spec("# Demo", Path::new(".")).unwrap();
//! ```
//!
//! # Modules
//!
//! - [`core`] — domain types, agent traits, ID types, error types
//! - [`protocol`] — JSON-RPC wire types and codec (all RPC methods)
//! - [`sandbox`] — sandboxing types and `wrap_command`
//! - [`exec`] — execution plan types for spec-driven workflows
pub mod core {
    pub use harness_core::agent;
    pub use harness_core::config;
    pub use harness_core::error;
    pub use harness_core::types;
    pub use harness_core::{
        AutoFixAttempt, AutoFixReport, Decision, Event, EventFilters, ExternalSignal, OtelExporter,
        RuleId, SessionId, Severity,
    };
}

pub mod protocol {
    pub use harness_protocol::codec;
    pub use harness_protocol::contract;
    pub use harness_protocol::methods;
    pub use harness_protocol::notifications;
    pub use harness_protocol::{
        AGENT_ERROR, INTERNAL_ERROR, INVALID_PARAMS, INVALID_REQUEST, METHOD_NOT_FOUND,
        PARSE_ERROR, STORAGE_ERROR, VALIDATION_ERROR,
    };
}

pub mod sandbox {
    pub use harness_sandbox::{wrap_command, SandboxEngine, SandboxSpec, WrappedCommand};
}

pub mod exec {
    pub use harness_exec::markdown;
    pub use harness_exec::plan;
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn core_facade_exposes_common_types() {
        let id = core::SessionId::new();
        assert!(!id.as_str().is_empty());

        let _mode = core::config::agents::SandboxMode::ReadOnly;
        let _severity = core::Severity::High;
    }

    #[test]
    fn protocol_facade_exposes_constants() {
        assert_eq!(protocol::INTERNAL_ERROR, harness_protocol::INTERNAL_ERROR);
        assert_eq!(
            protocol::METHOD_NOT_FOUND,
            harness_protocol::METHOD_NOT_FOUND
        );
    }

    #[test]
    fn sandbox_facade_exposes_types() {
        let spec = sandbox::SandboxSpec::new(core::config::agents::SandboxMode::ReadOnly, ".");
        assert!(spec.project_root.is_absolute());
        assert_eq!(sandbox::SandboxEngine::None, sandbox::SandboxEngine::None);
    }

    #[test]
    fn exec_facade_exposes_plan_api() {
        let plan = exec::plan::ExecPlan::from_spec("# Demo", Path::new(".")).expect("plan");
        assert_eq!(plan.purpose, "Demo");
    }
}
