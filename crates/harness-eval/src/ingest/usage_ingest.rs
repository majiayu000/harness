use crate::model::{Confidence, UsageSnapshot};
use serde_json::Value;

use super::{json_u64, optional_string_field};

pub(super) fn usage_snapshots_from_values(
    submission: Option<&Value>,
    task_detail: Option<&Value>,
    fallback_workflow_id: Option<&str>,
) -> Vec<UsageSnapshot> {
    let mut snapshots = Vec::new();
    for value in task_detail.into_iter().chain(submission) {
        for usage in usage_snapshots_from_value(value, fallback_workflow_id) {
            if !snapshots.contains(&usage) {
                snapshots.push(usage);
            }
        }
    }
    snapshots
}

fn usage_snapshots_from_value(
    value: &Value,
    fallback_workflow_id: Option<&str>,
) -> Vec<UsageSnapshot> {
    usage_arrays(value)
        .into_iter()
        .flat_map(|array| array.iter())
        .filter_map(|record| usage_snapshot_from_record(record, fallback_workflow_id))
        .collect()
}

fn usage_arrays(value: &Value) -> Vec<&Vec<Value>> {
    [
        "usage",
        "usage_snapshots",
        "usageSnapshots",
        "llm_usage",
        "llmUsage",
        "llm_usage_events",
        "llmUsageEvents",
    ]
    .into_iter()
    .filter_map(|field| value.get(field).and_then(Value::as_array))
    .collect()
}

fn usage_snapshot_from_record(
    record: &Value,
    fallback_workflow_id: Option<&str>,
) -> Option<UsageSnapshot> {
    let parsed_content = record.get("content").and_then(|content| {
        content
            .as_str()
            .and_then(|content| serde_json::from_str::<Value>(content).ok())
            .or_else(|| {
                if content.is_object() {
                    Some(content.clone())
                } else {
                    None
                }
            })
    });
    let payload = parsed_content
        .as_ref()
        .map(payload_value)
        .or_else(|| record.get("payload").map(payload_value))
        .or_else(|| record.get("usage").map(payload_value))
        .unwrap_or(record);
    let mut token_sources = vec![payload, record];
    let mut metadata_sources = vec![record];
    if let Some(content) = parsed_content.as_ref() {
        token_sources.push(content);
        metadata_sources.push(content);
    }
    metadata_sources.push(payload);

    let input_tokens = optional_u64_from_sources(&token_sources, &["input_tokens", "inputTokens"]);
    let output_tokens =
        optional_u64_from_sources(&token_sources, &["output_tokens", "outputTokens"]);
    let additive_cache_tokens = token_sources
        .iter()
        .find_map(|source| additive_cache_token_sum(source));
    let cached_input_tokens = optional_u64_from_sources(
        &token_sources,
        &["cached_input_tokens", "cachedInputTokens"],
    )
    .or(additive_cache_tokens);
    let component_total = component_token_total(input_tokens, output_tokens, additive_cache_tokens);
    let reported_total_tokens = optional_u64_from_sources(
        &token_sources,
        &[
            "total_tokens",
            "totalTokens",
            "reported_total_tokens",
            "reportedTotalTokens",
        ],
    );
    let total_tokens = max_token_total(reported_total_tokens, component_total);
    let cost_usd_micros = optional_u64_from_sources(
        &token_sources,
        &["cost_usd_micros", "estimated_cost_usd_micros"],
    );

    if input_tokens.is_none()
        && output_tokens.is_none()
        && cached_input_tokens.is_none()
        && total_tokens.is_none()
        && cost_usd_micros.is_none()
    {
        return None;
    }

    Some(UsageSnapshot {
        agent_invocation_id: optional_string_from_sources(
            &metadata_sources,
            &["agent_invocation_id", "agentInvocationId"],
        ),
        runtime_job_id: optional_string_from_sources(
            &metadata_sources,
            &["runtime_job_id", "runtimeJobId"],
        ),
        workflow_id: optional_string_from_sources(
            &metadata_sources,
            &["workflow_id", "workflowId"],
        )
        .or_else(|| fallback_workflow_id.map(ToString::to_string)),
        model: optional_string_from_sources(&metadata_sources, &["model"]),
        reasoning_effort: optional_string_from_sources(
            &metadata_sources,
            &["reasoning_effort", "reasoningEffort"],
        ),
        input_tokens,
        output_tokens,
        cached_input_tokens,
        total_tokens,
        cost_usd_micros,
        token_confidence: confidence_from_sources(
            &metadata_sources,
            &["token_confidence", "tokenConfidence"],
        )
        .unwrap_or_else(|| {
            if total_tokens.is_some() {
                Confidence::Exact
            } else {
                Confidence::Unknown
            }
        }),
        cost_confidence: confidence_from_sources(
            &metadata_sources,
            &["cost_confidence", "costConfidence"],
        )
        .unwrap_or(Confidence::Unknown),
    })
}

fn additive_cache_token_sum(value: &Value) -> Option<u64> {
    let read = optional_u64_from_sources(
        &[value],
        &["cache_read_input_tokens", "cacheReadInputTokens"],
    )
    .unwrap_or(0);
    let create = optional_u64_from_sources(
        &[value],
        &["cache_creation_input_tokens", "cacheCreationInputTokens"],
    )
    .unwrap_or(0);
    if read > 0 || create > 0 {
        Some(read.saturating_add(create))
    } else {
        None
    }
}

fn payload_value(value: &Value) -> &Value {
    value
        .get("payload")
        .or_else(|| value.get("usage"))
        .unwrap_or(value)
}

fn component_token_total(
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    additive_cache_tokens: Option<u64>,
) -> Option<u64> {
    let input_tokens = input_tokens?;
    let output_tokens = output_tokens?;
    Some(
        input_tokens
            .saturating_add(output_tokens)
            .saturating_add(additive_cache_tokens.unwrap_or(0)),
    )
}

fn max_token_total(reported: Option<u64>, component: Option<u64>) -> Option<u64> {
    match (reported, component) {
        (Some(reported), Some(component)) => Some(reported.max(component)),
        (Some(reported), None) => Some(reported),
        (None, Some(component)) => Some(component),
        (None, None) => None,
    }
}

fn optional_u64_field(value: &Value, field: &str) -> Option<u64> {
    value.get(field).and_then(json_u64)
}

fn optional_u64_from_sources(sources: &[&Value], fields: &[&str]) -> Option<u64> {
    sources.iter().find_map(|source| {
        fields
            .iter()
            .find_map(|field| optional_u64_field(source, field))
    })
}

fn optional_string_from_sources(sources: &[&Value], fields: &[&str]) -> Option<String> {
    sources.iter().find_map(|source| {
        fields
            .iter()
            .find_map(|field| optional_string_field(source, field))
    })
}

fn confidence_field(value: &Value, field: &str) -> Option<Confidence> {
    match value
        .get(field)
        .and_then(Value::as_str)?
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "exact" => Some(Confidence::Exact),
        "estimated" => Some(Confidence::Estimated),
        "observed" => Some(Confidence::Observed),
        "unknown" => Some(Confidence::Unknown),
        _ => None,
    }
}

fn confidence_from_sources(sources: &[&Value], fields: &[&str]) -> Option<Confidence> {
    sources.iter().find_map(|source| {
        fields
            .iter()
            .find_map(|field| confidence_field(source, field))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn preserves_canonical_usage_from_task_detail() {
        let task_detail = json!({
            "usage": [{
                "agent_invocation_id": "agent-1",
                "runtime_job_id": "job-1",
                "workflow_id": "workflow-1",
                "model": "codex-test",
                "reasoning_effort": "medium",
                "input_tokens": 100,
                "output_tokens": 50,
                "cached_input_tokens": 10,
                "total_tokens": 160,
                "cost_usd_micros": 123,
                "token_confidence": "exact",
                "cost_confidence": "estimated"
            }]
        });

        let usage = usage_snapshots_from_values(None, Some(&task_detail), None);

        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].agent_invocation_id.as_deref(), Some("agent-1"));
        assert_eq!(usage[0].runtime_job_id.as_deref(), Some("job-1"));
        assert_eq!(usage[0].workflow_id.as_deref(), Some("workflow-1"));
        assert_eq!(usage[0].total_tokens, Some(160));
        assert_eq!(usage[0].cost_usd_micros, Some(123));
        assert_eq!(usage[0].token_confidence, Confidence::Exact);
        assert_eq!(usage[0].cost_confidence, Confidence::Estimated);
    }

    #[test]
    fn maps_llm_usage_event_payloads_to_usage() {
        let task_detail = json!({
            "llm_usage_events": [{
                "content": "{\"agent\":\"codex\",\"model\":\"gpt-test\",\"input_tokens\":40,\"output_tokens\":5,\"cache_read_input_tokens\":3,\"cache_creation_input_tokens\":2,\"reported_total_tokens\":50}"
            }]
        });

        let usage = usage_snapshots_from_values(None, Some(&task_detail), Some("workflow-1"));

        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].workflow_id.as_deref(), Some("workflow-1"));
        assert_eq!(usage[0].model.as_deref(), Some("gpt-test"));
        assert_eq!(usage[0].input_tokens, Some(40));
        assert_eq!(usage[0].output_tokens, Some(5));
        assert_eq!(usage[0].cached_input_tokens, Some(5));
        assert_eq!(usage[0].total_tokens, Some(50));
        assert_eq!(usage[0].cost_usd_micros, None);
        assert_eq!(usage[0].token_confidence, Confidence::Exact);
        assert_eq!(usage[0].cost_confidence, Confidence::Unknown);
    }

    #[test]
    fn maps_nested_usage_event_content_to_usage() {
        let task_detail = json!({
            "llm_usage_events": [{
                "content": "{\"usage\":{\"input_tokens\":10,\"output_tokens\":2,\"total_tokens\":12}}"
            }]
        });

        let usage = usage_snapshots_from_values(None, Some(&task_detail), Some("workflow-1"));

        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].workflow_id.as_deref(), Some("workflow-1"));
        assert_eq!(usage[0].input_tokens, Some(10));
        assert_eq!(usage[0].output_tokens, Some(2));
        assert_eq!(usage[0].total_tokens, Some(12));
    }

    #[test]
    fn preserves_metadata_from_content_envelope_with_nested_usage() {
        let task_detail = json!({
            "llm_usage_events": [{
                "content": "{\"workflow_id\":\"workflow-from-content\",\"runtime_job_id\":\"job-from-content\",\"model\":\"gpt-test\",\"reasoning_effort\":\"medium\",\"usage\":{\"input_tokens\":10,\"output_tokens\":2,\"total_tokens\":12}}"
            }]
        });

        let usage =
            usage_snapshots_from_values(None, Some(&task_detail), Some("fallback-workflow"));

        assert_eq!(usage.len(), 1);
        assert_eq!(
            usage[0].workflow_id.as_deref(),
            Some("workflow-from-content")
        );
        assert_eq!(usage[0].runtime_job_id.as_deref(), Some("job-from-content"));
        assert_eq!(usage[0].model.as_deref(), Some("gpt-test"));
        assert_eq!(usage[0].reasoning_effort.as_deref(), Some("medium"));
        assert_eq!(usage[0].input_tokens, Some(10));
        assert_eq!(usage[0].output_tokens, Some(2));
        assert_eq!(usage[0].total_tokens, Some(12));
    }

    #[test]
    fn maps_camel_case_codex_usage_payload() {
        let task_detail = json!({
            "usage": [{
                "inputTokens": 10,
                "cachedInputTokens": 4,
                "outputTokens": 3,
                "totalTokens": 13
            }]
        });

        let usage = usage_snapshots_from_values(None, Some(&task_detail), None);

        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].input_tokens, Some(10));
        assert_eq!(usage[0].output_tokens, Some(3));
        assert_eq!(usage[0].cached_input_tokens, Some(4));
        assert_eq!(usage[0].total_tokens, Some(13));
    }

    #[test]
    fn maps_object_usage_event_content_to_usage() {
        let task_detail = json!({
            "llm_usage_events": [{
                "content": {
                    "payload": {
                        "input_tokens": 11,
                        "output_tokens": 3,
                        "total_tokens": 14
                    }
                }
            }]
        });

        let usage = usage_snapshots_from_values(None, Some(&task_detail), Some("workflow-1"));

        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].workflow_id.as_deref(), Some("workflow-1"));
        assert_eq!(usage[0].input_tokens, Some(11));
        assert_eq!(usage[0].output_tokens, Some(3));
        assert_eq!(usage[0].total_tokens, Some(14));
    }

    #[test]
    fn codex_cached_input_tokens_are_not_double_counted() {
        let task_detail = json!({
            "llm_usage_events": [{
                "content": "{\"input_tokens\":10,\"cached_input_tokens\":4,\"output_tokens\":3}"
            }]
        });

        let usage = usage_snapshots_from_values(None, Some(&task_detail), Some("workflow-1"));

        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].input_tokens, Some(10));
        assert_eq!(usage[0].cached_input_tokens, Some(4));
        assert_eq!(usage[0].output_tokens, Some(3));
        assert_eq!(usage[0].total_tokens, Some(13));
    }

    #[test]
    fn reported_total_uses_larger_additive_component_total() {
        let task_detail = json!({
            "llm_usage_events": [{
                "content": "{\"input_tokens\":10,\"output_tokens\":3,\"cache_read_input_tokens\":4,\"reported_total_tokens\":13}"
            }]
        });

        let usage = usage_snapshots_from_values(None, Some(&task_detail), Some("workflow-1"));

        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].input_tokens, Some(10));
        assert_eq!(usage[0].output_tokens, Some(3));
        assert_eq!(usage[0].cached_input_tokens, Some(4));
        assert_eq!(usage[0].total_tokens, Some(17));
    }

    #[test]
    fn partial_token_components_do_not_derive_exact_total() {
        let task_detail = json!({
            "usage": [{
                "input_tokens": 10
            }]
        });

        let usage = usage_snapshots_from_values(None, Some(&task_detail), Some("workflow-1"));

        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].input_tokens, Some(10));
        assert_eq!(usage[0].output_tokens, None);
        assert_eq!(usage[0].total_tokens, None);
        assert_eq!(usage[0].token_confidence, Confidence::Unknown);
    }

    #[test]
    fn ignores_records_without_token_or_cost_data() {
        let task_detail = json!({
            "usage": [{
                "model": "codex-test"
            }]
        });

        let usage = usage_snapshots_from_values(None, Some(&task_detail), Some("workflow-1"));

        assert!(usage.is_empty());
    }
}
