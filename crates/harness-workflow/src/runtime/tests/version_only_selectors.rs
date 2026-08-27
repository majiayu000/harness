#[tokio::test]
async fn version_only_v1_selectors_ignore_definition_hash_payload() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let terminal = project_issue_instance("/project-a", 901, "done")
        .with_server_data(json!({ "definition_hash": "ordinary-v1-payload" }));
    let progressing = project_issue_instance("/project-a", 902, "implementing")
        .with_server_data(json!({ "definition_hash": "ordinary-v1-payload" }));
    for instance in [&terminal, &progressing] {
        store.force_upsert_lifecycle_state_for_test(instance).await?;
    }

    let terminal_rows = store
        .list_recent_terminal_instances_by_definition(
            GITHUB_ISSUE_PR_DEFINITION_ID,
            super::WorkflowTerminalState::Succeeded,
            10,
        )
        .await?;
    assert!(terminal_rows.iter().any(|row| row.id == terminal.id));

    let nonterminal_rows = store
        .list_nonterminal_instances_by_definition(GITHUB_ISSUE_PR_DEFINITION_ID, None, Some(10))
        .await?;
    assert!(nonterminal_rows.iter().any(|row| row.id == progressing.id));
    assert!(!nonterminal_rows.iter().any(|row| row.id == terminal.id));

    let progress_rows = store
        .list_recent_instances_by_progress_mode(
            GITHUB_ISSUE_PR_DEFINITION_ID,
            super::WorkflowProgressMode::CommandDriven,
            10,
        )
        .await?;
    assert!(progress_rows.iter().any(|row| row.id == progressing.id));
    Ok(())
}
