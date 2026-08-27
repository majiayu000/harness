use harness_core::config::agents::{AgentPermissionMode, CapabilityProfile};
use harness_workflow::runtime::ActivityArtifact;
use serde_json::json;

use super::super::runtime_profile::ToolAllowlistEnforcement;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RuntimePermissionProfile {
    pub(super) permission_mode: AgentPermissionMode,
    pub(super) allowed_tools: Option<Vec<String>>,
    tool_allowlist_enforcement: ToolAllowlistEnforcement,
    correction_only: bool,
    classifier_only: bool,
}

impl RuntimePermissionProfile {
    pub(super) fn resolve(
        permission_mode: AgentPermissionMode,
        allowed_tools: Option<Vec<String>>,
        tool_allowlist_enforcement: ToolAllowlistEnforcement,
        correction_only: bool,
        classifier_only: bool,
    ) -> Self {
        if classifier_only {
            return Self {
                // Preserve provider egress. The explicit empty tool list and
                // runtime-specific enforcement disable the model's tools;
                // the CLI process still needs to reach its model provider.
                permission_mode,
                allowed_tools: Some(Vec::new()),
                tool_allowlist_enforcement,
                correction_only,
                classifier_only,
            };
        }
        if correction_only {
            return Self {
                // Keep the original mode for egress resolution. An explicit
                // empty tool list prevents Full from enabling unrestricted
                // agent tools while still allowing provider network access.
                permission_mode,
                allowed_tools: Some(Vec::new()),
                tool_allowlist_enforcement,
                correction_only,
                classifier_only,
            };
        }
        Self {
            permission_mode,
            allowed_tools,
            tool_allowlist_enforcement,
            correction_only: false,
            classifier_only: false,
        }
    }

    pub(super) fn artifact(
        &self,
        configured_capability_profile: CapabilityProfile,
        attempt: u32,
    ) -> ActivityArtifact {
        ActivityArtifact::new(
            "agent_permission_profile",
            json!({
                "attempt": attempt,
                "configured_capability_profile": configured_capability_profile,
                "permission_mode": self.permission_mode,
                "allowed_tools": self.allowed_tools,
                "tool_allowlist_enforcement": self.tool_allowlist_enforcement,
                "correction_only": self.correction_only,
                "classifier_only": self.classifier_only,
            }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_correction_retry_preserves_egress_mode_and_denies_tools() {
        let profile = RuntimePermissionProfile::resolve(
            AgentPermissionMode::Full,
            None,
            ToolAllowlistEnforcement::NotEnforcedByHarness,
            true,
            false,
        );

        assert_eq!(profile.permission_mode, AgentPermissionMode::Full);
        assert_eq!(profile.allowed_tools, Some(Vec::new()));
        assert_eq!(
            profile.artifact(CapabilityProfile::Full, 1).artifact,
            json!({
                "attempt": 1,
                "configured_capability_profile": "full",
                "permission_mode": "full",
                "allowed_tools": [],
                "tool_allowlist_enforcement": "not_enforced_by_harness",
                "correction_only": true,
                "classifier_only": false,
            })
        );
    }

    #[test]
    fn ordinary_turn_preserves_resolved_permissions() {
        let tools = Some(vec!["Read".to_string(), "Bash".to_string()]);
        let profile = RuntimePermissionProfile::resolve(
            AgentPermissionMode::Scoped,
            tools.clone(),
            ToolAllowlistEnforcement::ClaudeCli,
            false,
            false,
        );

        assert_eq!(profile.permission_mode, AgentPermissionMode::Scoped);
        assert_eq!(profile.allowed_tools, tools);
    }

    #[test]
    fn scoped_correction_retry_remains_scoped_deny_all() {
        let profile = RuntimePermissionProfile::resolve(
            AgentPermissionMode::Scoped,
            Some(vec!["Read".to_string()]),
            ToolAllowlistEnforcement::ClaudeCli,
            true,
            false,
        );

        assert_eq!(profile.permission_mode, AgentPermissionMode::Scoped);
        assert_eq!(profile.allowed_tools, Some(Vec::new()));
    }

    #[test]
    fn classifier_turn_forces_an_empty_tool_allowlist() {
        let profile = RuntimePermissionProfile::resolve(
            AgentPermissionMode::Full,
            None,
            ToolAllowlistEnforcement::ClaudeCli,
            false,
            true,
        );

        assert_eq!(profile.permission_mode, AgentPermissionMode::Full);
        assert_eq!(profile.allowed_tools, Some(Vec::new()));
        assert!(profile.classifier_only);
        assert!(!profile.correction_only);
    }
}
