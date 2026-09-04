use harness_core::agent::{AgentEgressMode, AGENT_ISOLATION_TIER_ENV, AGENT_NETWORK_ALLOWLIST_ENV};
use harness_core::config::agents::AgentPermissionMode;
use harness_core::types::Item;
use harness_workflow::runtime::{ActivityArtifact, ActivityErrorKind, ActivityResult, RuntimeKind};
use serde::Serialize;
use serde_json::json;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum EgressVerificationResult {
    VerifiedAtDispatch,
    Failed,
    Unverified,
    NotRequired,
    NotApplicable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum RecordedEgressMode {
    DenyAll,
    FirstPartyProxy,
    Unrestricted,
    NotApplicable,
}

impl From<AgentEgressMode> for RecordedEgressMode {
    fn from(mode: AgentEgressMode) -> Self {
        match mode {
            AgentEgressMode::DenyAll => Self::DenyAll,
            AgentEgressMode::FirstPartyProxy => Self::FirstPartyProxy,
            AgentEgressMode::Unrestricted => Self::Unrestricted,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AgentEgressEvidence {
    runtime_kind: RuntimeKind,
    mode: RecordedEgressMode,
    network_allowlist: Vec<String>,
    isolation_tier: String,
}

impl AgentEgressEvidence {
    pub(super) fn from_spawn_env(
        runtime_kind: RuntimeKind,
        permission_mode: AgentPermissionMode,
        env_vars: &HashMap<String, String>,
    ) -> Self {
        let network_allowlist = env_vars
            .get(AGENT_NETWORK_ALLOWLIST_ENV)
            .map(String::as_str)
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>();
        let isolation_tier = env_vars
            .get(AGENT_ISOLATION_TIER_ENV)
            .map(String::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("host")
            .to_string();
        Self {
            runtime_kind,
            mode: if runtime_kind == RuntimeKind::AnthropicApi {
                RecordedEgressMode::NotApplicable
            } else {
                AgentEgressMode::resolve(permission_mode, &network_allowlist).into()
            },
            network_allowlist,
            isolation_tier,
        }
    }

    pub(super) fn validate_provider_connectivity(&self, backend_name: &str) -> anyhow::Result<()> {
        if backend_name == "codex"
            && matches!(
                self.runtime_kind,
                RuntimeKind::CodexExec | RuntimeKind::CodexJsonrpc
            )
            && self.mode == RecordedEgressMode::DenyAll
        {
            anyhow::bail!(
                "scoped Codex runtime cannot reach its model provider with deny-all egress; configure exact provider hosts in isolation.network_allowlist"
            );
        }
        Ok(())
    }

    pub(super) fn provider_connectivity_failure(
        &self,
        activity: &str,
        backend_name: &str,
    ) -> Option<ActivityResult> {
        self.validate_provider_connectivity(backend_name)
            .err()
            .map(|error| {
                ActivityResult::failed(
                    activity,
                    "Runtime provider connectivity preflight failed.",
                    error.to_string(),
                )
                .with_error_kind(ActivityErrorKind::Configuration)
            })
    }

    pub(super) fn artifact(
        &self,
        items: &[Item],
        verified_at_dispatch: bool,
        attempt: u32,
    ) -> ActivityArtifact {
        let verification_result = match self.mode {
            RecordedEgressMode::NotApplicable => EgressVerificationResult::NotApplicable,
            RecordedEgressMode::DenyAll | RecordedEgressMode::Unrestricted => {
                EgressVerificationResult::NotRequired
            }
            RecordedEgressMode::FirstPartyProxy if verified_at_dispatch => {
                EgressVerificationResult::VerifiedAtDispatch
            }
            RecordedEgressMode::FirstPartyProxy if has_egress_error(items) => {
                EgressVerificationResult::Failed
            }
            RecordedEgressMode::FirstPartyProxy => EgressVerificationResult::Unverified,
        };
        ActivityArtifact::new(
            "agent_egress_enforcement",
            json!({
                "attempt": attempt,
                "mode": self.mode,
                "verification_result": verification_result,
                "network_allowlist": self.network_allowlist,
                "isolation_tier": self.isolation_tier,
                "runtime_kind": self.runtime_kind,
            }),
        )
    }
}

fn has_egress_error(items: &[Item]) -> bool {
    items.iter().any(|item| {
        matches!(
            item,
            Item::Error { message, .. } if message.to_ascii_lowercase().contains("egress")
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn artifact_value(
        runtime_kind: RuntimeKind,
        permission_mode: AgentPermissionMode,
        env_vars: HashMap<String, String>,
        items: &[Item],
        verified_at_dispatch: bool,
    ) -> Value {
        AgentEgressEvidence::from_spawn_env(runtime_kind, permission_mode, &env_vars)
            .artifact(items, verified_at_dispatch, 1)
            .artifact
    }

    #[test]
    fn completed_allowlisted_turn_records_verified_proxy_evidence() {
        let env_vars = HashMap::from([
            (
                AGENT_NETWORK_ALLOWLIST_ENV.to_string(),
                "api.openai.com, github.com".to_string(),
            ),
            (
                AGENT_ISOLATION_TIER_ENV.to_string(),
                "container".to_string(),
            ),
        ]);

        assert_eq!(
            artifact_value(
                RuntimeKind::CodexJsonrpc,
                AgentPermissionMode::Scoped,
                env_vars,
                &[],
                true,
            ),
            json!({
                "attempt": 1,
                "mode": "first_party_proxy",
                "verification_result": "verified_at_dispatch",
                "network_allowlist": ["api.openai.com", "github.com"],
                "isolation_tier": "container",
                "runtime_kind": "codex_jsonrpc",
            })
        );
    }

    #[test]
    fn scoped_codex_turn_rejects_empty_provider_allowlist_before_launch() {
        let evidence = AgentEgressEvidence::from_spawn_env(
            RuntimeKind::CodexExec,
            AgentPermissionMode::Scoped,
            &HashMap::new(),
        );

        let result = evidence
            .provider_connectivity_failure("run_local_review", "codex")
            .expect("scoped Codex cannot reach its provider through deny-all egress");

        assert_eq!(
            result.status,
            harness_workflow::runtime::ActivityStatus::Failed
        );
        assert_eq!(
            result.error_kind,
            Some(harness_workflow::runtime::ActivityErrorKind::Configuration)
        );
        assert!(result
            .error
            .as_deref()
            .is_some_and(|error| error.contains("network_allowlist")));
    }

    #[test]
    fn codex_runtime_profile_does_not_reclassify_a_simulated_backend() {
        let evidence = AgentEgressEvidence::from_spawn_env(
            RuntimeKind::CodexJsonrpc,
            AgentPermissionMode::Scoped,
            &HashMap::new(),
        );

        assert!(evidence
            .provider_connectivity_failure("implement_issue", "runtime-stream-agent")
            .is_none());
    }

    #[test]
    fn proxy_setup_error_records_failed_verification() {
        let env_vars = HashMap::from([(
            AGENT_NETWORK_ALLOWLIST_ENV.to_string(),
            "api.openai.com".to_string(),
        )]);
        let items = [Item::Error {
            code: -1,
            message: "first-party egress proxy did not become healthy".to_string(),
            failure_kind: None,
        }];

        assert_eq!(
            artifact_value(
                RuntimeKind::CodexJsonrpc,
                AgentPermissionMode::Scoped,
                env_vars,
                &items,
                false,
            )["verification_result"],
            "failed"
        );
    }

    #[test]
    fn unrelated_proxy_turn_failure_preserves_dispatch_verification() {
        let env_vars = HashMap::from([(
            AGENT_NETWORK_ALLOWLIST_ENV.to_string(),
            "api.openai.com".to_string(),
        )]);

        assert_eq!(
            artifact_value(
                RuntimeKind::CodexJsonrpc,
                AgentPermissionMode::Scoped,
                env_vars,
                &[Item::Error {
                    code: -1,
                    message: "agent protocol closed unexpectedly".to_string(),
                    failure_kind: None,
                }],
                true,
            )["verification_result"],
            "verified_at_dispatch"
        );
    }

    #[test]
    fn runtime_egress_error_preserves_dispatch_verification() {
        let env_vars = HashMap::from([(
            AGENT_NETWORK_ALLOWLIST_ENV.to_string(),
            "api.openai.com".to_string(),
        )]);

        assert_eq!(
            artifact_value(
                RuntimeKind::CodexJsonrpc,
                AgentPermissionMode::Scoped,
                env_vars,
                &[Item::Error {
                    code: -1,
                    message: "egress proxy connection closed during the turn".to_string(),
                    failure_kind: None,
                }],
                true,
            )["verification_result"],
            "verified_at_dispatch"
        );
    }

    #[test]
    fn proxy_spawn_failure_remains_unverified() {
        let env_vars = HashMap::from([(
            AGENT_NETWORK_ALLOWLIST_ENV.to_string(),
            "api.openai.com".to_string(),
        )]);

        assert_eq!(
            artifact_value(
                RuntimeKind::CodexJsonrpc,
                AgentPermissionMode::Scoped,
                env_vars,
                &[Item::Error {
                    code: -1,
                    message: "agent executable was not found".to_string(),
                    failure_kind: None,
                }],
                false,
            )["verification_result"],
            "unverified"
        );
    }

    #[test]
    fn non_proxy_modes_do_not_require_canary_verification() {
        for (permission_mode, expected_mode) in [
            (AgentPermissionMode::Scoped, "deny_all"),
            (AgentPermissionMode::Full, "unrestricted"),
        ] {
            let artifact = artifact_value(
                RuntimeKind::CodexJsonrpc,
                permission_mode,
                HashMap::new(),
                &[],
                false,
            );
            assert_eq!(artifact["mode"], expected_mode);
            assert_eq!(artifact["verification_result"], "not_required");
            assert_eq!(artifact["isolation_tier"], "host");
        }
    }

    #[test]
    fn in_process_anthropic_api_does_not_claim_spawn_egress_enforcement() {
        let env_vars = HashMap::from([(
            AGENT_NETWORK_ALLOWLIST_ENV.to_string(),
            "api.anthropic.com".to_string(),
        )]);

        let artifact = artifact_value(
            RuntimeKind::AnthropicApi,
            AgentPermissionMode::Scoped,
            env_vars,
            &[],
            false,
        );

        assert_eq!(artifact["mode"], "not_applicable");
        assert_eq!(artifact["verification_result"], "not_applicable");
        assert_eq!(artifact["runtime_kind"], "anthropic_api");
    }
}
