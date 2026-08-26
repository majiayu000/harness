use crate::task_runner::TaskId;
use harness_core::db::PgStoreContext;
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgPool;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};
use tokio::sync::Semaphore;

#[cfg(test)]
const TEST_REPOSITORY_LOCK_CAPACITY: u32 = 4;

#[path = "workspace_cleanup_store.rs"]
mod workspace_cleanup_store;
pub(crate) use workspace_cleanup_store::{
    PersistedWorkspaceCleanupClaim, WorkspaceCleanupHook, WorkspaceCleanupTargetRecord,
    WORKSPACE_CLEANUP_TARGETS_TABLE_SQL,
};
#[path = "workspace_repository_lock.rs"]
mod workspace_repository_lock;
pub(crate) use workspace_repository_lock::{
    RepositoryLeaseMode, RepositoryLeaseState, RepositoryWriteLease,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WorkspaceLeaseRecord {
    pub(crate) project_key: String,
    pub(crate) slot_index: u32,
    pub(crate) task_id: TaskId,
    pub(crate) workspace_key: String,
    pub(crate) workspace_path: PathBuf,
    pub(crate) source_repo: PathBuf,
    pub(crate) repo: Option<String>,
    pub(crate) runtime_workflow_id: Option<String>,
    pub(crate) owner_session: String,
    pub(crate) run_generation: u32,
    pub(crate) acquisition_id: Option<String>,
    pub(crate) process_id: u32,
    pub(crate) process_started_at: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct WorkspaceLeaseStore {
    pool: PgPool,
    repository_lock_pool: PgPool,
    repository_lock_slots: Arc<Semaphore>,
    store_key: String,
}

impl WorkspaceLeaseStore {
    #[cfg(test)]
    pub(crate) async fn open_shared_with_data_dir(
        context: &PgStoreContext,
        setup_pool: &PgPool,
        data_dir: &std::path::Path,
    ) -> anyhow::Result<Self> {
        let store_key = crate::task_db::TaskDb::store_key_for_data_dir(data_dir)?;
        Self::open_with_context_and_store_key(context, setup_pool, store_key).await
    }

    pub(crate) async fn open_shared_with_data_dir_and_repository_lock_capacity(
        context: &PgStoreContext,
        setup_pool: &PgPool,
        data_dir: &std::path::Path,
        repository_lock_capacity: usize,
    ) -> anyhow::Result<Self> {
        let store_key = crate::task_db::TaskDb::store_key_for_data_dir(data_dir)?;
        let repository_lock_capacity = u32::try_from(repository_lock_capacity)
            .map_err(|_| anyhow::anyhow!("repository lock capacity exceeds PostgreSQL limits"))?;
        Self::open_with_context_store_key_and_repository_lock_capacity(
            context,
            setup_pool,
            store_key,
            repository_lock_capacity,
        )
        .await
    }

    #[cfg(test)]
    async fn open_with_context_and_store_key(
        context: &PgStoreContext,
        setup_pool: &PgPool,
        store_key: String,
    ) -> anyhow::Result<Self> {
        let pool = context.open_pool_with_setup_pool(setup_pool).await?;
        // Session-level advisory locks retain their connection for the full
        // workspace lifetime. Keep them out of the ordinary lease-query pool.
        let repository_lock_pool = context
            .open_runtime_pool_with_max_connections(TEST_REPOSITORY_LOCK_CAPACITY)
            .await?;
        let repository_lock_slots =
            Arc::new(Semaphore::new(TEST_REPOSITORY_LOCK_CAPACITY as usize));
        Ok(Self {
            pool,
            repository_lock_pool,
            repository_lock_slots,
            store_key,
        })
    }

    async fn open_with_context_store_key_and_repository_lock_capacity(
        context: &PgStoreContext,
        setup_pool: &PgPool,
        store_key: String,
        repository_lock_capacity: u32,
    ) -> anyhow::Result<Self> {
        let pool = context.open_pool_with_setup_pool(setup_pool).await?;
        let repository_lock_pool = context
            .open_runtime_pool_with_max_connections(repository_lock_capacity)
            .await?;
        let repository_lock_slots = Arc::new(Semaphore::new(repository_lock_capacity as usize));
        Ok(Self {
            pool,
            repository_lock_pool,
            repository_lock_slots,
            store_key,
        })
    }

    #[cfg(test)]
    pub(crate) async fn open(path: &std::path::Path) -> anyhow::Result<Self> {
        Self::open_with_repository_lock_capacity(path, TEST_REPOSITORY_LOCK_CAPACITY).await
    }

    #[cfg(test)]
    pub(crate) async fn open_with_repository_lock_capacity(
        path: &std::path::Path,
        repository_lock_capacity: u32,
    ) -> anyhow::Result<Self> {
        let context = PgStoreContext::from_legacy_path_schema(path, None)?;
        let store_key = context.schema().to_owned();
        let pool = context.open_pool().await?;
        ensure_workspace_leases_table(&pool).await?;
        let repository_lock_pool = context
            .open_runtime_pool_with_max_connections(repository_lock_capacity)
            .await?;
        let repository_lock_slots = Arc::new(Semaphore::new(repository_lock_capacity as usize));
        Ok(Self {
            pool,
            repository_lock_pool,
            repository_lock_slots,
            store_key,
        })
    }

    #[cfg(test)]
    pub(crate) async fn try_acquire_lease(
        &self,
        record: &WorkspaceLeaseRecord,
    ) -> anyhow::Result<bool> {
        self.try_acquire_lease_with_cleanup_target(record, record.runtime_workflow_id.is_some())
            .await
    }

    pub(crate) async fn try_acquire_lease_with_cleanup_target(
        &self,
        record: &WorkspaceLeaseRecord,
        persist_cleanup_target: bool,
    ) -> anyhow::Result<bool> {
        let mut transaction = self.pool.begin().await?;
        let cleanup_claims: Vec<Option<String>> = sqlx::query_scalar(
            "SELECT cleanup_claim_id
             FROM workspace_cleanup_targets
             WHERE store_key = $1 AND workspace_path = $2
             FOR UPDATE",
        )
        .bind(&self.store_key)
        .bind(record.workspace_path.to_string_lossy().as_ref())
        .fetch_all(&mut *transaction)
        .await?;
        if cleanup_claims
            .into_iter()
            .any(|claim_id| claim_id.is_some())
        {
            transaction.rollback().await?;
            return Ok(false);
        }
        let result = sqlx::query(
            "INSERT INTO workspace_leases (
                store_key, project_key, slot_index, task_id, workspace_key, workspace_path,
                source_repo, repo, runtime_workflow_id, owner_session, run_generation,
                acquisition_id, process_id, process_started_at, state, acquired_at, released_at, last_used_at
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, 'leased',
                       CURRENT_TIMESTAMP, NULL, CURRENT_TIMESTAMP)
             ON CONFLICT(store_key, project_key, slot_index) DO UPDATE SET
                task_id = EXCLUDED.task_id,
                workspace_key = EXCLUDED.workspace_key,
                workspace_path = EXCLUDED.workspace_path,
                source_repo = EXCLUDED.source_repo,
                repo = EXCLUDED.repo,
                runtime_workflow_id = EXCLUDED.runtime_workflow_id,
                owner_session = EXCLUDED.owner_session,
                run_generation = EXCLUDED.run_generation,
                acquisition_id = EXCLUDED.acquisition_id,
                process_id = EXCLUDED.process_id,
                process_started_at = EXCLUDED.process_started_at,
                state = 'leased',
                acquired_at = CURRENT_TIMESTAMP,
                released_at = NULL,
                last_used_at = CURRENT_TIMESTAMP
             WHERE workspace_leases.state = 'released'
                OR (
                    workspace_leases.state = 'leased'
                    AND
                    workspace_leases.task_id = EXCLUDED.task_id
                    AND workspace_leases.owner_session = EXCLUDED.owner_session
                )",
        )
        .bind(&self.store_key)
        .bind(&record.project_key)
        .bind(record.slot_index as i64)
        .bind(record.task_id.as_str())
        .bind(&record.workspace_key)
        .bind(record.workspace_path.to_string_lossy().as_ref())
        .bind(record.source_repo.to_string_lossy().as_ref())
        .bind(record.repo.as_deref())
        .bind(record.runtime_workflow_id.as_deref())
        .bind(&record.owner_session)
        .bind(record.run_generation as i64)
        .bind(record.acquisition_id.as_deref())
        .bind(record.process_id as i64)
        .bind(i64::try_from(record.process_started_at)?)
        .execute(&mut *transaction)
        .await?;
        if result.rows_affected() == 0 {
            transaction.rollback().await?;
            return Ok(false);
        }
        if persist_cleanup_target {
            if let Some(runtime_workflow_id) = record.runtime_workflow_id.as_deref() {
                sqlx::query(
                    "INSERT INTO workspace_cleanup_targets (
                    store_key, runtime_workflow_id, workspace_path, task_id, project_key,
                    slot_index, workspace_key, source_repo, repo, owner_session, run_generation,
                    acquisition_id, process_id, process_started_at, created_at, last_used_at
                 ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14,
                           CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)
                 ON CONFLICT(store_key, runtime_workflow_id, workspace_path) DO UPDATE SET
                    task_id = EXCLUDED.task_id,
                    project_key = EXCLUDED.project_key,
                    slot_index = EXCLUDED.slot_index,
                    workspace_key = EXCLUDED.workspace_key,
                    source_repo = EXCLUDED.source_repo,
                    repo = EXCLUDED.repo,
                    owner_session = EXCLUDED.owner_session,
                    run_generation = EXCLUDED.run_generation,
                    acquisition_id = EXCLUDED.acquisition_id,
                    process_id = EXCLUDED.process_id,
                    process_started_at = EXCLUDED.process_started_at,
                    cleanup_in_progress = FALSE,
                    cleanup_claim_id = NULL,
                    cleanup_owner_session = NULL,
                    cleanup_process_id = NULL,
                    cleanup_process_started_at = NULL,
                    cleanup_claim_expires_at = NULL,
                    workflow_hook_claimed = FALSE,
                    manager_hook_claimed = FALSE,
                    last_used_at = CURRENT_TIMESTAMP",
                )
                .bind(&self.store_key)
                .bind(runtime_workflow_id)
                .bind(record.workspace_path.to_string_lossy().as_ref())
                .bind(record.task_id.as_str())
                .bind(&record.project_key)
                .bind(record.slot_index as i64)
                .bind(&record.workspace_key)
                .bind(record.source_repo.to_string_lossy().as_ref())
                .bind(record.repo.as_deref())
                .bind(&record.owner_session)
                .bind(record.run_generation as i64)
                .bind(record.acquisition_id.as_deref())
                .bind(record.process_id as i64)
                .bind(i64::try_from(record.process_started_at)?)
                .execute(&mut *transaction)
                .await?;
            }
        } else {
            sqlx::query(
                "DELETE FROM workspace_cleanup_targets
                 WHERE store_key = $1
                   AND runtime_workflow_id IS NOT DISTINCT FROM $2
                   AND workspace_path = $3
                   AND task_id = $4
                   AND project_key = $5
                   AND slot_index = $6
                   AND workspace_key = $7
                   AND source_repo = $8
                   AND repo IS NOT DISTINCT FROM $9
                   AND owner_session = $10
                   AND run_generation = $11
                   AND acquisition_id IS NOT DISTINCT FROM $12
                   AND process_id = $13
                   AND process_started_at = $14
                   AND cleanup_claim_id IS NULL",
            )
            .bind(&self.store_key)
            .bind(record.runtime_workflow_id.as_deref())
            .bind(record.workspace_path.to_string_lossy().as_ref())
            .bind(record.task_id.as_str())
            .bind(&record.project_key)
            .bind(record.slot_index as i64)
            .bind(&record.workspace_key)
            .bind(record.source_repo.to_string_lossy().as_ref())
            .bind(record.repo.as_deref())
            .bind(&record.owner_session)
            .bind(record.run_generation as i64)
            .bind(record.acquisition_id.as_deref())
            .bind(record.process_id as i64)
            .bind(i64::try_from(record.process_started_at)?)
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
        Ok(true)
    }

    pub(crate) async fn leased_slots_for_project(
        &self,
        project_key: &str,
    ) -> anyhow::Result<HashSet<u32>> {
        let rows: Vec<(i64,)> = sqlx::query_as(
            "SELECT slot_index
             FROM workspace_leases
             WHERE store_key = $1
               AND project_key = $2
               AND state = 'leased'",
        )
        .bind(&self.store_key)
        .bind(project_key)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(slot,)| Ok(u32::try_from(slot)?))
            .collect()
    }

    pub(crate) async fn release_owned_slot(
        &self,
        project_key: &str,
        slot_index: u32,
        task_id: &TaskId,
        owner_session: &str,
        run_generation: u32,
        acquisition_id: &str,
    ) -> anyhow::Result<bool> {
        let result = sqlx::query(
            "UPDATE workspace_leases
             SET state = 'released',
                 released_at = CURRENT_TIMESTAMP,
                 last_used_at = CURRENT_TIMESTAMP
             WHERE store_key = $1
               AND project_key = $2
               AND slot_index = $3
               AND task_id = $4
               AND owner_session = $5
               AND run_generation = $6
               AND acquisition_id = $7
               AND state = 'leased'",
        )
        .bind(&self.store_key)
        .bind(project_key)
        .bind(slot_index as i64)
        .bind(task_id.as_str())
        .bind(owner_session)
        .bind(run_generation as i64)
        .bind(acquisition_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    pub(crate) async fn release_foreign_orphaned_leases(
        &self,
        current_owner_session: &str,
    ) -> anyhow::Result<u64> {
        let rows = sqlx::query_as::<_, WorkspaceLeaseRow>(
            "SELECT project_key, slot_index, task_id, workspace_key, workspace_path, source_repo, repo,
                    runtime_workflow_id, owner_session, run_generation, acquisition_id, process_id,
                    process_started_at
             FROM workspace_leases
             WHERE store_key = $1
               AND state = 'leased'
               AND owner_session <> $2",
        )
        .bind(&self.store_key)
        .bind(current_owner_session)
        .fetch_all(&self.pool)
        .await?;

        let mut system = System::new();
        system.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::new());

        let mut released = 0;
        for row in rows {
            let record = WorkspaceLeaseRecord::try_from(row)?;
            if process_matches_lease(&system, record.process_id, record.process_started_at) {
                continue;
            }
            if self.release_exact_lease(&record).await? {
                released += 1;
            }
        }
        Ok(released)
    }

    pub(crate) async fn release_exact_lease(
        &self,
        record: &WorkspaceLeaseRecord,
    ) -> anyhow::Result<bool> {
        let result = sqlx::query(
            "UPDATE workspace_leases
             SET state = 'released',
                 released_at = CURRENT_TIMESTAMP,
                 last_used_at = CURRENT_TIMESTAMP
             WHERE store_key = $1
               AND project_key = $2
               AND slot_index = $3
               AND task_id = $4
               AND owner_session = $5
               AND acquisition_id IS NOT DISTINCT FROM $6
               AND process_id = $7
               AND process_started_at = $8
               AND state = 'leased'",
        )
        .bind(&self.store_key)
        .bind(&record.project_key)
        .bind(record.slot_index as i64)
        .bind(record.task_id.as_str())
        .bind(&record.owner_session)
        .bind(record.acquisition_id.as_deref())
        .bind(record.process_id as i64)
        .bind(i64::try_from(record.process_started_at)?)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    pub(crate) async fn latest_workspace_path_for_task(
        &self,
        task_id: &TaskId,
    ) -> anyhow::Result<Option<PathBuf>> {
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT workspace_path
             FROM workspace_leases
             WHERE store_key = $1
               AND task_id = $2
             ORDER BY last_used_at DESC
             LIMIT 1",
        )
        .bind(&self.store_key)
        .bind(task_id.as_str())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(path,)| PathBuf::from(path)))
    }

    pub(crate) async fn latest_released_lease_for_workspace_key(
        &self,
        project_key: &str,
        workspace_key: &str,
    ) -> anyhow::Result<Option<WorkspaceLeaseRecord>> {
        let row = sqlx::query_as::<_, WorkspaceLeaseRow>(
            "SELECT project_key, slot_index, task_id, workspace_key, workspace_path, source_repo, repo,
                    runtime_workflow_id, owner_session, run_generation, acquisition_id, process_id,
                    process_started_at
             FROM workspace_leases
             WHERE store_key = $1
               AND project_key = $2
               AND workspace_key = $3
               AND state = 'released'
             ORDER BY last_used_at DESC
             LIMIT 1",
        )
        .bind(&self.store_key)
        .bind(project_key)
        .bind(workspace_key)
        .fetch_optional(&self.pool)
        .await?;
        row.map(TryInto::try_into).transpose()
    }

    pub(crate) async fn leased_workspace_path(
        &self,
        workspace_path: &std::path::Path,
    ) -> anyhow::Result<Option<WorkspaceLeaseRecord>> {
        let row = sqlx::query_as::<_, WorkspaceLeaseRow>(
            "SELECT project_key, slot_index, task_id, workspace_key, workspace_path, source_repo, repo,
                    runtime_workflow_id, owner_session, run_generation, acquisition_id, process_id,
                    process_started_at
             FROM workspace_leases
             WHERE store_key = $1
               AND workspace_path = $2
               AND state = 'leased'
             ORDER BY last_used_at DESC
             LIMIT 1",
        )
        .bind(&self.store_key)
        .bind(workspace_path.to_string_lossy().as_ref())
        .fetch_optional(&self.pool)
        .await?;
        row.map(TryInto::try_into).transpose()
    }

    #[cfg(test)]
    pub(crate) async fn list_leased(&self) -> anyhow::Result<Vec<WorkspaceLeaseRecord>> {
        let rows = sqlx::query_as::<_, WorkspaceLeaseRow>(
            "SELECT project_key, slot_index, task_id, workspace_key, workspace_path, source_repo, repo,
                    runtime_workflow_id, owner_session, run_generation, acquisition_id, process_id,
                    process_started_at
             FROM workspace_leases
             WHERE store_key = $1
               AND state = 'leased'
             ORDER BY project_key, slot_index",
        )
        .bind(&self.store_key)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(TryInto::try_into).collect()
    }
    pub(crate) fn current_process_started_at() -> anyhow::Result<u64> {
        let mut system = System::new();
        system.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::new());
        process_start_time(&system, std::process::id()).ok_or_else(|| {
            anyhow::anyhow!(
                "failed to resolve current process start time for pid {}",
                std::process::id()
            )
        })
    }
}

fn process_start_time(system: &System, process_id: u32) -> Option<u64> {
    if process_id == 0 {
        return None;
    }
    system
        .process(sysinfo::Pid::from_u32(process_id))
        .map(sysinfo::Process::start_time)
}

fn process_matches_lease(system: &System, process_id: u32, process_started_at: u64) -> bool {
    process_started_at != 0
        && process_started_at == process_start_time(system, process_id).unwrap_or_default()
}

#[derive(sqlx::FromRow)]
struct WorkspaceLeaseRow {
    project_key: String,
    slot_index: i64,
    task_id: String,
    workspace_key: String,
    workspace_path: String,
    source_repo: String,
    repo: Option<String>,
    runtime_workflow_id: Option<String>,
    owner_session: String,
    run_generation: i64,
    acquisition_id: Option<String>,
    process_id: i64,
    process_started_at: i64,
}

impl TryFrom<WorkspaceLeaseRow> for WorkspaceLeaseRecord {
    type Error = anyhow::Error;

    fn try_from(row: WorkspaceLeaseRow) -> Result<Self, Self::Error> {
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
async fn ensure_workspace_leases_table(pool: &PgPool) -> anyhow::Result<()> {
    for table_sql in [
        WORKSPACE_LEASES_TABLE_SQL,
        WORKSPACE_CLEANUP_TARGETS_TABLE_SQL,
    ] {
        for statement in table_sql
            .split(';')
            .map(str::trim)
            .filter(|statement| !statement.is_empty())
        {
            sqlx::query(statement).execute(pool).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) const WORKSPACE_LEASES_TABLE_SQL: &str = "CREATE TABLE IF NOT EXISTS workspace_leases (
    store_key           TEXT NOT NULL DEFAULT current_schema(),
    project_key         TEXT NOT NULL,
    slot_index          BIGINT NOT NULL,
    task_id             TEXT NOT NULL,
    workspace_key       TEXT NOT NULL,
    workspace_path      TEXT NOT NULL,
    source_repo         TEXT NOT NULL,
    repo                TEXT,
    runtime_workflow_id TEXT,
    owner_session       TEXT NOT NULL,
    run_generation      BIGINT NOT NULL,
    acquisition_id      TEXT,
    process_id          BIGINT NOT NULL,
    process_started_at  BIGINT NOT NULL DEFAULT 0,
    state               TEXT NOT NULL DEFAULT 'leased',
    acquired_at         TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    released_at         TIMESTAMPTZ,
    last_used_at        TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY(store_key, project_key, slot_index)
);
CREATE INDEX IF NOT EXISTS idx_workspace_leases_task_state
    ON workspace_leases(store_key, task_id, state);
CREATE INDEX IF NOT EXISTS idx_workspace_leases_workspace_key_state
    ON workspace_leases(store_key, project_key, workspace_key, state);
CREATE INDEX IF NOT EXISTS idx_workspace_leases_state_owner
    ON workspace_leases(store_key, state, owner_session)";

pub(crate) const WORKSPACE_LEASES_TABLE_V24_SQL: &str =
    "CREATE TABLE IF NOT EXISTS workspace_leases (
    project_key         TEXT NOT NULL,
    slot_index          BIGINT NOT NULL,
    task_id             TEXT NOT NULL,
    workspace_key       TEXT NOT NULL,
    workspace_path      TEXT NOT NULL,
    source_repo         TEXT NOT NULL,
    repo                TEXT,
    runtime_workflow_id TEXT,
    owner_session       TEXT NOT NULL,
    run_generation      BIGINT NOT NULL,
    process_id          BIGINT NOT NULL,
    state               TEXT NOT NULL DEFAULT 'leased',
    acquired_at         TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    released_at         TIMESTAMPTZ,
    last_used_at        TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY(project_key, slot_index)
);
CREATE INDEX IF NOT EXISTS idx_workspace_leases_task_state
    ON workspace_leases(task_id, state);
CREATE INDEX IF NOT EXISTS idx_workspace_leases_workspace_key_state
    ON workspace_leases(project_key, workspace_key, state);
CREATE INDEX IF NOT EXISTS idx_workspace_leases_state_owner
    ON workspace_leases(state, owner_session)";
