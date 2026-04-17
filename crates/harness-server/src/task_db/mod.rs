mod artifacts;
mod checkpoints;
mod queries;
mod row;
mod schema;

pub use artifacts::TaskArtifact;
pub use checkpoints::{RecoveryResult, TaskCheckpoint};

use harness_core::db::{open_pool, Migrator};
use schema::TASK_MIGRATIONS;
use sqlx::sqlite::SqlitePool;
use std::collections::HashMap;
use std::path::Path;

use crate::task_runner::TaskState;

pub struct TaskDb {
    pool: SqlitePool,
}

impl TaskDb {
    pub async fn open(path: &Path) -> anyhow::Result<Self> {
        let pool = open_pool(path).await?;
        let db = Self { pool };
        Migrator::new(&db.pool, TASK_MIGRATIONS).run().await?;
        Ok(db)
    }

    pub async fn insert(&self, state: &TaskState) -> anyhow::Result<()> {
        queries::insert(&self.pool, state).await
    }

    pub async fn update(&self, state: &TaskState) -> anyhow::Result<()> {
        queries::update(&self.pool, state).await
    }

    pub async fn get(&self, id: &str) -> anyhow::Result<Option<TaskState>> {
        queries::get(&self.pool, id).await
    }

    pub async fn list(&self) -> anyhow::Result<Vec<TaskState>> {
        queries::list(&self.pool).await
    }

    pub async fn list_by_status(&self, statuses: &[&str]) -> anyhow::Result<Vec<TaskState>> {
        queries::list_by_status(&self.pool, statuses).await
    }

    pub async fn find_active_duplicate(
        &self,
        project: &str,
        external_id: &str,
    ) -> anyhow::Result<Option<String>> {
        queries::find_active_duplicate(&self.pool, project, external_id).await
    }

    pub async fn find_terminal_with_pr(
        &self,
        project: &str,
        external_id: &str,
    ) -> anyhow::Result<Option<(String, String)>> {
        queries::find_terminal_with_pr(&self.pool, project, external_id).await
    }

    pub async fn list_summaries(&self) -> anyhow::Result<Vec<crate::task_runner::TaskSummary>> {
        queries::list_summaries(&self.pool).await
    }

    pub async fn list_id_status(&self) -> anyhow::Result<Vec<(String, String)>> {
        queries::list_id_status(&self.pool).await
    }

    pub async fn list_ids_by_status(&self, statuses: &[&str]) -> anyhow::Result<Vec<String>> {
        queries::list_ids_by_status(&self.pool, statuses).await
    }

    pub async fn get_status_only(&self, id: &str) -> anyhow::Result<Option<String>> {
        queries::get_status_only(&self.pool, id).await
    }

    pub async fn exists_by_id(&self, id: &str) -> anyhow::Result<bool> {
        queries::exists_by_id(&self.pool, id).await
    }

    pub async fn apply_replayed_state(
        &self,
        task_id: &str,
        pr_url: Option<&str>,
        terminal_status: Option<&str>,
    ) -> anyhow::Result<()> {
        queries::apply_replayed_state(&self.pool, task_id, pr_url, terminal_status).await
    }

    pub async fn recover_in_progress(&self) -> anyhow::Result<RecoveryResult> {
        checkpoints::recover_in_progress(&self.pool).await
    }

    pub async fn write_checkpoint(
        &self,
        task_id: &str,
        triage_output: Option<&str>,
        plan_output: Option<&str>,
        pr_url: Option<&str>,
        last_phase: &str,
    ) -> anyhow::Result<()> {
        checkpoints::write_checkpoint(
            &self.pool,
            task_id,
            triage_output,
            plan_output,
            pr_url,
            last_phase,
        )
        .await
    }

    pub async fn load_checkpoint(&self, task_id: &str) -> anyhow::Result<Option<TaskCheckpoint>> {
        checkpoints::load_checkpoint(&self.pool, task_id).await
    }

    pub async fn latest_done_pr_url(&self) -> anyhow::Result<Option<String>> {
        queries::latest_done_pr_url(&self.pool).await
    }

    pub async fn latest_done_pr_url_by_project(
        &self,
        project: &str,
    ) -> anyhow::Result<Option<String>> {
        queries::latest_done_pr_url_by_project(&self.pool, project).await
    }

    pub async fn latest_done_pr_urls_all_projects(
        &self,
    ) -> anyhow::Result<HashMap<String, String>> {
        queries::latest_done_pr_urls_all_projects(&self.pool).await
    }

    pub async fn count_done_failed_by_project(
        &self,
    ) -> anyhow::Result<(u64, u64, Vec<(String, u64, u64)>)> {
        queries::count_done_failed_by_project(&self.pool).await
    }

    pub async fn list_children(&self, parent_id: &str) -> anyhow::Result<Vec<TaskState>> {
        queries::list_children(&self.pool, parent_id).await
    }

    pub async fn insert_artifact(
        &self,
        task_id: &str,
        turn: u32,
        artifact_type: &str,
        content: &str,
    ) -> anyhow::Result<()> {
        artifacts::insert_artifact(&self.pool, task_id, turn, artifact_type, content).await
    }

    pub async fn list_artifacts(&self, task_id: &str) -> anyhow::Result<Vec<TaskArtifact>> {
        artifacts::list_artifacts(&self.pool, task_id).await
    }

    // Expose pool for tests that need direct SQL access
    #[cfg(test)]
    pub(crate) fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}

#[cfg(test)]
mod tests {
    use super::row::TaskRow;
    use super::TaskDb;
    use crate::task_runner::{RoundResult, TaskState, TaskStatus};
    use harness_core::error::TaskDbDecodeError;

    fn build_task_row(rounds: &str, depends_on: &str) -> TaskRow {
        TaskRow {
            id: "task-1".to_string(),
            status: "pending".to_string(),
            turn: 1,
            pr_url: None,
            rounds: rounds.to_string(),
            error: None,
            source: None,
            external_id: None,
            parent_id: None,
            created_at: None,
            repo: None,
            depends_on: depends_on.to_string(),
            project: None,
            priority: 0,
            phase: r#""implement""#.to_string(),
            description: None,
            request_settings: None,
        }
    }

    #[test]
    fn invalid_rounds_json_returns_distinguishable_error() {
        let err = build_task_row("{not-json", "[]")
            .try_into_task_state()
            .expect_err("invalid rounds JSON should return error");
        let decode_error = err
            .downcast_ref::<TaskDbDecodeError>()
            .expect("error should expose task-db decode type");

        assert!(matches!(
            decode_error,
            TaskDbDecodeError::RoundsDeserialize { task_id, .. } if task_id == "task-1"
        ));
    }

    #[test]
    fn invalid_depends_on_json_returns_distinguishable_error() {
        let err = build_task_row("[]", "{not-json")
            .try_into_task_state()
            .expect_err("invalid depends_on JSON should return error");
        let decode_error = err
            .downcast_ref::<TaskDbDecodeError>()
            .expect("error should expose task-db decode type");

        assert!(matches!(
            decode_error,
            TaskDbDecodeError::DependsOnDeserialize { task_id, .. } if task_id == "task-1"
        ));
    }

    #[tokio::test]
    async fn get_distinguishes_missing_task_from_corrupted_rounds() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        assert!(db.get("missing-task").await?.is_none());

        sqlx::query("INSERT INTO tasks (id, status, turn, rounds) VALUES (?, ?, ?, ?)")
            .bind("task-corrupted")
            .bind("pending")
            .bind(1_i64)
            .bind("{not-json")
            .execute(db.pool())
            .await?;

        let err = db
            .get("task-corrupted")
            .await
            .expect_err("corrupted rounds should return an error");
        assert!(err.downcast_ref::<TaskDbDecodeError>().is_some());
        Ok(())
    }

    #[tokio::test]
    async fn get_returns_error_when_depends_on_json_is_corrupted() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        sqlx::query(
            "INSERT INTO tasks (id, status, turn, rounds, depends_on) VALUES (?, ?, ?, ?, ?)",
        )
        .bind("task-corrupted-deps")
        .bind("pending")
        .bind(1_i64)
        .bind("[]")
        .bind("{not-json")
        .execute(db.pool())
        .await?;

        let err = db
            .get("task-corrupted-deps")
            .await
            .expect_err("corrupted depends_on should return an error");
        let decode_error = err
            .downcast_ref::<TaskDbDecodeError>()
            .expect("error should expose task-db decode type");
        assert!(matches!(
            decode_error,
            TaskDbDecodeError::DependsOnDeserialize { task_id, .. } if task_id == "task-corrupted-deps"
        ));
        Ok(())
    }

    fn make_task(id: &str, status: TaskStatus) -> TaskState {
        TaskState {
            id: harness_core::types::TaskId(id.to_string()),
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
            phase: crate::task_runner::TaskPhase::default(),
            triage_output: None,
            plan_output: None,
            request_settings: None,
            repo: None,
        }
    }

    #[tokio::test]
    async fn insert_and_get_roundtrip() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let task = make_task("task-rt", TaskStatus::Pending);
        db.insert(&task).await?;

        let loaded = db
            .get("task-rt")
            .await?
            .expect("inserted task should exist");
        assert_eq!(loaded.id.0, "task-rt");
        assert!(matches!(loaded.status, TaskStatus::Pending));
        assert_eq!(loaded.turn, 0);
        assert!(loaded.pr_url.is_none());
        assert!(loaded.rounds.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn update_persists_status_and_pr_url() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let mut task = make_task("task-upd", TaskStatus::Pending);
        db.insert(&task).await?;

        task.status = TaskStatus::Implementing;
        task.turn = 1;
        task.pr_url = Some("https://github.com/org/repo/pull/42".to_string());
        task.rounds.push(RoundResult {
            turn: 1,
            action: "implement".to_string(),
            result: "created PR".to_string(),
            detail: None,
        });
        db.update(&task).await?;

        let loaded = db
            .get("task-upd")
            .await?
            .expect("updated task should exist");
        assert!(matches!(loaded.status, TaskStatus::Implementing));
        assert_eq!(loaded.turn, 1);
        assert_eq!(
            loaded.pr_url.as_deref(),
            Some("https://github.com/org/repo/pull/42")
        );
        assert_eq!(loaded.rounds.len(), 1);
        assert_eq!(loaded.rounds[0].action, "implement");
        Ok(())
    }

    #[tokio::test]
    async fn list_returns_all_tasks_in_order() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        db.insert(&make_task("task-a", TaskStatus::Pending)).await?;
        db.insert(&make_task("task-b", TaskStatus::Done)).await?;
        db.insert(&make_task("task-c", TaskStatus::Failed)).await?;

        let all = db.list().await?;
        assert_eq!(all.len(), 3);
        Ok(())
    }

    #[tokio::test]
    async fn get_returns_none_for_missing_task() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;
        assert!(db.get("nonexistent").await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn status_roundtrip_all_variants() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let variants = [
            ("s-pending", TaskStatus::Pending),
            ("s-impl", TaskStatus::Implementing),
            ("s-review", TaskStatus::AgentReview),
            ("s-waiting", TaskStatus::Waiting),
            ("s-reviewing", TaskStatus::Reviewing),
            ("s-done", TaskStatus::Done),
            ("s-failed", TaskStatus::Failed),
        ];

        for (id, status) in &variants {
            db.insert(&make_task(id, status.clone())).await?;
        }

        for (id, expected) in &variants {
            let loaded = db.get(id).await?.expect("task should exist");
            assert_eq!(
                std::mem::discriminant(&loaded.status),
                std::mem::discriminant(expected),
                "status mismatch for task {id}"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn task_with_error_persists() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let mut task = make_task("task-err", TaskStatus::Failed);
        task.error = Some("agent panicked".to_string());
        db.insert(&task).await?;

        let loaded = db
            .get("task-err")
            .await?
            .expect("task with error should exist");
        assert_eq!(loaded.error.as_deref(), Some("agent panicked"));
        Ok(())
    }

    #[tokio::test]
    async fn source_and_external_id_roundtrip() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let mut task = make_task("task-meta", TaskStatus::Pending);
        task.source = Some("github".to_string());
        task.external_id = Some("20".to_string());
        task.repo = Some("acme/harness".to_string());
        db.insert(&task).await?;

        let loaded = db.get("task-meta").await?.expect("task should exist");
        assert_eq!(loaded.source.as_deref(), Some("github"));
        assert_eq!(loaded.external_id.as_deref(), Some("20"));
        assert_eq!(loaded.repo.as_deref(), Some("acme/harness"));
        Ok(())
    }

    #[tokio::test]
    async fn survives_reopen() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db_path = tmp.path().join("tasks.db");

        {
            let db = TaskDb::open(&db_path).await?;
            db.insert(&make_task("task-persist", TaskStatus::Done))
                .await?;
        }

        let db = TaskDb::open(&db_path).await?;
        let loaded = db
            .get("task-persist")
            .await?
            .expect("task should survive reopen");
        assert!(matches!(loaded.status, TaskStatus::Done));
        Ok(())
    }

    #[tokio::test]
    async fn list_children_returns_subtasks_by_parent_id() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let parent = make_task("parent-1", TaskStatus::Pending);
        db.insert(&parent).await?;

        let mut child1 = make_task("child-1", TaskStatus::Pending);
        child1.parent_id = Some(harness_core::types::TaskId("parent-1".to_string()));
        db.insert(&child1).await?;

        let mut child2 = make_task("child-2", TaskStatus::Done);
        child2.parent_id = Some(harness_core::types::TaskId("parent-1".to_string()));
        db.insert(&child2).await?;

        // Unrelated task.
        db.insert(&make_task("unrelated", TaskStatus::Pending))
            .await?;

        let children = db.list_children("parent-1").await?;
        assert_eq!(children.len(), 2);
        assert!(children.iter().all(|c| c
            .parent_id
            .as_ref()
            .map(|id| id.0 == "parent-1")
            .unwrap_or(false)));

        let no_children = db.list_children("nonexistent").await?;
        assert!(no_children.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn recover_no_checkpoint_marks_failed() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        db.insert(&make_task("t-pending", TaskStatus::Pending))
            .await?;
        // All four interrupted statuses, no checkpoint → should all fail
        db.insert(&make_task("t-implementing", TaskStatus::Implementing))
            .await?;
        db.insert(&make_task("t-agent-review", TaskStatus::AgentReview))
            .await?;
        db.insert(&make_task("t-reviewing", TaskStatus::Reviewing))
            .await?;
        db.insert(&make_task("t-waiting", TaskStatus::Waiting))
            .await?;
        db.insert(&make_task("t-done", TaskStatus::Done)).await?;
        db.insert(&make_task("t-failed", TaskStatus::Failed))
            .await?;
        db.insert(&make_task("t-cancelled", TaskStatus::Cancelled))
            .await?;

        let result = db.recover_in_progress().await?;
        assert_eq!(
            result.failed, 4,
            "implementing + agent_review + reviewing + waiting without checkpoints should fail"
        );
        assert_eq!(result.resumed, 0);
        assert_eq!(result.transient_failed, 0);

        // pending stays pending, no error set
        let pending = db.get("t-pending").await?.expect("should exist");
        assert!(matches!(pending.status, TaskStatus::Pending));
        assert!(pending.error.is_none());

        // implementing (no checkpoint, no PR) → failed with diagnostic info
        let implementing = db
            .get("t-implementing")
            .await?
            .ok_or_else(|| anyhow::anyhow!("t-implementing should exist"))?;
        assert!(matches!(implementing.status, TaskStatus::Failed));
        let err = implementing.error.as_deref().unwrap_or("");
        assert!(
            err.contains("was: implementing"),
            "error should contain original status"
        );
        assert!(err.contains("round:"), "error should contain round info");

        // agent_review (no checkpoint, no PR) → failed
        let agent_review = db
            .get("t-agent-review")
            .await?
            .ok_or_else(|| anyhow::anyhow!("t-agent-review should exist"))?;
        assert!(matches!(agent_review.status, TaskStatus::Failed));
        assert!(agent_review
            .error
            .as_deref()
            .unwrap_or("")
            .contains("was: agent_review"));

        // reviewing → failed
        let reviewing = db.get("t-reviewing").await?.expect("should exist");
        assert!(matches!(reviewing.status, TaskStatus::Failed));
        assert!(reviewing
            .error
            .as_deref()
            .unwrap_or("")
            .contains("was: reviewing"));

        // waiting → failed
        let waiting = db.get("t-waiting").await?.expect("should exist");
        assert!(matches!(waiting.status, TaskStatus::Failed));
        assert!(waiting
            .error
            .as_deref()
            .unwrap_or("")
            .contains("was: waiting"));

        // terminal states unchanged
        let done = db.get("t-done").await?.expect("should exist");
        assert!(matches!(done.status, TaskStatus::Done));
        let failed = db.get("t-failed").await?.expect("should exist");
        assert!(matches!(failed.status, TaskStatus::Failed));
        let cancelled = db.get("t-cancelled").await?.expect("should exist");
        assert!(matches!(cancelled.status, TaskStatus::Cancelled));

        Ok(())
    }

    #[tokio::test]
    async fn apply_replayed_state_does_not_touch_cancelled() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let mut task = make_task("t-cancelled", TaskStatus::Cancelled);
        task.pr_url = Some("https://github.com/owner/repo/pull/9".to_string());
        db.insert(&task).await?;

        db.apply_replayed_state(
            "t-cancelled",
            Some("https://github.com/owner/repo/pull/10"),
            Some("failed"),
        )
        .await?;

        let loaded = db.get("t-cancelled").await?.expect("should exist");
        assert!(matches!(loaded.status, TaskStatus::Cancelled));
        assert_eq!(
            loaded.pr_url.as_deref(),
            Some("https://github.com/owner/repo/pull/9")
        );
        Ok(())
    }

    #[tokio::test]
    async fn count_done_failed_by_project_excludes_cancelled() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let root = "/projects/alpha";
        let mut done = make_task("done", TaskStatus::Done);
        done.project_root = Some(std::path::PathBuf::from(root));
        db.insert(&done).await?;

        let mut failed = make_task("failed", TaskStatus::Failed);
        failed.project_root = Some(std::path::PathBuf::from(root));
        db.insert(&failed).await?;

        let mut cancelled = make_task("cancelled", TaskStatus::Cancelled);
        cancelled.project_root = Some(std::path::PathBuf::from(root));
        db.insert(&cancelled).await?;

        let (global_done, global_failed, by_project) = db.count_done_failed_by_project().await?;
        assert_eq!(global_done, 1);
        assert_eq!(global_failed, 1);
        assert_eq!(by_project, vec![(root.to_string(), 1, 1)]);
        Ok(())
    }

    #[tokio::test]
    async fn recover_with_tasks_pr_url_resumes_pending() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        // implementing WITH tasks.pr_url → resumed (not failed)
        let mut with_pr = make_task("t-impl-pr", TaskStatus::Implementing);
        with_pr.pr_url = Some("https://github.com/owner/repo/pull/42".to_string());
        db.insert(&with_pr).await?;

        // agent_review WITH tasks.pr_url → resumed
        let mut review_pr = make_task("t-review-pr", TaskStatus::AgentReview);
        review_pr.pr_url = Some("https://github.com/owner/repo/pull/43".to_string());
        db.insert(&review_pr).await?;

        // implementing WITHOUT PR and no checkpoint → still failed
        db.insert(&make_task("t-impl-no-pr", TaskStatus::Implementing))
            .await?;

        let result = db.recover_in_progress().await?;
        assert_eq!(result.resumed, 2, "tasks with pr_url should be resumed");
        assert_eq!(
            result.failed, 1,
            "task without pr_url or checkpoint should fail"
        );
        assert_eq!(result.transient_failed, 0);

        // Verify: implementing with PR → pending (resumed)
        let impl_pr = db
            .get("t-impl-pr")
            .await?
            .ok_or_else(|| anyhow::anyhow!("t-impl-pr should exist"))?;
        assert!(
            matches!(impl_pr.status, TaskStatus::Pending),
            "implementing with PR should be resumed to pending"
        );
        assert!(
            impl_pr.pr_url.as_deref() == Some("https://github.com/owner/repo/pull/42"),
            "pr_url should be preserved"
        );
        let err = impl_pr.error.as_deref().unwrap_or("");
        assert!(
            err.contains("resumed after restart"),
            "error should note resumption"
        );
        assert!(err.contains("pull/42"), "should reference PR URL");

        // Verify: agent_review with PR → pending (resumed)
        let review = db
            .get("t-review-pr")
            .await?
            .ok_or_else(|| anyhow::anyhow!("t-review-pr should exist"))?;
        assert!(
            matches!(review.status, TaskStatus::Pending),
            "agent_review with PR should be resumed to pending"
        );

        // Verify: implementing without PR → failed
        let no_pr = db
            .get("t-impl-no-pr")
            .await?
            .ok_or_else(|| anyhow::anyhow!("t-impl-no-pr should exist"))?;
        assert!(matches!(no_pr.status, TaskStatus::Failed));

        Ok(())
    }

    #[tokio::test]
    async fn recover_with_plan_checkpoint_resumes_pending() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        // Task interrupted at implementing stage, plan was completed and checkpointed
        db.insert(&make_task("t-impl-plan", TaskStatus::Implementing))
            .await?;
        db.write_checkpoint(
            "t-impl-plan",
            None,
            Some("## Plan\nStep 1: do X"),
            None,
            "plan_done",
        )
        .await?;

        let result = db.recover_in_progress().await?;
        assert_eq!(result.resumed, 1);
        assert_eq!(result.failed, 0);

        let task = db
            .get("t-impl-plan")
            .await?
            .ok_or_else(|| anyhow::anyhow!("t-impl-plan should exist"))?;
        assert!(matches!(task.status, TaskStatus::Pending));
        assert!(
            task.error
                .as_deref()
                .unwrap_or("")
                .contains("plan checkpoint"),
            "error should mention plan checkpoint"
        );

        Ok(())
    }

    #[tokio::test]
    async fn checkpoint_write_and_load_roundtrip() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        // No checkpoint yet → None
        assert!(db.load_checkpoint("task-1").await?.is_none());

        // Write triage checkpoint
        db.write_checkpoint(
            "task-1",
            Some("triage output here"),
            None,
            None,
            "triage_done",
        )
        .await?;

        let ck = db
            .load_checkpoint("task-1")
            .await?
            .ok_or_else(|| anyhow::anyhow!("checkpoint should exist"))?;
        assert_eq!(ck.task_id, "task-1");
        assert_eq!(ck.triage_output.as_deref(), Some("triage output here"));
        assert!(ck.plan_output.is_none());
        assert!(ck.pr_url.is_none());
        assert_eq!(ck.last_phase, "triage_done");

        // Advance to plan checkpoint — triage_output preserved via COALESCE
        db.write_checkpoint("task-1", None, Some("plan text"), None, "plan_done")
            .await?;

        let ck = db
            .load_checkpoint("task-1")
            .await?
            .ok_or_else(|| anyhow::anyhow!("checkpoint should exist after plan write"))?;
        assert_eq!(
            ck.triage_output.as_deref(),
            Some("triage output here"),
            "triage preserved"
        );
        assert_eq!(ck.plan_output.as_deref(), Some("plan text"));
        assert!(ck.pr_url.is_none());
        assert_eq!(ck.last_phase, "plan_done");

        // Advance to pr_created — all previous fields preserved
        db.write_checkpoint(
            "task-1",
            None,
            None,
            Some("https://github.com/o/r/pull/5"),
            "pr_created",
        )
        .await?;

        let ck = db
            .load_checkpoint("task-1")
            .await?
            .ok_or_else(|| anyhow::anyhow!("checkpoint should exist after pr write"))?;
        assert_eq!(
            ck.triage_output.as_deref(),
            Some("triage output here"),
            "triage preserved"
        );
        assert_eq!(
            ck.plan_output.as_deref(),
            Some("plan text"),
            "plan preserved"
        );
        assert_eq!(ck.pr_url.as_deref(), Some("https://github.com/o/r/pull/5"));
        assert_eq!(ck.last_phase, "pr_created");

        Ok(())
    }

    #[tokio::test]
    async fn checkpoint_upsert_replaces_last_phase() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        db.write_checkpoint("task-x", Some("triage"), None, None, "triage_done")
            .await?;
        db.write_checkpoint("task-x", None, Some("plan"), None, "plan_done")
            .await?;

        let ck = db
            .load_checkpoint("task-x")
            .await?
            .ok_or_else(|| anyhow::anyhow!("checkpoint for task-x should exist"))?;
        assert_eq!(
            ck.last_phase, "plan_done",
            "last_phase should advance to plan_done"
        );
        assert_eq!(
            ck.triage_output.as_deref(),
            Some("triage"),
            "triage preserved"
        );
        assert_eq!(ck.plan_output.as_deref(), Some("plan"));

        Ok(())
    }

    #[tokio::test]
    async fn recover_result_separates_resumed_from_failed() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        // Will be resumed: has a plan checkpoint
        db.insert(&make_task("t-resumable", TaskStatus::Implementing))
            .await?;
        db.write_checkpoint("t-resumable", None, Some("plan output"), None, "plan_done")
            .await?;

        // Will be failed: no checkpoint, no PR
        db.insert(&make_task("t-no-checkpoint", TaskStatus::Reviewing))
            .await?;

        // Will be resumed: has PR in tasks table
        let mut with_pr = make_task("t-has-pr", TaskStatus::AgentReview);
        with_pr.pr_url = Some("https://github.com/o/r/pull/99".to_string());
        db.insert(&with_pr).await?;

        let result = db.recover_in_progress().await?;
        assert_eq!(
            result.resumed, 2,
            "t-resumable and t-has-pr should be resumed"
        );
        assert_eq!(result.failed, 1, "t-no-checkpoint should fail");
        assert_eq!(result.transient_failed, 0);

        Ok(())
    }

    #[tokio::test]
    async fn recover_corrupted_pr_url_goes_to_failed() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        // Corrupted: pr_url is Some("") — is_some() passes but parse fails
        let mut empty_url = make_task("t-empty-url", TaskStatus::Reviewing);
        empty_url.pr_url = Some(String::new());
        db.insert(&empty_url).await?;

        // Corrupted: pr_url is garbage with no pull number
        let mut garbage_url = make_task("t-garbage-url", TaskStatus::Implementing);
        garbage_url.pr_url = Some("not-a-pr-url".to_string());
        db.insert(&garbage_url).await?;

        let result = db.recover_in_progress().await?;
        assert_eq!(result.resumed, 0, "corrupted URLs must not be resumed");
        assert_eq!(result.failed, 2, "both corrupted-URL tasks must fail");

        Ok(())
    }

    #[tokio::test]
    async fn task_db_rejects_unknown_status() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db_path = tmp.path().join("tasks.db");
        let db = TaskDb::open(&db_path).await?;

        let task = make_task("task-unknown", TaskStatus::Pending);
        db.insert(&task).await?;

        sqlx::query("UPDATE tasks SET status = ? WHERE id = ?")
            .bind("unknown_status")
            .bind("task-unknown")
            .execute(db.pool())
            .await?;

        let err = db
            .get("task-unknown")
            .await
            .expect_err("unknown status must return an explicit error");
        let message = format!("{err:#}");
        assert!(message.contains("unknown task status"));
        Ok(())
    }

    /// Guard: TASK_ROW_COLUMNS must list exactly the same fields as TaskRow.
    /// If a field is added to TaskRow but not to the constant, this test fails
    /// at compile time (struct construction) or at runtime (count mismatch).
    #[test]
    fn task_row_columns_match_struct() {
        let columns: Vec<&str> = super::schema::TASK_ROW_COLUMNS.split(", ").collect();

        // Build a TaskRow using every column name — compile error if a field is missing.
        let _row = TaskRow {
            id: String::new(),
            status: String::new(),
            turn: 0,
            pr_url: None,
            rounds: String::new(),
            error: None,
            source: None,
            external_id: None,
            parent_id: None,
            created_at: None,
            repo: None,
            depends_on: String::new(),
            project: None,
            priority: 0,
            phase: String::new(),
            description: None,
            request_settings: None,
        };

        // Count must match — catches column added to constant but not struct (or vice versa).
        assert_eq!(
            columns.len(),
            17, // bump this when adding a field
            "TASK_ROW_COLUMNS has {} entries but expected 17 — update both the constant and this test",
            columns.len()
        );
    }

    /// Regression: list_by_status must map to TaskRow correctly (including priority).
    /// This was the exact crash path: startup calls list_by_status to load active tasks.
    #[tokio::test]
    async fn list_by_status_returns_matching_tasks() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let mut task = make_task("task-pending", TaskStatus::Pending);
        task.priority = 2;
        db.insert(&task).await?;
        db.insert(&make_task("task-done", TaskStatus::Done)).await?;

        let pending = db.list_by_status(&["pending"]).await?;
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id.0, "task-pending");
        assert_eq!(
            pending[0].priority, 2,
            "priority must survive list_by_status roundtrip"
        );

        let multi = db.list_by_status(&["pending", "done"]).await?;
        assert_eq!(multi.len(), 2);

        let empty = db.list_by_status(&[]).await?;
        assert!(empty.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn find_active_duplicate_returns_pending_task() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;
        let mut task = make_task("task-issue-42", TaskStatus::Pending);
        task.external_id = Some("issue:42".to_string());
        task.project_root = Some(std::path::PathBuf::from("/repo/foo"));
        db.insert(&task).await?;
        let dup = db.find_active_duplicate("/repo/foo", "issue:42").await?;
        assert_eq!(dup, Some("task-issue-42".to_string()));
        Ok(())
    }

    #[tokio::test]
    async fn find_active_duplicate_returns_implementing_task() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;
        let mut task = make_task("task-impl-43", TaskStatus::Implementing);
        task.external_id = Some("issue:43".to_string());
        task.project_root = Some(std::path::PathBuf::from("/repo/foo"));
        db.insert(&task).await?;
        let dup = db.find_active_duplicate("/repo/foo", "issue:43").await?;
        assert_eq!(dup, Some("task-impl-43".to_string()));
        Ok(())
    }

    #[tokio::test]
    async fn find_active_duplicate_ignores_terminal_tasks() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;
        let mut task = make_task("task-done-42", TaskStatus::Done);
        task.external_id = Some("issue:42".to_string());
        task.project_root = Some(std::path::PathBuf::from("/repo/foo"));
        db.insert(&task).await?;
        let dup = db.find_active_duplicate("/repo/foo", "issue:42").await?;
        assert_eq!(dup, None);
        Ok(())
    }

    #[tokio::test]
    async fn find_active_duplicate_different_project_no_match() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;
        let mut task = make_task("task-proj-a", TaskStatus::Pending);
        task.external_id = Some("issue:42".to_string());
        task.project_root = Some(std::path::PathBuf::from("/repo/a"));
        db.insert(&task).await?;
        let dup = db.find_active_duplicate("/repo/b", "issue:42").await?;
        assert_eq!(dup, None);
        Ok(())
    }

    #[tokio::test]
    async fn find_active_duplicate_different_external_id_no_match() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;
        let mut task = make_task("task-issue-42", TaskStatus::Pending);
        task.external_id = Some("issue:42".to_string());
        task.project_root = Some(std::path::PathBuf::from("/repo/foo"));
        db.insert(&task).await?;
        let dup = db.find_active_duplicate("/repo/foo", "issue:99").await?;
        assert_eq!(dup, None);
        Ok(())
    }

    #[tokio::test]
    async fn phase_round_trips_through_db() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let mut task = make_task("task-phase-rt", TaskStatus::Implementing);
        task.phase = crate::task_runner::TaskPhase::Review;
        db.insert(&task).await?;

        let loaded = db
            .get("task-phase-rt")
            .await?
            .expect("inserted task should exist");
        assert_eq!(loaded.phase, crate::task_runner::TaskPhase::Review);
        Ok(())
    }

    #[tokio::test]
    async fn description_round_trips_through_db() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let mut task = make_task("task-desc-rt", TaskStatus::Pending);
        task.description = Some("fix: crash on empty input".to_string());
        db.insert(&task).await?;

        let loaded = db
            .get("task-desc-rt")
            .await?
            .expect("inserted task should exist");
        assert_eq!(
            loaded.description.as_deref(),
            Some("fix: crash on empty input")
        );
        Ok(())
    }

    #[tokio::test]
    async fn list_summaries_includes_phase_and_description() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let mut task = make_task("task-summary-pd", TaskStatus::Pending);
        task.phase = crate::task_runner::TaskPhase::Plan;
        task.description = Some("architect PR #5".to_string());
        db.insert(&task).await?;

        let summaries = db.list_summaries().await?;
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].phase, crate::task_runner::TaskPhase::Plan);
        assert_eq!(summaries[0].description.as_deref(), Some("architect PR #5"));
        Ok(())
    }

    #[tokio::test]
    async fn phase_defaults_for_legacy_rows() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        // Insert a row without supplying phase/description — SQLite uses column defaults.
        sqlx::query(
            "INSERT INTO tasks (id, status, turn, rounds, depends_on) VALUES (?, ?, ?, ?, ?)",
        )
        .bind("task-legacy")
        .bind("pending")
        .bind(0_i64)
        .bind("[]")
        .bind("[]")
        .execute(db.pool())
        .await?;

        let loaded = db
            .get("task-legacy")
            .await?
            .expect("legacy task should exist");
        assert_eq!(
            loaded.phase,
            crate::task_runner::TaskPhase::default(),
            "phase should default to Implement for legacy rows"
        );
        assert!(
            loaded.description.is_none(),
            "description should be None for legacy rows"
        );
        Ok(())
    }
}
