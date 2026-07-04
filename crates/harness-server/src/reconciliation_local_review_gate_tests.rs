use super::*;

#[tokio::test]
async fn local_review_gate_runtime_reconciliation_marks_merged_pr_done() -> anyhow::Result<()> {
    let _env_guard = async_env_lock().lock().await;
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let _db_guard = crate::test_helpers::acquire_db_state_guard().await;
    let api_base = github_state_server(vec![(
        "/repos/owner/repo/pulls/79",
        r#"{"state":"closed","merged_at":"2026-05-10T00:00:00Z"}"#,
    )])
    .await;
    let _api_base_guard = ScopedEnvVar::set("HARNESS_GITHUB_API_BASE_URL", &api_base);
    let Some(stores) = open_runtime_stores().await? else {
        return Ok(());
    };

    let project_root = stores.dir.path().join("project-local-review");
    std::fs::create_dir(&project_root)?;
    let project_id = project_root.to_string_lossy();
    stores
        .issue_store
        .record_issue_scheduled(&project_id, Some("owner/repo"), 47, "task-6", &[], false)
        .await?;
    stores
        .issue_store
        .record_pr_detected(
            &project_id,
            Some("owner/repo"),
            47,
            "task-6",
            79,
            "https://github.com/owner/repo/pull/79",
        )
        .await?;

    let workflow_id =
        harness_workflow::issue_lifecycle::workflow_id(&project_id, Some("owner/repo"), 47);
    let instance = WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "local_review_gate",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:47"),
    )
    .with_id(&workflow_id)
    .with_data(json!({
        "project_id": project_id.as_ref(),
        "repo": "owner/repo",
        "issue_number": 47,
        "task_id": "task-6",
        "pr_number": 79,
        "pr_url": "https://github.com/owner/repo/pull/79",
    }));
    stores.runtime_store.upsert_instance(&instance).await?;

    let report = run_once_with_runtime_token(
        &stores.task_store,
        Some(&stores.runtime_store),
        Some(&stores.issue_store),
        20,
        false,
        None,
    )
    .await;

    assert_eq!(report.workflow_transitions.len(), 1);
    assert_eq!(report.workflow_transitions[0].from, "local_review_gate");
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
    assert_eq!(updated.data["pr_number"], 79);

    let issue_workflow = stores
        .issue_store
        .get_by_issue(&project_id, Some("owner/repo"), 47)
        .await?
        .expect("issue workflow should exist");
    assert_eq!(
        issue_workflow.state,
        harness_workflow::issue_lifecycle::IssueLifecycleState::Done
    );

    Ok(())
}
