//! Stable public API surface for Harness.
//!
//! This crate provides a single import point for consumers of the Harness
//! ecosystem. It curates a stable subset of interfaces from the underlying
//! crates, shielding consumers from internal reorganization.
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
    //! Curated stable exports from `harness-core`.

    pub mod agent {
        pub use harness_core::agent::{
            AgentAdapter, AgentEvent, AgentRequest, AgentResponse, ApprovalDecision, CodeAgent,
            StreamItem, TaskClassification, TaskComplexity, TurnRequest,
        };
    }

    pub mod config {
        pub mod agents {
            pub use harness_core::config::agents::{
                AgentReviewConfig, AgentsConfig, AnthropicApiConfig, ApprovalPolicy,
                CapabilityProfile, ClaudeAgentConfig, CodexAgentConfig, CodexCloudConfig,
                SandboxMode,
            };
        }

        pub mod intake {
            pub use harness_core::config::intake::{
                FeishuIntakeConfig, GitHubIntakeConfig, GitHubRepoConfig, IntakeConfig,
            };
        }

        pub mod misc {
            pub use harness_core::config::misc::{
                ConcurrencyConfig, GcConfig, ObserveConfig, OtelConfig, OtelExporter, ReviewConfig,
                ReviewStrategy, RulesConfig, SignalThresholdsConfig, ValidationConfig,
                WorkspaceConfig,
            };
        }

        pub mod server {
            pub use harness_core::config::server::{ServerConfig, Transport};
        }

        pub use harness_core::config::{HarnessConfig, ProjectEntry};
    }

    pub mod error {
        pub use harness_core::error::{
            Error, HarnessError, Result, SandboxError, TaskDbDecodeError,
        };
    }

    pub mod types {
        pub use harness_core::types::{
            AgentId, AutoFixAttempt, AutoFixReport, Capability, ContextItem, Decision, Event,
            EventFilters, ExecPlanId, ExecPlanStatus, ExecutionPhase, ExternalSignal, Grade, Item,
            Language, ReasoningBudget, RuleId, SessionId, Severity, SkillId, TaskId, ThreadId,
            TokenUsage, TurnId, Violation,
        };
    }

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
    fn core_facade_exposes_curated_common_types() {
        let id = core::SessionId::new();
        assert!(!id.as_str().is_empty());

        let _mode = core::config::agents::SandboxMode::ReadOnly;
        let _severity = core::types::Severity::High;
        let _capability = core::types::Capability::Read;
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

    #[test]
    fn core_facade_supports_agent_consumer_paths() {
        fn accept_result(_: core::error::Result<()>) {}
        fn accept_capability(_: core::types::Capability) {}

        accept_result(Ok(()));
        accept_capability(core::types::Capability::Execute);

        let request = core::agent::AgentRequest::default();
        assert!(request.uses_dangerously_skip_permissions());
    }
}
