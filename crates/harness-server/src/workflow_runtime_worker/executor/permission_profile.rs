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
}

impl RuntimePermissionProfile {
    pub(super) fn resolve(
        permission_mode: AgentPermissionMode,
        allowed_tools: Option<Vec<String>>,
        tool_allowlist_enforcement: ToolAllowlistEnforcement,
        correction_only: bool,
    ) -> Self {
        if correction_only {
            return Self {
                permission_mode: AgentPermissionMode::Scoped,
                allowed_tools: Some(Vec::new()),
                tool_allowlist_enforcement,
                correction_only: true,
            };
        }
        Self {
            permission_mode,
            allowed_tools,
            tool_allowlist_enforcement,
            correction_only: false,
        }
    }

    pub(super) fn artifact(
        &self,
        configured_capability_profile: CapabilityProfile,
    ) -> ActivityArtifact {
        ActivityArtifact::new(
            "agent_permission_profile",
            json!({
                "configured_capability_profile": configured_capability_profile,
                "permission_mode": self.permission_mode,
                "allowed_tools": self.allowed_tools,
                "tool_allowlist_enforcement": self.tool_allowlist_enforcement,
                "correction_only": self.correction_only,
            }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn correction_retry_is_scoped_deny_all() {
        let profile = RuntimePermissionProfile::resolve(
            AgentPermissionMode::Full,
            None,
            ToolAllowlistEnforcement::NotEnforcedByHarness,
            true,
        );

        assert_eq!(profile.permission_mode, AgentPermissionMode::Scoped);
        assert_eq!(profile.allowed_tools, Some(Vec::new()));
        assert_eq!(
            profile.artifact(CapabilityProfile::Full).artifact,
            json!({
                "configured_capability_profile": "full",
                "permission_mode": "scoped",
                "allowed_tools": [],
                "tool_allowlist_enforcement": "not_enforced_by_harness",
                "correction_only": true,
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
        );

        assert_eq!(profile.permission_mode, AgentPermissionMode::Scoped);
        assert_eq!(profile.allowed_tools, tools);
    }
}
