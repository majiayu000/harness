use super::*;
use crate::test_helpers;
use axum::{body::to_bytes, routing::get, Router};
use harness_core::types::SessionId;

#[test]
fn hour_axis_has_expected_length_and_order() {
    let now = Utc::now();
    let axis = build_hour_axis(now);
    assert_eq!(axis.len(), THROUGHPUT_BUCKETS);
    // Oldest first, strictly increasing.
    for pair in axis.windows(2) {
        assert!(pair[0] < pair[1]);
    }
}

#[test]
fn relative_ago_formats_each_bucket() {
    let now = Utc::now();
    assert!(relative_ago(now, now).ends_with('s'));
    assert!(relative_ago(now, now - Duration::minutes(5)).ends_with('m'));
    assert!(relative_ago(now, now - Duration::hours(3)).ends_with('h'));
    assert!(relative_ago(now, now - Duration::days(2)).ends_with('d'));
}

#[test]
fn rule_fail_rate_zero_when_no_events() {
    assert_eq!(compute_rule_fail_rate_pct(&[]), 0.0);
}

#[test]
fn parse_evolution_returns_none_for_old_reason_format() {
    // Old events without the new fields must degrade to None, not panic.
    let old_reason = "rules=0 skills=0 scored=0 quarantine=0 retired=0";
    assert!(parse_evolution_from_reason(Some(old_reason)).is_none());
    assert!(parse_evolution_from_reason(None).is_none());
}

#[test]
fn parse_evolution_extracts_three_fields() {
    let reason = "rules=2 skills=1 scored=5 quarantine=0 retired=1 drafts_pending=3 drafts_adopted=2 skills_invoked=7";
    let v = parse_evolution_from_reason(Some(reason)).expect("should parse");
    assert_eq!(v["drafts_pending"], 3u64);
    assert_eq!(v["drafts_auto_adopted"], 2u64);
    assert_eq!(v["skills_invoked_in_window"], 7u64);
}

#[test]
fn llm_usage_summary_sums_projects_and_agents() {
    let mut claude = Event::new(SessionId::new(), "llm_usage", "claude", Decision::Complete);
    claude.content = Some(
        serde_json::json!({
            "agent": "claude",
            "project": "/repo-a",
            "input_tokens": 10,
            "output_tokens": 5,
            "cache_read_input_tokens": 3,
            "cache_creation_input_tokens": 2,
        })
        .to_string(),
    );
    let mut codex = Event::new(SessionId::new(), "llm_usage", "codex", Decision::Complete);
    codex.content = Some(
        serde_json::json!({
            "agent": "codex",
            "project": "/repo-a",
            "input_tokens": 7,
            "output_tokens": 1,
        })
        .to_string(),
    );
    let mut other = Event::new(SessionId::new(), "llm_usage", "claude", Decision::Complete);
    other.content = Some(
        serde_json::json!({
            "agent": "claude",
            "project": "/repo-b",
            "input_tokens": 4,
            "output_tokens": 1,
        })
        .to_string(),
    );

    let summary = aggregate_llm_usage(&[claude, codex, other]);

    assert_eq!(summary.total_tokens_json(), serde_json::json!(33));
    assert_eq!(
        summary.project_tokens_json("/repo-a"),
        serde_json::json!(28)
    );
    assert_eq!(summary.project_tokens_json("/repo-b"), serde_json::json!(5));
    assert_eq!(
        summary.agent_tokens_json(),
        serde_json::json!([
            {"agent": "claude", "tokens_24h": 25},
            {"agent": "codex", "tokens_24h": 8},
        ])
    );
}

#[test]
fn llm_usage_summary_returns_null_without_events() {
    let summary = aggregate_llm_usage(&[]);
    assert_eq!(summary.total_tokens_json(), Value::Null);
    assert_eq!(summary.project_tokens_json("/repo-a"), Value::Null);
    assert_eq!(summary.agent_tokens_json(), serde_json::json!([]));
}

#[test]
fn feed_omits_llm_usage_events() {
    let now = Utc::now();
    let mut usage = Event::new(SessionId::new(), "llm_usage", "codex", Decision::Complete);
    usage.ts = now;
    usage.detail = Some("tokens=10".to_string());
    let mut rule = Event::new(SessionId::new(), "rule_check", "vibeguard", Decision::Warn);
    rule.ts = now - Duration::seconds(1);
    rule.reason = Some("check warning".to_string());

    let feed = build_feed(&[usage, rule], now);

    assert_eq!(feed.len(), 1);
    assert_eq!(feed[0]["kind"], "rule_check");
}

#[test]
fn runtime_workflow_active_counts_match_dashboard_buckets() {
    let mut counts = ActiveTaskOverviewCounts::default();
    let running = harness_workflow::runtime::WorkflowInstance::new(
        harness_workflow::runtime::GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "implementing",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:1"),
    )
    .with_data(serde_json::json!({
        "project_id": "/repo",
        "task_id": "runtime-1",
    }));
    let waiting = harness_workflow::runtime::WorkflowInstance::new(
        harness_workflow::runtime::GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "ready_to_merge",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:2"),
    )
    .with_data(serde_json::json!({
        "project_id": "/repo",
        "task_id": "runtime-2",
    }));
    let terminal = harness_workflow::runtime::WorkflowInstance::new(
        harness_workflow::runtime::GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "done",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:3"),
    )
    .with_data(serde_json::json!({
        "project_id": "/repo",
        "task_id": "runtime-3",
    }));

    add_active_runtime_workflow(&mut counts, &running);
    add_active_runtime_workflow(&mut counts, &waiting);
    add_active_runtime_workflow(&mut counts, &terminal);

    assert_eq!(counts.total, 2);
    assert_eq!(counts.running, 1);
    assert_eq!(counts.queued, 1);
    assert_eq!(counts.by_project["/repo"].running, 1);
    assert_eq!(counts.by_project["/repo"].queued, 1);
}

#[test]
fn planning_runtime_workflow_counts_as_running_work() {
    let mut counts = ActiveTaskOverviewCounts::default();
    let planning = harness_workflow::runtime::WorkflowInstance::new(
        harness_workflow::runtime::GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "planning",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:1"),
    )
    .with_data(serde_json::json!({
        "project_id": "/repo",
        "task_id": "runtime-1",
    }));

    assert!(add_active_runtime_workflow(&mut counts, &planning));

    assert_eq!(counts.total, 1);
    assert_eq!(counts.running, 1);
    assert_eq!(counts.queued, 0);
    assert_eq!(counts.by_project["/repo"].running, 1);
}

#[test]
fn runtime_workflow_without_task_handle_still_counts_active_work() {
    let mut counts = ActiveTaskOverviewCounts::default();
    let active_legacy_task_ids = HashSet::from(["legacy-task".to_string()]);
    let runtime = harness_workflow::runtime::WorkflowInstance::new(
        harness_workflow::runtime::GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "implementing",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:1"),
    )
    .with_data(serde_json::json!({
        "project_id": "/repo",
    }));

    assert!(runtime_workflow_dedupe_task_handle(&runtime).is_none());
    assert!(!runtime_workflow_matches_active_legacy_task(
        &runtime,
        &active_legacy_task_ids,
    ));
    if !runtime_workflow_matches_active_legacy_task(&runtime, &active_legacy_task_ids) {
        add_active_runtime_workflow(&mut counts, &runtime);
    }

    assert_eq!(counts.total, 1);
    assert_eq!(counts.running, 1);
    assert_eq!(counts.by_project["/repo"].running, 1);
}

#[test]
fn terminal_legacy_summary_does_not_suppress_active_runtime_workflow() {
    let mut counts = ActiveTaskOverviewCounts::default();
    let mut active_legacy_task_ids = HashSet::new();
    let mut terminal_legacy = crate::task_runner::TaskState::new(harness_core::types::TaskId(
        "runtime-task-1".to_string(),
    ))
    .summary();
    terminal_legacy.status = crate::task_runner::TaskStatus::Failed;
    terminal_legacy.scheduler.authority_state = SchedulerAuthorityState::Failed;
    terminal_legacy.project = Some("/repo".to_string());
    if add_active_summary(&mut counts, &terminal_legacy) {
        active_legacy_task_ids.insert(terminal_legacy.id.as_str().to_string());
    }

    let runtime = harness_workflow::runtime::WorkflowInstance::new(
        harness_workflow::runtime::GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "implementing",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:1"),
    )
    .with_data(serde_json::json!({
        "project_id": "/repo",
        "task_id": "runtime-task-1",
    }));
    let task_id = runtime_workflow_dedupe_task_handle(&runtime)
        .expect("runtime workflow should expose a task handle");
    if !active_legacy_task_ids.contains(task_id.as_str()) {
        add_active_runtime_workflow(&mut counts, &runtime);
    }

    assert_eq!(counts.total, 1);
    assert_eq!(counts.running, 1);
    assert_eq!(counts.by_project["/repo"].running, 1);
}

#[test]
fn active_legacy_summary_still_suppresses_matching_runtime_workflow() {
    let mut counts = ActiveTaskOverviewCounts::default();
    let mut active_legacy_task_ids = HashSet::new();
    let mut active_legacy = crate::task_runner::TaskState::new(harness_core::types::TaskId(
        "runtime-task-1".to_string(),
    ))
    .summary();
    active_legacy.status = crate::task_runner::TaskStatus::Implementing;
    active_legacy.scheduler.authority_state = SchedulerAuthorityState::Running;
    active_legacy.project = Some("/repo".to_string());
    if add_active_summary(&mut counts, &active_legacy) {
        active_legacy_task_ids.insert(active_legacy.id.as_str().to_string());
    }

    let runtime = harness_workflow::runtime::WorkflowInstance::new(
        harness_workflow::runtime::GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "implementing",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:1"),
    )
    .with_data(serde_json::json!({
        "project_id": "/repo",
        "task_id": "runtime-task-1",
    }));
    let task_id = runtime_workflow_dedupe_task_handle(&runtime)
        .expect("runtime workflow should expose a task handle");
    if !active_legacy_task_ids.contains(task_id.as_str()) {
        add_active_runtime_workflow(&mut counts, &runtime);
    }

    assert_eq!(counts.total, 1);
    assert_eq!(counts.running, 1);
    assert_eq!(counts.by_project["/repo"].running, 1);
}

#[test]
fn leased_legacy_summary_counts_as_running_work() {
    let mut counts = ActiveTaskOverviewCounts::default();
    let mut leased = crate::task_runner::TaskState::new(harness_core::types::TaskId(
        "leased-runtime-task".to_string(),
    ))
    .summary();
    leased.status = crate::task_runner::TaskStatus::Implementing;
    leased.scheduler.authority_state = SchedulerAuthorityState::Leased;
    leased.project = Some("/repo".to_string());

    assert!(add_active_summary(&mut counts, &leased));

    assert_eq!(counts.total, 1);
    assert_eq!(counts.running, 1);
    assert_eq!(counts.queued, 0);
    assert_eq!(counts.by_project["/repo"].running, 1);
    assert_eq!(counts.by_project["/repo"].queued, 0);
}

#[test]
fn active_legacy_summary_suppresses_runtime_workflow_by_current_task_id() {
    let mut counts = ActiveTaskOverviewCounts::default();
    let mut active_legacy_task_ids = HashSet::new();
    let mut active_legacy = crate::task_runner::TaskState::new(harness_core::types::TaskId(
        "current-runtime-task".to_string(),
    ))
    .summary();
    active_legacy.status = crate::task_runner::TaskStatus::Implementing;
    active_legacy.scheduler.authority_state = SchedulerAuthorityState::Running;
    active_legacy.project = Some("/repo".to_string());
    if add_active_summary(&mut counts, &active_legacy) {
        active_legacy_task_ids.insert(active_legacy.id.as_str().to_string());
    }

    let runtime = harness_workflow::runtime::WorkflowInstance::new(
        harness_workflow::runtime::GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "implementing",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:1"),
    )
    .with_data(serde_json::json!({
        "project_id": "/repo",
        "task_id": "current-runtime-task",
        "task_ids": ["historical-runtime-task", "current-runtime-task"],
    }));
    let task_id = runtime_workflow_dedupe_task_handle(&runtime)
        .expect("runtime workflow should expose a task handle");
    assert_eq!(task_id, "current-runtime-task");
    if !active_legacy_task_ids.contains(task_id.as_str()) {
        add_active_runtime_workflow(&mut counts, &runtime);
    }

    assert_eq!(counts.total, 1);
    assert_eq!(counts.running, 1);
    assert_eq!(counts.by_project["/repo"].running, 1);
}

#[tokio::test]
async fn overview_returns_expected_shape() -> anyhow::Result<()> {
    let _lock = test_helpers::HOME_LOCK.lock().await;
    if !test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let dir = test_helpers::tempdir_in_home("harness-test-overview-")?;
    let state = Arc::new(test_helpers::make_test_state(dir.path()).await?);

    let app = Router::new()
        .route("/api/overview", get(overview))
        .with_state(state);

    let req = axum::http::Request::builder()
        .uri("/api/overview")
        .body(axum::body::Body::empty())?;

    let resp = tower::ServiceExt::oneshot(app, req).await?;
    assert_eq!(resp.status(), axum::http::StatusCode::OK);

    let bytes = to_bytes(resp.into_body(), usize::MAX).await?;
    let body: serde_json::Value = serde_json::from_slice(&bytes)?;

    for key in [
        "window",
        "kpi",
        "agent_tokens",
        "distribution",
        "throughput",
        "projects",
        "runtimes",
        "heatmap",
        "feed",
        "alerts",
        "global",
    ] {
        assert!(body.get(key).is_some(), "missing top-level key: {key}");
    }
    assert!(body["kpi"]["active_tasks"].is_number());
    assert!(body["kpi"]["merged_24h"].is_number());
    assert!(body["kpi"]["worktrees"]["used"].is_number());
    assert!(body["kpi"]["worktrees"]["total"].is_number());
    assert!(body["throughput"]["hours"].is_array());
    assert_eq!(
        body["throughput"]["hours"].as_array().unwrap().len(),
        THROUGHPUT_BUCKETS
    );

    Ok(())
}

#[tokio::test]
async fn worktrees_used_includes_local_workspace_manager() -> anyhow::Result<()> {
    let _lock = test_helpers::HOME_LOCK.lock().await;
    if !test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let dir = test_helpers::tempdir_in_home("harness-test-overview-wt-")?;
    let mut state = test_helpers::make_test_state(dir.path()).await?;

    // Seed the workspace manager with 3 live entries (no real git ops needed).
    let ws_config = harness_core::config::misc::WorkspaceConfig {
        root: dir.path().join("ws"),
        ..Default::default()
    };
    let mgr = Arc::new(crate::workspace::WorkspaceManager::new(ws_config)?);
    for i in 0..3u64 {
        mgr.active.insert(
            harness_core::types::TaskId(format!("fake-{i}")),
            crate::workspace::ActiveWorkspace {
                workspace_path: dir.path().join(format!("ws/fake-{i}")),
                source_repo: dir.path().to_path_buf(),
                repo: None,
                runtime_workflow_id: None,
                branch: format!("harness/fake-{i}"),
                created_at: std::time::SystemTime::now(),
                owner_session: format!("session-{i}"),
                run_generation: 1,
            },
        );
    }
    state.concurrency.workspace_mgr = Some(mgr);

    let app = Router::new()
        .route("/api/overview", get(overview))
        .with_state(Arc::new(state));

    let req = axum::http::Request::builder()
        .uri("/api/overview")
        .body(axum::body::Body::empty())?;

    let resp = tower::ServiceExt::oneshot(app, req).await?;
    assert_eq!(resp.status(), axum::http::StatusCode::OK);

    let bytes = to_bytes(resp.into_body(), usize::MAX).await?;
    let body: serde_json::Value = serde_json::from_slice(&bytes)?;

    assert_eq!(
        body["kpi"]["worktrees"]["used"].as_u64(),
        Some(3),
        "kpi.worktrees.used should count local workspace manager entries"
    );

    Ok(())
}
