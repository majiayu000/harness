use super::evidence::activity_result_from_job;
use super::model::{Confidence, UsageSnapshot};
use crate::runtime::{RuntimeEvent, RuntimeJob};
use serde_json::Value;
use std::collections::BTreeMap;

pub(super) fn usage_snapshots(
    workflow_id: Option<&str>,
    runtime_events: &BTreeMap<String, Vec<RuntimeEvent>>,
    runtime_jobs: &[RuntimeJob],
) -> Vec<UsageSnapshot> {
    let mut usage = event_usage_snapshots(workflow_id, runtime_events);
    for job in runtime_jobs {
        let Some(result) = activity_result_from_job(job) else {
            continue;
        };
        let Some(artifact) = result.artifacts.iter().find(|artifact| {
            artifact.artifact_type
                == crate::runtime::completion_evidence::ARTIFACT_RUNTIME_HOST_USAGE
        }) else {
            continue;
        };
        let payload = &artifact.artifact;
        let cost_usd_micros = payload.get("cost_usd_micros").and_then(Value::as_u64);
        let mut snapshot = UsageSnapshot {
            agent_invocation_id: None,
            runtime_job_id: Some(job.id.clone()),
            workflow_id: workflow_id.map(ToOwned::to_owned),
            model: payload
                .get("model")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            reasoning_effort: None,
            input_tokens: payload.get("input_tokens").and_then(Value::as_u64),
            output_tokens: payload.get("output_tokens").and_then(Value::as_u64),
            cached_input_tokens: cached_input_tokens_from_payload(payload),
            total_tokens: payload.get("total_tokens").and_then(Value::as_u64),
            cost_usd_micros,
            token_confidence: Confidence::Observed,
            cost_confidence: if cost_usd_micros.is_some() {
                Confidence::Observed
            } else {
                Confidence::Unknown
            },
        };
        snapshot.total_tokens = derived_total_tokens_from_payload(payload, &snapshot);
        if usage_snapshot_has_measurement(&snapshot) {
            usage.push(snapshot);
        }
    }
    usage
}

fn event_usage_snapshots(
    workflow_id: Option<&str>,
    runtime_events: &BTreeMap<String, Vec<RuntimeEvent>>,
) -> Vec<UsageSnapshot> {
    let mut usage = Vec::new();
    for (runtime_job_id, events) in runtime_events {
        for event in events {
            if !matches!(
                event.event_type.as_str(),
                "UsageRecorded" | "TokenUsageRecorded"
            ) {
                continue;
            }
            let snapshot = usage_snapshot_from_event(workflow_id, runtime_job_id, &event.event);
            if usage_snapshot_has_measurement(&snapshot) {
                usage.push(snapshot);
            }
        }
    }
    usage
}

pub(super) fn usage_snapshot_from_event(
    workflow_id: Option<&str>,
    runtime_job_id: &str,
    event: &Value,
) -> UsageSnapshot {
    let payload = event.get("usage").unwrap_or(event);
    let input_tokens = first_u64_field(payload, &["input_tokens", "input"]);
    let output_tokens = first_u64_field(payload, &["output_tokens", "output"]);
    let cached_input_tokens = cached_input_tokens_from_payload(payload);
    let cost_usd_micros = first_u64_field(payload, &["cost_usd_micros"]);
    let mut snapshot = UsageSnapshot {
        agent_invocation_id: payload
            .get("agent_invocation_id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        runtime_job_id: Some(runtime_job_id.to_string()),
        workflow_id: workflow_id.map(ToOwned::to_owned),
        model: payload
            .get("model")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        reasoning_effort: payload
            .get("reasoning_effort")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        input_tokens,
        output_tokens,
        cached_input_tokens,
        total_tokens: first_u64_field(payload, &["total_tokens"]),
        cost_usd_micros,
        token_confidence: Confidence::Observed,
        cost_confidence: if cost_usd_micros.is_some() {
            Confidence::Estimated
        } else {
            Confidence::Unknown
        },
    };
    snapshot.total_tokens = derived_total_tokens_from_payload(payload, &snapshot);
    snapshot
}

fn first_u64_field(value: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter().find_map(|key| value.get(*key)?.as_u64())
}

fn cached_input_tokens_from_payload(payload: &Value) -> Option<u64> {
    payload
        .get("cached_input_tokens")
        .and_then(Value::as_u64)
        .or_else(|| additive_cached_input_tokens_from_payload(payload))
}

fn additive_cached_input_tokens_from_payload(payload: &Value) -> Option<u64> {
    let cache_read = payload
        .get("cache_read_input_tokens")
        .and_then(Value::as_u64);
    let cache_creation = payload
        .get("cache_creation_input_tokens")
        .and_then(Value::as_u64);
    (cache_read.is_some() || cache_creation.is_some()).then(|| {
        cache_read
            .unwrap_or(0)
            .saturating_add(cache_creation.unwrap_or(0))
    })
}

fn derived_total_tokens_from_payload(payload: &Value, snapshot: &UsageSnapshot) -> Option<u64> {
    let has_components = snapshot.input_tokens.is_some()
        || snapshot.output_tokens.is_some()
        || snapshot.cached_input_tokens.is_some();
    snapshot.total_tokens.or_else(|| {
        has_components.then(|| {
            harness_observe::usage::derived_total_tokens(
                None,
                snapshot.input_tokens.unwrap_or(0),
                snapshot.output_tokens.unwrap_or(0),
                additive_cached_input_tokens_from_payload(payload).unwrap_or(0),
            )
        })
    })
}

fn usage_snapshot_has_measurement(snapshot: &UsageSnapshot) -> bool {
    snapshot.input_tokens.is_some()
        || snapshot.output_tokens.is_some()
        || snapshot.cached_input_tokens.is_some()
        || snapshot.total_tokens.is_some()
        || snapshot.cost_usd_micros.is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{ActivityArtifact, ActivityResult, RuntimeKind};
    use serde_json::json;

    #[test]
    fn server_reserved_host_usage_is_collected_without_runtime_usage_events() {
        let mut job = RuntimeJob::pending(
            "command-1",
            RuntimeKind::RemoteHost,
            "eval-host",
            json!({"activity": "implement_issue"}),
        );
        job.output = Some(
            serde_json::to_value(
                ActivityResult::succeeded("implement_issue", "done").with_artifact(
                    ActivityArtifact::new(
                        crate::runtime::completion_evidence::ARTIFACT_RUNTIME_HOST_USAGE,
                        json!({
                            "model": "test-model",
                            "input_tokens": 10,
                            "output_tokens": 5,
                            "cached_input_tokens": 2,
                            "total_tokens": 15,
                            "cost_usd_micros": 20,
                        }),
                    ),
                ),
            )
            .expect("activity result should serialize"),
        );

        let usage = usage_snapshots(Some("workflow-1"), &BTreeMap::new(), &[job]);

        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].total_tokens, Some(15));
        assert_eq!(usage[0].token_confidence, Confidence::Observed);
        assert_eq!(usage[0].cost_confidence, Confidence::Observed);
    }

    #[test]
    fn event_usage_derives_total_from_cache_read_and_creation_aliases() {
        let snapshot = usage_snapshot_from_event(
            Some("workflow-1"),
            "job-1",
            &json!({
                "usage": {
                    "input_tokens": 10,
                    "output_tokens": 5,
                    "cache_read_input_tokens": 3,
                    "cache_creation_input_tokens": 2
                }
            }),
        );

        assert_eq!(snapshot.cached_input_tokens, Some(5));
        assert_eq!(snapshot.total_tokens, Some(20));
    }

    #[test]
    fn event_usage_does_not_add_subset_cached_input_tokens_to_total() {
        let snapshot = usage_snapshot_from_event(
            Some("workflow-1"),
            "job-1",
            &json!({
                "usage": {
                    "input_tokens": 10,
                    "cached_input_tokens": 4,
                    "output_tokens": 3
                }
            }),
        );

        assert_eq!(snapshot.cached_input_tokens, Some(4));
        assert_eq!(snapshot.total_tokens, Some(13));
    }
}
