use super::*;

#[tokio::test]
async fn ready_to_merge_reconciliation_marks_closed_unmerged_pr_cancelled() -> anyhow::Result<()> {
    let _env_guard = async_env_lock().lock().await;
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let _db_guard = crate::test_helpers::acquire_db_state_guard().await;
    let api_base = github_state_server(vec![(
        "/repos/owner/repo/pulls/102",
        r#"{"state":"closed","merged_at":null}"#,
    )])
    .await;
    let _api_base_guard = ScopedEnvVar::set("HARNESS_GITHUB_API_BASE_URL", &api_base);
    let Some(stores) = open_runtime_stores().await? else {
        return Ok(());
    };
    let (project_id, workflow_id) =
        persist_ready_to_merge_runtime(&stores, "project-closed", 47, "task-6", 102, 120, true)
            .await?;
    let report = run_once_with_runtime_config(
        Some(&stores.runtime_store),
        Some(&stores.issue_store),
        &ready_to_merge_config(0, 3600),
        false,
        None,
    )
    .await;
    assert_eq!(report.workflow_transitions.len(), 1);
    assert!(report.workflow_alerts.is_empty());
    assert_eq!(report.workflow_transitions[0].from, "ready_to_merge");
    assert_eq!(report.workflow_transitions[0].to, "cancelled");
    assert!(report.workflow_transitions[0].applied);
    let updated = stores
        .runtime_store
        .get_instance(&workflow_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("expected workflow to remain persisted"))?;
    assert_eq!(updated.state, "cancelled");
    assert_eq!(updated.data["last_decision"], "reconcile_pr_closed");
    let issue_workflow = stores
        .issue_store
        .get_by_issue(&project_id, Some("owner/repo"), 47)
        .await?
        .ok_or_else(|| anyhow::anyhow!("expected issue workflow to exist"))?;
    assert_eq!(
        issue_workflow.state,
        harness_workflow::issue_lifecycle::IssueLifecycleState::Cancelled
    );
    Ok(())
}
