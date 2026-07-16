use super::*;
use chrono::{Duration, Utc};
use harness_core::db::resolve_database_url;
use harness_workflow::runtime::{
    build_declarative_definition, register_declarative_workflow_definitions, ActivityArtifact,
    ActivityResult, ActivitySignal, RuntimeJobEnqueueOutcome, RuntimeKind, WorkflowCommandStatus,
    WorkflowCommandType,
};
use serde_json::json;

const DEFINITION_ID: &str = "declarative_submission_e2e_v1";
const CANCEL_DEFINITION_ID: &str = "declarative_submission_cancel_v1";

fn write_workflow_file(project_root: &Path, definition_id: &str) -> anyhow::Result<()> {
    std::fs::create_dir_all(project_root)?;
    std::fs::write(
        project_root.join("WORKFLOW.md"),
        format!(
            r#"---
definition:
  id: {definition_id}
  initial: reviewing
  states:
    reviewing:
      activity: review_docs
      on_success: failed
      on_failure: failed
      on_blocked: blocked
      on_signal:
        approved: done
        cancel: withdrawn
    blocked:
      progress: operator_gate
  terminal:
    done: succeeded
    failed: failed
    withdrawn: cancelled
  evidence_required:
    done: [review_report]
  recovery_targets: [reviewing]
activities:
  review_docs:
    prompt: Verify the submitted documentation and emit an approved signal.
---
Run the documentation review declared above.
"#,
        ),
    )?;
    Ok(())
}

#[tokio::test]
async fn workflow_file_submission_reaches_terminal_state_through_stub_completion(
) -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let database_url = resolve_database_url(None)?;
    let sandbox = tempfile::tempdir()?;
    let project_root = sandbox.path().join("project");
    write_workflow_file(&project_root, DEFINITION_ID)?;
    let document = harness_core::config::workflow::load_workflow_document(&project_root)?;
    let policy = document
        .config
        .definition
        .as_ref()
        .expect("fixture should contain a declarative definition");
    let definition = build_declarative_definition(policy, &document.config.activities)?;
    register_declarative_workflow_definitions([definition.clone()])?;

    let store = WorkflowRuntimeStore::open_with_database_url(
        &sandbox.path().join("runtime"),
        Some(&database_url),
    )
    .await?;
    let task_id = TaskId::from_str("declarative-e2e-task");
    let submission = record_declarative_submission(
        &store,
        DeclarativeSubmissionRuntimeContext {
            project_root: &project_root,
            task_id: &task_id,
            definition_id: DEFINITION_ID,
            prompt: "Review the release documentation.",
            depends_on: &[],
            serialization_depends_on: &[],
            source: Some("api"),
            external_id: Some("release-2026-07"),
        },
    )
    .await?;
    assert!(submission.accepted);

    let submitted = store
        .get_instance(&submission.workflow_id)
        .await?
        .expect("submitted declarative workflow instance");
    assert_eq!(submitted.definition_id, DEFINITION_ID);
    assert_eq!(
        submitted.definition_version,
        definition.definition_version()
    );
    assert_eq!(
        submitted.data["definition_hash"],
        definition.definition_hash()
    );
    assert_eq!(submitted.state, "reviewing");
    let prompt_ref = submitted.data["prompt_ref"]
        .as_str()
        .expect("submission should persist a prompt reference");
    assert_eq!(
        store.get_prompt_payload(prompt_ref).await?.as_deref(),
        Some("Review the release documentation.")
    );
    let persisted_definition = store
        .get_definition(DEFINITION_ID, definition.definition_version())
        .await?
        .expect("declarative definition version should be durable");
    assert_eq!(
        persisted_definition.definition_hash,
        definition.definition_hash()
    );

    let commands = store.commands_for(&submission.workflow_id).await?;
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].status, WorkflowCommandStatus::Pending);
    assert_eq!(
        commands[0].command.command_type,
        WorkflowCommandType::EnqueueActivity
    );
    assert_eq!(commands[0].command.activity_name(), Some("review_docs"));
    assert_eq!(
        commands[0].command.dedupe_key,
        format!("{}:reviewing:submit", submission.workflow_id)
    );

    let completion = ActivityResult::succeeded("review_docs", "Documentation approved.")
        .with_signal(ActivitySignal::new("approved", json!({ "approved": true })))
        .with_artifact(ActivityArtifact::new(
            "review_report",
            json!({ "approved": true }),
        ));
    let runtime_job = match store
        .enqueue_runtime_job_for_pending_command(
            &commands[0].id,
            RuntimeKind::CodexJsonrpc,
            "stub-runtime",
            json!({ "activity": "review_docs" }),
            None,
        )
        .await?
    {
        RuntimeJobEnqueueOutcome::Enqueued(job) => job,
        outcome => anyhow::bail!("initial activity was not dispatchable: {outcome:?}"),
    };
    let claimed = store
        .claim_next_runtime_job("stub-runtime", Utc::now() + Duration::minutes(5))
        .await?
        .ok_or_else(|| anyhow::anyhow!("stub runtime could not claim the initial activity"))?;
    assert_eq!(claimed.id, runtime_job.id);
    let lease = claimed
        .lease
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("claimed stub runtime job did not carry a lease"))?;
    let terminal_decision = store
        .commit_runtime_activity_completion_if_owned(
            &claimed.id,
            "stub-runtime",
            lease.expires_at,
            &completion,
        )
        .await?
        .ok_or_else(|| anyhow::anyhow!("stub runtime completion lost ownership"))?
        .decision
        .ok_or_else(|| anyhow::anyhow!("stub completion did not produce a decision"))?;
    assert!(terminal_decision.accepted);
    assert_eq!(terminal_decision.decision.next_state, "done");
    assert_eq!(
        terminal_decision.decision.commands[0].command_type,
        WorkflowCommandType::MarkDone
    );
    let terminal = store
        .get_instance(&submission.workflow_id)
        .await?
        .expect("terminal declarative workflow instance");
    assert_eq!(terminal.state, "done");
    assert!(terminal.is_terminal());
    Ok(())
}

#[tokio::test]
async fn declarative_submission_can_be_cancelled_into_its_declared_terminal_state(
) -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let database_url = resolve_database_url(None)?;
    let sandbox = tempfile::tempdir()?;
    let project_root = sandbox.path().join("cancel-project");
    write_workflow_file(&project_root, CANCEL_DEFINITION_ID)?;
    let document = harness_core::config::workflow::load_workflow_document(&project_root)?;
    let policy = document
        .config
        .definition
        .as_ref()
        .expect("fixture should contain a declarative definition");
    let definition = build_declarative_definition(policy, &document.config.activities)?;
    register_declarative_workflow_definitions([definition])?;
    let store = WorkflowRuntimeStore::open_with_database_url(
        &sandbox.path().join("runtime"),
        Some(&database_url),
    )
    .await?;
    let task_id = TaskId::from_str("declarative-cancel-task");
    let submission = record_declarative_submission(
        &store,
        DeclarativeSubmissionRuntimeContext {
            project_root: &project_root,
            task_id: &task_id,
            definition_id: CANCEL_DEFINITION_ID,
            prompt: "Cancel this documentation review.",
            depends_on: &[],
            serialization_depends_on: &[],
            source: Some("api"),
            external_id: Some("cancel-2026-07"),
        },
    )
    .await?;
    let submitted = store
        .get_instance(&submission.workflow_id)
        .await?
        .expect("submitted declarative workflow instance");
    let prompt_ref = submitted.data["prompt_ref"]
        .as_str()
        .expect("submission should persist a prompt reference")
        .to_string();

    let outcome = cancel_submission_by_workflow_id(&store, &submission.workflow_id).await?;
    let RuntimeSubmissionCancelOutcome::Cancelled(cancelled) = outcome else {
        anyhow::bail!("declarative submission did not cancel")
    };
    assert_eq!(cancelled.state, "withdrawn");
    assert!(cancelled.is_terminal());
    assert!(store.get_prompt_payload(&prompt_ref).await?.is_none());
    Ok(())
}
