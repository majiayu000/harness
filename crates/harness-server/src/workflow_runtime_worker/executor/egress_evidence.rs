use harness_core::agent::{AgentEgressMode, AGENT_ISOLATION_TIER_ENV, AGENT_NETWORK_ALLOWLIST_ENV};
use harness_core::config::agents::AgentPermissionMode;
use harness_core::types::{Item, TurnStatus};
use harness_workflow::runtime::ActivityArtifact;
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AgentEgressEvidence {
    mode: AgentEgressMode,
    network_allowlist: Vec<String>,
    isolation_tier: String,
}

impl AgentEgressEvidence {
    pub(super) fn from_spawn_env(
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
            mode: AgentEgressMode::resolve(permission_mode, &network_allowlist),
            network_allowlist,
            isolation_tier,
        }
    }

    pub(super) fn artifact(&self, status: TurnStatus, items: &[Item]) -> ActivityArtifact {
        let verification_result = match self.mode {
            AgentEgressMode::DenyAll | AgentEgressMode::Unrestricted => {
                EgressVerificationResult::NotRequired
            }
            AgentEgressMode::FirstPartyProxy if has_egress_error(items) => {
                EgressVerificationResult::Failed
            }
            AgentEgressMode::FirstPartyProxy if status == TurnStatus::Completed => {
                EgressVerificationResult::VerifiedAtDispatch
            }
            AgentEgressMode::FirstPartyProxy => EgressVerificationResult::Unverified,
        };
        ActivityArtifact::new(
            "agent_egress_enforcement",
            json!({
                "mode": self.mode,
                "verification_result": verification_result,
                "network_allowlist": self.network_allowlist,
                "isolation_tier": self.isolation_tier,
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
        permission_mode: AgentPermissionMode,
        env_vars: HashMap<String, String>,
        status: TurnStatus,
        items: &[Item],
    ) -> Value {
        AgentEgressEvidence::from_spawn_env(permission_mode, &env_vars)
            .artifact(status, items)
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
                AgentPermissionMode::Scoped,
                env_vars,
                TurnStatus::Completed,
                &[],
            ),
            json!({
                "mode": "first_party_proxy",
                "verification_result": "verified_at_dispatch",
                "network_allowlist": ["api.openai.com", "github.com"],
                "isolation_tier": "container",
            })
        );
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
        }];

        assert_eq!(
            artifact_value(
                AgentPermissionMode::Scoped,
                env_vars,
                TurnStatus::Failed,
                &items,
            )["verification_result"],
            "failed"
        );
    }

    #[test]
    fn unrelated_proxy_turn_failure_does_not_claim_verification() {
        let env_vars = HashMap::from([(
            AGENT_NETWORK_ALLOWLIST_ENV.to_string(),
            "api.openai.com".to_string(),
        )]);

        assert_eq!(
            artifact_value(
                AgentPermissionMode::Scoped,
                env_vars,
                TurnStatus::Failed,
                &[Item::Error {
                    code: -1,
                    message: "agent executable was not found".to_string(),
                }],
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
            let artifact = artifact_value(permission_mode, HashMap::new(), TurnStatus::Failed, &[]);
            assert_eq!(artifact["mode"], expected_mode);
            assert_eq!(artifact["verification_result"], "not_required");
            assert_eq!(artifact["isolation_tier"], "host");
        }
    }
}
