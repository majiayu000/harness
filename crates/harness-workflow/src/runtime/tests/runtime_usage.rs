use super::*;
use crate::runtime::{RuntimeUsageMetrics, RuntimeUsageUpsert, RuntimeUsageUpsertOutcome};

#[tokio::test]
async fn runtime_usage_upsert_skips_zero_placeholders() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let usage = runtime_usage_upsert(RuntimeUsageMetrics::default());

    let outcome = store.upsert_runtime_usage(&usage).await?;
    let records = store
        .runtime_usage_between(Utc::now() - Duration::minutes(1), Utc::now())
        .await?;

    assert_eq!(outcome, RuntimeUsageUpsertOutcome::SkippedZeroUsage);
    assert!(records.is_empty());
    Ok(())
}

#[tokio::test]
async fn runtime_usage_upsert_replaces_cumulative_turn_usage() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let first = runtime_usage_upsert(RuntimeUsageMetrics {
        input_tokens: 10,
        output_tokens: 5,
        cache_read_input_tokens: 0,
        cache_creation_input_tokens: 0,
        reported_total_tokens: Some(15),
    });
    let first = RuntimeUsageUpsert {
        cost_usd: 0.5,
        ..first
    };
    let mut second = first.clone();
    second.metrics = RuntimeUsageMetrics {
        input_tokens: 20,
        output_tokens: 7,
        cache_read_input_tokens: 3,
        cache_creation_input_tokens: 0,
        reported_total_tokens: Some(30),
    };
    second.cost_usd = 1.25;
    second.model = "gpt-5.1".to_string();

    store.upsert_runtime_usage(&first).await?;
    let outcome = store.upsert_runtime_usage(&second).await?;
    let records = store
        .runtime_usage_between(Utc::now() - Duration::minutes(1), Utc::now())
        .await?;

    assert_eq!(records.len(), 1);
    assert_eq!(records[0].metrics.total_tokens(), 30);
    assert_eq!(records[0].cost_usd, 1.25);
    assert_eq!(records[0].model, "gpt-5.1");
    assert!(matches!(outcome, RuntimeUsageUpsertOutcome::Persisted));
    Ok(())
}

#[tokio::test]
async fn runtime_usage_upsert_validates_and_persists_cost() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let mut usage = runtime_usage_upsert(RuntimeUsageMetrics::default());
    usage.runtime_job_id = "runtime-job-cost-only".to_string();
    usage.turn_id = Some("turn-cost-only".to_string());
    usage.cost_usd = 0.25;

    let outcome = store.upsert_runtime_usage(&usage).await?;
    assert_eq!(outcome, RuntimeUsageUpsertOutcome::Persisted);

    usage.cost_usd = f64::NAN;
    let error = store
        .upsert_runtime_usage(&usage)
        .await
        .expect_err("non-finite costs must be rejected");
    assert!(error.to_string().contains("finite and nonnegative"));
    Ok(())
}

#[tokio::test]
async fn runtime_usage_for_workflow_aggregates_distinct_turns() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let first = runtime_usage_upsert(RuntimeUsageMetrics {
        input_tokens: 10,
        output_tokens: 5,
        cache_read_input_tokens: 2,
        cache_creation_input_tokens: 0,
        reported_total_tokens: Some(17),
    });
    let first = RuntimeUsageUpsert {
        cost_usd: 0.75,
        ..first
    };
    let mut second = first.clone();
    second.runtime_job_id = "runtime-job-2".to_string();
    second.turn_id = Some("turn-2".to_string());
    second.metrics = RuntimeUsageMetrics {
        input_tokens: 20,
        output_tokens: 7,
        cache_read_input_tokens: 3,
        cache_creation_input_tokens: 1,
        reported_total_tokens: Some(31),
    };
    second.cost_usd = 1.25;

    store.upsert_runtime_usage(&first).await?;
    store.upsert_runtime_usage(&second).await?;
    let usage = store
        .runtime_usage_for_workflow("workflow-1")
        .await?
        .expect("workflow usage should exist");

    assert_eq!(usage.metrics.input_tokens, 30);
    assert_eq!(usage.metrics.output_tokens, 12);
    assert_eq!(usage.metrics.cache_read_input_tokens, 5);
    assert_eq!(usage.metrics.cache_creation_input_tokens, 1);
    assert_eq!(usage.metrics.total_tokens(), 48);
    assert_eq!(usage.cost_usd, 2.0);
    assert!(store
        .runtime_usage_for_workflow("missing-workflow")
        .await?
        .is_none());
    Ok(())
}

fn runtime_usage_upsert(metrics: RuntimeUsageMetrics) -> RuntimeUsageUpsert {
    RuntimeUsageUpsert {
        runtime_job_id: "runtime-job-1".to_string(),
        command_id: "command-1".to_string(),
        workflow_id: "workflow-1".to_string(),
        turn_id: Some("turn-1".to_string()),
        runtime_kind: RuntimeKind::CodexExec,
        runtime_profile: "codex-default".to_string(),
        agent: "codex".to_string(),
        model: "gpt-5".to_string(),
        project: "/repo".to_string(),
        task_id: Some("issue-1439".to_string()),
        candidate_group_id: None,
        candidate_id: None,
        candidate_index: None,
        candidate_count: None,
        metrics,
        cost_usd: 0.0,
        reported_at: Utc::now(),
    }
}
