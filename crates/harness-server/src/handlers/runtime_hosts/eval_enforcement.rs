use super::*;

pub(crate) fn eval_resource_limit_enforcement_for_job(
    job: &RuntimeJob,
) -> Result<Option<CappedResourceLimits>, String> {
    let Some(eval) = eval_metadata(&job.input) else {
        return Ok(None);
    };
    if let Some(value) = eval.get("resource_limits") {
        if value.get("requested").is_some() && value.get("effective").is_some() {
            return serde_json::from_value(value.clone())
                .map(Some)
                .map_err(|error| format!("invalid eval resource_limits: {error}"));
        }
        let requested: ResourceLimits = serde_json::from_value(value.clone())
            .map_err(|error| format!("invalid eval resource_limits: {error}"))?;
        return requested
            .cap_by(ResourceLimits::operator_default_maxima())
            .map(Some)
            .map_err(|error| format!("invalid eval resource_limits: {error}"));
    }
    let timeout_secs = eval
        .get("timeout_secs")
        .and_then(Value::as_u64)
        .filter(|value| *value > 0)
        .ok_or_else(|| {
            "eval runtime job must include timeout_secs or resource_limits".to_string()
        })?;
    ResourceLimits::evaluation_defaults(timeout_secs)
        .cap_by(ResourceLimits::operator_default_maxima())
        .map(Some)
        .map_err(|error| format!("invalid eval resource_limits: {error}"))
}

pub(super) fn eval_network_policy_enforcement_for_job(
    job: &RuntimeJob,
) -> Result<Option<EvalNetworkPolicy>, String> {
    let Some(eval) = eval_metadata(&job.input) else {
        return Ok(None);
    };
    if let Some(value) = eval.get("network_policy") {
        return parse_eval_network_policy(value).map(Some);
    }
    let network_allowlist = job
        .input
        .pointer("/isolation/network_allowlist")
        .or_else(|| eval.pointer("/isolation/network_allowlist"))
        .map(|value| {
            serde_json::from_value::<Vec<String>>(value.clone())
                .map_err(|error| format!("invalid eval network_allowlist: {error}"))
        })
        .transpose()?
        .unwrap_or_default();
    EvalNetworkPolicy::for_allowlist(&network_allowlist)
        .map(Some)
        .map_err(|error| format!("invalid eval network policy: {error}"))
}

pub(crate) fn applied_eval_network_policy(
    job: &RuntimeJob,
) -> Result<Option<EvalNetworkPolicy>, String> {
    let Some(eval) = eval_metadata(&job.input) else {
        return Ok(None);
    };
    let Some(value) = eval.get("network_policy") else {
        return Ok(None);
    };
    parse_eval_network_policy(value).map(Some)
}

fn parse_eval_network_policy(value: &Value) -> Result<EvalNetworkPolicy, String> {
    let policy: EvalNetworkPolicy = serde_json::from_value(value.clone())
        .map_err(|error| format!("invalid eval network_policy: {error}"))?;
    policy
        .revalidated()
        .map_err(|error| format!("invalid eval network_policy: {error}"))
}

pub(crate) fn eval_metadata(input: &Value) -> Option<&Value> {
    input
        .pointer("/command/eval")
        .or_else(|| input.get("eval"))
        .filter(|value| value.is_object())
}

pub(super) fn set_eval_resource_limit_enforcement(
    job: &mut RuntimeJob,
    resource_limits: &CappedResourceLimits,
) {
    let value = json!(resource_limits);
    if let Some(eval) = job
        .input
        .pointer_mut("/command/eval")
        .and_then(Value::as_object_mut)
    {
        eval.insert("resource_limits".to_string(), value.clone());
    }
    if let Some(eval) = job.input.get_mut("eval").and_then(Value::as_object_mut) {
        eval.insert("resource_limits".to_string(), value);
    }
}

pub(super) fn set_eval_network_policy_enforcement(
    job: &mut RuntimeJob,
    network_policy: &EvalNetworkPolicy,
) {
    let value = json!(network_policy);
    if let Some(eval) = job
        .input
        .pointer_mut("/command/eval")
        .and_then(Value::as_object_mut)
    {
        eval.insert("network_policy".to_string(), value.clone());
    }
    if let Some(eval) = job.input.get_mut("eval").and_then(Value::as_object_mut) {
        eval.insert("network_policy".to_string(), value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_sandbox::EvalNetworkAccess;
    use harness_workflow::runtime::RuntimeKind;

    fn eval_job_with_network_policy(policy: Value) -> RuntimeJob {
        RuntimeJob::pending(
            "cmd-claim-network-policy",
            RuntimeKind::RemoteHost,
            "remote-host-default",
            json!({
                "activity": "implement_issue",
                "command": {
                    "eval": {
                        "eval_run_id": "run-1",
                        "case_id": "case-1",
                        "timeout_secs": 45,
                        "network_policy": policy,
                    }
                }
            }),
        )
    }

    #[test]
    fn claim_path_rejects_injected_hex_numeric_allowlist_aliases() {
        let job = eval_job_with_network_policy(json!({
            "inbound": "allowlist",
            "outbound": "allowlist",
            "network_allowlist": ["0x7f.0.0.1"],
        }));
        let err = eval_network_policy_enforcement_for_job(&job)
            .expect_err("hex numeric alias injection must fail closed at claim");
        assert!(
            err.contains("invalid eval network_policy"),
            "unexpected error: {err}"
        );
        let applied_err = applied_eval_network_policy(&job)
            .expect_err("completion path must also reject injected aliases");
        assert!(
            applied_err.contains("invalid eval network_policy"),
            "unexpected error: {applied_err}"
        );
    }

    #[test]
    fn claim_path_renormalizes_deserialized_network_policy() {
        let job = eval_job_with_network_policy(json!({
            "inbound": "allowlist",
            "outbound": "deny",
            "network_allowlist": [" API.GitHub.COM. "],
        }));
        let policy = eval_network_policy_enforcement_for_job(&job)
            .expect("valid DNS hosts should renormalize")
            .expect("eval jobs must produce a policy");
        assert_eq!(policy.inbound, EvalNetworkAccess::Deny);
        assert_eq!(policy.outbound, EvalNetworkAccess::Allowlist);
        assert_eq!(policy.network_allowlist, vec!["api.github.com".to_string()]);
    }
}
