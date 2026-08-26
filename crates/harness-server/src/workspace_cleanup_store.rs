use super::{WorkspaceLeaseRecord, WorkspaceLeaseRow, WorkspaceLeaseStore};
use crate::task_runner::TaskId;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[path = "workspace_cleanup_store/claim.rs"]
mod claim;

const CLEANUP_CLAIM_TTL_SECS: i64 = 30;
const CLEANUP_CLAIM_ABANDON_RETRY_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkspaceCleanupHook {
    Workflow,
    Manager,
}

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
    pub(crate) acquisition_id: Option<String>,
    pub(crate) process_id: u32,
    pub(crate) process_started_at: u64,
}

pub(crate) struct PersistedWorkspaceCleanupClaim {
    store: Arc<WorkspaceLeaseStore>,
    record: WorkspaceCleanupTargetRecord,
    claim_id: String,
    cleanup_owner_session: String,
    heartbeat_task: Option<tokio::task::JoinHandle<()>>,
    claim_lost: tokio::sync::watch::Receiver<bool>,
    armed: bool,
}

impl PersistedWorkspaceCleanupClaim {
    pub(crate) async fn claim(
        store: Arc<WorkspaceLeaseStore>,
        record: WorkspaceCleanupTargetRecord,
        claim_id: String,
        cleanup_owner_session: String,
        cleanup_process_id: u32,
        cleanup_process_started_at: u64,
    ) -> anyhow::Result<Option<Self>> {
        if !store
            .claim_workspace_cleanup_target(
                &record,
                &claim_id,
                &cleanup_owner_session,
                cleanup_process_id,
                cleanup_process_started_at,
            )
            .await?
        {
            return Ok(None);
        }

        let (claim_lost_tx, claim_lost) = tokio::sync::watch::channel(false);
        let heartbeat_store = store.clone();
        let heartbeat_record = record.clone();
        let heartbeat_claim_id = claim_id.clone();
        let heartbeat_owner_session = cleanup_owner_session.clone();
        let claim_ttl = WorkspaceLeaseStore::workspace_cleanup_claim_ttl();
        let heartbeat_task = tokio::spawn(async move {
            let heartbeat_interval = claim_ttl / 3;
            let mut expiry_deadline = tokio::time::Instant::now() + claim_ttl;
            loop {
                tokio::time::sleep(heartbeat_interval).await;
                match heartbeat_store
                    .renew_workspace_cleanup_claim(
                        &heartbeat_record,
                        &heartbeat_claim_id,
                        &heartbeat_owner_session,
                    )
                    .await
                {
                    Ok(true) => expiry_deadline = tokio::time::Instant::now() + claim_ttl,
                    Ok(false) => {
                        tracing::error!(
                            workspace_path = %heartbeat_record.workspace_path.display(),
                            cleanup_claim_id = %heartbeat_claim_id,
                            "workspace cleanup claim was replaced while cleanup was active"
                        );
                        if claim_lost_tx.send(true).is_err() {
                            tracing::debug!(
                                cleanup_claim_id = %heartbeat_claim_id,
                                "workspace cleanup claim loss receiver was already dropped"
                            );
                        }
                        return;
                    }
                    Err(error) if tokio::time::Instant::now() < expiry_deadline => {
                        tracing::warn!(
                            workspace_path = %heartbeat_record.workspace_path.display(),
                            cleanup_claim_id = %heartbeat_claim_id,
                            "workspace cleanup claim heartbeat failed; retrying before expiry: {error}"
                        );
                    }
                    Err(error) => {
                        tracing::error!(
                            workspace_path = %heartbeat_record.workspace_path.display(),
                            cleanup_claim_id = %heartbeat_claim_id,
                            "workspace cleanup claim heartbeat expired: {error}"
                        );
                        if claim_lost_tx.send(true).is_err() {
                            tracing::debug!(
                                cleanup_claim_id = %heartbeat_claim_id,
                                "workspace cleanup claim loss receiver was already dropped"
                            );
                        }
                        return;
                    }
                }
            }
        });

        Ok(Some(Self {
            store,
            record,
            claim_id,
            cleanup_owner_session,
            heartbeat_task: Some(heartbeat_task),
            claim_lost,
            armed: true,
        }))
    }

    pub(crate) fn loss_receiver(&self) -> tokio::sync::watch::Receiver<bool> {
        self.claim_lost.clone()
    }

    pub(crate) async fn complete(mut self) -> anyhow::Result<()> {
        if let Some(heartbeat_task) = self.heartbeat_task.take() {
            heartbeat_task.abort();
        }
        self.store
            .complete_claimed_workspace_cleanup_target(
                &self.record,
                &self.claim_id,
                &self.cleanup_owner_session,
            )
            .await?;
        self.armed = false;
        Ok(())
    }
}

impl Drop for PersistedWorkspaceCleanupClaim {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Some(heartbeat_task) = self.heartbeat_task.take() {
            heartbeat_task.abort();
        }
        let store = self.store.clone();
        let record = self.record.clone();
        let claim_id = self.claim_id.clone();
        let cleanup_owner_session = self.cleanup_owner_session.clone();
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            tracing::error!(
                workspace_path = %record.workspace_path.display(),
                cleanup_claim_id = %claim_id,
                "no Tokio runtime was available to abandon cancelled workspace cleanup claim; the durable claim will expire"
            );
            return;
        };
        runtime.spawn(async move {
            let deadline = tokio::time::Instant::now()
                + WorkspaceLeaseStore::workspace_cleanup_claim_ttl();
            loop {
                match store
                    .abandon_workspace_cleanup_claim(
                        &record,
                        &claim_id,
                        &cleanup_owner_session,
                    )
                    .await
                {
                    Ok(true) => return,
                    Ok(false) => {
                        tracing::warn!(
                            workspace_path = %record.workspace_path.display(),
                            cleanup_claim_id = %claim_id,
                            "cancelled workspace cleanup claim was already released or replaced"
                        );
                        return;
                    }
                    Err(error) if tokio::time::Instant::now() < deadline => {
                        tracing::warn!(
                            workspace_path = %record.workspace_path.display(),
                            cleanup_claim_id = %claim_id,
                            "failed to abandon cancelled workspace cleanup claim; retrying: {error}"
                        );
                        tokio::time::sleep(CLEANUP_CLAIM_ABANDON_RETRY_INTERVAL).await;
                    }
                    Err(error) => {
                        tracing::error!(
                            workspace_path = %record.workspace_path.display(),
                            cleanup_claim_id = %claim_id,
                            "failed to abandon cancelled workspace cleanup claim before expiry; the durable claim will now expire: {error}"
                        );
                        return;
                    }
                }
            }
        });
    }
}

impl WorkspaceLeaseStore {
    pub(crate) async fn claim_workspace_cleanup_hook(
        &self,
        runtime_workflow_id: &str,
        workspace_path: &std::path::Path,
        hook: WorkspaceCleanupHook,
    ) -> anyhow::Result<Option<bool>> {
        let query = match hook {
            WorkspaceCleanupHook::Workflow => {
                "UPDATE workspace_cleanup_targets_v2
                 SET workflow_hook_claimed = TRUE, last_used_at = CURRENT_TIMESTAMP
                 WHERE store_key = $1 AND runtime_workflow_id = $2 AND workspace_path = $3
                   AND workflow_hook_claimed = FALSE"
            }
            WorkspaceCleanupHook::Manager => {
                "UPDATE workspace_cleanup_targets_v2
                 SET manager_hook_claimed = TRUE, last_used_at = CURRENT_TIMESTAMP
                 WHERE store_key = $1 AND runtime_workflow_id = $2 AND workspace_path = $3
                   AND manager_hook_claimed = FALSE"
            }
        };
        let claimed = sqlx::query(query)
            .bind(&self.store_key)
            .bind(runtime_workflow_id)
            .bind(workspace_path.to_string_lossy().as_ref())
            .execute(&self.pool)
            .await?
            .rows_affected()
            > 0;
        if claimed {
            return Ok(Some(true));
        }
        let target_exists = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(
                SELECT 1 FROM workspace_cleanup_targets_v2
                WHERE store_key = $1 AND runtime_workflow_id = $2 AND workspace_path = $3
             )",
        )
        .bind(&self.store_key)
        .bind(runtime_workflow_id)
        .bind(workspace_path.to_string_lossy().as_ref())
        .fetch_one(&self.pool)
        .await?;
        Ok(target_exists.then_some(false))
    }

    pub(crate) async fn complete_owned_workspace(
        &self,
        project_key: &str,
        slot_index: u32,
        task_id: &TaskId,
        owner_session: &str,
        run_generation: u32,
        acquisition_id: &str,
    ) -> anyhow::Result<()> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query(
            "DELETE FROM workspace_cleanup_targets_v2
             WHERE store_key = $1 AND project_key = $2 AND slot_index = $3
               AND task_id = $4 AND owner_session = $5 AND run_generation = $6
               AND acquisition_id = $7",
        )
        .bind(&self.store_key)
        .bind(project_key)
        .bind(slot_index as i64)
        .bind(task_id.as_str())
        .bind(owner_session)
        .bind(run_generation as i64)
        .bind(acquisition_id)
        .execute(&mut *transaction)
        .await?;
        let released = sqlx::query(
            "UPDATE workspace_leases SET state = 'released', released_at = CURRENT_TIMESTAMP,
                    last_used_at = CURRENT_TIMESTAMP
             WHERE store_key = $1 AND project_key = $2 AND slot_index = $3
               AND task_id = $4 AND owner_session = $5 AND run_generation = $6
               AND acquisition_id = $7 AND state = 'leased'",
        )
        .bind(&self.store_key)
        .bind(project_key)
        .bind(slot_index as i64)
        .bind(task_id.as_str())
        .bind(owner_session)
        .bind(run_generation as i64)
        .bind(acquisition_id)
        .execute(&mut *transaction)
        .await?;
        if released.rows_affected() == 0 {
            let already_released = sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(
                    SELECT 1 FROM workspace_leases
                    WHERE store_key = $1 AND project_key = $2 AND slot_index = $3
                      AND task_id = $4 AND owner_session = $5 AND run_generation = $6
                      AND acquisition_id = $7 AND state = 'released'
                )",
            )
            .bind(&self.store_key)
            .bind(project_key)
            .bind(slot_index as i64)
            .bind(task_id.as_str())
            .bind(owner_session)
            .bind(run_generation as i64)
            .bind(acquisition_id)
            .fetch_one(&mut *transaction)
            .await?;
            if !already_released {
                anyhow::bail!("workspace acquisition changed before durable completion");
            }
        }
        transaction.commit().await?;
        Ok(())
    }

    pub(crate) async fn runtime_workspace_cleanup_workflow_ids_after(
        &self,
        after: Option<&str>,
        limit: i64,
    ) -> anyhow::Result<Vec<String>> {
        Ok(sqlx::query_scalar(
            "SELECT DISTINCT runtime_workflow_id
             FROM workspace_cleanup_targets_v2
             WHERE store_key = $1
               AND ($2::TEXT IS NULL OR runtime_workflow_id > $2)
             ORDER BY runtime_workflow_id
             LIMIT $3",
        )
        .bind(&self.store_key)
        .bind(after)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    pub(crate) async fn workspace_cleanup_targets_for_runtime_workflow(
        &self,
        runtime_workflow_id: &str,
    ) -> anyhow::Result<Vec<WorkspaceCleanupTargetRecord>> {
        let rows = sqlx::query_as::<_, WorkspaceCleanupTargetRow>(
            "SELECT project_key, slot_index, task_id, workspace_key, workspace_path, source_repo,
                    repo, runtime_workflow_id, owner_session, run_generation, acquisition_id, process_id,
                    process_started_at
             FROM workspace_cleanup_targets_v2
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
                    repo, runtime_workflow_id, owner_session, run_generation, acquisition_id, process_id,
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
            "DELETE FROM workspace_cleanup_targets_v2
             WHERE store_key = $1
               AND runtime_workflow_id = $2
               AND workspace_path = $3
               AND task_id = $4
               AND project_key = $5
               AND slot_index = $6
               AND owner_session = $7
               AND run_generation = $8
               AND acquisition_id IS NOT DISTINCT FROM $9
               AND process_id = $10
               AND process_started_at = $11
               AND cleanup_claim_id IS NULL",
        )
        .bind(&self.store_key)
        .bind(&target.runtime_workflow_id)
        .bind(target.workspace_path.to_string_lossy().as_ref())
        .bind(target.task_id.as_str())
        .bind(&target.project_key)
        .bind(target.slot_index as i64)
        .bind(&target.owner_session)
        .bind(target.run_generation as i64)
        .bind(target.acquisition_id.as_deref())
        .bind(target.process_id as i64)
        .bind(i64::try_from(target.process_started_at)?)
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
               AND acquisition_id IS NOT DISTINCT FROM $9
               AND process_id = $10
               AND process_started_at = $11",
        )
        .bind(&self.store_key)
        .bind(&target.project_key)
        .bind(target.slot_index as i64)
        .bind(target.task_id.as_str())
        .bind(target.workspace_path.to_string_lossy().as_ref())
        .bind(&target.runtime_workflow_id)
        .bind(&target.owner_session)
        .bind(target.run_generation as i64)
        .bind(target.acquisition_id.as_deref())
        .bind(target.process_id as i64)
        .bind(i64::try_from(target.process_started_at)?)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(())
    }

    pub(crate) async fn complete_claimed_workspace_cleanup_target(
        &self,
        target: &WorkspaceCleanupTargetRecord,
        cleanup_claim_id: &str,
        cleanup_owner_session: &str,
    ) -> anyhow::Result<()> {
        let mut transaction = self.pool.begin().await?;
        let deleted = sqlx::query(
            "DELETE FROM workspace_cleanup_targets_v2
             WHERE store_key = $1 AND runtime_workflow_id = $2 AND workspace_path = $3
               AND task_id = $4 AND project_key = $5 AND slot_index = $6
               AND owner_session = $7 AND run_generation = $8
               AND acquisition_id IS NOT DISTINCT FROM $9
               AND process_id = $10 AND process_started_at = $11
               AND cleanup_claim_id = $12
               AND cleanup_owner_session = $13",
        )
        .bind(&self.store_key)
        .bind(&target.runtime_workflow_id)
        .bind(target.workspace_path.to_string_lossy().as_ref())
        .bind(target.task_id.as_str())
        .bind(&target.project_key)
        .bind(target.slot_index as i64)
        .bind(&target.owner_session)
        .bind(target.run_generation as i64)
        .bind(target.acquisition_id.as_deref())
        .bind(target.process_id as i64)
        .bind(i64::try_from(target.process_started_at)?)
        .bind(cleanup_claim_id)
        .bind(cleanup_owner_session)
        .execute(&mut *transaction)
        .await?;
        if deleted.rows_affected() == 0 {
            anyhow::bail!("workspace cleanup claim changed before completion");
        }
        sqlx::query(
            "DELETE FROM workspace_leases
             WHERE store_key = $1 AND project_key = $2 AND slot_index = $3
               AND task_id = $4 AND workspace_path = $5 AND runtime_workflow_id = $6
               AND owner_session = $7 AND run_generation = $8
               AND acquisition_id IS NOT DISTINCT FROM $9
               AND process_id = $10 AND process_started_at = $11",
        )
        .bind(&self.store_key)
        .bind(&target.project_key)
        .bind(target.slot_index as i64)
        .bind(target.task_id.as_str())
        .bind(target.workspace_path.to_string_lossy().as_ref())
        .bind(&target.runtime_workflow_id)
        .bind(&target.owner_session)
        .bind(target.run_generation as i64)
        .bind(target.acquisition_id.as_deref())
        .bind(target.process_id as i64)
        .bind(i64::try_from(target.process_started_at)?)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(())
    }

    pub(crate) async fn abandon_workspace_cleanup_claim(
        &self,
        target: &WorkspaceCleanupTargetRecord,
        cleanup_claim_id: &str,
        cleanup_owner_session: &str,
    ) -> anyhow::Result<bool> {
        let updated = sqlx::query(
            "UPDATE workspace_cleanup_targets_v2
             SET cleanup_in_progress = FALSE,
                 cleanup_claim_id = NULL,
                 cleanup_owner_session = NULL,
                 cleanup_process_id = NULL,
                 cleanup_process_started_at = NULL,
                 cleanup_claim_expires_at = NULL,
                 last_used_at = CURRENT_TIMESTAMP
             WHERE store_key = $1 AND runtime_workflow_id = $2 AND workspace_path = $3
               AND cleanup_claim_id = $4 AND cleanup_owner_session = $5",
        )
        .bind(&self.store_key)
        .bind(&target.runtime_workflow_id)
        .bind(target.workspace_path.to_string_lossy().as_ref())
        .bind(cleanup_claim_id)
        .bind(cleanup_owner_session)
        .execute(&self.pool)
        .await?;
        Ok(updated.rows_affected() > 0)
    }

    pub(crate) async fn renew_workspace_cleanup_claim(
        &self,
        target: &WorkspaceCleanupTargetRecord,
        cleanup_claim_id: &str,
        cleanup_owner_session: &str,
    ) -> anyhow::Result<bool> {
        let updated = sqlx::query(
            "UPDATE workspace_cleanup_targets_v2
             SET cleanup_claim_expires_at = CURRENT_TIMESTAMP + ($6 * INTERVAL '1 second'),
                 last_used_at = CURRENT_TIMESTAMP
             WHERE store_key = $1 AND runtime_workflow_id = $2 AND workspace_path = $3
               AND cleanup_claim_id = $4 AND cleanup_owner_session = $5",
        )
        .bind(&self.store_key)
        .bind(&target.runtime_workflow_id)
        .bind(target.workspace_path.to_string_lossy().as_ref())
        .bind(cleanup_claim_id)
        .bind(cleanup_owner_session)
        .bind(CLEANUP_CLAIM_TTL_SECS)
        .execute(&self.pool)
        .await?;
        Ok(updated.rows_affected() > 0)
    }

    pub(crate) fn workspace_cleanup_claim_ttl() -> std::time::Duration {
        std::time::Duration::from_secs(CLEANUP_CLAIM_TTL_SECS as u64)
    }

    #[cfg(test)]
    pub(crate) async fn expire_workspace_cleanup_claim_for_test(
        &self,
        target: &WorkspaceCleanupTargetRecord,
        cleanup_claim_id: &str,
        cleanup_owner_session: &str,
    ) -> anyhow::Result<bool> {
        let updated = sqlx::query(
            "UPDATE workspace_cleanup_targets_v2
             SET cleanup_claim_expires_at = CURRENT_TIMESTAMP - INTERVAL '1 second'
             WHERE store_key = $1 AND runtime_workflow_id = $2 AND workspace_path = $3
               AND cleanup_claim_id = $4 AND cleanup_owner_session = $5",
        )
        .bind(&self.store_key)
        .bind(&target.runtime_workflow_id)
        .bind(target.workspace_path.to_string_lossy().as_ref())
        .bind(cleanup_claim_id)
        .bind(cleanup_owner_session)
        .execute(&self.pool)
        .await?;
        Ok(updated.rows_affected() > 0)
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
    acquisition_id: Option<String>,
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
            acquisition_id: row.acquisition_id,
            process_id: u32::try_from(row.process_id)?,
            process_started_at: u64::try_from(row.process_started_at)?,
        })
    }
}

#[cfg(test)]
pub(crate) const WORKSPACE_CLEANUP_TARGETS_TABLE_SQL: &str =
    "ALTER TABLE workspace_leases ADD COLUMN IF NOT EXISTS acquisition_id TEXT;
CREATE TABLE IF NOT EXISTS workspace_cleanup_targets_v2 (
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
    acquisition_id      TEXT,
    process_id          BIGINT NOT NULL,
    process_started_at  BIGINT NOT NULL DEFAULT 0,
    cleanup_in_progress BOOLEAN NOT NULL DEFAULT FALSE,
    cleanup_claim_id TEXT,
    cleanup_owner_session TEXT,
    cleanup_process_id BIGINT,
    cleanup_process_started_at BIGINT,
    cleanup_claim_expires_at TIMESTAMPTZ,
    workflow_hook_claimed BOOLEAN NOT NULL DEFAULT FALSE,
    manager_hook_claimed BOOLEAN NOT NULL DEFAULT FALSE,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    last_used_at        TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY(store_key, runtime_workflow_id, workspace_path)
);
ALTER TABLE workspace_cleanup_targets_v2
    ADD COLUMN IF NOT EXISTS workflow_hook_claimed BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE workspace_cleanup_targets_v2
    ADD COLUMN IF NOT EXISTS manager_hook_claimed BOOLEAN NOT NULL DEFAULT FALSE;
CREATE INDEX IF NOT EXISTS idx_workspace_cleanup_targets_workflow
    ON workspace_cleanup_targets_v2(store_key, runtime_workflow_id, last_used_at);
INSERT INTO workspace_cleanup_targets_v2 (
    store_key, runtime_workflow_id, workspace_path, task_id, project_key, slot_index,
    workspace_key, source_repo, repo, owner_session, run_generation, acquisition_id, process_id,
    process_started_at, created_at, last_used_at
)
SELECT store_key, runtime_workflow_id, workspace_path, task_id, project_key, slot_index,
       workspace_key, source_repo, repo, owner_session, run_generation, acquisition_id, process_id,
       process_started_at, acquired_at, last_used_at
FROM workspace_leases
WHERE runtime_workflow_id IS NOT NULL
ON CONFLICT(store_key, runtime_workflow_id, workspace_path) DO NOTHING";
