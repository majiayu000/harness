use super::*;
use crate::test_helpers;
use axum::{body::to_bytes, routing::get, Router};
use harness_core::types::TaskId;
use harness_workflow::runtime::{WorkflowSubject, GITHUB_ISSUE_PR_DEFINITION_ID};

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
        workflow("awaiting_feedback", json!({})),
        workflow("ready_to_merge", json!({})),
        workflow("awaiting_dependencies", json!({})),
        workflow("failed", json!({})),
        workflow("done", json!({})),
    ];

    let counts = runtime_workflow_counts(&workflows);

    assert_eq!(counts.running, 1);
    assert_eq!(counts.review, 1);
    assert_eq!(counts.ready_to_merge, 1);
    assert_eq!(counts.awaiting_dependencies, 1);
    assert_eq!(counts.failed, 1);
    assert_eq!(counts.done, 1);
}

#[test]
fn grouped_failures_classifies_and_counts_github_fetch_failures() {
    let failures = vec![
        github_fetch_failure("task-1", 1, "2026-06-12T00:00:00Z"),
        github_fetch_failure("task-2", 2, "2026-06-12T00:05:00Z"),
    ];

    let groups = grouped_failures(&failures);

    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].family, "github_fetch");
    assert_eq!(groups[0].severity, "warn");
    assert_eq!(groups[0].count, 2);
    assert_eq!(groups[0].repo.as_deref(), Some("harness"));
    assert!(groups[0].retryable);
    assert_eq!(groups[0].last_seen.as_deref(), Some("2026-06-12T00:05:00Z"));
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
        ],
        &[legacy_row, queued_row],
    );

    assert_eq!(by_source.len(), 1);
    let source = by_source.pop().expect("source row");
    assert_eq!(source.source, "github");
    assert_eq!(source.ready_to_merge, 1);
    assert_eq!(source.running, 0);
    assert_eq!(source.blocked, 1);
}
