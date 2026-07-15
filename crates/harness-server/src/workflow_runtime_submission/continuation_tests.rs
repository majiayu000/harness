use super::*;
use harness_core::db::resolve_database_url;
use harness_workflow::runtime::{
    ActivityResult, ActivitySignal, PromptContinuationPolicy, ValidationRecord,
    WorkflowCommandStatus, PROMPT_TASK_IMPLEMENT_ACTIVITY,
};
use serde_json::json;
use std::collections::BTreeSet;

async fn open_continuation_runtime_store(dir: &Path) -> anyhow::Result<WorkflowRuntimeStore> {
    let database_url = resolve_database_url(None)?;
    WorkflowRuntimeStore::open_with_database_url(dir, Some(&database_url)).await
}

fn continuation_policy(max_attempts: u32) -> PromptContinuationPolicy {
    PromptContinuationPolicy {
        max_attempts,
        attempt_delay_secs: 0,
        active_states: BTreeSet::from(["In Progress".to_string()]),
        no_progress_limit: 3,
    }
}

#[tokio::test]
async fn prompt_continuation_submit_active_settled_reaches_done() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = open_continuation_runtime_store(dir.path()).await?;
    let project_root = dir.path().join("project");
    std::fs::create_dir(&project_root)?;
    let task_id = TaskId::from_str("continuation-e2e");
    let policy = continuation_policy(4);
    let submission = record_prompt_submission(
        &store,
        PromptSubmissionRuntimeContext {
            project_root: &project_root,
            task_id: &task_id,
            prompt: "Continue TEAM-123 until it settles.",
            depends_on: &[],
            serialization_depends_on: &[],
            dependencies_blocked: false,
            source: Some("dashboard"),
            external_id: Some("TEAM-123"),
            continuation: Some(&policy),
        },
    )
    .await?;
    assert!(submission.accepted);
    let submitted = store
        .get_instance(&submission.workflow_id)
        .await?
        .expect("submitted continuation instance");
    assert_eq!(submitted.data["continuation"]["attempt"], 1);
    let initial_commands = store.commands_for(&submission.workflow_id).await?;
    assert_eq!(initial_commands.len(), 1);
    assert_eq!(
        initial_commands[0].command.dedupe_key,
        format!("prompt-task:{}:attempt:1", submission.workflow_id)
    );

    let active = ActivityResult::succeeded(
        PROMPT_TASK_IMPLEMENT_ACTIVITY,
        "Created the implementation branch.",
    )
    .with_signal(ActivitySignal::new(
        "external_state",
        json!({ "state": "In Progress", "subject": "TEAM-123" }),
    ));
    let continued = store
        .commit_parent_runtime_completion(
            &submission.workflow_id,
            "runtime-1",
            json!({
                "command_id": initial_commands[0].id,
                "runtime_job_id": "continuation-job-1",
                "activity_result": active,
            }),
        )
        .await?
        .expect("active state should commit a continuation");
    assert_eq!(continued.decision.decision, "continue_prompt_task");
    let after_active = store
        .get_instance(&submission.workflow_id)
        .await?
        .expect("continued instance");
    assert_eq!(after_active.state, "implementing");
    assert_eq!(after_active.data["continuation"]["attempt"], 2);
    assert_eq!(
        after_active.data["continuation"]["last_summary"],
        "Created the implementation branch."
    );

    let settled =
        ActivityResult::succeeded(PROMPT_TASK_IMPLEMENT_ACTIVITY, "TEAM-123 reached Done.")
            .with_signal(ActivitySignal::new(
                "external_state",
                json!({ "state": "Done", "subject": "TEAM-123" }),
            ))
            .with_validation(ValidationRecord::new("cargo test", "passed"));
    let finished = store
        .commit_parent_runtime_completion(
            &submission.workflow_id,
            "runtime-1",
            json!({
                "command_id": continued.decision.commands[0].dedupe_key,
                "runtime_job_id": "continuation-job-2",
                "activity_result": settled,
            }),
        )
        .await?
        .expect("settled state should finish");
    assert_eq!(
        finished.decision.decision,
        "finish_prompt_task_external_settled"
    );
    let done = store
        .get_instance(&submission.workflow_id)
        .await?
        .expect("finished continuation instance");
    assert_eq!(done.state, "done");
    Ok(())
}

#[tokio::test]
async fn cancelled_prompt_continuation_does_not_enqueue_another_attempt() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = open_continuation_runtime_store(dir.path()).await?;
    let project_root = dir.path().join("project");
    std::fs::create_dir(&project_root)?;
    let task_id = TaskId::from_str("continuation-cancel");
    let policy = continuation_policy(4);
    let submission = record_prompt_submission(
        &store,
        PromptSubmissionRuntimeContext {
            project_root: &project_root,
            task_id: &task_id,
            prompt: "Continue TEAM-456 until it settles.",
            depends_on: &[],
            serialization_depends_on: &[],
            dependencies_blocked: false,
            source: Some("dashboard"),
            external_id: Some("TEAM-456"),
            continuation: Some(&policy),
        },
    )
    .await?;
    let before = store.commands_for(&submission.workflow_id).await?;
    assert_eq!(before.len(), 1);
    let cancelled = cancel_submission_by_workflow_id(&store, &submission.workflow_id).await?;
    assert!(matches!(
        cancelled,
        RuntimeSubmissionCancelOutcome::Cancelled(_)
    ));

    let stale = ActivityResult::succeeded(PROMPT_TASK_IMPLEMENT_ACTIVITY, "Still active.")
        .with_signal(ActivitySignal::new(
            "external_state",
            json!({ "state": "In Progress", "subject": "TEAM-456" }),
        ));
    let decision = store
        .commit_parent_runtime_completion(
            &submission.workflow_id,
            "runtime-1",
            json!({
                "command_id": before[0].id,
                "runtime_job_id": "continuation-cancelled-job",
                "activity_result": stale,
            }),
        )
        .await?;
    assert!(decision.is_none());
    let after = store.commands_for(&submission.workflow_id).await?;
    assert_eq!(
        after.len(),
        2,
        "only initial and cancellation commands should exist"
    );
    assert!(after
        .iter()
        .all(|command| command.status != WorkflowCommandStatus::Pending));
    Ok(())
}
