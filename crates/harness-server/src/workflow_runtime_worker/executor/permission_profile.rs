use harness_core::config::agents::{AgentPermissionMode, CapabilityProfile};
use harness_workflow::runtime::ActivityArtifact;
use serde_json::json;

use super::super::runtime_profile::{ResolvedRuntimeSettings, ToolAllowlistEnforcement};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RuntimePermissionProfile {
    pub(super) permission_mode: AgentPermissionMode,
    pub(super) allowed_tools: Option<Vec<String>>,
    tool_allowlist_enforcement: ToolAllowlistEnforcement,
    correction_only: bool,
    classifier_only: bool,
}

impl RuntimePermissionProfile {
    pub(super) fn preflight_classifier(
        settings: &ResolvedRuntimeSettings,
        classifier_only: bool,
    ) -> anyhow::Result<()> {
        if classifier_only {
            Self::resolve(
                settings.permission_mode,
                settings.allowed_tools.clone(),
                settings.tool_allowlist_enforcement,
                false,
                true,
            )?;
        }
        Ok(())
    }

    pub(super) fn resolve(
        permission_mode: AgentPermissionMode,
        allowed_tools: Option<Vec<String>>,
        tool_allowlist_enforcement: ToolAllowlistEnforcement,
        correction_only: bool,
        classifier_only: bool,
    ) -> anyhow::Result<Self> {
        if classifier_only && !tool_allowlist_enforcement.enforces_empty_allowlist() {
            anyhow::bail!(
                "runtime backend cannot mechanically enforce the deny-all tool policy required by classifier turns"
            );
        }
        if classifier_only && !tool_allowlist_enforcement.supports_classifier_model_attestation() {
            anyhow::bail!(
                "runtime backend cannot report the executed model identity required by classifier turns"
            );
        }
        if correction_only && !tool_allowlist_enforcement.supports_correction_denylist() {
            anyhow::bail!(
                    "runtime backend cannot enforce the deny-all tool policy required by correction turns"
                );
        }
        if correction_only || classifier_only {
            return Ok(Self {
                // Keep the original mode for egress resolution. An explicit
                // empty tool list prevents Full from enabling unrestricted
                // agent tools while still allowing provider network access.
                permission_mode,
                allowed_tools: Some(Vec::new()),
                tool_allowlist_enforcement,
                correction_only,
                classifier_only,
            });
        }
        Ok(Self {
            permission_mode,
            allowed_tools,
            tool_allowlist_enforcement,
            correction_only: false,
            classifier_only: false,
        })
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
        )
        .expect_err("unsupported deny-all enforcement must fail closed");

        assert!(profile.to_string().contains("cannot enforce"));
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
        )
        .expect("Claude enforces explicit tool allowlists");

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
        )
        .expect("Claude enforces explicit tool allowlists");

        assert_eq!(profile.permission_mode, AgentPermissionMode::Scoped);
        assert_eq!(profile.allowed_tools, Some(Vec::new()));
    }

    #[test]
    fn classifier_turn_denies_all_tools() {
        let profile = RuntimePermissionProfile::resolve(
            AgentPermissionMode::Full,
            None,
            ToolAllowlistEnforcement::ClaudeCli,
            false,
            true,
        )
        .expect("Claude deny-all flags enforce classifier isolation");

        assert_eq!(profile.permission_mode, AgentPermissionMode::Full);
        assert_eq!(profile.allowed_tools, Some(Vec::new()));
        assert_eq!(
            profile.artifact(CapabilityProfile::Full, 1).artifact["classifier_only"],
            true
        );
    }

    #[test]
    fn classifier_rejects_codex_feature_denylist() {
        let error = RuntimePermissionProfile::resolve(
            AgentPermissionMode::Scoped,
            None,
            ToolAllowlistEnforcement::CodexCliFeatureDenylist,
            false,
            true,
        )
        .expect_err("a finite feature denylist must not claim complete classifier isolation");

        assert!(error.to_string().contains("classifier turns"));
    }

    #[test]
    fn classifier_rejects_backend_without_reported_model_identity() {
        let error = RuntimePermissionProfile::resolve(
            AgentPermissionMode::Scoped,
            None,
            ToolAllowlistEnforcement::OpenCodePermissionEnv,
            false,
            true,
        )
        .expect_err("classifier attestation requires a backend-reported model identity");

        assert!(error.to_string().contains("executed model identity"));
    }
}
