use super::*;
use harness_core::db::resolve_database_url;
use serde_json::json;

async fn open_runtime_store(dir: &Path) -> anyhow::Result<WorkflowRuntimeStore> {
    let database_url = resolve_database_url(None)?;
    WorkflowRuntimeStore::open_with_database_url(dir, Some(&database_url)).await
}

#[tokio::test]
async fn rejected_new_issue_submission_does_not_persist_live_instance_or_commands(
) -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = open_runtime_store(dir.path()).await?;
    let project_root = dir.path().join("project");
    std::fs::create_dir(&project_root)?;
    let project_id = project_root.to_string_lossy().into_owned();
    let workflow_id =
        harness_workflow::issue_lifecycle::workflow_id(&project_id, Some("owner/repo"), 409);
    let instance = issue_instance(
        workflow_id.clone(),
        project_id,
        Some("owner/repo".to_string()),
        409,
    );
    let task_id = TaskId::from_str("runtime-rejected-new-issue");
    let ctx = IssueSubmissionRuntimeContext {
        project_root: &project_root,
        repo: Some("owner/repo"),
        issue_number: 409,
        task_id: &task_id,
        labels: &[],
        force_execute: false,
        additional_prompt: None,
        depends_on: &[],
        dependencies_blocked: false,
        source: Some("github"),
        external_id: Some("issue:409"),
    };
    let decision = WorkflowDecision::new(
        &workflow_id,
        "discovered",
        "submit_issue",
        "done",
        "invalid issue submission transition",
    );

    let result = commit::apply_decision(&store, instance, true, decision, &ctx, json!({})).await?;

    assert!(!result.accepted);
    assert!(result.rejection_reason.is_some());
    assert!(store.get_instance(&workflow_id).await?.is_none());
    assert!(store.commands_for(&workflow_id).await?.is_empty());
    let decisions = store.decisions_for(&workflow_id).await?;
    assert_eq!(decisions.len(), 1);
    assert!(!decisions[0].accepted);
    let events = store.events_for(&workflow_id).await?;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event_type, "IssueSubmitted");
    Ok(())
}

#[tokio::test]
async fn accepted_issue_replay_repairs_pending_command_without_advancing_instance(
) -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = open_runtime_store(dir.path()).await?;
    let project_root = dir.path().join("project");
    std::fs::create_dir(&project_root)?;
    let task_id = TaskId::from_str("runtime-accepted-replay");
    let labels = vec!["bug".to_string()];

    let first = record_issue_submission(
        &store,
        IssueSubmissionRuntimeContext {
            project_root: &project_root,
            repo: Some("owner/repo"),
            issue_number: 410,
            task_id: &task_id,
            labels: &labels,
            force_execute: false,
            additional_prompt: None,
            depends_on: &[],
            dependencies_blocked: false,
            source: Some("github"),
            external_id: Some("issue:410"),
        },
    )
    .await?;
    let first_instance = store
        .get_instance(&first.workflow_id)
        .await?
        .expect("accepted issue submission should persist an instance");

    let second = record_issue_submission(
        &store,
        IssueSubmissionRuntimeContext {
            project_root: &project_root,
            repo: Some("owner/repo"),
            issue_number: 410,
            task_id: &task_id,
            labels: &labels,
            force_execute: false,
            additional_prompt: None,
            depends_on: &[],
            dependencies_blocked: false,
            source: Some("github"),
            external_id: Some("issue:410"),
        },
    )
    .await?;

    assert!(second.accepted);
    assert_eq!(second.decision_id, first.decision_id);
    assert_eq!(second.command_ids, first.command_ids);
    let replayed_instance = store
        .get_instance(&first.workflow_id)
        .await?
        .expect("workflow should remain persisted");
    assert_eq!(replayed_instance.state, first_instance.state);
    assert_eq!(replayed_instance.version, first_instance.version);
    assert_eq!(replayed_instance.data, first_instance.data);
    assert_eq!(store.decisions_for(&first.workflow_id).await?.len(), 1);
    assert_eq!(store.commands_for(&first.workflow_id).await?.len(), 1);
    let events = store.events_for(&first.workflow_id).await?;
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "IssueSubmitted")
            .count(),
        1
    );
    Ok(())
}
