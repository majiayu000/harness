//! Regression tests: corrupted `rounds` column must surface as a distinguishable
//! error rather than silently defaulting to an empty list (issue #62).

use harness_core::error::TaskDbDecodeError;
use harness_core::types::TaskId as CoreTaskId;
use harness_server::task_db::TaskDb;
use harness_server::task_runner::{TaskKind, TaskPhase, TaskSchedulerState, TaskState, TaskStatus};

fn make_task(id: &str) -> TaskState {
    TaskState {
        id: CoreTaskId(id.to_string()),
        task_kind: TaskKind::Prompt,
        status: TaskStatus::Pending,
        failure_kind: None,
        turn: 0,
        pr_url: None,
        rounds: vec![],
        error: None,
        source: None,
        external_id: None,
        parent_id: None,
        depends_on: vec![],
        subtask_ids: vec![],
        project_root: None,
        workspace_path: None,
        workspace_owner: None,
        run_generation: 0,
        issue: None,
        description: None,
        created_at: None,
        updated_at: None,
        priority: 0,
        phase: TaskPhase::default(),
        triage_output: None,
        plan_output: None,
        repo: None,
        request_settings: None,
        scheduler: TaskSchedulerState::queued(),
    }
}

/// A missing task returns `Ok(None)`; a task with a corrupted `rounds` column
/// returns a distinguishable `TaskDbDecodeError::RoundsDeserialize` error,
/// not `Ok(Some(_))` with silently-defaulted empty rounds.
#[tokio::test]
async fn get_distinguishes_missing_task_from_corrupted_rounds() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let db_path = tmp.path().join("tasks.db");
    let db = TaskDb::open(&db_path).await?;

    // Missing task must return Ok(None), not an error.
    let missing = db.get("task-does-not-exist").await?;
    assert!(missing.is_none(), "missing task must return Ok(None)");

    // Insert a valid task, then corrupt its rounds column with invalid JSON.
    db.insert(&make_task("task-corrupted-rounds")).await?;
    db.overwrite_rounds_for_test("task-corrupted-rounds", "not-valid-json[[{{{")
        .await?;

    // Fetching a task with corrupted rounds must return Err, not Ok with empty rounds.
    let result = db.get("task-corrupted-rounds").await;
    let err = result.expect_err("corrupted rounds must return Err, not Ok(Some(_))");

    // The error must be identifiable as RoundsDeserialize, not a generic anyhow error.
    let is_rounds_err = err
        .downcast_ref::<TaskDbDecodeError>()
        .is_some_and(|e| matches!(e, TaskDbDecodeError::RoundsDeserialize { .. }));
    assert!(
        is_rounds_err,
        "error must be TaskDbDecodeError::RoundsDeserialize; got: {err}"
    );

    std::mem::forget(tmp);
    Ok(())
}
