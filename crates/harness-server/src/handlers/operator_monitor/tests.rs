use super::*;
use crate::test_helpers;
use axum::{body::to_bytes, routing::get, Router};
use harness_core::types::TaskId;
use harness_workflow::runtime::{
    WorkflowRuntimeStore, WorkflowSubject, GITHUB_ISSUE_PR_DEFINITION_ID,
    QUALITY_GATE_DEFINITION_ID,
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
        workflow("awaiting_feedback", json!({})),
        workflow("ready_to_merge", json!({})),
        workflow("awaiting_dependencies", json!({})),
        workflow("failed", json!({})),
        workflow("done", json!({})),
    ];

    let counts = runtime_workflow_counts(&workflows);

    assert_eq!(counts.running, 2);
    assert_eq!(counts.review, 1);
    assert_eq!(counts.ready_to_merge, 1);
    assert_eq!(counts.awaiting_dependencies, 1);
    assert_eq!(counts.failed, 1);
    assert_eq!(counts.done, 1);
}

#[test]
fn workflow_sample_truncation_preserves_operator_action_and_failed_states() {
    let base = Utc::now();
    let mut workflows = (0..500)
        .map(|index| {
            let mut workflow = workflow("checking", json!({ "source": "repo_backlog" }))
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
    Ok(())
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

    let workflows = list_recent_failed_workflows(&workflow_runtime_store, 1).await?;

    assert_eq!(workflows.len(), 1);
    assert_eq!(workflows[0].id, "recent-failed");
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
                &workflow("checking", json!({ "source": "repo_backlog" }))
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
            workflow("awaiting_feedback", json!({ "source": "github" })),
            workflow("scheduled", json!({ "source": "github" })),
        ],
        &[legacy_row, queued_row],
    );

    assert_eq!(by_source.len(), 1);
    let source = by_source.pop().expect("source row");
    assert_eq!(source.source, "github");
    assert_eq!(source.pending, 1);
    assert_eq!(source.ready_to_merge, 1);
    assert_eq!(source.review, 1);
    assert_eq!(source.running, 1);
    assert_eq!(source.blocked, 1);
}
