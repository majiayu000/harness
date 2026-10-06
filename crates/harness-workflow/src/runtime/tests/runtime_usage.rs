use super::*;
use crate::runtime::{
    cost_usd_from_micros, cost_usd_to_micros, RuntimeUsageMetrics, RuntimeUsageUpsert,
    RuntimeUsageUpsertOutcome,
};
use harness_core::run_id::RunId;
use harness_core::types::{Decision, Event, SessionId, TokenUsage};
use harness_observe::event_store::EventStore;
use std::str::FromStr;

#[tokio::test]
async fn runtime_usage_upsert_skips_zero_placeholders() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let mut usage = runtime_usage_upsert(RuntimeUsageMetrics::default());
    usage.cost_usd_observed = false;

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
        cost_usd_micros: 500_000,
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
    second.cost_usd_micros = 1_250_000;
    second.model = "gpt-5.1".to_string();

    store.upsert_runtime_usage(&first).await?;
    let outcome = store.upsert_runtime_usage(&second).await?;
    let records = store
        .runtime_usage_between(Utc::now() - Duration::minutes(1), Utc::now())
        .await?;

    assert_eq!(records.len(), 1);
    assert_eq!(records[0].metrics.total_tokens(), 30);
    assert_eq!(records[0].cost_usd_micros, 1_250_000);
    assert!(records[0].cost_usd_observed);
    assert_eq!(records[0].model, "gpt-5.1");
    assert!(matches!(outcome, RuntimeUsageUpsertOutcome::Persisted));
    Ok(())
}

#[tokio::test]
async fn runtime_usage_upsert_merges_token_and_cost_observations_in_both_orders(
) -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    for cost_first in [true, false] {
        let dir = tempfile::tempdir()?;
        let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
        let mut tokens = runtime_usage_upsert(RuntimeUsageMetrics::from_token_usage(&TokenUsage {
            input_tokens: 10,
            output_tokens: 3,
            total_tokens: 13,
            cost_usd: 0.0,
        }));
        tokens.cost_usd_observed = false;
        let mut cost = runtime_usage_upsert(RuntimeUsageMetrics::default());
        cost.cost_usd_micros = 125_000;
        if cost_first {
            store.upsert_runtime_agent_run(&cost).await?;
            store.upsert_runtime_usage(&tokens).await?;
        } else {
            store.upsert_runtime_usage(&tokens).await?;
            store.upsert_runtime_agent_run(&cost).await?;
        }
        // A delayed start marker contains neither observation and must not
        // clear evidence already persisted for this runtime job and turn.
        let mut placeholder = runtime_usage_upsert(RuntimeUsageMetrics::default());
        placeholder.cost_usd_observed = false;
        store.upsert_runtime_agent_run(&placeholder).await?;
        let records = store
            .runtime_usage_between(Utc::now() - Duration::minutes(1), Utc::now())
            .await?;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].metrics, tokens.metrics);
        assert_eq!(records[0].cost_usd_micros, 125_000);
        assert!(records[0].cost_usd_observed);

        tokens.metrics = RuntimeUsageMetrics::from_token_usage(&TokenUsage::default());
        assert_eq!(
            store.upsert_runtime_usage(&tokens).await?,
            RuntimeUsageUpsertOutcome::Persisted
        );
        let usage = store
            .runtime_usage_for_workflow("workflow-1")
            .await?
            .unwrap();
        assert_eq!(
            usage.metrics.reported_total_tokens,
            Some(0),
            "observed zero tokens replace prior counts"
        );
        assert_eq!(
            usage.cost_usd_micros, 125_000,
            "token-only zero preserves the observed cost"
        );
        assert!(usage.cost_usd_observed);

        cost.cost_usd_micros = 0;
        assert_eq!(
            store.upsert_runtime_usage(&cost).await?,
            RuntimeUsageUpsertOutcome::Persisted
        );
        store.upsert_runtime_agent_run(&placeholder).await?;
        let usage = store
            .runtime_usage_for_workflow("workflow-1")
            .await?
            .unwrap();
        assert_eq!(usage.metrics.reported_total_tokens, Some(0));
        assert_eq!(
            usage.cost_usd_micros, 0,
            "observed zero USD replaces prior cost"
        );
        assert!(usage.cost_usd_observed);
    }
    Ok(())
}

#[tokio::test]
async fn runtime_usage_upsert_persists_cost_only() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let mut usage = runtime_usage_upsert(RuntimeUsageMetrics::default());
    usage.runtime_job_id = "runtime-job-cost-only".to_string();
    usage.turn_id = Some("turn-cost-only".to_string());
    usage.cost_usd_micros = 250_000;

    let outcome = store.upsert_runtime_usage(&usage).await?;
    assert_eq!(outcome, RuntimeUsageUpsertOutcome::Persisted);
    let aggregate = store
        .runtime_usage_for_workflow(&usage.workflow_id)
        .await?
        .unwrap();
    assert_eq!(aggregate.metrics.reported_total_tokens, None);
    assert_eq!(aggregate.metrics.component_total_tokens(), 0);
    assert_eq!(aggregate.cost_usd_micros, 250_000);
    assert!(aggregate.cost_usd_observed);
    Ok(())
}

#[tokio::test]
async fn runtime_usage_for_workflow_keeps_missing_reported_total_unknown() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let mut cost_only = runtime_usage_upsert(RuntimeUsageMetrics::default());
    cost_only.cost_usd_micros = 250_000;
    store.upsert_runtime_usage(&cost_only).await?;
    let mut known = runtime_usage_upsert(RuntimeUsageMetrics {
        input_tokens: 10,
        output_tokens: 3,
        cache_read_input_tokens: 2,
        cache_creation_input_tokens: 1,
        reported_total_tokens: Some(12),
    });
    known.turn_id = Some("turn-known".into());
    known.cost_usd_micros = 750_000;
    store.upsert_runtime_usage(&known).await?;

    let usage = store
        .runtime_usage_for_workflow("workflow-1")
        .await?
        .unwrap();
    assert_eq!(usage.metrics.reported_total_tokens, None);
    assert_eq!(usage.metrics.component_total_tokens(), 16);
    assert_eq!(usage.cost_usd_micros, 1_000_000);
    assert!(usage.cost_usd_observed);
    Ok(())
}

#[test]
fn runtime_usage_cost_converts_to_exact_micros() -> anyhow::Result<()> {
    assert_eq!(cost_usd_to_micros(0.1)?, 100_000);
    assert_eq!(cost_usd_to_micros(0.2)?, 200_000);
    assert_eq!(cost_usd_to_micros(0.123_456_4)?, 123_456);
    assert_eq!(cost_usd_from_micros(300_000), 0.3);
    assert!(cost_usd_to_micros(f64::NAN).is_err());
    assert!(cost_usd_to_micros(-0.01).is_err());
    assert!(cost_usd_to_micros(f64::MAX).is_err());
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
        cost_usd_micros: 750_000,
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
    second.cost_usd_micros = 1_250_000;
    second.cost_usd_observed = false;

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
    assert_eq!(usage.metrics.reported_total_tokens, Some(48));
    assert_eq!(usage.cost_usd_micros, 2_000_000);
    assert!(!usage.cost_usd_observed);
    assert!(store
        .runtime_usage_for_workflow("missing-workflow")
        .await?
        .is_none());
    Ok(())
}

#[tokio::test]
async fn runtime_usage_for_workflow_preserves_reported_total_when_components_are_higher(
) -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let usage = runtime_usage_upsert(RuntimeUsageMetrics {
        input_tokens: 10,
        output_tokens: 5,
        cache_read_input_tokens: 3,
        cache_creation_input_tokens: 2,
        reported_total_tokens: Some(12),
    });

    store.upsert_runtime_usage(&usage).await?;
    let usage = store
        .runtime_usage_for_workflow("workflow-1")
        .await?
        .expect("workflow usage should exist");

    assert_eq!(usage.metrics.total_tokens(), 12);
    Ok(())
}

#[tokio::test]
async fn runtime_agent_telemetry_for_workflow_returns_outcome_and_agent_usage() -> anyhow::Result<()>
{
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let event_store = EventStore::new(&dir.path().join("event_store")).await?;
    let workflow = WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "done",
        WorkflowSubject::new("issue", "issue:1804"),
    )
    .with_id("workflow-1");
    // Telemetry aggregation only needs the instance row to exist; the
    // canonical insert path now requires the definition's initial state
    // (GH-1864), so the fixture uses the test-only raw path.
    store
        .force_upsert_lifecycle_state_for_test(&workflow)
        .await?;

    let codex = RuntimeUsageUpsert {
        cost_usd_micros: 750_000,
        ..runtime_usage_upsert(RuntimeUsageMetrics {
            input_tokens: 10,
            output_tokens: 5,
            reported_total_tokens: Some(15),
            ..Default::default()
        })
    };
    let mut claude = codex.clone();
    claude.runtime_job_id = "runtime-job-2".to_string();
    claude.turn_id = Some("turn-2".to_string());
    claude.agent = "claude".to_string();
    claude.metrics.input_tokens = 99;

    store.upsert_runtime_usage(&codex).await?;
    store.upsert_runtime_usage(&claude).await?;
    let run_id = RunId::from_str("ar-01j1qb3c9r7v5m2k8x4tznq6wd")?;
    let mut tool_policy_event = Event::new(
        SessionId::from_str("session-1"),
        "PostToolUse",
        "Bash",
        Decision::Pass,
    );
    tool_policy_event.run_id = Some(run_id.clone());
    let mut rule_policy_event = Event::new(
        SessionId::from_str("session-1"),
        "rule_check",
        "policy.allowed_cmd",
        Decision::Warn,
    );
    rule_policy_event.run_id = Some(run_id.clone());
    let mut other_run_policy_event = Event::new(
        SessionId::from_str("session-2"),
        "PostToolUse",
        "Bash",
        Decision::Pass,
    );
    other_run_policy_event.run_id = Some(RunId::from_str("ar-01j1qb3c9r7v5m2k8x4tznq6we")?);
    event_store
        .log_many(&[tool_policy_event, rule_policy_event, other_run_policy_event])
        .await?;

    let telemetry = store
        .runtime_agent_telemetry_for_workflow("workflow-1", "codex", &event_store)
        .await?
        .expect("workflow telemetry should exist");

    assert_eq!(telemetry.workflow_state, "done");
    assert!(telemetry.terminal);
    assert_eq!(telemetry.agent, "codex");
    assert_eq!(telemetry.usage_records.len(), 1);
    assert_eq!(telemetry.usage_records[0].agent, "codex");
    assert_eq!(
        telemetry.usage_records[0]
            .agent_run_id
            .as_ref()
            .map(RunId::as_str),
        Some("ar-01j1qb3c9r7v5m2k8x4tznq6wd")
    );
    assert_eq!(telemetry.policy_events.len(), 2);
    assert!(telemetry
        .policy_events
        .iter()
        .all(|event| event.run_id.as_ref() == Some(&run_id)));
    assert!(telemetry
        .policy_events
        .iter()
        .any(|event| event.tool == "Bash"));
    assert!(telemetry
        .policy_events
        .iter()
        .any(|event| event.tool == "policy.allowed_cmd"));
    let usage = telemetry.usage.expect("codex usage should aggregate");
    assert_eq!(usage.metrics.input_tokens, 10);
    assert_eq!(usage.metrics.total_tokens(), 15);
    assert_eq!(usage.cost_usd_micros, 750_000);
    assert!(store
        .runtime_agent_telemetry_for_workflow("missing-workflow", "codex", &event_store)
        .await?
        .is_none());
    let zero_workflow = WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "blocked",
        WorkflowSubject::new("issue", "issue:1805"),
    )
    .with_id("workflow-zero");
    store
        .force_upsert_lifecycle_state_for_test(&zero_workflow)
        .await?;
    let zero_run_id = RunId::from_str("ar-01j1qb3c9r7v5m2k8x4tznq6wf")?;
    store
        .upsert_runtime_agent_run(&RuntimeUsageUpsert {
            runtime_job_id: "runtime-job-zero".to_string(),
            workflow_id: "workflow-zero".to_string(),
            turn_id: Some("turn-zero".to_string()),
            agent_run_id: Some(zero_run_id.clone()),
            ..runtime_usage_upsert(RuntimeUsageMetrics::default())
        })
        .await?;
    let mut zero_usage_policy_event = Event::new(
        SessionId::from_str("session-zero"),
        "rule_check",
        "zero_usage_policy",
        Decision::Block,
    );
    zero_usage_policy_event.run_id = Some(zero_run_id);
    event_store.log(&zero_usage_policy_event).await?;
    let zero_telemetry = store
        .runtime_agent_telemetry_for_workflow("workflow-zero", "codex", &event_store)
        .await?
        .expect("zero-usage workflow telemetry should exist");
    assert_eq!(zero_telemetry.usage_records.len(), 1);
    assert_eq!(
        zero_telemetry
            .usage
            .expect("zero usage should aggregate")
            .metrics
            .total_tokens(),
        0
    );
    assert_eq!(zero_telemetry.policy_events.len(), 1);
    event_store.close().await;
    Ok(())
}

#[tokio::test]
async fn runtime_turn_counts_group_usage_by_workflow() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let first = runtime_usage_upsert(RuntimeUsageMetrics {
        input_tokens: 1,
        output_tokens: 1,
        ..Default::default()
    });
    let mut second = first.clone();
    second.runtime_job_id = "runtime-job-2".to_string();
    second.turn_id = Some("turn-2".to_string());
    let mut other_workflow = first.clone();
    other_workflow.runtime_job_id = "runtime-job-3".to_string();
    other_workflow.workflow_id = "workflow-2".to_string();
    other_workflow.turn_id = Some("turn-3".to_string());

    for usage in [&first, &second, &other_workflow] {
        store.upsert_runtime_usage(usage).await?;
    }

    assert_eq!(store.runtime_turn_counts().await?, vec![1, 2]);
    Ok(())
}

fn runtime_usage_upsert(metrics: RuntimeUsageMetrics) -> RuntimeUsageUpsert {
    RuntimeUsageUpsert {
        runtime_job_id: "runtime-job-1".to_string(),
        command_id: "command-1".to_string(),
        workflow_id: "workflow-1".to_string(),
        turn_id: Some("turn-1".to_string()),
        agent_run_id: Some(RunId::from_str("ar-01j1qb3c9r7v5m2k8x4tznq6wd").unwrap()),
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
        cost_usd_micros: 0,
        cost_usd_observed: true,
        reported_at: Utc::now(),
    }
}
