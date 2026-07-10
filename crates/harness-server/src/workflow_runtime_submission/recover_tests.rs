use super::*;
use harness_core::db::resolve_database_url;
use harness_workflow::runtime::{WorkflowCommandType, WorkflowSubject};
use serde_json::json;
use std::path::Path;

async fn open_runtime_store(dir: &Path) -> anyhow::Result<WorkflowRuntimeStore> {
    let database_url = resolve_database_url(None)?;
    WorkflowRuntimeStore::open_with_database_url(dir, Some(&database_url)).await
}

async fn upsert_stopped_issue_instance(
    store: &WorkflowRuntimeStore,
    workflow_id: &str,
    state: &str,
    data: serde_json::Value,
) -> anyhow::Result<()> {
    upsert_github_issue_pr_definition(store).await?;
    store
        .upsert_instance(
            &WorkflowInstance::new(
                GITHUB_ISSUE_PR_DEFINITION_ID,
                1,
                state,
                WorkflowSubject::new("issue", "owner/repo#1"),
            )
            .with_id(workflow_id.to_string())
            .with_data(data),
        )
        .await?;
    Ok(())
}

fn expect_recovered(outcome: RuntimeRecoverOutcome, context: &str) -> (WorkflowInstance, String) {
    match outcome {
        RuntimeRecoverOutcome::Recovered {
            instance,
            previous_state,
        } => (*instance, previous_state),
        other => panic!("{context}: expected recovered workflow, got {other:?}"),
    }
}

#[tokio::test]
async fn unblock_moves_blocked_issue_to_planning_and_clears_reason() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = open_runtime_store(dir.path()).await?;
    let workflow_id = "recover-unblock-1";
    upsert_stopped_issue_instance(
        &store,
        workflow_id,
        "blocked",
        json!({
            "blocked_reason": "awaiting maintainer approval on issue #841",
            "unblock_hint": "post the approval comment, then unblock",
            "last_stop": { "state": "blocked", "runtime_job_id": "job-1" },
        }),
    )
    .await?;

    let outcome =
        unblock_submission_by_workflow_id(&store, workflow_id, "operator approved").await?;
    let (instance, previous_state) = expect_recovered(outcome, "unblock");
    assert_eq!(previous_state, "blocked");
    assert_eq!(instance.state, "planning");
    // Active stop reason cleared; historical evidence preserved.
    assert!(instance.data.get("blocked_reason").is_none());
    assert!(instance.data.get("unblock_hint").is_none());
    assert_eq!(instance.data["last_stop"]["runtime_job_id"], "job-1");
    assert_eq!(
        instance.data["last_recovery"]["action"],
        "unblock_issue_submission"
    );

    // Persisted shape matches the returned instance.
    let Some(reloaded) = store.get_instance(workflow_id).await? else {
        panic!("instance should persist");
    };
    assert_eq!(reloaded.state, "planning");
    assert!(reloaded.data.get("blocked_reason").is_none());

    // A plan activity was re-enqueued so the dispatcher resumes the workflow.
    let commands = store.commands_for(workflow_id).await?;
    assert!(commands
        .iter()
        .any(|command| command.command.command_type == WorkflowCommandType::EnqueueActivity));
    Ok(())
}

#[tokio::test]
async fn retry_moves_failed_issue_to_planning() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = open_runtime_store(dir.path()).await?;
    let workflow_id = "recover-retry-1";
    upsert_stopped_issue_instance(
        &store,
        workflow_id,
        "failed",
        json!({
            "failure_reason": "codex proxy transient transport error",
            "error_kind": "external_dependency",
        }),
    )
    .await?;

    let outcome =
        retry_submission_by_workflow_id(&store, workflow_id, "proxy healthy again").await?;
    let (instance, previous_state) = expect_recovered(outcome, "retry");
    assert_eq!(previous_state, "failed");
    assert_eq!(instance.state, "planning");
    assert!(instance.data.get("failure_reason").is_none());
    Ok(())
}

#[tokio::test]
async fn retry_rejects_non_retryable_failure() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = open_runtime_store(dir.path()).await?;
    let workflow_id = "recover-retry-nonretryable";
    upsert_stopped_issue_instance(
        &store,
        workflow_id,
        "failed",
        json!({
            "failure_reason": "invalid workflow configuration",
            "error_kind": "configuration",
        }),
    )
    .await?;

    let outcome = retry_submission_by_workflow_id(&store, workflow_id, "").await?;
    match outcome {
        RuntimeRecoverOutcome::NotRetryable { error_kind } => {
            assert_eq!(error_kind, "configuration");
        }
        other => panic!("expected NotRetryable, got {other:?}"),
    }
    // State is unchanged when retry is refused.
    let Some(reloaded) = store.get_instance(workflow_id).await? else {
        panic!("instance should persist");
    };
    assert_eq!(reloaded.state, "failed");
    Ok(())
}

#[tokio::test]
async fn unblock_rejects_wrong_state() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = open_runtime_store(dir.path()).await?;
    let workflow_id = "recover-wrong-state";
    upsert_stopped_issue_instance(&store, workflow_id, "implementing", json!({})).await?;

    let outcome = unblock_submission_by_workflow_id(&store, workflow_id, "").await?;
    match outcome {
        RuntimeRecoverOutcome::WrongState { current_state } => {
            assert_eq!(current_state, "implementing");
        }
        other => panic!("expected WrongState, got {other:?}"),
    }
    Ok(())
}

#[tokio::test]
async fn recover_returns_not_found_for_missing_workflow() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = open_runtime_store(dir.path()).await?;
    let outcome = unblock_submission_by_workflow_id(&store, "no-such-workflow", "").await?;
    assert!(matches!(outcome, RuntimeRecoverOutcome::NotFound));
    Ok(())
}
