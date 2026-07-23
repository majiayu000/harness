use super::queries_recovery::observe_recovery_outcome;
use super::{TaskDb, TaskRecoveryConflict, TaskRecoveryWriteOutcome};
use crate::task_runner::{TaskSchedulerState, TaskState, TaskStatus};
use harness_core::types::TaskId;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Record};
use tracing::{Event, Id, Metadata, Subscriber};

const PR_URL: &str = "https://github.com/owner/repo/pull/1716";
const OTHER_PR_URL: &str = "https://github.com/owner/repo/pull/999";
const FAILURE_ERROR: &str = "recovered after restart";

#[derive(Debug, Clone, Copy)]
enum RecoverySite {
    TerminalReplay,
    PrReplay,
    ResumeWithPr,
    ResumeWithoutPr,
    NoCheckpointFailure,
    TransientFailure,
}

const RECOVERY_SITES: [RecoverySite; 6] = [
    RecoverySite::TerminalReplay,
    RecoverySite::PrReplay,
    RecoverySite::ResumeWithPr,
    RecoverySite::ResumeWithoutPr,
    RecoverySite::NoCheckpointFailure,
    RecoverySite::TransientFailure,
];

#[derive(Clone, Default)]
struct CapturedSubscriber {
    output: Arc<Mutex<Vec<String>>>,
    next_span_id: Arc<AtomicU64>,
}

#[derive(Default)]
struct EventVisitor(String);

impl Visit for EventVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if !self.0.is_empty() {
            self.0.push(' ');
        }
        self.0.push_str(field.name());
        self.0.push('=');
        self.0.push_str(&format!("{value:?}"));
    }
}

impl Subscriber for CapturedSubscriber {
    fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _attributes: &Attributes<'_>) -> Id {
        Id::from_u64(self.next_span_id.fetch_add(1, Ordering::Relaxed) + 1)
    }

    fn record(&self, _span: &Id, _values: &Record<'_>) {}

    fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut visitor = EventVisitor::default();
        event.record(&mut visitor);
        match self.output.lock() {
            Ok(mut output) => output.push(visitor.0),
            Err(poisoned) => poisoned.into_inner().push(visitor.0),
        }
    }

    fn enter(&self, _span: &Id) {}

    fn exit(&self, _span: &Id) {}
}

impl CapturedSubscriber {
    fn output(&self) -> String {
        let events = match self.output.lock() {
            Ok(output) => output.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        events.join("\n")
    }
}

fn database_tests_configured() -> bool {
    std::env::var("HARNESS_DATABASE_URL").is_ok_and(|url| !url.trim().is_empty())
}

fn site_name(site: RecoverySite) -> &'static str {
    match site {
        RecoverySite::TerminalReplay => "terminal",
        RecoverySite::PrReplay => "pr",
        RecoverySite::ResumeWithPr => "resume-with-pr",
        RecoverySite::ResumeWithoutPr => "resume-without-pr",
        RecoverySite::NoCheckpointFailure => "no-checkpoint",
        RecoverySite::TransientFailure => "transient",
    }
}

fn target_scheduler(site: RecoverySite) -> TaskSchedulerState {
    let mut scheduler = TaskSchedulerState::queued();
    match site {
        RecoverySite::TerminalReplay => scheduler.mark_terminal(&TaskStatus::Done),
        RecoverySite::ResumeWithPr | RecoverySite::ResumeWithoutPr => {
            scheduler.mark_recovering("startup-recovery");
        }
        RecoverySite::NoCheckpointFailure | RecoverySite::TransientFailure => {
            scheduler.mark_terminal(&TaskStatus::Failed);
        }
        RecoverySite::PrReplay => {}
    }
    scheduler
}

fn seed_task(site: RecoverySite, id: &str) -> TaskState {
    let mut task = TaskState::new(TaskId(id.to_string()));
    match site {
        RecoverySite::TransientFailure => {
            task.status = TaskStatus::Pending;
            task.error = Some("retrying after transient failure (attempt 2)".to_string());
        }
        _ => task.status = TaskStatus::Implementing,
    }
    if matches!(site, RecoverySite::ResumeWithoutPr) {
        task.pr_url = Some(PR_URL.to_string());
    }
    task
}

async fn apply_site(
    db: &TaskDb,
    site: RecoverySite,
    task_id: &str,
    expected_version: i32,
) -> anyhow::Result<TaskRecoveryWriteOutcome> {
    let scheduler = target_scheduler(site);
    let scheduler_json = serde_json::to_string(&scheduler)?;
    match site {
        RecoverySite::TerminalReplay => {
            db.apply_terminal_replay_at_version(
                task_id,
                Some(PR_URL),
                &TaskStatus::Done,
                &scheduler,
                &scheduler_json,
                expected_version,
            )
            .await
        }
        RecoverySite::PrReplay => {
            db.apply_pr_replay_at_version(task_id, PR_URL, expected_version)
                .await
        }
        RecoverySite::ResumeWithPr => {
            db.apply_checkpoint_resume_with_pr_at_version(
                task_id,
                PR_URL,
                &scheduler,
                &scheduler_json,
                expected_version,
            )
            .await
        }
        RecoverySite::ResumeWithoutPr => {
            db.apply_checkpoint_resume_without_pr_at_version(
                task_id,
                Some(PR_URL),
                &scheduler,
                &scheduler_json,
                expected_version,
            )
            .await
        }
        RecoverySite::NoCheckpointFailure => {
            db.apply_no_checkpoint_failure_at_version(
                task_id,
                FAILURE_ERROR,
                &scheduler,
                &scheduler_json,
                expected_version,
            )
            .await
        }
        RecoverySite::TransientFailure => {
            db.apply_transient_failure_at_version(
                task_id,
                FAILURE_ERROR,
                &scheduler,
                &scheduler_json,
                expected_version,
            )
            .await
        }
    }
}

async fn write_equivalent_result(
    db: &TaskDb,
    site: RecoverySite,
    task_id: &str,
) -> anyhow::Result<()> {
    let scheduler = target_scheduler(site);
    let scheduler_json = serde_json::to_string(&scheduler)?;
    let (status, pr_url, error) = match site {
        RecoverySite::TerminalReplay => ("done", Some(PR_URL), None),
        RecoverySite::PrReplay => ("implementing", Some(PR_URL), None),
        RecoverySite::ResumeWithPr | RecoverySite::ResumeWithoutPr => {
            ("pending", Some(PR_URL), None)
        }
        RecoverySite::NoCheckpointFailure | RecoverySite::TransientFailure => {
            ("failed", None, Some(FAILURE_ERROR))
        }
    };
    sqlx::query(
        "UPDATE tasks SET status = $1, pr_url = $2, error = $3, scheduler_state = $4, \
         version = version + 1 WHERE store_key = $5 AND id = $6",
    )
    .bind(status)
    .bind(pr_url)
    .bind(error)
    .bind(scheduler_json)
    .bind(db.store_key())
    .bind(task_id)
    .execute(&db.postgres_pool())
    .await?;
    Ok(())
}

async fn bump_version(db: &TaskDb, task_id: &str) -> anyhow::Result<()> {
    sqlx::query("UPDATE tasks SET version = version + 1 WHERE store_key = $1 AND id = $2")
        .bind(db.store_key())
        .bind(task_id)
        .execute(&db.postgres_pool())
        .await?;
    Ok(())
}

#[test]
fn write_outcomes_are_closed_and_exclusive() {
    assert!(TaskRecoveryWriteOutcome::Applied
        .applied_or_error()
        .expect("applied should succeed"));
    assert!(!TaskRecoveryWriteOutcome::Superseded
        .applied_or_error()
        .expect("superseded should succeed without counting"));

    let error = TaskRecoveryWriteOutcome::Conflict {
        action: "test recovery",
        task_id: "task-1".to_string(),
        expected_version: 3,
        current_version: Some(4),
    }
    .applied_or_error()
    .expect_err("conflict should fail closed");
    assert!(error.to_string().contains("task task-1"));
    assert!(error.to_string().contains("action test recovery"));
    assert!(error.to_string().contains("expected version 3"));
    assert!(error.to_string().contains("current version 4"));
}

#[tokio::test]
async fn all_six_version_guarded_sites_apply_exactly_once() -> anyhow::Result<()> {
    if !database_tests_configured() {
        return Ok(());
    }
    let _guard = crate::test_helpers::acquire_db_state_guard().await;
    let dir = tempfile::tempdir()?;
    let db = TaskDb::open(&dir.path().join("tasks.db")).await?;

    for site in RECOVERY_SITES {
        let task_id = format!("applied-{}", site_name(site));
        db.insert(&seed_task(site, &task_id)).await?;
        assert_eq!(
            apply_site(&db, site, &task_id, 0).await?,
            TaskRecoveryWriteOutcome::Applied,
            "{site:?} should apply"
        );
    }
    Ok(())
}

#[tokio::test]
async fn lost_cas_is_superseded_only_with_authoritative_evidence() -> anyhow::Result<()> {
    if !database_tests_configured() {
        return Ok(());
    }
    let _guard = crate::test_helpers::acquire_db_state_guard().await;
    let dir = tempfile::tempdir()?;
    let db = TaskDb::open(&dir.path().join("tasks.db")).await?;

    for site in RECOVERY_SITES {
        let task_id = format!("superseded-{}", site_name(site));
        db.insert(&seed_task(site, &task_id)).await?;
        write_equivalent_result(&db, site, &task_id).await?;
        assert_eq!(
            apply_site(&db, site, &task_id, 0).await?,
            TaskRecoveryWriteOutcome::Superseded,
            "{site:?} should accept only the authoritative equivalent result"
        );
    }

    let deleted_id = "superseded-deleted";
    db.insert(&seed_task(RecoverySite::PrReplay, deleted_id))
        .await?;
    sqlx::query("DELETE FROM tasks WHERE store_key = $1 AND id = $2")
        .bind(db.store_key())
        .bind(deleted_id)
        .execute(&db.postgres_pool())
        .await?;
    assert_eq!(
        apply_site(&db, RecoverySite::PrReplay, deleted_id, 0).await?,
        TaskRecoveryWriteOutcome::Superseded
    );
    Ok(())
}

#[tokio::test]
async fn contradictory_or_still_eligible_stale_write_is_conflict() -> anyhow::Result<()> {
    if !database_tests_configured() {
        return Ok(());
    }
    let _guard = crate::test_helpers::acquire_db_state_guard().await;
    let dir = tempfile::tempdir()?;
    let db = TaskDb::open(&dir.path().join("tasks.db")).await?;

    for site in RECOVERY_SITES {
        let task_id = format!("conflict-{}", site_name(site));
        db.insert(&seed_task(site, &task_id)).await?;
        bump_version(&db, &task_id).await?;
        let outcome = apply_site(&db, site, &task_id, 0).await?;
        assert!(
            matches!(
                outcome,
                TaskRecoveryWriteOutcome::Conflict {
                    expected_version: 0,
                    current_version: Some(1),
                    ..
                }
            ),
            "{site:?} should conflict while still eligible: {outcome:?}"
        );
    }

    let different_pr_id = "conflict-different-pr";
    db.insert(&seed_task(RecoverySite::PrReplay, different_pr_id))
        .await?;
    sqlx::query(
        "UPDATE tasks SET pr_url = $1, version = version + 1 \
         WHERE store_key = $2 AND id = $3",
    )
    .bind(OTHER_PR_URL)
    .bind(db.store_key())
    .bind(different_pr_id)
    .execute(&db.postgres_pool())
    .await?;
    assert!(matches!(
        apply_site(&db, RecoverySite::PrReplay, different_pr_id, 0).await?,
        TaskRecoveryWriteOutcome::Conflict { .. }
    ));

    let terminal_different_pr_id = "conflict-terminal-different-pr";
    let mut terminal_different_pr =
        seed_task(RecoverySite::TerminalReplay, terminal_different_pr_id);
    terminal_different_pr.pr_url = Some(OTHER_PR_URL.to_string());
    db.insert(&terminal_different_pr).await?;
    assert!(matches!(
        db.apply_replayed_state_outcome(terminal_different_pr_id, Some(PR_URL), Some("done"))
            .await?,
        TaskRecoveryWriteOutcome::Conflict { .. }
    ));

    let different_terminal_id = "conflict-different-terminal";
    db.insert(&seed_task(
        RecoverySite::TerminalReplay,
        different_terminal_id,
    ))
    .await?;
    sqlx::query(
        "UPDATE tasks SET status = 'failed', version = version + 1 \
         WHERE store_key = $1 AND id = $2",
    )
    .bind(db.store_key())
    .bind(different_terminal_id)
    .execute(&db.postgres_pool())
    .await?;
    assert!(matches!(
        apply_site(&db, RecoverySite::TerminalReplay, different_terminal_id, 0).await?,
        TaskRecoveryWriteOutcome::Conflict { .. }
    ));
    Ok(())
}

#[tokio::test]
async fn repeated_recovery_converges_without_rewrite() -> anyhow::Result<()> {
    if !database_tests_configured() {
        return Ok(());
    }
    let _guard = crate::test_helpers::acquire_db_state_guard().await;
    let dir = tempfile::tempdir()?;
    let db = TaskDb::open(&dir.path().join("tasks.db")).await?;
    let task_id = "repeat-terminal";
    db.insert(&seed_task(RecoverySite::TerminalReplay, task_id))
        .await?;

    assert_eq!(
        db.apply_replayed_state_outcome(task_id, Some(PR_URL), Some("done"))
            .await?,
        TaskRecoveryWriteOutcome::Applied
    );
    assert_eq!(
        db.apply_replayed_state_outcome(task_id, Some(PR_URL), Some("done"))
            .await?,
        TaskRecoveryWriteOutcome::Superseded
    );
    let version: i32 =
        sqlx::query_scalar("SELECT version FROM tasks WHERE store_key = $1 AND id = $2")
            .bind(db.store_key())
            .bind(task_id)
            .fetch_one(&db.postgres_pool())
            .await?;
    assert_eq!(version, 1, "superseded replay must not rewrite the row");
    Ok(())
}

#[test]
fn recovery_counts_and_success_logs_require_applied_write() {
    let captured = CapturedSubscriber::default();
    let _subscriber_guard = tracing::subscriber::set_default(captured.clone());
    let mut applied_count = 0;
    for outcome in [
        TaskRecoveryWriteOutcome::Applied,
        TaskRecoveryWriteOutcome::Superseded,
    ] {
        if observe_recovery_outcome("task-count", outcome).expect("closed non-conflict outcome") {
            applied_count += 1;
        }
    }
    assert_eq!(applied_count, 1);

    let error = observe_recovery_outcome(
        "task-count",
        TaskRecoveryWriteOutcome::Conflict {
            action: "test recovery",
            task_id: "task-count".to_string(),
            expected_version: 4,
            current_version: Some(5),
        },
    )
    .expect_err("conflict must abort before aggregate success logging");
    assert!(error.downcast_ref::<TaskRecoveryConflict>().is_some());

    let output = captured.output();
    assert!(
        output.contains("action superseded by authoritative durable state"),
        "superseded outcome should emit bounded non-success evidence: {output}"
    );
    for success_wording in [
        "resumed task",
        "wrote back pr_url",
        "applied replayed state",
        "marked interrupted task",
        "pending mid-transient-retry",
    ] {
        assert!(
            !output.contains(success_wording),
            "non-applied outcomes must not emit success wording {success_wording:?}: {output}"
        );
    }
}
