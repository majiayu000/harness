use super::*;
use crate::test_helpers;
use axum::{body::to_bytes, routing::get, Router};
use harness_core::types::TaskId;
use harness_workflow::runtime::{
    RuntimeKind, WorkflowCommand, WorkflowRuntimeStore, WorkflowSubject,
    GITHUB_ISSUE_PR_DEFINITION_ID, QUALITY_GATE_DEFINITION_ID,
};

fn workflow(state: &str, data: Value) -> WorkflowInstance {
    WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        state,
        WorkflowSubject::new("issue", "issue:1"),
    )
    .with_data(data)
}

fn github_fetch_failure(id: &str, issue: u64, failed_at: &str) -> RecentFailureTask {
    RecentFailureTask {
        id: TaskId(id.to_string()),
        failure_kind: None,
        external_id: Some(format!("issue:{issue}")),
        project: Some("/tmp/harness".to_string()),
        workspace_path: None,
        workspace_owner: None,
        run_generation: 0,
        error: Some(
            "git fetch origin main failed: LibreSSL SSL_connect: SSL_ERROR_SYSCALL in connection to github.com:443"
                .to_string(),
        ),
        failed_at: Some(failed_at.to_string()),
    }
}

#[test]
fn runtime_workflow_counts_reconcile_execution_review_and_terminal_states() {
    let workflows = vec![
        workflow("implementing", json!({})),
        workflow("checking", json!({})),
        workflow("dispatching", json!({})),
        workflow("inspecting", json!({})),
        workflow("planning_batch", json!({})),
        workflow("reconciling", json!({})),
        workflow("scanning", json!({})),
        workflow("awaiting_feedback", json!({})),
        workflow("feedback_found", json!({})),
        workflow("no_actionable_feedback", json!({})),
        workflow("ready_to_merge", json!({})),
        workflow("awaiting_dependencies", json!({})),
        workflow("idle", json!({})),
        workflow("failed", json!({})),
        workflow("done", json!({})),
    ];

    let counts = runtime_workflow_counts(&workflows);

    assert_eq!(counts.running, 7);
    assert_eq!(counts.pending, 2);
    assert_eq!(counts.review, 1);
    assert_eq!(counts.ready_to_merge, 1);
    assert_eq!(counts.awaiting_dependencies, 1);
    assert_eq!(counts.failed, 1);
    assert_eq!(counts.done, 1);
    assert_eq!(counts.other, 1);
}

#[test]
fn workflow_sample_truncation_preserves_operator_action_and_failed_states() {
    let base = Utc::now();
    let mut workflows = (0..500)
        .map(|index| {
            let mut workflow = workflow("checking", json!({ "source": "github" }))
                .with_id(format!("checking-{index}"));
            workflow.updated_at = base + chrono::Duration::seconds(index);
            workflow
        })
        .collect::<Vec<_>>();
    let mut ready = workflow(
        "ready_to_merge",
        json!({
            "source": "github",
            "pr_number": 7,
            "pr_url": "https://github.com/owner/repo/pull/7",
        }),
    )
    .with_id("older-ready".to_string());
    ready.updated_at = base - chrono::Duration::hours(1);
    workflows.push(ready);
    let mut failed =
        workflow("failed", json!({ "source": "quality_gate" })).with_id("older-failed".to_string());
    failed.updated_at = base - chrono::Duration::hours(2);
    workflows.push(failed);

    truncate_workflow_sample(&mut workflows, 500);

    assert!(workflows
        .iter()
        .any(|workflow| workflow.id == "older-ready"));
    assert!(workflows
        .iter()
        .any(|workflow| workflow.id == "older-failed"));
    assert_eq!(workflows.len(), 500);
}

#[test]
fn workflow_sample_truncation_preserves_failed_reserve_before_operator_actions() {
    let base = Utc::now();
    let mut workflows = (0..500)
        .map(|index| {
            let mut workflow = workflow(
                "ready_to_merge",
                json!({
                    "source": "github",
                    "pr_number": index,
                    "pr_url": format!("https://github.com/owner/repo/pull/{index}"),
                }),
            )
            .with_id(format!("ready-{index}"));
            workflow.updated_at = base + chrono::Duration::seconds(index);
            workflow
        })
        .collect::<Vec<_>>();
    let mut failed = workflow(
        "failed",
        json!({
            "source": "quality_gate",
            "failure_reason": "Runtime transport timed out.",
            "error_kind": "timeout",
        }),
    )
    .with_id("reserved-failed-workflow".to_string());
    failed.updated_at = base - chrono::Duration::hours(1);
    workflows.push(failed);

    truncate_workflow_sample(&mut workflows, 500);

    assert_eq!(workflows.len(), 500);
    assert!(workflows
        .iter()
        .any(|workflow| workflow.id == "reserved-failed-workflow"));
}

#[test]
fn grouped_failures_classifies_and_counts_github_fetch_failures() {
    let failures = vec![
        github_fetch_failure("task-1", 1, "2026-06-12T00:00:00Z"),
        github_fetch_failure("task-2", 2, "2026-06-12T00:05:00Z"),
    ];

    let groups = grouped_failures(&failures, &[]);

    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].family, "github_fetch");
    assert_eq!(groups[0].severity, "warn");
    assert_eq!(groups[0].count, 2);
    assert_eq!(groups[0].repo.as_deref(), Some("harness"));
    assert!(groups[0].retryable);
    assert_eq!(groups[0].last_seen.as_deref(), Some("2026-06-12T00:05:00Z"));
}

#[test]
fn grouped_failures_includes_runtime_only_workflow_failures() {
    let mut failed_workflow = workflow(
        "failed",
        json!({
            "failure_reason": "Quality gate execution failed.",
            "repo": "owner/repo",
        }),
    )
    .with_id("quality-gate-workflow".to_string());
    failed_workflow.updated_at = Utc::now();

    let groups = grouped_failures(&[], &[failed_workflow]);

    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].message, "Quality gate execution failed.");
    assert_eq!(groups[0].repo.as_deref(), Some("owner/repo"));
    assert_eq!(groups[0].task_id.as_deref(), Some("quality-gate-workflow"));
}

#[test]
fn grouped_failures_deduplicates_workflow_failures_backed_by_task_rows() {
    let failures = vec![RecentFailureTask {
        id: TaskId("legacy-task".to_string()),
        failure_kind: None,
        external_id: Some("issue:1".to_string()),
        project: Some("/tmp/harness".to_string()),
        workspace_path: None,
        workspace_owner: None,
        run_generation: 0,
        error: Some("Quality gate execution failed.".to_string()),
        failed_at: Some("2026-06-12T00:00:00Z".to_string()),
    }];
    let mut failed_workflow = workflow(
        "failed",
        json!({
            "failure_reason": "Quality gate execution failed.",
            "repo": "owner/repo",
            "submission_id": "stable-submission",
            "task_id": "legacy-task",
        }),
    )
    .with_id("workflow-row".to_string());
    failed_workflow.updated_at = Utc::now();

    let groups = grouped_failures(&failures, &[failed_workflow]);

    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].message, "Quality gate execution failed.");
    assert_eq!(groups[0].count, 1);
    assert_eq!(groups[0].task_id.as_deref(), Some("legacy-task"));
}

#[test]
fn worktree_used_count_includes_stale_live_worktrees() {
    assert_eq!(worktree_used_count(Some(5), 3), 5);
    assert_eq!(stale_worktree_count(Some(5), 3), Some(2));
    assert_eq!(worktree_used_count(None, 3), 3);
    assert_eq!(stale_worktree_count(None, 3), None);
}

#[tokio::test]
async fn endpoint_returns_monitor_payload_on_fresh_state() -> anyhow::Result<()> {
    let _lock = test_helpers::HOME_LOCK.lock().await;
    let dir = test_helpers::tempdir_in_home("harness-test-operator-monitor-")?;
    let state = Arc::new(test_helpers::make_test_state(dir.path()).await?);

    let app = Router::new()
        .route("/api/operator-monitor", get(operator_monitor))
        .with_state(state);

    let req = axum::http::Request::builder()
        .uri("/api/operator-monitor")
        .body(axum::body::Body::empty())?;
    let resp = tower::ServiceExt::oneshot(app, req).await?;
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = to_bytes(resp.into_body(), usize::MAX).await?;
    let body: Value = serde_json::from_slice(&bytes)?;

    for key in [
        "generated_at",
        "health",
        "activity",
        "operator_actions",
        "failures",
        "worktrees",
    ] {
        assert!(body.get(key).is_some(), "missing top-level key: {key}");
    }
    assert_eq!(body["worktrees"]["metrics_state"], "unavailable");
    assert!(body["activity"]["token_dispatch_by_repo"].is_array());
    Ok(())
}

#[tokio::test]
async fn endpoint_includes_github_token_dispatch_counters() -> anyhow::Result<()> {
    let _lock = test_helpers::HOME_LOCK.lock().await;
    let dir = test_helpers::tempdir_in_home("harness-test-operator-monitor-token-dispatch-")?;
    let state = Arc::new(test_helpers::make_test_state(dir.path()).await?);
    state.intake.record_github_token_dispatch(
        "owner/repo",
        crate::http::GitHubTokenDispatchMetric::ServerGithubPoll,
    );
    state.intake.record_github_token_dispatch(
        "owner/repo",
        crate::http::GitHubTokenDispatchMetric::AgentImplementIssue,
    );
    state.intake.record_github_token_dispatch(
        "owner/repo",
        crate::http::GitHubTokenDispatchMetric::AgentSkippedCoveredIssue,
    );

    let app = Router::new()
        .route("/api/operator-monitor", get(operator_monitor))
        .with_state(state);

    let req = axum::http::Request::builder()
        .uri("/api/operator-monitor")
        .body(axum::body::Body::empty())?;
    let resp = tower::ServiceExt::oneshot(app, req).await?;
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = to_bytes(resp.into_body(), usize::MAX).await?;
    let body: Value = serde_json::from_slice(&bytes)?;
    let counters = body["activity"]["token_dispatch_by_repo"]
        .as_array()
        .expect("token_dispatch_by_repo should be an array");

    assert_eq!(counters.len(), 1);
    assert_eq!(counters[0]["repo"], "owner/repo");
    assert_eq!(counters[0]["server_github_poll_count"], 1);
    assert_eq!(counters[0]["agent_implement_issue_count"], 1);
    assert_eq!(counters[0]["agent_skipped_covered_issue_count"], 1);
    assert_eq!(counters[0]["agent_address_feedback_count"], 0);
    assert_eq!(counters[0]["agent_merge_pr_count"], 0);
    Ok(())
}

#[tokio::test]
async fn endpoint_marks_health_degraded_when_runtime_state_is_dirty() -> anyhow::Result<()> {
    let _lock = test_helpers::HOME_LOCK.lock().await;
    let dir = test_helpers::tempdir_in_home("harness-test-operator-monitor-dirty-state-")?;
    let state = Arc::new(test_helpers::make_test_state(dir.path()).await?);
    state
        .runtime_state_dirty
        .store(true, std::sync::atomic::Ordering::Release);

    let app = Router::new()
        .route("/api/operator-monitor", get(operator_monitor))
        .with_state(state);

    let req = axum::http::Request::builder()
        .uri("/api/operator-monitor")
        .body(axum::body::Body::empty())?;
    let resp = tower::ServiceExt::oneshot(app, req).await?;
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = to_bytes(resp.into_body(), usize::MAX).await?;
    let body: Value = serde_json::from_slice(&bytes)?;

    assert_eq!(body["health"]["status"], "degraded");
    Ok(())
}

#[test]
fn operator_actions_link_evidence_to_current_legacy_task_id() {
    let mut ready = workflow(
        "ready_to_merge",
        json!({
            "repo": "owner/repo",
            "pr_number": 7,
            "pr_url": "https://github.com/owner/repo/pull/7",
            "submission_id": "stable-submission",
            "task_id": "current-task",
        }),
    )
    .with_id("ready-workflow".to_string());
    ready.updated_at = Utc::now();

    let actions = operator_actions(&[ready], Utc::now(), &std::collections::HashMap::new());

    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0].task_id.as_deref(), Some("current-task"));
    assert_eq!(
        actions[0].evidence_url.as_deref(),
        Some("/tasks/current-task")
    );
}

#[test]
fn operator_monitor_actions_expose_structured_stop_metadata_and_eligibility() {
    let blocked = workflow(
        "blocked",
        json!({
            "repo": "owner/repo",
            "issue_number": 1567,
            "blocked_reason": "Waiting for maintainer approval.",
            "unblock_hint": "Post the approval comment, then call unblock.",
            "last_stop": {
                "state": "blocked",
                "activity": "implement_issue",
                "runtime_job_id": "job-blocked"
            },
        }),
    )
    .with_id("blocked-workflow".to_string());
    let retryable_failed = workflow(
        "failed",
        json!({
            "repo": "owner/repo",
            "issue_number": 1568,
            "failure_reason": "Runtime transport timed out.",
            "error_kind": "timeout",
            "retry_hint": "Fix the transient condition, then call retry.",
            "last_stop": {
                "state": "failed",
                "activity": "implement_issue",
                "runtime_job_id": "job-failed"
            },
        }),
    )
    .with_id("retryable-failed-workflow".to_string());
    let configuration_failed = workflow(
        "failed",
        json!({
            "repo": "owner/repo",
            "issue_number": 1569,
            "failure_reason": "Missing runtime configuration.",
            "error_kind": "configuration",
            "retry_hint": "Fix the non-retryable failure before retrying.",
        }),
    )
    .with_id("configuration-failed-workflow".to_string());
    let cancelled = workflow(
        "cancelled",
        json!({
            "repo": "owner/repo",
            "issue_number": 1570,
            "failure_reason": "Operator cancelled the workflow.",
        }),
    )
    .with_id("cancelled-workflow".to_string());

    let mut stopped_eligibility = std::collections::HashMap::new();
    stopped_eligibility.insert(
        "blocked-workflow".to_string(),
        RuntimeStoppedActionEligibility {
            can_unblock: true,
            can_retry: false,
        },
    );
    stopped_eligibility.insert(
        "retryable-failed-workflow".to_string(),
        RuntimeStoppedActionEligibility {
            can_unblock: false,
            can_retry: true,
        },
    );
    let actions = operator_actions(
        &[blocked, retryable_failed, configuration_failed, cancelled],
        Utc::now(),
        &stopped_eligibility,
    );
    let row = |workflow_id: &str| {
        actions
            .iter()
            .find(|action| action.workflow_id == workflow_id)
            .map(|action| serde_json::to_value(action).expect("action should serialize"))
            .unwrap_or_else(|| panic!("missing operator action for {workflow_id}"))
    };

    let blocked = row("blocked-workflow");
    assert_eq!(blocked["kind"], "blocked");
    assert_eq!(
        blocked["blocked_reason"],
        "Waiting for maintainer approval."
    );
    assert_eq!(
        blocked["unblock_hint"],
        "Post the approval comment, then call unblock."
    );
    assert_eq!(blocked["last_stop"]["activity"], "implement_issue");
    assert_eq!(blocked["can_unblock"], true);
    assert_eq!(blocked["can_retry"], false);

    let retryable_failed = row("retryable-failed-workflow");
    assert_eq!(retryable_failed["kind"], "failed");
    assert_eq!(
        retryable_failed["failure_reason"],
        "Runtime transport timed out."
    );
    assert_eq!(retryable_failed["error_kind"], "timeout");
    assert_eq!(retryable_failed["next_action"], "Retry failed workflow");
    assert_eq!(
        retryable_failed["retry_hint"],
        "Fix the transient condition, then call retry."
    );
    assert_eq!(
        retryable_failed["last_stop"]["runtime_job_id"],
        "job-failed"
    );
    assert_eq!(retryable_failed["can_unblock"], false);
    assert_eq!(retryable_failed["can_retry"], true);

    let configuration_failed = row("configuration-failed-workflow");
    assert_eq!(configuration_failed["error_kind"], "configuration");
    assert_eq!(
        configuration_failed["next_action"],
        "Inspect failed workflow"
    );
    assert_eq!(configuration_failed["can_unblock"], false);
    assert_eq!(configuration_failed["can_retry"], false);
    assert!(actions
        .iter()
        .all(|action| action.workflow_id != "cancelled-workflow"));
}

#[test]
fn operator_monitor_stuck_workflows_expose_structured_stop_metadata() {
    let blocked = workflow(
        "blocked",
        json!({
            "repo": "owner/repo",
            "issue_number": 1567,
            "blocked_reason": "Waiting for maintainer approval.",
            "unblock_hint": "Post the approval comment, then call unblock.",
            "last_stop": {
                "state": "blocked",
                "activity": "implement_issue",
                "runtime_job_id": "job-blocked",
            },
        }),
    )
    .with_id("stuck-blocked-workflow".to_string());

    let mut stopped_eligibility = std::collections::HashMap::new();
    stopped_eligibility.insert(
        "stuck-blocked-workflow".to_string(),
        RuntimeStoppedActionEligibility {
            can_unblock: true,
            can_retry: false,
        },
    );
    let stuck = stuck_workflows_from_instances(&[blocked], Utc::now(), &stopped_eligibility);
    let row = serde_json::to_value(&stuck[0]).expect("stuck workflow should serialize");

    assert_eq!(row["workflow_id"], "stuck-blocked-workflow");
    assert_eq!(row["blocked_reason"], "Waiting for maintainer approval.");
    assert_eq!(
        row["unblock_hint"],
        "Post the approval comment, then call unblock."
    );
    assert_eq!(row["last_stop"]["runtime_job_id"], "job-blocked");
    assert_eq!(row["can_unblock"], true);
    assert_eq!(row["can_retry"], false);
}

#[test]
fn operator_monitor_stuck_workflows_expose_auto_recovery_fields() {
    // GH-1584 exposure: persisted classification and attempt state surface
    // as optional fields; legacy rows omit them entirely (B-014).
    let classified = workflow(
        "blocked",
        json!({
            "repo": "owner/repo",
            "blocked_reason": "GitHub API rate limited",
            "stop_reason_code": "rate_limited",
            "reason_class": "transient",
            "auto_recovery": {
                "episode_event_id": "episode-1",
                "attempts": 2,
                "next_attempt_at": "2026-07-14T10:00:00Z",
                "exhausted": false,
            },
            "last_stop": {
                "state": "blocked",
                "activity": "implement_issue",
                "event_id": "episode-1",
            },
        }),
    )
    .with_id("stuck-auto-recovery-workflow".to_string());
    let legacy = workflow(
        "blocked",
        json!({ "repo": "owner/repo", "blocked_reason": "legacy free text" }),
    )
    .with_id("stuck-legacy-workflow".to_string());

    let stuck = stuck_workflows_from_instances(
        &[classified, legacy],
        Utc::now(),
        &std::collections::HashMap::new(),
    );
    let by_id = |id: &str| {
        stuck
            .iter()
            .find(|row| row.workflow_id == id)
            .map(|row| serde_json::to_value(row).expect("stuck workflow should serialize"))
            .expect("row present")
    };

    let classified_row = by_id("stuck-auto-recovery-workflow");
    assert_eq!(classified_row["stop_reason_code"], "rate_limited");
    assert_eq!(classified_row["reason_class"], "transient");
    assert_eq!(classified_row["auto_recovery_attempts"], 2);
    assert_eq!(classified_row["next_recheck_at"], "2026-07-14T10:00:00Z");
    assert_eq!(classified_row["auto_recovery_exhausted"], false);

    let legacy_row = by_id("stuck-legacy-workflow");
    for field in [
        "stop_reason_code",
        "reason_class",
        "auto_recovery_attempts",
        "next_recheck_at",
        "auto_recovery_exhausted",
    ] {
        assert!(
            legacy_row.get(field).is_none(),
            "legacy rows must omit {field} instead of fabricating it"
        );
    }
}

#[tokio::test]
async fn stopped_action_eligibility_matches_recovery_contract_rejections() -> anyhow::Result<()> {
    if !test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let _lock = test_helpers::HOME_LOCK.lock().await;
    let dir = test_helpers::tempdir_in_home("harness-test-stopped-action-eligibility-")?;
    let store = WorkflowRuntimeStore::open_with_database_url(
        &harness_core::config::dirs::default_db_path(dir.path(), "workflow_runtime"),
        Some(&test_helpers::test_database_url()?),
    )
    .await?;

    let legacy_blocked = store_workflow(
        &store,
        workflow("blocked", json!({ "blocked_reason": "legacy blocked row" }))
            .with_id("legacy-blocked".to_string()),
    )
    .await?;
    let legacy_null_last_stop = store_workflow(
        &store,
        workflow(
            "failed",
            json!({
                "failure_reason": "Legacy workflow failed before structured metadata shipped.",
                "last_stop": null,
            }),
        )
        .with_id("legacy-null-last-stop".to_string()),
    )
    .await?;
    let valid_blocked = store_stopped_workflow_with_source(
        &store,
        "valid-blocked",
        "blocked",
        json!({
            "blocked_reason": "Waiting for maintainer approval.",
            "last_stop": {
                "state": "blocked",
                "activity": "implement_issue",
            },
        }),
        WorkflowCommand::enqueue_activity("implement_issue", "valid-blocked-source"),
    )
    .await?;
    let valid_unknown = store_stopped_workflow_with_source(
        &store,
        "valid-unknown",
        "failed",
        json!({
            "error_kind": "unknown",
            "last_stop": {
                "state": "failed",
                "activity": "implement_issue",
                "error_kind": "unknown",
            },
        }),
        WorkflowCommand::enqueue_activity("implement_issue", "valid-unknown-source"),
    )
    .await?;
    let non_github = store_quality_gate_workflow(
        &store,
        "non-github",
        "blocked",
        json!({
            "last_stop": {
                "state": "blocked",
                "activity": "implement_issue",
            },
        }),
        WorkflowCommand::enqueue_activity("implement_issue", "non-github-source"),
    )
    .await?;
    let empty_last_stop = store_workflow(
        &store,
        workflow("blocked", json!({ "last_stop": {} })).with_id("empty-last-stop".to_string()),
    )
    .await?;
    let partial_last_stop = store_workflow(
        &store,
        workflow(
            "failed",
            json!({
                "error_kind": "timeout",
                "last_stop": {
                    "state": "failed",
                    "activity": "implement_issue",
                },
            }),
        )
        .with_id("partial-last-stop".to_string()),
    )
    .await?;
    let non_object_last_stop = store_workflow(
        &store,
        workflow("blocked", json!({ "last_stop": 42 })).with_id("non-object-last-stop".to_string()),
    )
    .await?;
    let invalid_error_kind = store_stopped_workflow_with_source(
        &store,
        "invalid-error-kind",
        "failed",
        json!({
            "error_kind": "not_a_kind",
            "last_stop": {
                "state": "failed",
                "activity": "implement_issue",
            },
        }),
        WorkflowCommand::enqueue_activity("implement_issue", "invalid-error-kind-source"),
    )
    .await?;
    let unsupported_activity = store_stopped_workflow_with_source(
        &store,
        "unsupported-activity",
        "failed",
        json!({
            "error_kind": "timeout",
            "last_stop": {
                "state": "failed",
                "activity": "quality_gate",
            },
        }),
        WorkflowCommand::enqueue_activity("quality_gate", "unsupported-activity-source"),
    )
    .await?;
    let missing_runtime_job_id = store_workflow(
        &store,
        workflow(
            "blocked",
            json!({
                "last_stop": {
                    "state": "blocked",
                    "activity": "implement_issue",
                },
            }),
        )
        .with_id("missing-runtime-job-id".to_string()),
    )
    .await?;
    let missing_source_command = store_workflow(
        &store,
        workflow(
            "failed",
            json!({
                "error_kind": "timeout",
                "last_stop": {
                    "state": "failed",
                    "activity": "implement_issue",
                    "runtime_job_id": "missing-runtime-job",
                },
            }),
        )
        .with_id("missing-source-command".to_string()),
    )
    .await?;
    let command_target_mismatch = store_stopped_workflow_with_source(
        &store,
        "command-target-mismatch",
        "failed",
        json!({
            "error_kind": "timeout",
            "last_stop": {
                "state": "failed",
                "activity": "implement_issue",
            },
        }),
        WorkflowCommand::enqueue_activity("replan_issue", "command-target-mismatch-source"),
    )
    .await?;

    let workflows = vec![
        legacy_blocked,
        legacy_null_last_stop,
        valid_blocked,
        valid_unknown,
        non_github,
        empty_last_stop,
        partial_last_stop,
        non_object_last_stop,
        invalid_error_kind,
        unsupported_activity,
        missing_runtime_job_id,
        missing_source_command,
        command_target_mismatch,
    ];
    let eligibility = stopped_action_eligibility_for_workflows(Some(&store), &workflows).await?;
    let flags = |workflow_id: &str| eligibility.get(workflow_id).copied().unwrap_or_default();

    assert_eq!(
        flags("legacy-blocked"),
        RuntimeStoppedActionEligibility {
            can_unblock: true,
            can_retry: false,
        }
    );
    assert_eq!(
        flags("legacy-null-last-stop"),
        RuntimeStoppedActionEligibility {
            can_unblock: false,
            can_retry: true,
        }
    );
    assert_eq!(
        flags("valid-blocked"),
        RuntimeStoppedActionEligibility {
            can_unblock: true,
            can_retry: false,
        }
    );
    assert_eq!(
        flags("valid-unknown"),
        RuntimeStoppedActionEligibility {
            can_unblock: false,
            can_retry: true,
        }
    );
    for workflow_id in [
        "non-github",
        "empty-last-stop",
        "partial-last-stop",
        "non-object-last-stop",
        "invalid-error-kind",
        "unsupported-activity",
        "missing-runtime-job-id",
        "missing-source-command",
        "command-target-mismatch",
    ] {
        assert_eq!(
            flags(workflow_id),
            RuntimeStoppedActionEligibility::default(),
            "{workflow_id}"
        );
    }
    Ok(())
}

async fn store_workflow(
    store: &WorkflowRuntimeStore,
    workflow: WorkflowInstance,
) -> anyhow::Result<WorkflowInstance> {
    store.upsert_instance(&workflow).await?;
    Ok(workflow)
}

async fn store_stopped_workflow_with_source(
    store: &WorkflowRuntimeStore,
    workflow_id: &str,
    state: &str,
    data: Value,
    command: WorkflowCommand,
) -> anyhow::Result<WorkflowInstance> {
    let workflow = workflow(state, data).with_id(workflow_id.to_string());
    store.upsert_instance(&workflow).await?;
    let workflow = attach_recovery_source_job(store, workflow, command).await?;
    Ok(workflow)
}

async fn store_quality_gate_workflow(
    store: &WorkflowRuntimeStore,
    workflow_id: &str,
    state: &str,
    data: Value,
    command: WorkflowCommand,
) -> anyhow::Result<WorkflowInstance> {
    let workflow = WorkflowInstance::new(
        QUALITY_GATE_DEFINITION_ID,
        1,
        state,
        WorkflowSubject::new("quality_gate", workflow_id),
    )
    .with_id(workflow_id.to_string())
    .with_data(data);
    store.upsert_instance(&workflow).await?;
    attach_recovery_source_job(store, workflow, command).await
}

async fn attach_recovery_source_job(
    store: &WorkflowRuntimeStore,
    mut workflow: WorkflowInstance,
    command: WorkflowCommand,
) -> anyhow::Result<WorkflowInstance> {
    let command_id = store.enqueue_command(&workflow.id, None, &command).await?;
    let job = store
        .enqueue_runtime_job(
            &command_id,
            RuntimeKind::CodexJsonrpc,
            "codex-test",
            command.command.clone(),
        )
        .await?;
    workflow.data["last_stop"]["runtime_job_id"] = json!(job.id);
    store.upsert_instance(&workflow).await?;
    Ok(workflow)
}

#[test]
fn idle_workflows_are_inactive_for_source_activity() {
    let workflows = vec![workflow("idle", json!({ "source": "github" }))];

    let counts = runtime_workflow_counts(&workflows);
    let by_source = source_activity(&workflows, &[]);

    assert_eq!(counts.pending, 0);
    assert_eq!(counts.running, 0);
    assert_eq!(counts.other, 1);
    assert!(by_source.is_empty());
}

#[tokio::test]
async fn endpoint_includes_failed_runtime_workflows_without_legacy_tasks() -> anyhow::Result<()> {
    let _lock = test_helpers::HOME_LOCK.lock().await;
    let dir = test_helpers::tempdir_in_home("harness-test-operator-monitor-failed-runtime-")?;
    let mut state = test_helpers::make_test_state(dir.path()).await?;
    let workflow_runtime_store = Arc::new(
        WorkflowRuntimeStore::open_with_database_url(
            &harness_core::config::dirs::default_db_path(dir.path(), "workflow_runtime"),
            Some(&test_helpers::test_database_url()?),
        )
        .await?,
    );
    workflow_runtime_store
        .upsert_instance(
            &WorkflowInstance::new(
                QUALITY_GATE_DEFINITION_ID,
                1,
                "failed",
                WorkflowSubject::new("quality_gate", "quality_gate:1"),
            )
            .with_id("quality-gate-failed".to_string())
            .with_data(json!({
                "source": "quality_gate",
                "repo": "owner/repo",
            })),
        )
        .await?;
    state.core.workflow_runtime_store = Some(workflow_runtime_store);

    let app = Router::new()
        .route("/api/operator-monitor", get(operator_monitor))
        .with_state(Arc::new(state));

    let req = axum::http::Request::builder()
        .uri("/api/operator-monitor")
        .body(axum::body::Body::empty())?;
    let resp = tower::ServiceExt::oneshot(app, req).await?;
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = to_bytes(resp.into_body(), usize::MAX).await?;
    let body: Value = serde_json::from_slice(&bytes)?;

    assert_eq!(body["activity"]["runtime_workflows"]["failed"], 1);
    let sources = body["activity"]["by_source"]
        .as_array()
        .expect("source rows");
    let quality_gate = sources
        .iter()
        .find(|source| source["source"] == "quality_gate")
        .expect("quality gate source row");
    assert_eq!(quality_gate["failed"], 1);
    Ok(())
}

#[tokio::test]
async fn endpoint_includes_config_enabled_stuck_workflows() -> anyhow::Result<()> {
    let _lock = test_helpers::HOME_LOCK.lock().await;
    let dir = test_helpers::tempdir_in_home("harness-test-operator-monitor-stuck-workflow-")?;
    std::fs::write(
        dir.path().join("WORKFLOW.md"),
        "---\nstorage:\n  workflow_watchdog_enabled: true\n  workflow_watchdog_age_minutes: 60\n---\nBody\n",
    )?;
    let mut state = test_helpers::make_test_state(dir.path()).await?;
    let workflow_runtime_store = Arc::new(
        WorkflowRuntimeStore::open_with_database_url(
            &harness_core::config::dirs::default_db_path(dir.path(), "workflow_runtime"),
            Some(&test_helpers::test_database_url()?),
        )
        .await?,
    );
    workflow_runtime_store
        .upsert_instance(
            &workflow(
                "awaiting_feedback",
                json!({
                    "source": "github",
                    "repo": "owner/repo",
                    "issue_number": 44,
                    "pr_number": 45,
                    "pr_url": "https://github.com/owner/repo/pull/45",
                }),
            )
            .with_id("stuck-awaiting-feedback".to_string()),
        )
        .await?;
    sqlx::query(
        "UPDATE workflow_instances SET updated_at = NOW() - INTERVAL '2 hours' WHERE id = $1",
    )
    .bind("stuck-awaiting-feedback")
    .execute(workflow_runtime_store.pool())
    .await?;
    state.core.workflow_runtime_store = Some(workflow_runtime_store);

    let app = Router::new()
        .route("/api/operator-monitor", get(operator_monitor))
        .with_state(Arc::new(state));

    let req = axum::http::Request::builder()
        .uri("/api/operator-monitor")
        .body(axum::body::Body::empty())?;
    let resp = tower::ServiceExt::oneshot(app, req).await?;
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = to_bytes(resp.into_body(), usize::MAX).await?;
    let body: Value = serde_json::from_slice(&bytes)?;
    let stuck = body["stuck_workflows"]
        .as_array()
        .expect("stuck_workflows should be an array");

    assert_eq!(stuck.len(), 1);
    assert_eq!(stuck[0]["workflow_id"], "stuck-awaiting-feedback");
    assert_eq!(stuck[0]["state"], "awaiting_feedback");
    assert_eq!(stuck[0]["repo"], "owner/repo");
    assert_eq!(stuck[0]["issue"], 44);
    assert_eq!(stuck[0]["pr"], 45);
    Ok(())
}

const DECLARATIVE_VISIBILITY_DEFINITION_ID: &str = "operator_monitor_visibility_flow";
static REGISTER_DECLARATIVE_VISIBILITY_DEFINITION: std::sync::Once = std::sync::Once::new();

/// Register a uniquely-named declarative definition into the process-global
/// registry (GH-1609 fixture pattern, `Once`-guarded so it is idempotent across
/// tests in this binary). It carries no built-in instances, so counting tests in
/// sibling suites are unaffected.
fn register_declarative_visibility_definition() {
    use harness_core::config::workflow::{
        DeclaredProgressMode, DeclaredState, WorkflowActivityPolicy, WorkflowDefinitionPolicy,
    };
    use std::collections::BTreeMap;

    REGISTER_DECLARATIVE_VISIBILITY_DEFINITION.call_once(|| {
        let policy = WorkflowDefinitionPolicy {
            id: DECLARATIVE_VISIBILITY_DEFINITION_ID.to_string(),
            initial: "working".to_string(),
            states: BTreeMap::from([
                (
                    "working".to_string(),
                    DeclaredState {
                        activity: Some("perform_work".to_string()),
                        on_success: Some("done".to_string()),
                        on_failure: Some("failed".to_string()),
                        on_blocked: Some("blocked".to_string()),
                        ..DeclaredState::default()
                    },
                ),
                (
                    "blocked".to_string(),
                    DeclaredState {
                        progress: Some(DeclaredProgressMode::OperatorGate),
                        ..DeclaredState::default()
                    },
                ),
            ]),
            terminal: BTreeMap::from([
                ("done".to_string(), "succeeded".to_string()),
                ("failed".to_string(), "failed".to_string()),
            ]),
            evidence_required: BTreeMap::new(),
            recovery_targets: vec!["working".to_string()],
        };
        let definition = harness_workflow::runtime::build_declarative_definition(
            &policy,
            &BTreeMap::from([(
                "perform_work".to_string(),
                WorkflowActivityPolicy::default(),
            )]),
        )
        .expect("visibility fixture definition should be valid");
        harness_workflow::runtime::register_declarative_workflow_definitions([definition])
            .expect("visibility fixture definition should register");
    });
}

/// B-003 / B-005: a blocked instance of a declarative definition surfaces in the
/// operator monitor's runtime-workflow sample (registry-driven enumeration), and
/// a declarative definition with no instances contributes nothing.
#[tokio::test]
async fn declarative_definition_instances_are_visible_in_operator_monitor() -> anyhow::Result<()> {
    if !test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    register_declarative_visibility_definition();

    let _lock = test_helpers::HOME_LOCK.lock().await;
    let dir = test_helpers::tempdir_in_home("harness-test-operator-monitor-declarative-")?;
    let store = WorkflowRuntimeStore::open_with_database_url(
        &harness_core::config::dirs::default_db_path(dir.path(), "workflow_runtime"),
        Some(&test_helpers::test_database_url()?),
    )
    .await?;

    store
        .upsert_instance(
            &WorkflowInstance::new(
                DECLARATIVE_VISIBILITY_DEFINITION_ID,
                1,
                "blocked",
                WorkflowSubject::new("issue", "issue:9001"),
            )
            .with_id("declarative-blocked".to_string())
            .with_data(json!({ "repo": "owner/repo", "blocked_reason": "operator gate" })),
        )
        .await?;

    let workflows = list_runtime_workflows_from_store(&store).await?;

    assert!(
        workflows
            .iter()
            .any(|workflow| workflow.id == "declarative-blocked"
                && workflow.definition_id == DECLARATIVE_VISIBILITY_DEFINITION_ID),
        "declarative blocked instance must appear in the operator monitor sample"
    );
    // B-005: no instances were created for any other definition, so the built-ins
    // contribute nothing here — the only sampled workflow is the declarative one.
    assert_eq!(workflows.len(), 1);
    Ok(())
}

#[tokio::test]
async fn recent_failed_workflow_sampling_prefers_newest_rows() -> anyhow::Result<()> {
    let _lock = test_helpers::HOME_LOCK.lock().await;
    let dir = test_helpers::tempdir_in_home("harness-test-operator-monitor-recent-failed-")?;
    let workflow_runtime_store = WorkflowRuntimeStore::open_with_database_url(
        &harness_core::config::dirs::default_db_path(dir.path(), "workflow_runtime"),
        Some(&test_helpers::test_database_url()?),
    )
    .await?;
    workflow_runtime_store
        .upsert_instance(
            &WorkflowInstance::new(
                QUALITY_GATE_DEFINITION_ID,
                1,
                "failed",
                WorkflowSubject::new("quality_gate", "quality_gate:old"),
            )
            .with_id("old-failed".to_string()),
        )
        .await?;
    workflow_runtime_store
        .upsert_instance(
            &WorkflowInstance::new(
                QUALITY_GATE_DEFINITION_ID,
                1,
                "failed",
                WorkflowSubject::new("quality_gate", "quality_gate:recent"),
            )
            .with_id("recent-failed".to_string()),
        )
        .await?;
    sqlx::query("UPDATE workflow_instances SET updated_at = $2 WHERE id = $1")
        .bind("old-failed")
        .bind(Utc::now() - chrono::Duration::hours(1))
        .execute(workflow_runtime_store.pool())
        .await?;
    sqlx::query("UPDATE workflow_instances SET updated_at = $2 WHERE id = $1")
        .bind("recent-failed")
        .bind(Utc::now())
        .execute(workflow_runtime_store.pool())
        .await?;

    let definition_ids = crate::handlers::definition_ids::operator_definition_ids()?;
    let workflows =
        list_recent_failed_workflows(&workflow_runtime_store, &definition_ids, 1).await?;

    assert_eq!(workflows.len(), 1);
    assert_eq!(workflows[0].id, "recent-failed");
    Ok(())
}

#[tokio::test]
async fn operator_action_age_uses_store_updated_at() -> anyhow::Result<()> {
    let _lock = test_helpers::HOME_LOCK.lock().await;
    let dir = test_helpers::tempdir_in_home("harness-test-operator-monitor-action-age-")?;
    let workflow_runtime_store = WorkflowRuntimeStore::open_with_database_url(
        &harness_core::config::dirs::default_db_path(dir.path(), "workflow_runtime"),
        Some(&test_helpers::test_database_url()?),
    )
    .await?;
    let mut ready = workflow(
        "ready_to_merge",
        json!({
            "source": "github",
            "pr_number": 7,
            "pr_url": "https://github.com/owner/repo/pull/7",
        }),
    )
    .with_id("ready-store-age".to_string());
    ready.updated_at = Utc::now() - chrono::Duration::days(2);
    workflow_runtime_store.upsert_instance(&ready).await?;
    sqlx::query("UPDATE workflow_instances SET updated_at = NOW() WHERE id = $1")
        .bind("ready-store-age")
        .execute(workflow_runtime_store.pool())
        .await?;

    let workflows = list_runtime_workflows_from_store(&workflow_runtime_store).await?;
    let actions = operator_actions(&workflows, Utc::now(), &std::collections::HashMap::new());

    let action = actions
        .iter()
        .find(|action| action.workflow_id == "ready-store-age")
        .expect("ready action");
    assert!(
        action.age_secs < 60,
        "action age should use the fresh store timestamp, got {}",
        action.age_secs
    );
    Ok(())
}

#[tokio::test]
async fn runtime_workflow_sampling_fetches_action_states_before_definition_cap(
) -> anyhow::Result<()> {
    let _lock = test_helpers::HOME_LOCK.lock().await;
    let dir = test_helpers::tempdir_in_home("harness-test-operator-monitor-action-cap-")?;
    let workflow_runtime_store = WorkflowRuntimeStore::open_with_database_url(
        &harness_core::config::dirs::default_db_path(dir.path(), "workflow_runtime"),
        Some(&test_helpers::test_database_url()?),
    )
    .await?;
    workflow_runtime_store
        .upsert_instance(
            &workflow(
                "ready_to_merge",
                json!({
                    "source": "github",
                    "pr_number": 7,
                    "pr_url": "https://github.com/owner/repo/pull/7",
                }),
            )
            .with_id("older-ready".to_string()),
        )
        .await?;
    for index in 0..500 {
        workflow_runtime_store
            .upsert_instance(
                &workflow("checking", json!({ "source": "github" }))
                    .with_id(format!("checking-{index}")),
            )
            .await?;
    }
    sqlx::query(
        "UPDATE workflow_instances SET updated_at = NOW() - INTERVAL '1 hour' WHERE id = $1",
    )
    .bind("older-ready")
    .execute(workflow_runtime_store.pool())
    .await?;
    sqlx::query("UPDATE workflow_instances SET updated_at = NOW() WHERE state = $1")
        .bind("checking")
        .execute(workflow_runtime_store.pool())
        .await?;

    let workflows = list_runtime_workflows_from_store(&workflow_runtime_store).await?;

    assert_eq!(workflows.len(), WORKFLOW_SAMPLE_LIMIT as usize);
    assert!(workflows
        .iter()
        .any(|workflow| workflow.id == "older-ready"));
    Ok(())
}

#[test]
fn workflow_backed_and_queued_tasks_are_not_counted_by_source() {
    let legacy_row = TaskSummary {
        id: TaskId("legacy-row".to_string()),
        task_kind: crate::task_runner::TaskKind::Issue,
        status: crate::task_runner::TaskStatus::Waiting,
        failure_kind: None,
        turn: 0,
        pr_url: None,
        error: None,
        source: Some("github".to_string()),
        parent_id: None,
        external_id: Some("issue:1".to_string()),
        repo: Some("owner/repo".to_string()),
        description: None,
        created_at: None,
        phase: crate::task_runner::TaskPhase::Review,
        depends_on: vec![],
        subtask_ids: vec![],
        project: None,
        workspace_path: None,
        workspace_owner: None,
        run_generation: 0,
        workflow: None,
        scheduler: crate::task_runner::TaskSchedulerState::queued(),
    };
    let mut queued_row = legacy_row.clone();
    queued_row.id = TaskId("queued-row".to_string());
    queued_row.workflow = None;
    queued_row.status = crate::task_runner::TaskStatus::Pending;

    let mut by_source = source_activity(
        &[
            workflow(
                "ready_to_merge",
                json!({
                    "source": "github",
                    "task_id": "legacy-row",
                    "pr_number": 7,
                    "pr_url": "https://github.com/owner/repo/pull/7",
                }),
            ),
            workflow("awaiting_dependencies", json!({ "source": "github" })),
            workflow("checking", json!({ "source": "github" })),
            workflow("inspecting", json!({ "source": "github" })),
            workflow("awaiting_feedback", json!({ "source": "github" })),
            workflow("feedback_found", json!({ "source": "github" })),
            workflow("no_actionable_feedback", json!({ "source": "github" })),
            workflow("scheduled", json!({ "source": "github" })),
        ],
        &[legacy_row, queued_row],
    );

    assert_eq!(by_source.len(), 1);
    let source = by_source.pop().expect("source row");
    assert_eq!(source.source, "github");
    assert_eq!(source.pending, 3);
    assert_eq!(source.ready_to_merge, 1);
    assert_eq!(source.review, 1);
    assert_eq!(source.running, 2);
    assert_eq!(source.blocked, 1);
}
