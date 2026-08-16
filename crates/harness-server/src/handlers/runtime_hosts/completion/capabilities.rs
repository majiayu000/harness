use super::*;

pub(super) fn validate_eval_host_capabilities(
    state: &AppState,
    host_id: &str,
    job: &RuntimeJob,
) -> Result<(), serde_json::Value> {
    let Some(host) = state.runtime_hosts.hosts.get(host_id) else {
        return Err(json!({"error": "runtime host is not registered"}));
    };
    let missing = missing_required_eval_capabilities(&job.input, &host.capabilities);
    if missing.is_empty() {
        Ok(())
    } else {
        Err(json!({
            "error": "runtime host no longer advertises required eval capabilities",
            "missing_capabilities": missing,
        }))
    }
}

fn missing_required_eval_capabilities(input: &Value, supported: &[String]) -> Vec<String> {
    input
        .get("eval")
        .or_else(|| input.pointer("/command/eval"))
        .and_then(|eval| eval.get("required_runtime_host_capabilities"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|required| !supported.iter().any(|capability| capability == required))
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_rejects_a_host_that_lost_the_trusted_verifier_capability() {
        let input = json!({
            "command": {
                "eval": {
                    "required_runtime_host_capabilities": [
                        "eval_resource_limits",
                        "trusted_eval_verifier_v1"
                    ]
                }
            }
        });
        let supported = vec!["eval_resource_limits".to_string()];

        assert_eq!(
            missing_required_eval_capabilities(&input, &supported),
            vec!["trusted_eval_verifier_v1"]
        );
    }
}
