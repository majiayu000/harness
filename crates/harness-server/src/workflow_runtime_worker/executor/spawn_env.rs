use harness_core::agent::{AGENT_ISOLATION_TIER_ENV, AGENT_NETWORK_ALLOWLIST_ENV};
use harness_workflow::runtime::RuntimeJob;
use serde_json::Value;
use std::collections::HashMap;

pub(super) fn isolation_spawn_env_vars(job: &RuntimeJob) -> HashMap<String, String> {
    let mut env_vars = HashMap::new();
    let Some(isolation) = job.input.get("isolation").and_then(Value::as_object) else {
        return env_vars;
    };
    if let Some(tier) = isolation
        .get("tier")
        .and_then(Value::as_str)
        .filter(|tier| !tier.trim().is_empty())
    {
        env_vars.insert(AGENT_ISOLATION_TIER_ENV.to_string(), tier.to_string());
    }
    let allowlist = isolation
        .get("network_allowlist")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_default();
    if !allowlist.is_empty() {
        env_vars.insert(AGENT_NETWORK_ALLOWLIST_ENV.to_string(), allowlist);
    }
    harness_core::agent::inherit_agent_spawn_control_env(&mut env_vars);
    env_vars
}

pub(super) fn correction_spawn_env_vars(job: &RuntimeJob) -> HashMap<String, String> {
    // Correction turns deny every agent tool through RuntimePermissionProfile;
    // keep the original egress route so the CLI can still reach its provider.
    isolation_spawn_env_vars(job)
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_workflow::runtime::RuntimeKind;
    use serde_json::json;

    fn runtime_job() -> RuntimeJob {
        RuntimeJob::pending(
            "command-1",
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({ "activity": "implement_issue" }),
        )
    }

    #[test]
    fn isolation_spawn_env_vars_extracts_tier_and_allowlist() {
        let mut job = runtime_job();
        job.input = json!({
            "activity": "implement_issue",
            "isolation": {
                "tier": "container",
                "trust_class": "non_collaborator",
                "network_allowlist": ["github.com", " api.com ", ""],
            }
        });

        let env_vars = isolation_spawn_env_vars(&job);

        assert_eq!(env_vars[AGENT_ISOLATION_TIER_ENV], "container");
        assert_eq!(env_vars[AGENT_NETWORK_ALLOWLIST_ENV], "github.com,api.com");
    }

    #[test]
    fn correction_spawn_env_preserves_provider_allowlist() {
        let mut job = runtime_job();
        job.input = json!({
            "activity": "implement_issue",
            "isolation": {
                "tier": "container",
                "network_allowlist": ["api.openai.com", "github.com"],
            }
        });

        let env_vars = correction_spawn_env_vars(&job);

        assert_eq!(env_vars[AGENT_ISOLATION_TIER_ENV], "container");
        assert_eq!(
            env_vars[AGENT_NETWORK_ALLOWLIST_ENV],
            "api.openai.com,github.com"
        );
    }
}
