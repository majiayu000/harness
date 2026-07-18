use super::*;

#[tokio::test]
async fn blocked_runtime_reconciliation_marks_merged_pr_done() -> anyhow::Result<()> {
    let _env_guard = async_env_lock().lock().await;
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let _db_guard = crate::test_helpers::acquire_db_state_guard().await;
    let api_base = github_state_server(vec![(
        "/repos/owner/repo/pulls/103",
        r#"{"state":"closed","merged_at":"2026-05-10T00:00:00Z"}"#,
    )])
    .await;
    let _api_base_guard = ScopedEnvVar::set("HARNESS_GITHUB_API_BASE_URL", &api_base);
    let Some(stores) = open_runtime_stores().await? else {
        return Ok(());
    };

    let project_root = stores.dir.path().join("project-blocked");
    std::fs::create_dir(&project_root)?;
    let project_id = project_root.to_string_lossy();
    stores
        .issue_store
        .record_issue_scheduled(&project_id, Some("owner/repo"), 48, "task-7", &[], false)
        .await?;
    stores
        .issue_store
        .record_pr_detected(
            &project_id,
            Some("owner/repo"),
            48,
            "task-7",
            103,
            "https://github.com/owner/repo/pull/103",
        )
        .await?;

    let workflow_id =
        harness_workflow::issue_lifecycle::workflow_id(&project_id, Some("owner/repo"), 48);
    let instance = WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "blocked",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:48"),
    )
    .with_id(&workflow_id)
    .with_data(json!({
        "project_id": project_id.as_ref(),
        "repo": "owner/repo",
        "issue_number": 48,
        "task_id": "task-7",
        "pr_number": 103,
        "pr_url": "https://github.com/owner/repo/pull/103",
    }));
    stores.runtime_store.upsert_instance(&instance).await?;

    let report = run_once_with_runtime_config(
        Some(&stores.runtime_store),
        Some(&stores.issue_store),
        &runtime_config(),
        false,
        None,
    )
    .await;

    assert_eq!(report.workflow_transitions.len(), 1);
    assert_eq!(report.workflow_transitions[0].from, "blocked");
    assert_eq!(report.workflow_transitions[0].to, "done");
    assert_eq!(
        report.workflow_transitions[0].reason,
        "reconciled: PR merged externally"
    );
    assert!(report.workflow_transitions[0].applied);

    let updated = stores
        .runtime_store
        .get_instance(&workflow_id)
        .await?
        .expect("workflow should remain persisted");
    assert_eq!(updated.state, "done");
    assert_eq!(updated.data["last_decision"], "reconcile_pr_merged");
    assert_eq!(updated.data["external_pr_state"], "done");

    let issue_workflow = stores
        .issue_store
        .get_by_issue(&project_id, Some("owner/repo"), 48)
        .await?
        .expect("issue workflow should exist");
    assert_eq!(
        issue_workflow.state,
        harness_workflow::issue_lifecycle::IssueLifecycleState::Done
    );
    assert_eq!(issue_workflow.pr_number, Some(103));
    assert_eq!(
        issue_workflow.pr_url.as_deref(),
        Some("https://github.com/owner/repo/pull/103")
    );

    Ok(())
}

#[tokio::test]
async fn blocked_runtime_reconciliation_marks_slug_only_merged_pr_done() -> anyhow::Result<()> {
    let _env_guard = async_env_lock().lock().await;
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let _db_guard = crate::test_helpers::acquire_db_state_guard().await;
    let api_base = github_state_server(vec![(
        "/repos/owner/repo/pulls/104",
        r#"{"state":"closed","merged_at":"2026-05-10T00:00:00Z"}"#,
    )])
    .await;
    let _api_base_guard = ScopedEnvVar::set("HARNESS_GITHUB_API_BASE_URL", &api_base);
    let Some(stores) = open_runtime_stores().await? else {
        return Ok(());
    };

    let project_root = stores.dir.path().join("project-blocked-slug");
    std::fs::create_dir(&project_root)?;
    let project_id = project_root.to_string_lossy();
    stores
        .issue_store
        .record_issue_scheduled(&project_id, Some("owner/repo"), 49, "task-8", &[], false)
        .await?;

    let workflow_id =
        harness_workflow::issue_lifecycle::workflow_id(&project_id, Some("owner/repo"), 49);
    let instance = WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "blocked",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:49"),
    )
    .with_id(&workflow_id)
    .with_data(json!({
        "project_id": project_id.as_ref(),
        "repo": "owner/repo",
        "issue_number": 49,
        "task_id": "task-8",
        "pr_number": 104,
    }));
    stores.runtime_store.upsert_instance(&instance).await?;

    let report = run_once_with_runtime_config(
        Some(&stores.runtime_store),
        Some(&stores.issue_store),
        &runtime_config(),
        false,
        None,
    )
    .await;

    assert_eq!(report.workflow_transitions.len(), 1);
    assert_eq!(report.workflow_transitions[0].from, "blocked");
    assert_eq!(report.workflow_transitions[0].to, "done");
    assert!(report.workflow_transitions[0].applied);

    let updated = stores
        .runtime_store
        .get_instance(&workflow_id)
        .await?
        .expect("workflow should remain persisted");
    assert_eq!(updated.state, "done");
    assert_eq!(updated.data["last_decision"], "reconcile_pr_merged");
    assert_eq!(updated.data["external_pr_state"], "done");
    assert_eq!(updated.data["repo"], "owner/repo");
    assert!(updated.data.get("pr_url").is_none());

    let issue_workflow = stores
        .issue_store
        .get_by_issue(&project_id, Some("owner/repo"), 49)
        .await?
        .expect("issue workflow should exist");
    assert_eq!(
        issue_workflow.state,
        harness_workflow::issue_lifecycle::IssueLifecycleState::Done
    );
    assert_eq!(issue_workflow.pr_number, Some(104));
    assert!(issue_workflow.pr_url.is_none());

    Ok(())
}
