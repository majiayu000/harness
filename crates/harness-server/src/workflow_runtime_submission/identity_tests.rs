use super::*;
use harness_core::db::resolve_database_url;
use harness_workflow::runtime::{WorkflowInstance, WorkflowRuntimeStore, WorkflowSubject};
use serde_json::json;

#[tokio::test]
async fn issue_submission_records_explicit_submission_id() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let database_url = resolve_database_url(None)?;
    let store =
        WorkflowRuntimeStore::open_with_database_url(dir.path(), Some(&database_url)).await?;
    let project_root = dir.path().join("project");
    std::fs::create_dir(&project_root)?;
    let task_id = TaskId::from_str("issue-submission-id");

    record_issue_submission(
        &store,
        IssueSubmissionRuntimeContext {
            project_root: &project_root,
            repo: Some("owner/repo"),
            issue_number: 1129,
            task_id: &task_id,
            labels: &[],
            force_execute: false,
            additional_prompt: None,
            depends_on: &[],
            dependencies_blocked: false,
            source: Some("github"),
            external_id: Some("issue:1129"),
            remote_fact_hash: None,
            author_trust_class: None,
        },
    )
    .await?;

    let workflow_id = harness_workflow::issue_lifecycle::workflow_id(
        &project_root.to_string_lossy(),
        Some("owner/repo"),
        1129,
    );
    let instance = store
        .get_instance(&workflow_id)
        .await?
        .expect("issue workflow should be persisted");
    assert_eq!(instance.data["submission_id"], "issue-submission-id");
    assert_eq!(instance.data["source"], "github");
    assert_eq!(instance.data["external_id"], "issue:1129");
    assert_eq!(instance.data["tracker_source"], "github");
    assert_eq!(instance.data["tracker_external_id"], "issue:1129");
    assert_eq!(
        runtime_issue_task_handle(&instance)
            .expect("issue workflow should expose submission id")
            .as_str(),
        "issue-submission-id"
    );
    let events = store.events_for(&workflow_id).await?;
    let submitted = events
        .iter()
        .find(|event| event.event_type == "IssueSubmitted")
        .expect("issue submission event should be recorded");
    assert_eq!(submitted.event["source"], "github");
    assert_eq!(submitted.event["external_id"], "issue:1129");
    assert_eq!(submitted.event["tracker_source"], "github");
    assert_eq!(submitted.event["tracker_external_id"], "issue:1129");
    Ok(())
}

#[tokio::test]
async fn prompt_submission_records_explicit_submission_id() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let database_url = resolve_database_url(None)?;
    let store =
        WorkflowRuntimeStore::open_with_database_url(dir.path(), Some(&database_url)).await?;
    let project_root = dir.path().join("project");
    std::fs::create_dir(&project_root)?;
    let task_id = TaskId::from_str("prompt-submission-id");

    let result = record_prompt_submission(
        &store,
        PromptSubmissionRuntimeContext {
            project_root: &project_root,
            task_id: &task_id,
            prompt: "keep prompt submission identity explicit",
            depends_on: &[],
            serialization_depends_on: &[],
            dependencies_blocked: false,
            source: Some("dashboard"),
            external_id: Some("manual:prompt:1129"),
            continuation: None,
        },
    )
    .await?;

    let instance = store
        .get_instance(&result.workflow_id)
        .await?
        .expect("prompt workflow should be persisted");
    assert_eq!(instance.data["submission_id"], "prompt-submission-id");
    assert_eq!(
        runtime_issue_task_handle(&instance)
            .expect("prompt workflow should expose submission id")
            .as_str(),
        "prompt-submission-id"
    );
    Ok(())
}

#[test]
fn runtime_issue_task_handle_prefers_explicit_submission_id() {
    let workflow = WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "implementing",
        WorkflowSubject::new("issue", "issue:1129"),
    )
    .with_data(json!({
        "submission_id": "runtime-submission",
        "task_id": "latest-task",
        "task_ids": ["historical-task", "latest-task"]
    }));

    assert_eq!(
        runtime_issue_task_handle(&workflow)
            .expect("runtime workflow should expose submission id")
            .as_str(),
        "runtime-submission"
    );
}

#[test]
fn task_id_history_preserves_existing_submission_id_first() {
    let new_task_id = TaskId::from_str("retry-task");
    let task_ids = task_id_history(
        &json!({
            "submission_id": "stable-submission",
            "task_id": "previous-task",
            "task_ids": ["previous-task", "older-task"]
        }),
        &new_task_id,
    );

    assert_eq!(
        task_ids,
        vec![
            "stable-submission".to_string(),
            "previous-task".to_string(),
            "older-task".to_string(),
            "retry-task".to_string()
        ]
    );
}
