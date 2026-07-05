use super::usage_monitor_aggregate::UsageAggregate;
use super::usage_monitor_candidate::{
    candidate_attribution_index, candidate_usage_groups, CandidateUsageAttribution,
};
use super::usage_monitor_records::{parse_usage_event, usage_record_from_runtime_usage};
use super::*;
use harness_core::types::{Decision, Event, SessionId};
use harness_workflow::runtime::{RuntimeKind, RuntimeUsageRecord, WorkflowCommandType};

#[test]
fn burn_level_marks_stale_running_job_high() {
    assert_eq!(
        burn_level("running", "implement_issue", Some("low"), 30, true),
        "high"
    );
}

#[test]
fn usage_aggregate_requires_configured_price_for_cost() {
    let mut usage = UsageAggregate::default();
    usage.add(
        &UsageMetrics {
            input_tokens: 1_000_000,
            output_tokens: 0,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            reported_total_tokens: None,
        },
        None,
    );
    assert_eq!(usage.total_tokens(), 1_000_000);
    assert_eq!(usage.estimated_cost_json(true), None);
}

#[tokio::test]
async fn usage_monitor_response_includes_postgres_catalog_census() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let state = crate::test_helpers::make_test_state(dir.path()).await?;
    let response = build_usage_monitor_response(
        &state,
        UsageMonitorQuery {
            hours: Some(1),
            limit: Some(1),
        },
    )
    .await?;

    assert_eq!(
        response.postgres_catalog.state,
        crate::postgres_catalog::PostgresCatalogState::Available
    );
    assert!(response.postgres_catalog.schema_count.is_some());
    assert!(response.postgres_catalog.catalog_object_count.is_some());
    assert!(response.postgres_catalog.database_size_bytes.is_some());
    Ok(())
}

#[test]
fn parse_runtime_usage_row_reports_corrupt_json_without_panicking() -> anyhow::Result<()> {
    let workflow = WorkflowInstance::new(
        "github_issue_pr",
        1,
        "implementing",
        harness_workflow::runtime::WorkflowSubject::new("repo", "owner/repo"),
    )
    .with_id("workflow-1");
    let command = WorkflowCommand::enqueue_activity("implement_issue", "impl-1");
    let error = parse_runtime_usage_row(
        "workflow-1",
        "command-1".to_string(),
        "dispatched".to_string(),
        &serde_json::to_string(&workflow)?,
        &serde_json::to_string(&command)?,
        "{not-json",
        None,
        None,
        false,
        false,
    )
    .err()
    .ok_or_else(|| anyhow::anyhow!("invalid runtime job json should be reported"))?;

    assert!(
        error
            .to_string()
            .contains("runtime job for command `command-1` JSON is invalid"),
        "{error}"
    );
    Ok(())
}

#[test]
fn runtime_attribution_tokens_include_runtime_job_and_command_ids() {
    let workflow = WorkflowInstance::new(
        "github_issue_pr",
        1,
        "implementing",
        harness_workflow::runtime::WorkflowSubject::new("repo", "owner/repo"),
    )
    .with_id("workflow-1");
    let command = WorkflowCommand::enqueue_activity("implement_issue", "impl-1");
    let runtime_job = RuntimeJob::pending(
        "command-1",
        RuntimeKind::CodexExec,
        "codex-default",
        serde_json::json!({ "activity": "implement_issue" }),
    );
    let runtime_job_id = runtime_job.id.clone();
    let row = RuntimeUsageRow {
        workflow,
        command_id: "command-1".to_string(),
        command_status: "dispatched".to_string(),
        command,
        runtime_job,
        latest_runtime_event_type: None,
        latest_runtime_event_at: None,
        runtime_turn_started: false,
        activity_result_ready_after_latest_turn: false,
    };

    let tokens = runtime_attribution_tokens(&[row]);

    assert!(tokens.contains("command-1"));
    assert!(tokens.contains(&runtime_job_id));
}

#[test]
fn candidate_usage_attribution_index_matches_runtime_candidate_keys() {
    let row = candidate_runtime_row(2);
    let runtime_job_id = row.runtime_job.id.clone();
    let index = candidate_attribution_index(&[row]);

    assert_eq!(
        index
            .get("command-1449-c2")
            .map(|candidate| candidate.candidate_id.as_str()),
        Some("workflow-1449:candidate-group:issue-1449:c2")
    );
    assert_eq!(
        index
            .get(runtime_job_id.as_str())
            .map(|candidate| candidate.candidate_id.as_str()),
        Some("workflow-1449:candidate-group:issue-1449:c2")
    );
    assert_eq!(
        index
            .get("workflow-1449:candidate-group:issue-1449:c2")
            .map(|candidate| candidate.candidate_index),
        Some(Some(2))
    );
    assert_eq!(
        index
            .get("issue-1449-c2")
            .map(|candidate| candidate.candidate_id.as_str()),
        Some("workflow-1449:candidate-group:issue-1449:c2")
    );
    assert!(
        !index.contains_key("workflow-task"),
        "shared workflow task IDs must not be attributed to a single candidate"
    );
}

#[test]
fn candidate_usage_parse_event_uses_runtime_candidate_attribution() {
    let row = candidate_runtime_row(2);
    let index = candidate_attribution_index(&[row]);
    let mut event = Event::new(SessionId::new(), "llm_usage", "codex", Decision::Complete);
    event.content = Some(
        serde_json::json!({
            "agent": "codex",
            "model": "gpt-5",
            "task_id": "issue-1449-c2",
            "project": "/repo",
            "input_tokens": 10,
            "output_tokens": 5,
            "cache_read_input_tokens": 3,
            "cache_creation_input_tokens": 2,
        })
        .to_string(),
    );

    let record = parse_usage_event(&event, &PriceCatalog::default(), &index)
        .expect("usage event should parse");
    let candidate = record
        .candidate
        .expect("candidate attribution should be resolved");

    assert_eq!(
        candidate.candidate_group_id,
        "workflow-1449:candidate-group:issue-1449"
    );
    assert_eq!(
        candidate.candidate_id,
        "workflow-1449:candidate-group:issue-1449:c2"
    );
    assert_eq!(candidate.candidate_index, Some(2));
    assert_eq!(record.metrics.total_tokens(), 20);
}

#[test]
fn parse_usage_event_reports_malformed_payload() {
    let mut event = Event::new(SessionId::new(), "llm_usage", "codex", Decision::Complete);
    event.content = Some(r#"{"agent":"codex","model":"gpt-5"}"#.to_string());

    let error = parse_usage_event(
        &event,
        &PriceCatalog::default(),
        &candidate_attribution_index(&[]),
    )
    .expect_err("missing usage metrics should be reported");

    assert!(error.to_string().contains("missing token metrics"));
}

#[test]
fn runtime_usage_record_becomes_usage_record_with_candidate() -> anyhow::Result<()> {
    let runtime_record = RuntimeUsageRecord {
        id: "usage-1".to_string(),
        runtime_job_id: "runtime-job-1".to_string(),
        usage_key: "turn:turn-1".to_string(),
        command_id: "command-1".to_string(),
        workflow_id: "workflow-1".to_string(),
        turn_id: Some("turn-1".to_string()),
        runtime_kind: "codex_exec".to_string(),
        runtime_profile: "codex-default".to_string(),
        agent: "codex".to_string(),
        model: "gpt-5".to_string(),
        project: "/repo".to_string(),
        task_id: Some("issue-1449-c2".to_string()),
        candidate_group_id: Some("workflow-1449:candidate-group:issue-1449".to_string()),
        candidate_id: Some("workflow-1449:candidate-group:issue-1449:c2".to_string()),
        candidate_index: Some(2),
        candidate_count: Some(3),
        metrics: UsageMetrics {
            input_tokens: 21,
            output_tokens: 8,
            cache_read_input_tokens: 1,
            cache_creation_input_tokens: 0,
            reported_total_tokens: Some(30),
        },
        reported_at: Utc::now(),
        updated_at: Utc::now(),
    };

    let record = usage_record_from_runtime_usage(
        runtime_record,
        &PriceCatalog::default(),
        &candidate_attribution_index(&[]),
    )?;

    assert_eq!(record.agent, "codex");
    assert_eq!(record.metrics.total_tokens(), 30);
    assert_eq!(
        record
            .candidate
            .as_ref()
            .map(|candidate| candidate.candidate_id.as_str()),
        Some("workflow-1449:candidate-group:issue-1449:c2")
    );
    Ok(())
}

#[test]
fn candidate_usage_group_total_equals_candidate_sum() {
    let records = vec![
        candidate_usage_record(
            1,
            UsageMetrics {
                input_tokens: 10,
                output_tokens: 5,
                cache_read_input_tokens: 2,
                cache_creation_input_tokens: 1,
                reported_total_tokens: None,
            },
            Some(1.0),
        ),
        candidate_usage_record(
            2,
            UsageMetrics {
                input_tokens: 7,
                output_tokens: 9,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 3,
                reported_total_tokens: None,
            },
            Some(2.0),
        ),
    ];

    let groups = candidate_usage_groups(&records, true);

    assert_eq!(groups.len(), 1);
    let group = &groups[0];
    assert_eq!(
        group.candidate_group_id,
        "workflow-1449:candidate-group:issue-1449"
    );
    assert_eq!(group.candidates.len(), 2);
    assert_eq!(group.candidates[0].candidate_index, Some(1));
    assert_eq!(group.candidates[1].candidate_index, Some(2));

    let candidate_token_sum: u64 = group
        .candidates
        .iter()
        .map(|candidate| candidate.total_tokens)
        .sum();
    let candidate_request_sum: u64 = group
        .candidates
        .iter()
        .map(|candidate| candidate.request_count)
        .sum();
    let candidate_cost_sum: f64 = group
        .candidates
        .iter()
        .map(|candidate| candidate.estimated_cost_usd.unwrap_or_default())
        .sum();

    assert_eq!(group.total_tokens, candidate_token_sum);
    assert_eq!(group.request_count, candidate_request_sum);
    assert_eq!(group.estimated_cost_usd, Some(candidate_cost_sum));
    assert_eq!(group.input_tokens, 17);
    assert_eq!(group.output_tokens, 14);
    assert_eq!(group.cache_read_input_tokens, 2);
    assert_eq!(group.cache_creation_input_tokens, 4);
}

#[test]
fn active_counts_skip_terminal_invocations() {
    let invocations = [
        test_invocation(
            "owner/active",
            "implement_issue",
            "running",
            "high",
            Some("active_leased"),
        ),
        test_invocation(
            "owner/active",
            "implement_issue",
            "pending",
            "low",
            Some("missing_lease"),
        ),
        test_invocation(
            "owner/done",
            "quality_gate",
            "succeeded",
            "high",
            Some("active_leased"),
        ),
        test_invocation(
            "owner/failed",
            "review_pr",
            "failed",
            "high",
            Some("expired_lease"),
        ),
        test_invocation("owner/cancelled", "plan_issue", "cancelled", "high", None),
    ];

    let active_by_repo = aggregate_active_counts(&invocations, |invocation| {
        invocation
            .repo
            .clone()
            .unwrap_or_else(|| "unassigned".to_string())
    });
    let active_by_activity =
        aggregate_active_counts(&invocations, |invocation| invocation.activity.clone());

    assert_eq!(active_by_repo.len(), 1);
    assert_eq!(active_by_repo[0].name, "owner/active");
    assert_eq!(active_by_repo[0].running, 1);
    assert_eq!(active_by_repo[0].pending, 1);
    assert_eq!(active_by_repo[0].active_leased, 1);
    assert_eq!(active_by_repo[0].expired_or_missing_lease, 1);
    assert_eq!(active_by_repo[0].high_burn, 1);

    assert_eq!(active_by_activity.len(), 1);
    assert_eq!(active_by_activity[0].name, "implement_issue");
    assert_eq!(active_by_activity[0].running, 1);
    assert_eq!(active_by_activity[0].pending, 1);
    assert_eq!(active_by_activity[0].high_burn, 1);
}

#[test]
fn agent_invocation_exposes_running_lease_state_and_observation() {
    let now = Utc::now();
    let workflow = WorkflowInstance::new(
        "github_issue_pr",
        1,
        "implementing",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:1170"),
    )
    .with_id("workflow-1170");
    let command = WorkflowCommand::enqueue_activity("implement_issue", "impl-1170");
    let mut runtime_job = RuntimeJob::pending(
        "command-1170",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        serde_json::json!({
            "activity": "implement_issue",
            "runtime_profile": {
                "kind": "codex_jsonrpc",
                "reasoning_effort": "xhigh"
            }
        }),
    );
    runtime_job.claim("runtime-1", now + chrono::Duration::minutes(5));
    runtime_job.updated_at = now - chrono::Duration::seconds(10);
    let row = RuntimeUsageRow {
        workflow,
        command_id: "command-1170".to_string(),
        command_status: "dispatched".to_string(),
        command,
        runtime_job,
        latest_runtime_event_type: Some("RuntimeTurnStarted".to_string()),
        latest_runtime_event_at: Some(now - chrono::Duration::seconds(30)),
        runtime_turn_started: true,
        activity_result_ready_after_latest_turn: false,
    };

    let invocation = agent_invocation_from_row(&row, now);

    assert_eq!(invocation.lease_state, Some("active_leased"));
    assert!(invocation.in_flight_model_turn);
    assert_eq!(
        invocation.latest_runtime_event_type.as_deref(),
        Some("RuntimeTurnStarted")
    );
    assert_eq!(
        invocation.last_runtime_observation_at,
        Some(row.runtime_job.updated_at)
    );
    assert!(!invocation.stale);
}

#[test]
fn agent_invocation_ends_in_flight_after_latest_turn_result() {
    let now = Utc::now();
    let workflow = WorkflowInstance::new(
        "github_issue_pr",
        1,
        "implementing",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:1170"),
    )
    .with_id("workflow-1170");
    let command = WorkflowCommand::enqueue_activity("implement_issue", "impl-1170");
    let mut runtime_job = RuntimeJob::pending(
        "command-1170",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        serde_json::json!({ "activity": "implement_issue" }),
    );
    runtime_job.claim("runtime-1", now + chrono::Duration::minutes(5));
    let row = RuntimeUsageRow {
        workflow,
        command_id: "command-1170".to_string(),
        command_status: "dispatched".to_string(),
        command,
        runtime_job,
        latest_runtime_event_type: Some("ActivityResultReady".to_string()),
        latest_runtime_event_at: Some(now - chrono::Duration::seconds(5)),
        runtime_turn_started: true,
        activity_result_ready_after_latest_turn: true,
    };

    let invocation = agent_invocation_from_row(&row, now);

    assert!(!invocation.in_flight_model_turn);
}

fn candidate_runtime_row(candidate_index: u32) -> RuntimeUsageRow {
    let workflow = WorkflowInstance::new(
        "github_issue_pr",
        1,
        "implementing",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:1449"),
    )
    .with_id("workflow-1449")
    .with_data(serde_json::json!({
        "issue_number": 1449,
        "task_id": "workflow-task",
    }));
    let command_id = format!("command-1449-c{candidate_index}");
    let command = WorkflowCommand::new(
        WorkflowCommandType::EnqueueActivity,
        format!("impl-1449-c{candidate_index}"),
        serde_json::json!({
            "activity": "implement_issue",
            "candidate": candidate_metadata(candidate_index),
        }),
    );
    let mut runtime_job = RuntimeJob::pending(
        command_id.clone(),
        RuntimeKind::CodexExec,
        "codex-default",
        serde_json::json!({
            "workflow_id": "workflow-1449",
            "command_id": command_id,
            "activity": "implement_issue",
            "command": command.command.clone(),
        }),
    );
    runtime_job.id = format!("runtime-job-c{candidate_index}");

    RuntimeUsageRow {
        workflow,
        command_id: format!("command-1449-c{candidate_index}"),
        command_status: "dispatched".to_string(),
        command,
        runtime_job,
        latest_runtime_event_type: None,
        latest_runtime_event_at: None,
        runtime_turn_started: false,
        activity_result_ready_after_latest_turn: false,
    }
}

fn candidate_usage_record(
    candidate_index: u32,
    metrics: UsageMetrics,
    estimated_cost_usd: Option<f64>,
) -> UsageRecord {
    UsageRecord {
        agent: "codex".to_string(),
        model: "gpt-5".to_string(),
        project: "/repo".to_string(),
        metrics,
        estimated_cost_usd,
        candidate: Some(candidate_attribution(candidate_index)),
    }
}

fn candidate_attribution(candidate_index: u32) -> CandidateUsageAttribution {
    CandidateUsageAttribution {
        candidate_group_id: "workflow-1449:candidate-group:issue-1449".to_string(),
        candidate_id: format!("workflow-1449:candidate-group:issue-1449:c{candidate_index}"),
        candidate_index: Some(candidate_index),
        candidate_count: Some(2),
    }
}

fn candidate_metadata(candidate_index: u32) -> serde_json::Value {
    serde_json::json!({
        "candidate_group_id": "workflow-1449:candidate-group:issue-1449",
        "candidate_id": format!("workflow-1449:candidate-group:issue-1449:c{candidate_index}"),
        "candidate_index": candidate_index,
        "candidate_count": 2,
    })
}

fn test_invocation(
    repo: &str,
    activity: &str,
    status: &'static str,
    burn_level: &'static str,
    lease_state: Option<&'static str>,
) -> AgentInvocation {
    AgentInvocation {
        agent_invocation_id: format!("{repo}-{activity}-{status}"),
        source: "workflow_runtime",
        runtime_job_id: format!("{repo}-{activity}-{status}"),
        command_id: format!("command-{repo}-{activity}-{status}"),
        workflow_id: format!("workflow-{repo}"),
        workflow_definition: "github_issue_pr".to_string(),
        workflow_state: "implementing".to_string(),
        subject_type: "repo".to_string(),
        subject_key: repo.to_string(),
        repo: Some(repo.to_string()),
        project: None,
        issue_number: None,
        pr_number: None,
        task_id: None,
        activity: activity.to_string(),
        status,
        command_status: "dispatched".to_string(),
        agent_runtime: "codex_exec",
        runtime_profile: "codex-default".to_string(),
        model: None,
        reasoning_effort: None,
        lease_owner: None,
        lease_expires_at: None,
        lease_state,
        in_flight_model_turn: false,
        latest_runtime_event_type: None,
        last_runtime_observation_at: None,
        stale: false,
        age_secs: 60,
        updated_age_secs: 10,
        burn_level,
        cost_confidence: "estimated_runtime_burn",
    }
}
