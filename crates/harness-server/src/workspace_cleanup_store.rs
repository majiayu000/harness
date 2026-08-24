use super::{WorkspaceLeaseRecord, WorkspaceLeaseRow, WorkspaceLeaseStore};
use crate::task_runner::TaskId;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkspaceCleanupTargetRecord {
    pub(crate) project_key: String,
    pub(crate) slot_index: u32,
    pub(crate) task_id: TaskId,
    pub(crate) workspace_key: String,
    pub(crate) workspace_path: PathBuf,
    pub(crate) source_repo: PathBuf,
    pub(crate) repo: Option<String>,
    pub(crate) runtime_workflow_id: String,
    pub(crate) owner_session: String,
    pub(crate) run_generation: u32,
    pub(crate) process_id: u32,
    pub(crate) process_started_at: u64,
}

impl WorkspaceLeaseStore {
    pub(crate) async fn workspace_cleanup_targets_for_runtime_workflow(
        &self,
        runtime_workflow_id: &str,
    ) -> anyhow::Result<Vec<WorkspaceCleanupTargetRecord>> {
        let rows = sqlx::query_as::<_, WorkspaceCleanupTargetRow>(
            "SELECT project_key, slot_index, task_id, workspace_key, workspace_path, source_repo,
                    repo, runtime_workflow_id, owner_session, run_generation, process_id,
                    process_started_at
             FROM workspace_cleanup_targets
             WHERE store_key = $1
               AND runtime_workflow_id = $2
             ORDER BY last_used_at ASC, project_key, slot_index",
        )
        .bind(&self.store_key)
        .bind(runtime_workflow_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(TryInto::try_into).collect()
    }

    pub(crate) async fn current_workspace_lease_for_slot(
        &self,
        project_key: &str,
        slot_index: u32,
    ) -> anyhow::Result<Option<WorkspaceLeaseRecord>> {
        let row = sqlx::query_as::<_, WorkspaceLeaseRow>(
            "SELECT project_key, slot_index, task_id, workspace_key, workspace_path, source_repo,
                    repo, runtime_workflow_id, owner_session, run_generation, process_id,
                    process_started_at
             FROM workspace_leases
             WHERE store_key = $1
               AND project_key = $2
               AND slot_index = $3",
        )
        .bind(&self.store_key)
        .bind(project_key)
        .bind(slot_index as i64)
        .fetch_optional(&self.pool)
        .await?;
        row.map(TryInto::try_into).transpose()
    }

    pub(crate) async fn complete_workspace_cleanup_target(
        &self,
        target: &WorkspaceCleanupTargetRecord,
    ) -> anyhow::Result<()> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query(
            "DELETE FROM workspace_cleanup_targets
             WHERE store_key = $1
               AND runtime_workflow_id = $2
               AND workspace_path = $3",
        )
        .bind(&self.store_key)
        .bind(&target.runtime_workflow_id)
        .bind(target.workspace_path.to_string_lossy().as_ref())
        .execute(&mut *transaction)
        .await?;
        sqlx::query(
            "DELETE FROM workspace_leases
             WHERE store_key = $1
               AND project_key = $2
               AND slot_index = $3
               AND task_id = $4
               AND workspace_path = $5
               AND runtime_workflow_id = $6
               AND owner_session = $7
               AND run_generation = $8
               AND process_id = $9
               AND process_started_at = $10",
        )
        .bind(&self.store_key)
        .bind(&target.project_key)
        .bind(target.slot_index as i64)
        .bind(target.task_id.as_str())
        .bind(target.workspace_path.to_string_lossy().as_ref())
        .bind(&target.runtime_workflow_id)
        .bind(&target.owner_session)
        .bind(target.run_generation as i64)
        .bind(target.process_id as i64)
        .bind(i64::try_from(target.process_started_at)?)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(())
    }
}

#[derive(sqlx::FromRow)]
struct WorkspaceCleanupTargetRow {
    project_key: String,
    slot_index: i64,
    task_id: String,
    workspace_key: String,
    workspace_path: String,
    source_repo: String,
    repo: Option<String>,
    runtime_workflow_id: String,
    owner_session: String,
    run_generation: i64,
    process_id: i64,
    process_started_at: i64,
}

impl TryFrom<WorkspaceCleanupTargetRow> for WorkspaceCleanupTargetRecord {
    type Error = anyhow::Error;

    fn try_from(row: WorkspaceCleanupTargetRow) -> Result<Self, Self::Error> {
        Ok(Self {
            project_key: row.project_key,
            slot_index: u32::try_from(row.slot_index)?,
            task_id: TaskId::from_str(&row.task_id),
            workspace_key: row.workspace_key,
            workspace_path: PathBuf::from(row.workspace_path),
            source_repo: PathBuf::from(row.source_repo),
            repo: row.repo,
            runtime_workflow_id: row.runtime_workflow_id,
            owner_session: row.owner_session,
            run_generation: u32::try_from(row.run_generation)?,
            process_id: u32::try_from(row.process_id)?,
            process_started_at: u64::try_from(row.process_started_at)?,
        })
    }
}

pub(crate) const WORKSPACE_CLEANUP_TARGETS_TABLE_SQL: &str =
    "CREATE TABLE IF NOT EXISTS workspace_cleanup_targets (
    store_key           TEXT NOT NULL DEFAULT current_schema(),
    runtime_workflow_id TEXT NOT NULL,
    workspace_path      TEXT NOT NULL,
    task_id             TEXT NOT NULL,
    project_key         TEXT NOT NULL,
    slot_index          BIGINT NOT NULL,
    workspace_key       TEXT NOT NULL,
    source_repo         TEXT NOT NULL,
    repo                TEXT,
    owner_session       TEXT NOT NULL,
    run_generation      BIGINT NOT NULL,
    process_id          BIGINT NOT NULL,
    process_started_at  BIGINT NOT NULL DEFAULT 0,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    last_used_at        TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY(store_key, runtime_workflow_id, workspace_path)
);
CREATE INDEX IF NOT EXISTS idx_workspace_cleanup_targets_workflow
    ON workspace_cleanup_targets(store_key, runtime_workflow_id, last_used_at);
INSERT INTO workspace_cleanup_targets (
    store_key, runtime_workflow_id, workspace_path, task_id, project_key, slot_index,
    workspace_key, source_repo, repo, owner_session, run_generation, process_id,
    process_started_at, created_at, last_used_at
)
SELECT store_key, runtime_workflow_id, workspace_path, task_id, project_key, slot_index,
       workspace_key, source_repo, repo, owner_session, run_generation, process_id,
       process_started_at, acquired_at, last_used_at
FROM workspace_leases
WHERE runtime_workflow_id IS NOT NULL
ON CONFLICT(store_key, runtime_workflow_id, workspace_path) DO NOTHING";
