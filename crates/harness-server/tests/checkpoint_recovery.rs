//! Integration tests: phase checkpoint recovery for long-running tasks (issue #573).
//!
//! Tests the full restart + recovery path via `TaskStore::open()`, which internally
//! calls `TaskDb::recover_in_progress()`.  Each test seeds the SQLite database using
//! `TaskDb` directly (simulating a first server session), then opens a fresh
//! `TaskStore` backed by the same DB path (simulating a restart), and asserts that
//! tasks with checkpoints are resumed while tasks without are failed.

use harness_core::types::TaskId as CoreTaskId;
use harness_server::task_db::TaskDb;
use harness_server::task_runner::{TaskPhase, TaskState, TaskStatus, TaskStore};

fn make_task(id: &str, status: TaskStatus) -> TaskState {
    TaskState {
        id: CoreTaskId(id.to_string()),
        status,
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
        issue: None,
        description: None,
        created_at: None,
        priority: 0,
        phase: TaskPhase::default(),
        triage_output: None,
        plan_output: None,
        repo: None,
    }
}

/// Seeds the DB, closes it, then opens a fresh `TaskStore` simulating a restart.
async fn setup_store<F, Fut>(seed: F) -> anyhow::Result<std::sync::Arc<TaskStore>>
where
    F: FnOnce(TaskDb) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    let tmp = tempfile::tempdir()?;
    let db_path = tmp.path().join("tasks.db");
    {
        let db = TaskDb::open(&db_path).await?;
        seed(db).await?;
    }
    // `tmp` is kept alive by forgetting it — the OS cleans up on process exit.
    let store = TaskStore::open(&db_path).await?;
    std::mem::forget(tmp);
    Ok(store)
}

/// Restart with no checkpoints: all interrupted tasks (implementing, agent_review,
/// reviewing, waiting) must become Failed; Pending/Done/Failed are left unchanged.
#[tokio::test]
async fn restart_no_checkpoint_fails_interrupted_tasks() -> anyhow::Result<()> {
    let store = setup_store(|db| async move {
        db.insert(&make_task("t-impl", TaskStatus::Implementing))
            .await?;
        db.insert(&make_task("t-review", TaskStatus::AgentReview))
            .await?;
        db.insert(&make_task("t-reviewing", TaskStatus::Reviewing))
            .await?;
        db.insert(&make_task("t-waiting", TaskStatus::Waiting))
            .await?;
        db.insert(&make_task("t-pending", TaskStatus::Pending))
            .await?;
        db.insert(&make_task("t-done", TaskStatus::Done)).await?;
        db.insert(&make_task("t-failed", TaskStatus::Failed))
            .await?;
        Ok(())
    })
    .await?;

    // All 4 interrupted tasks with no checkpoint → Failed.
    // These are no longer in the in-memory cache (terminal tasks are excluded at startup),
    // so fetch them from the DB via get_with_db_fallback.
    for id in ["t-impl", "t-review", "t-reviewing", "t-waiting"] {
        let task = store
            .get_with_db_fallback(&CoreTaskId(id.to_string()))
            .await
            .expect("DB query should not fail")
            .unwrap_or_else(|| panic!("{id} should be fetchable from DB"));
        assert!(
            matches!(task.status, TaskStatus::Failed),
            "{id}: expected Failed, got {:?}",
            task.status
        );
        let err = task.error.as_deref().unwrap_or("");
        assert!(
            err.contains("recovered after restart"),
            "{id}: error should mention restart, got: {err:?}"
        );
    }

    // Pending stays Pending (active — present in the in-memory cache).
    let pending = store
        .get(&CoreTaskId("t-pending".into()))
        .expect("t-pending must exist in cache");
    assert!(matches!(pending.status, TaskStatus::Pending));
    assert!(pending.error.is_none());

    // Terminal states unchanged (not in cache; fetch from DB).
    let done = store
        .get_with_db_fallback(&CoreTaskId("t-done".into()))
        .await
        .expect("DB query should not fail")
        .expect("t-done must be fetchable from DB");
    assert!(matches!(done.status, TaskStatus::Done));

    let failed = store
        .get_with_db_fallback(&CoreTaskId("t-failed".into()))
        .await
        .expect("DB query should not fail")
        .expect("t-failed must be fetchable from DB");
    assert!(matches!(failed.status, TaskStatus::Failed));

    Ok(())
}

/// Restart with a `tasks.pr_url` set: the task resumes to Pending with a diagnostic
/// error message referencing the PR URL.
#[tokio::test]
async fn restart_with_pr_url_resumes_task() -> anyhow::Result<()> {
    let store = setup_store(|db| async move {
        let mut task = make_task("t-impl-pr", TaskStatus::Implementing);
        task.pr_url = Some("https://github.com/owner/repo/pull/42".to_string());
        db.insert(&task).await?;
        Ok(())
    })
    .await?;

    let task = store
        .get(&CoreTaskId("t-impl-pr".into()))
        .expect("task must exist");
    assert!(
        matches!(task.status, TaskStatus::Pending),
        "task with PR URL should be resumed to Pending, got {:?}",
        task.status
    );
    assert_eq!(
        task.pr_url.as_deref(),
        Some("https://github.com/owner/repo/pull/42"),
        "pr_url must be preserved after resume"
    );
    assert!(
        task.error.is_none(),
        "resumed task must not have error set, got {:?}",
        task.error
    );

    Ok(())
}

/// Restart with a checkpoint PR URL (checkpoint table, not tasks table): task resumes.
#[tokio::test]
async fn restart_with_checkpoint_pr_url_resumes_task() -> anyhow::Result<()> {
    let store = setup_store(|db| async move {
        db.insert(&make_task("t-ck-pr", TaskStatus::AgentReview))
            .await?;
        db.write_checkpoint(
            "t-ck-pr",
            None,
            None,
            Some("https://github.com/owner/repo/pull/55"),
            "pr_created",
        )
        .await?;
        Ok(())
    })
    .await?;

    let task = store
        .get(&CoreTaskId("t-ck-pr".into()))
        .expect("task must exist");
    assert!(
        matches!(task.status, TaskStatus::Pending),
        "task with checkpoint PR URL should resume, got {:?}",
        task.status
    );
    assert!(
        task.error.is_none(),
        "resumed task must not have error set, got {:?}",
        task.error
    );

    Ok(())
}

/// Restart with a plan checkpoint: task resumes to Pending.
#[tokio::test]
async fn restart_with_plan_checkpoint_resumes_task() -> anyhow::Result<()> {
    let store = setup_store(|db| async move {
        db.insert(&make_task("t-plan", TaskStatus::Implementing))
            .await?;
        db.write_checkpoint(
            "t-plan",
            None,
            Some("## Plan\nStep 1: do X"),
            None,
            "plan_done",
        )
        .await?;
        Ok(())
    })
    .await?;

    let task = store
        .get(&CoreTaskId("t-plan".into()))
        .expect("task must exist");
    assert!(
        matches!(task.status, TaskStatus::Pending),
        "task with plan checkpoint should resume, got {:?}",
        task.status
    );
    assert!(
        task.error.is_none(),
        "resumed task must not have error set, got {:?}",
        task.error
    );

    Ok(())
}

/// Restart with a triage-only checkpoint: task resumes to Pending.
#[tokio::test]
async fn restart_with_triage_checkpoint_resumes_task() -> anyhow::Result<()> {
    let store = setup_store(|db| async move {
        db.insert(&make_task("t-triage", TaskStatus::Implementing))
            .await?;
        db.write_checkpoint(
            "t-triage",
            Some("complexity: low"),
            None,
            None,
            "triage_done",
        )
        .await?;
        Ok(())
    })
    .await?;

    let task = store
        .get(&CoreTaskId("t-triage".into()))
        .expect("task must exist");
    assert!(
        matches!(task.status, TaskStatus::Pending),
        "task with triage checkpoint should resume, got {:?}",
        task.status
    );
    assert!(
        task.error.is_none(),
        "resumed task must not have error set, got {:?}",
        task.error
    );

    Ok(())
}

/// Tasks that crashed mid-transient-retry (status=pending, error starts with
/// "retrying after transient failure") must become Failed on restart.
#[tokio::test]
async fn restart_transient_retry_task_fails() -> anyhow::Result<()> {
    let store = setup_store(|db| async move {
        let mut task = make_task("t-transient", TaskStatus::Pending);
        task.error = Some("retrying after transient failure (attempt 2)".to_string());
        db.insert(&task).await?;
        // Normal pending task (no transient error) must not be affected.
        db.insert(&make_task("t-normal-pending", TaskStatus::Pending))
            .await?;
        Ok(())
    })
    .await?;

    // Transient-retry task was failed by recovery; not in cache — fetch from DB.
    let transient = store
        .get_with_db_fallback(&CoreTaskId("t-transient".into()))
        .await
        .expect("DB query should not fail")
        .expect("task must be fetchable from DB");
    assert!(
        matches!(transient.status, TaskStatus::Failed),
        "transient-retry pending task should be Failed on restart, got {:?}",
        transient.status
    );
    let err = transient.error.as_deref().unwrap_or("");
    assert!(
        err.contains("transient"),
        "error should reference transient retry, got: {err:?}"
    );

    // Normal pending task unaffected.
    let normal = store
        .get(&CoreTaskId("t-normal-pending".into()))
        .expect("t-normal-pending must exist");
    assert!(
        matches!(normal.status, TaskStatus::Pending),
        "normal pending task should stay Pending, got {:?}",
        normal.status
    );
    assert!(
        normal.error.is_none(),
        "normal pending task must have no error set"
    );

    Ok(())
}

/// Mixed scenario: some tasks resume, some fail — recovery counts are correct.
#[tokio::test]
async fn restart_mixed_recovery_counts() -> anyhow::Result<()> {
    let store = setup_store(|db| async move {
        // Will be resumed: has plan checkpoint.
        db.insert(&make_task("t-resumable-plan", TaskStatus::Implementing))
            .await?;
        db.write_checkpoint(
            "t-resumable-plan",
            None,
            Some("plan text"),
            None,
            "plan_done",
        )
        .await?;

        // Will be resumed: has pr_url.
        let mut with_pr = make_task("t-resumable-pr", TaskStatus::AgentReview);
        with_pr.pr_url = Some("https://github.com/o/r/pull/7".to_string());
        db.insert(&with_pr).await?;

        // Will be failed: no checkpoint, no PR.
        db.insert(&make_task("t-fail-1", TaskStatus::Reviewing))
            .await?;
        db.insert(&make_task("t-fail-2", TaskStatus::Waiting))
            .await?;
        Ok(())
    })
    .await?;

    // Resumed tasks are Pending and present in the in-memory cache.
    let resumed_count = store
        .list_all()
        .iter()
        .filter(|t| matches!(t.status, TaskStatus::Pending))
        .count();
    assert_eq!(resumed_count, 2, "two tasks should have been resumed");

    // Failed tasks are terminal and not in cache; fetch each from DB.
    let mut failed_count = 0;
    for id in ["t-fail-1", "t-fail-2"] {
        if let Ok(Some(task)) = store
            .get_with_db_fallback(&CoreTaskId(id.to_string()))
            .await
        {
            if matches!(task.status, TaskStatus::Failed) {
                failed_count += 1;
            }
        }
    }
    assert_eq!(failed_count, 2, "two tasks should have been failed");

    Ok(())
}
