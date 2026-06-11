use super::*;
use harness_workflow::runtime::RuntimeKind;

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
        activity_result_ready: false,
    };

    let tokens = runtime_attribution_tokens(&[row]);

    assert!(tokens.contains("command-1"));
    assert!(tokens.contains(&runtime_job_id));
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
        activity_result_ready: false,
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
