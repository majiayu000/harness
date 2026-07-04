use super::{usage_monitor_candidate, AppState, PriceCatalog, UsageRecord, UsageWindow};
use harness_core::types::{Event, EventFilters};
use harness_observe::usage::UsageMetrics;
use harness_workflow::runtime::RuntimeUsageRecord;
use serde_json::{json, Value};

#[derive(Debug, Default)]
pub(super) struct UsageLoadDiagnostics {
    pub(super) malformed_legacy_usage_events: u64,
    pub(super) malformed_runtime_usage_rows: u64,
}

#[derive(Debug, Default)]
pub(super) struct UsageRecordsLoad {
    pub(super) records: Vec<UsageRecord>,
    pub(super) diagnostics: UsageLoadDiagnostics,
}

pub(super) async fn load_usage_records(
    state: &AppState,
    window: UsageWindow,
    price_catalog: &PriceCatalog,
    candidate_attributions: &usage_monitor_candidate::CandidateAttributionIndex,
) -> anyhow::Result<UsageRecordsLoad> {
    let events = state
        .observability
        .events
        .query(&EventFilters {
            hook: Some("llm_usage".to_string()),
            since: Some(window.since),
            until: Some(window.now),
            include_content: true,
            ..Default::default()
        })
        .await?;
    let mut load = UsageRecordsLoad::default();
    for event in &events {
        match parse_usage_event(event, price_catalog, candidate_attributions) {
            Ok(record) => load.records.push(record),
            Err(error) => {
                load.diagnostics.malformed_legacy_usage_events += 1;
                tracing::error!(
                    event_id = %event.id.as_str(),
                    "usage_monitor: malformed llm_usage event: {error}"
                );
            }
        }
    }
    if let Some(store) = state.core.workflow_runtime_store.as_ref() {
        for record in store
            .runtime_usage_between(window.since, window.now)
            .await?
        {
            match usage_record_from_runtime_usage(record, price_catalog, candidate_attributions) {
                Ok(record) => load.records.push(record),
                Err(error) => {
                    load.diagnostics.malformed_runtime_usage_rows += 1;
                    tracing::error!("usage_monitor: malformed workflow runtime usage row: {error}");
                }
            }
        }
    }
    Ok(load)
}

pub(super) fn parse_usage_event(
    event: &Event,
    price_catalog: &PriceCatalog,
    candidate_attributions: &usage_monitor_candidate::CandidateAttributionIndex,
) -> anyhow::Result<UsageRecord> {
    let content = event
        .content
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("missing content"))?;
    let payload: Value = serde_json::from_str(content)?;
    let metrics = UsageMetrics::from_payload(&payload)
        .ok_or_else(|| anyhow::anyhow!("missing token metrics"))?;
    let agent = payload
        .get("agent")
        .and_then(Value::as_str)
        .unwrap_or(event.tool.as_str())
        .to_string();
    let model = payload
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let project = payload
        .get("project")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let estimated_cost_usd = price_catalog.estimate(&model, &metrics);
    let candidate =
        usage_monitor_candidate::usage_record_candidate(&payload, candidate_attributions);
    Ok(UsageRecord {
        agent,
        model,
        project,
        metrics,
        estimated_cost_usd,
        candidate,
    })
}

pub(super) fn usage_record_from_runtime_usage(
    record: RuntimeUsageRecord,
    price_catalog: &PriceCatalog,
    candidate_attributions: &usage_monitor_candidate::CandidateAttributionIndex,
) -> anyhow::Result<UsageRecord> {
    if record.metrics.total_tokens() == 0 {
        anyhow::bail!("runtime usage row {} has zero tokens", record.id);
    }
    let payload = json!({
        "runtime_job_id": record.runtime_job_id,
        "command_id": record.command_id,
        "task_id": record.task_id,
        "candidate_group_id": record.candidate_group_id,
        "candidate_id": record.candidate_id,
        "candidate_index": record.candidate_index,
        "candidate_count": record.candidate_count,
    });
    let estimated_cost_usd = price_catalog.estimate(&record.model, &record.metrics);
    let candidate =
        usage_monitor_candidate::usage_record_candidate(&payload, candidate_attributions);
    Ok(UsageRecord {
        agent: record.agent,
        model: record.model,
        project: record.project,
        metrics: record.metrics,
        estimated_cost_usd,
        candidate,
    })
}
