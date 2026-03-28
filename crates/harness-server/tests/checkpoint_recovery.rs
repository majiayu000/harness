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
        phase: TaskPhase::default(),
        triage_output: None,
        plan_output: None,
        repo: None,
    }
}

/// Restart with no checkpoints: all interrupted tasks (implementing, agent_review,
/// reviewing, waiting) must become Failed; Pending/Done/Failed are left unchanged.
#[tokio::test]
async fn restart_no_checkpoint_fails_interrupted_tasks() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let db_path = tmp.path().join("tasks.db");

    // Session 1: seed interrupted tasks with no checkpoints.
    {
        let db = TaskDb::open(&db_path).await?;
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
    }

    // Session 2: simulate restart — TaskStore::open calls recover_in_progress internally.
    let store = TaskStore::open(&db_path).await?;

    // All 4 interrupted tasks with no checkpoint → Failed.
    for id in ["t-impl", "t-review", "t-reviewing", "t-waiting"] {
        let task = store
            .get(&CoreTaskId(id.to_string()))
            .unwrap_or_else(|| panic!("{id} should be in cache"));
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

    // Pending stays Pending (no error set by recovery).
    let pending = store
        .get(&CoreTaskId("t-pending".into()))
        .expect("t-pending must exist");
    assert!(matches!(pending.status, TaskStatus::Pending));
    assert!(pending.error.is_none());

    // Terminal states unchanged.
    let done = store
        .get(&CoreTaskId("t-done".into()))
        .expect("t-done must exist");
    assert!(matches!(done.status, TaskStatus::Done));

    let failed = store
        .get(&CoreTaskId("t-failed".into()))
        .expect("t-failed must exist");
    assert!(matches!(failed.status, TaskStatus::Failed));

    Ok(())
}

/// Restart with a `tasks.pr_url` set: the task resumes to Pending with a diagnostic
/// error message referencing the PR URL.
#[tokio::test]
async fn restart_with_pr_url_resumes_task() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let db_path = tmp.path().join("tasks.db");

    {
        let db = TaskDb::open(&db_path).await?;
        let mut task = make_task("t-impl-pr", TaskStatus::Implementing);
        task.pr_url = Some("https://github.com/owner/repo/pull/42".to_string());
        db.insert(&task).await?;
    }

    let store = TaskStore::open(&db_path).await?;

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
    let err = task.error.as_deref().unwrap_or("");
    assert!(
        err.contains("resumed after restart"),
        "error should note resumption, got: {err:?}"
    );
    assert!(
        err.contains("pull/42"),
        "error should reference the PR URL, got: {err:?}"
    );

    Ok(())
}

/// Restart with a checkpoint PR URL (checkpoint table, not tasks table): task resumes.
#[tokio::test]
async fn restart_with_checkpoint_pr_url_resumes_task() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let db_path = tmp.path().join("tasks.db");

    {
        let db = TaskDb::open(&db_path).await?;
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
    }

    let store = TaskStore::open(&db_path).await?;

    let task = store
        .get(&CoreTaskId("t-ck-pr".into()))
        .expect("task must exist");
    assert!(
        matches!(task.status, TaskStatus::Pending),
        "task with checkpoint PR URL should resume, got {:?}",
        task.status
    );
    let err = task.error.as_deref().unwrap_or("");
    assert!(
        err.contains("resumed after restart"),
        "error should note resumption, got: {err:?}"
    );

    Ok(())
}

/// Restart with a plan checkpoint: task resumes to Pending.
#[tokio::test]
async fn restart_with_plan_checkpoint_resumes_task() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let db_path = tmp.path().join("tasks.db");

    {
        let db = TaskDb::open(&db_path).await?;
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
    }

    let store = TaskStore::open(&db_path).await?;

    let task = store
        .get(&CoreTaskId("t-plan".into()))
        .expect("task must exist");
    assert!(
        matches!(task.status, TaskStatus::Pending),
        "task with plan checkpoint should resume, got {:?}",
        task.status
    );
    let err = task.error.as_deref().unwrap_or("");
    assert!(
        err.contains("plan checkpoint"),
        "error should mention plan checkpoint, got: {err:?}"
    );

    Ok(())
}

/// Restart with a triage-only checkpoint: task resumes to Pending.
#[tokio::test]
async fn restart_with_triage_checkpoint_resumes_task() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let db_path = tmp.path().join("tasks.db");

    {
        let db = TaskDb::open(&db_path).await?;
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
    }

    let store = TaskStore::open(&db_path).await?;

    let task = store
        .get(&CoreTaskId("t-triage".into()))
        .expect("task must exist");
    assert!(
        matches!(task.status, TaskStatus::Pending),
        "task with triage checkpoint should resume, got {:?}",
        task.status
    );
    let err = task.error.as_deref().unwrap_or("");
    assert!(
        err.contains("triage checkpoint"),
        "error should mention triage checkpoint, got: {err:?}"
    );

    Ok(())
}

/// Tasks that crashed mid-transient-retry (status=pending, error starts with
/// "retrying after transient failure") must become Failed on restart.
#[tokio::test]
async fn restart_transient_retry_task_fails() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let db_path = tmp.path().join("tasks.db");

    {
        let db = TaskDb::open(&db_path).await?;
        let mut task = make_task("t-transient", TaskStatus::Pending);
        task.error = Some("retrying after transient failure (attempt 2)".to_string());
        db.insert(&task).await?;

        // Normal pending task (no transient error) must not be affected.
        db.insert(&make_task("t-normal-pending", TaskStatus::Pending))
            .await?;
    }

    let store = TaskStore::open(&db_path).await?;

    let transient = store
        .get(&CoreTaskId("t-transient".into()))
        .expect("task must exist");
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
    let tmp = tempfile::tempdir()?;
    let db_path = tmp.path().join("tasks.db");

    {
        let db = TaskDb::open(&db_path).await?;

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
    }

    let store = TaskStore::open(&db_path).await?;

    let all = store.list_all();
    let resumed: Vec<_> = all
        .iter()
        .filter(|t| matches!(t.status, TaskStatus::Pending))
        .collect();
    let failed: Vec<_> = all
        .iter()
        .filter(|t| matches!(t.status, TaskStatus::Failed))
        .collect();

    assert_eq!(resumed.len(), 2, "two tasks should have been resumed");
    assert_eq!(failed.len(), 2, "two tasks should have been failed");

    Ok(())
}
