//! Integration tests: phase checkpoint recovery for long-running tasks (issue #573).
//!
//! Tests the full restart + recovery path via `TaskStore::open()`, which internally
//! calls `TaskDb::recover_in_progress()`.  Each test seeds the SQLite database using
//! `TaskDb` directly (simulating a first server session), then opens a fresh
//! `TaskStore` backed by the same DB path (simulating a restart), and asserts that
//! tasks with checkpoints are resumed while tasks without are failed.

use harness_core::types::TaskId as CoreTaskId;
use harness_server::event_replay::{TaskEvent, TaskEventLog};
use harness_server::task_db::TaskDb;
use harness_server::task_runner::{TaskKind, TaskPhase, TaskState, TaskStatus, TaskStore};

fn make_task(id: &str, status: TaskStatus) -> TaskState {
    TaskState {
        id: CoreTaskId(id.to_string()),
        task_kind: TaskKind::Prompt,
        status,
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
        scheduler: harness_server::task_runner::TaskSchedulerState::queued(),
        version: 0,
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

async fn recovered_task(store: &TaskStore, id: &str) -> anyhow::Result<TaskState> {
    store
        .get_with_db_fallback(&CoreTaskId(id.to_string()))
        .await?
        .ok_or_else(|| anyhow::anyhow!("{id} should be fetchable after recovery"))
}

fn assert_not_limbo(task: &TaskState) {
    assert!(
        !task.status.is_inflight(),
        "{} recovered into in-flight limbo status {:?}",
        task.id.0,
        task.status
    );
}

fn database_tests_configured() -> bool {
    std::env::var("HARNESS_DATABASE_URL").is_ok_and(|url| !url.trim().is_empty())
}

/// Restart with no checkpoints: all interrupted tasks (implementing, agent_review,
/// reviewing, waiting) must become Failed; Pending/Done/Failed are left unchanged.
#[tokio::test]
async fn restart_no_checkpoint_fails_interrupted_tasks() -> anyhow::Result<()> {
    if !database_tests_configured() {
        return Ok(());
    }

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

#[tokio::test]
async fn recovery_no_limbo_restart_mid_review_states_resume_or_terminalize() -> anyhow::Result<()> {
    if !database_tests_configured() {
        return Ok(());
    }

    let store = setup_store(|db| async move {
        let mut review_with_pr = make_task("review-with-pr", TaskStatus::AgentReview);
        review_with_pr.pr_url = Some("https://github.com/owner/repo/pull/1465".to_string());
        db.insert(&review_with_pr).await?;

        db.insert(&make_task("review-with-plan", TaskStatus::ReviewGenerating))
            .await?;
        db.write_checkpoint(
            "review-with-plan",
            None,
            Some("persisted implementation plan"),
            None,
            "plan_done",
        )
        .await?;

        db.insert(&make_task("reviewing-no-checkpoint", TaskStatus::Reviewing))
            .await?;
        db.insert(&make_task("waiting-no-checkpoint", TaskStatus::Waiting))
            .await?;
        Ok(())
    })
    .await?;

    let review_with_pr = recovered_task(&store, "review-with-pr").await?;
    assert_not_limbo(&review_with_pr);
    assert_eq!(review_with_pr.status, TaskStatus::Pending);
    assert!(matches!(
        review_with_pr.scheduler.authority_state,
        harness_server::task_runner::SchedulerAuthorityState::Recovering
    ));

    let review_with_plan = recovered_task(&store, "review-with-plan").await?;
    assert_not_limbo(&review_with_plan);
    assert_eq!(review_with_plan.status, TaskStatus::Pending);
    assert!(matches!(
        review_with_plan.scheduler.authority_state,
        harness_server::task_runner::SchedulerAuthorityState::Recovering
    ));

    for id in ["reviewing-no-checkpoint", "waiting-no-checkpoint"] {
        let task = recovered_task(&store, id).await?;
        assert_not_limbo(&task);
        assert_eq!(task.status, TaskStatus::Failed);
        assert!(
            task.error
                .as_deref()
                .is_some_and(|error| error.contains("recovered after restart")),
            "{id} should carry a restart recovery failure reason"
        );
    }

    Ok(())
}

#[tokio::test]
async fn recovery_no_limbo_shutdown_drain_states_are_queued_or_terminal() -> anyhow::Result<()> {
    if !database_tests_configured() {
        return Ok(());
    }

    let store = setup_store(|db| async move {
        db.insert(&make_task("drained-pending", TaskStatus::Pending))
            .await?;

        let mut implementing = make_task("drained-implementing", TaskStatus::Implementing);
        implementing.scheduler.claim_scheduler("shutdown-worker");
        db.insert(&implementing).await?;

        let mut review_waiting = make_task("drained-review-waiting", TaskStatus::ReviewWaiting);
        review_waiting.scheduler.claim_scheduler("shutdown-worker");
        db.insert(&review_waiting).await?;
        Ok(())
    })
    .await?;

    let pending = recovered_task(&store, "drained-pending").await?;
    assert_not_limbo(&pending);
    assert_eq!(pending.status, TaskStatus::Pending);
    assert!(matches!(
        pending.scheduler.authority_state,
        harness_server::task_runner::SchedulerAuthorityState::Queued
    ));

    for id in ["drained-implementing", "drained-review-waiting"] {
        let task = recovered_task(&store, id).await?;
        assert_not_limbo(&task);
        assert_eq!(task.status, TaskStatus::Failed);
        assert!(
            task.error
                .as_deref()
                .is_some_and(|error| error.contains("recovered after restart")),
            "{id} should carry a restart recovery failure reason"
        );
    }

    Ok(())
}

/// Restart with a `tasks.pr_url` set: the task resumes to Pending with a diagnostic
/// error message referencing the PR URL.
#[tokio::test]
async fn restart_with_pr_url_resumes_task() -> anyhow::Result<()> {
    if !database_tests_configured() {
        return Ok(());
    }

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
    if !database_tests_configured() {
        return Ok(());
    }

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
    if !database_tests_configured() {
        return Ok(());
    }

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
    if !database_tests_configured() {
        return Ok(());
    }

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

#[tokio::test]
async fn restart_review_task_without_system_input_fails() -> anyhow::Result<()> {
    if !database_tests_configured() {
        return Ok(());
    }

    let store = setup_store(|db| async move {
        let mut task = make_task("t-review-missing-input", TaskStatus::ReviewGenerating);
        task.task_kind = TaskKind::Review;
        db.insert(&task).await?;
        Ok(())
    })
    .await?;

    let task = store
        .get_with_db_fallback(&CoreTaskId("t-review-missing-input".into()))
        .await?
        .expect("task should be fetchable from DB");
    assert!(
        matches!(task.status, TaskStatus::Failed),
        "review task without persisted input should fail on restart, got {:?}",
        task.status
    );
    assert!(task.error.is_some());
    Ok(())
}

/// Tasks that crashed mid-transient-retry (status=pending, error starts with
/// "retrying after transient failure") must become Failed on restart.
#[tokio::test]
async fn restart_transient_retry_task_fails() -> anyhow::Result<()> {
    if !database_tests_configured() {
        return Ok(());
    }

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
    if !database_tests_configured() {
        return Ok(());
    }

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

#[tokio::test]
async fn restart_marks_resumed_task_as_recovering_in_scheduler_state() -> anyhow::Result<()> {
    if !database_tests_configured() {
        return Ok(());
    }

    let store = setup_store(|db| async move {
        db.insert(&make_task("t-recovering", TaskStatus::Implementing))
            .await?;
        db.write_checkpoint("t-recovering", None, Some("plan text"), None, "plan_done")
            .await?;
        Ok(())
    })
    .await?;

    let recovered = store
        .get(&CoreTaskId("t-recovering".into()))
        .expect("recovered task should remain active");
    assert!(matches!(recovered.status, TaskStatus::Pending));
    assert!(matches!(
        recovered.scheduler.authority_state,
        harness_server::task_runner::SchedulerAuthorityState::Recovering
    ));
    assert_eq!(recovered.scheduler.recovery_generation, 1);
    assert_eq!(
        recovered
            .scheduler
            .owner
            .as_ref()
            .map(|owner| owner.id.as_str()),
        Some("startup-recovery")
    );

    Ok(())
}

#[tokio::test]
async fn replay_conflict_fails_startup_before_checkpoint_recovery() -> anyhow::Result<()> {
    if !database_tests_configured() {
        return Ok(());
    }

    let tmp = tempfile::tempdir()?;
    let db_path = tmp.path().join("tasks.db");
    let db = TaskDb::open(&db_path).await?;

    let mut replay_conflict = make_task("startup-replay-conflict", TaskStatus::Failed);
    replay_conflict.pr_url = Some("https://github.com/o/r/pull/999".to_string());
    db.insert(&replay_conflict).await?;
    db.insert(&make_task(
        "checkpoint-must-not-run",
        TaskStatus::Implementing,
    ))
    .await?;
    drop(db);

    let event_log_path = tmp.path().join("task-events.jsonl");
    let event_log = TaskEventLog::open(&event_log_path)?;
    event_log.append(&TaskEvent::PrDetected {
        task_id: "startup-replay-conflict".into(),
        ts: 1,
        pr_url: "https://github.com/o/r/pull/1716".into(),
    });
    event_log.append(&TaskEvent::Completed {
        task_id: "startup-replay-conflict".into(),
        ts: 2,
    });
    drop(event_log);

    let error = match TaskStore::open(&db_path).await {
        Ok(_) => anyhow::bail!("typed replay conflict must fail task-store startup"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("task recovery conflict")
            && error.to_string().contains("startup-replay-conflict"),
        "startup should preserve recovery conflict context: {error:#}"
    );

    let db = TaskDb::open(&db_path).await?;
    let untouched = db
        .get("checkpoint-must-not-run")
        .await?
        .expect("checkpoint task should still exist");
    assert_eq!(
        untouched.status,
        TaskStatus::Implementing,
        "checkpoint recovery must not run after a replay conflict"
    );
    Ok(())
}

#[tokio::test]
async fn non_conflict_replay_failure_remains_non_fatal_at_startup() -> anyhow::Result<()> {
    if !database_tests_configured() {
        return Ok(());
    }

    let tmp = tempfile::tempdir()?;
    let db_path = tmp.path().join("tasks.db");
    let db = TaskDb::open(&db_path).await?;
    db.insert(&make_task(
        "checkpoint-after-replay-error",
        TaskStatus::Implementing,
    ))
    .await?;
    db.write_checkpoint(
        "checkpoint-after-replay-error",
        None,
        Some("plan text"),
        None,
        "plan_done",
    )
    .await?;
    drop(db);

    std::fs::create_dir(tmp.path().join("task-events.jsonl"))?;
    let store = TaskStore::open(&db_path)
        .await
        .expect("a non-conflict replay I/O error remains non-fatal");
    let recovered = store
        .get_with_db_fallback(&CoreTaskId("checkpoint-after-replay-error".into()))
        .await?
        .expect("checkpoint recovery must still run after a non-conflict replay error");
    assert_eq!(recovered.status, TaskStatus::Pending);
    assert_eq!(recovered.scheduler.recovery_generation, 1);
    Ok(())
}
