use super::{WorkspaceCleanupTargetRecord, CLEANUP_CLAIM_TTL_SECS};
use crate::workspace_lease_store::WorkspaceLeaseStore;
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};

#[derive(sqlx::FromRow)]
struct WorkspaceCleanupClaimRow {
    cleanup_in_progress: bool,
    claim_has_expiry: bool,
    claim_is_unexpired: bool,
    cleanup_process_id: Option<i64>,
    cleanup_process_started_at: Option<i64>,
}

impl WorkspaceCleanupClaimRow {
    fn blocks_replacement(&self, system: &System) -> anyhow::Result<bool> {
        if !self.cleanup_in_progress {
            return Ok(false);
        }
        if self.claim_has_expiry {
            return Ok(self.claim_is_unexpired);
        }
        let process_id = self
            .cleanup_process_id
            .ok_or_else(|| anyhow::anyhow!("legacy workspace cleanup claim has no process id"))?;
        let process_started_at = self.cleanup_process_started_at.ok_or_else(|| {
            anyhow::anyhow!("legacy workspace cleanup claim has no process start time")
        })?;
        Ok(super::super::process_matches_lease(
            system,
            u32::try_from(process_id)?,
            u64::try_from(process_started_at)?,
        ))
    }
}

impl WorkspaceLeaseStore {
    pub(crate) async fn claim_workspace_cleanup_target(
        &self,
        target: &WorkspaceCleanupTargetRecord,
        cleanup_claim_id: &str,
        cleanup_owner_session: &str,
        cleanup_process_id: u32,
        cleanup_process_started_at: u64,
    ) -> anyhow::Result<bool> {
        let mut transaction = self.pool.begin().await?;
        let existing_claim = sqlx::query_as::<_, WorkspaceCleanupClaimRow>(
            "SELECT cleanup_in_progress,
                    cleanup_claim_expires_at IS NOT NULL AS claim_has_expiry,
                    COALESCE(cleanup_claim_expires_at > CURRENT_TIMESTAMP, FALSE)
                        AS claim_is_unexpired,
                    cleanup_process_id,
                    cleanup_process_started_at
             FROM workspace_cleanup_targets_v2
             WHERE store_key = $1 AND runtime_workflow_id = $2 AND workspace_path = $3
               AND task_id = $4 AND project_key = $5 AND slot_index = $6
               AND owner_session = $7 AND run_generation = $8
               AND acquisition_id IS NOT DISTINCT FROM $9
               AND process_id = $10 AND process_started_at = $11
             FOR UPDATE",
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
        .fetch_optional(&mut *transaction)
        .await?;
        let Some(existing_claim) = existing_claim else {
            transaction.rollback().await?;
            return Ok(false);
        };
        let mut system = System::new();
        system.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::new());
        if existing_claim.blocks_replacement(&system)? {
            transaction.rollback().await?;
            return Ok(false);
        }

        let leased_path_matches: Vec<bool> = sqlx::query_scalar(
            "SELECT project_key = $3
                    AND slot_index = $4
                    AND task_id = $5
                    AND workspace_key = $6
                    AND source_repo = $7
                    AND repo IS NOT DISTINCT FROM $8
                    AND runtime_workflow_id = $9
                    AND owner_session = $10
                    AND run_generation = $11
                    AND acquisition_id IS NOT DISTINCT FROM $12
                    AND process_id = $13
                    AND process_started_at = $14
             FROM workspace_leases
             WHERE store_key = $1 AND workspace_path = $2 AND state = 'leased'
             FOR UPDATE",
        )
        .bind(&self.store_key)
        .bind(target.workspace_path.to_string_lossy().as_ref())
        .bind(&target.project_key)
        .bind(target.slot_index as i64)
        .bind(target.task_id.as_str())
        .bind(&target.workspace_key)
        .bind(target.source_repo.to_string_lossy().as_ref())
        .bind(target.repo.as_deref())
        .bind(&target.runtime_workflow_id)
        .bind(&target.owner_session)
        .bind(target.run_generation as i64)
        .bind(target.acquisition_id.as_deref())
        .bind(target.process_id as i64)
        .bind(i64::try_from(target.process_started_at)?)
        .fetch_all(&mut *transaction)
        .await?;
        if leased_path_matches.into_iter().any(|matches| !matches) {
            transaction.rollback().await?;
            return Ok(false);
        }
        sqlx::query(
            "UPDATE workspace_cleanup_targets_v2
             SET cleanup_in_progress = TRUE,
                 cleanup_claim_id = $4,
                 cleanup_owner_session = $5,
                 cleanup_process_id = $6,
                 cleanup_process_started_at = $7,
                 cleanup_claim_expires_at = CURRENT_TIMESTAMP + ($8 * INTERVAL '1 second'),
                 last_used_at = CURRENT_TIMESTAMP
             WHERE store_key = $1 AND runtime_workflow_id = $2 AND workspace_path = $3",
        )
        .bind(&self.store_key)
        .bind(&target.runtime_workflow_id)
        .bind(target.workspace_path.to_string_lossy().as_ref())
        .bind(cleanup_claim_id)
        .bind(cleanup_owner_session)
        .bind(cleanup_process_id as i64)
        .bind(i64::try_from(cleanup_process_started_at)?)
        .bind(CLEANUP_CLAIM_TTL_SECS)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(true)
    }

    #[cfg(test)]
    pub(crate) async fn remove_workspace_cleanup_claim_expiry_for_test(
        &self,
        target: &WorkspaceCleanupTargetRecord,
        cleanup_claim_id: &str,
    ) -> anyhow::Result<bool> {
        let updated = sqlx::query(
            "UPDATE workspace_cleanup_targets_v2
             SET cleanup_claim_expires_at = NULL
             WHERE store_key = $1 AND runtime_workflow_id = $2 AND workspace_path = $3
               AND cleanup_claim_id = $4",
        )
        .bind(&self.store_key)
        .bind(&target.runtime_workflow_id)
        .bind(target.workspace_path.to_string_lossy().as_ref())
        .bind(cleanup_claim_id)
        .execute(&self.pool)
        .await?;
        Ok(updated.rows_affected() > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn system_with_processes() -> System {
        let mut system = System::new();
        system.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::new());
        system
    }

    #[test]
    fn live_legacy_cleanup_claim_blocks_replacement() -> anyhow::Result<()> {
        let claim = WorkspaceCleanupClaimRow {
            cleanup_in_progress: true,
            claim_has_expiry: false,
            claim_is_unexpired: false,
            cleanup_process_id: Some(i64::from(std::process::id())),
            cleanup_process_started_at: Some(i64::try_from(
                WorkspaceLeaseStore::current_process_started_at()?,
            )?),
        };

        assert!(claim.blocks_replacement(&system_with_processes())?);
        Ok(())
    }

    #[test]
    fn crashed_legacy_cleanup_claim_can_be_replaced() -> anyhow::Result<()> {
        let claim = WorkspaceCleanupClaimRow {
            cleanup_in_progress: true,
            claim_has_expiry: false,
            claim_is_unexpired: false,
            cleanup_process_id: Some(i64::from(std::process::id())),
            cleanup_process_started_at: Some(1),
        };

        assert!(!claim.blocks_replacement(&system_with_processes())?);
        Ok(())
    }
}
