use super::*;
use harness_core::db::resolve_database_url;
use harness_workflow::runtime::WorkflowCommandStatus;

async fn open_runtime_store(dir: &Path) -> anyhow::Result<WorkflowRuntimeStore> {
    let database_url = resolve_database_url(None)?;
    WorkflowRuntimeStore::open_with_database_url(dir, Some(&database_url)).await
}

#[tokio::test]
async fn issue_resubmission_after_completed_command_creates_fresh_attempt() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = open_runtime_store(dir.path()).await?;
    let project_root = dir.path().join("project");
    std::fs::create_dir(&project_root)?;
    let task_id = TaskId::from_str("repeat-issue-task");
    let labels = vec!["force-execute".to_string()];

    let first = record_issue_submission(
        &store,
        IssueSubmissionRuntimeContext {
            project_root: &project_root,
            repo: Some("owner/repo"),
            issue_number: 501,
            task_id: &task_id,
            labels: &labels,
            force_execute: true,
            additional_prompt: None,
            depends_on: &[],
            dependencies_blocked: false,
            source: Some("github"),
            external_id: Some("issue:501"),
            remote_fact_hash: None,
            author_trust_class: None,
        },
    )
    .await?;
    let workflow_id = harness_workflow::issue_lifecycle::workflow_id(
        &project_root.to_string_lossy(),
        Some("owner/repo"),
        501,
    );
    store
        .mark_command_status(&first.command_ids[0], WorkflowCommandStatus::Completed)
        .await?;
    let mut instance = store.get_instance(&workflow_id).await?.unwrap();
    instance.state = "failed".to_string();
    store.upsert_instance(&instance).await?;

    let second = record_issue_submission(
        &store,
        IssueSubmissionRuntimeContext {
            project_root: &project_root,
            repo: Some("owner/repo"),
            issue_number: 501,
            task_id: &task_id,
            labels: &labels,
            force_execute: true,
            additional_prompt: None,
            depends_on: &[],
            dependencies_blocked: false,
            source: Some("github"),
            external_id: Some("issue:501"),
            remote_fact_hash: None,
            author_trust_class: None,
        },
    )
    .await?;

    assert!(second.accepted);
    assert_ne!(second.decision_id, first.decision_id);
    assert_ne!(second.command_ids[0], first.command_ids[0]);
    let events = store.events_for(&workflow_id).await?;
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "IssueSubmitted")
            .count(),
        2
    );
    let commands = store.commands_for(&workflow_id).await?;
    assert_eq!(commands.len(), 2);
    let second_command = commands
        .iter()
        .find(|command| command.id == second.command_ids[0])
        .unwrap();
    assert_eq!(second_command.status, WorkflowCommandStatus::Pending);
    assert!(second_command.command.dedupe_key.contains(":event:"));
    Ok(())
}

#[tokio::test]
async fn prompt_resubmission_after_completed_command_creates_fresh_attempt() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = open_runtime_store(dir.path()).await?;
    let project_root = dir.path().join("project");
    std::fs::create_dir(&project_root)?;
    let task_id = TaskId::from_str("repeat-prompt-task");
    let external_id = "manual:prompt:repeat";

    let first = record_prompt_submission(
        &store,
        PromptSubmissionRuntimeContext {
            project_root: &project_root,
            task_id: &task_id,
            prompt: "first prompt body",
            depends_on: &[],
            serialization_depends_on: &[],
            dependencies_blocked: false,
            source: Some("dashboard"),
            external_id: Some(external_id),
        },
    )
    .await?;
    let workflow_id =
        prompt_workflow_id(&project_root.to_string_lossy(), Some(external_id), &task_id);
    store
        .mark_command_status(&first.command_ids[0], WorkflowCommandStatus::Completed)
        .await?;
    let mut instance = store.get_instance(&workflow_id).await?.unwrap();
    instance.state = "failed".to_string();
    store.upsert_instance(&instance).await?;

    let second = record_prompt_submission(
        &store,
        PromptSubmissionRuntimeContext {
            project_root: &project_root,
            task_id: &task_id,
            prompt: "second prompt body",
            depends_on: &[],
            serialization_depends_on: &[],
            dependencies_blocked: false,
            source: Some("dashboard"),
            external_id: Some(external_id),
        },
    )
    .await?;

    assert!(second.accepted);
    assert_ne!(second.decision_id, first.decision_id);
    assert_ne!(second.command_ids[0], first.command_ids[0]);
    let events = store.events_for(&workflow_id).await?;
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "PromptSubmitted")
            .count(),
        2
    );
    let commands = store.commands_for(&workflow_id).await?;
    assert_eq!(commands.len(), 2);
    let second_command = commands
        .iter()
        .find(|command| command.id == second.command_ids[0])
        .unwrap();
    assert_eq!(second_command.status, WorkflowCommandStatus::Pending);
    assert!(second_command.command.dedupe_key.contains(":event:"));
    Ok(())
}
