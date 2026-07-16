use super::*;

impl TaskStore {
    /// Count live runtime-host leases from the authoritative scheduler state
    /// carried on active task rows.
    pub(crate) fn active_runtime_host_lease_count(&self, host_id: &str) -> usize {
        let now = Utc::now();
        self.cache
            .iter()
            .filter(|entry| {
                let scheduler = &entry.value().scheduler;
                scheduler.runtime_host_id() == Some(host_id)
                    && scheduler.has_live_runtime_host_lease(now)
            })
            .count()
    }

    /// Attempt to claim a pending task for a runtime host by updating the
    /// authoritative scheduler state on the task row itself.
    pub(crate) async fn claim_for_runtime_host(
        &self,
        id: &TaskId,
        host_id: &str,
        lease_secs: Option<i64>,
    ) -> anyhow::Result<Option<crate::runtime_hosts::TaskClaimResult>> {
        let ttl = lease_secs
            .unwrap_or(crate::runtime_hosts::DEFAULT_LEASE_SECS)
            .max(0);
        let lease_duration = chrono::TimeDelta::try_seconds(ttl).ok_or_else(|| {
            anyhow::anyhow!(
                "lease_secs value {ttl} is too large to compute a valid expiration timestamp"
            )
        })?;
        let now = Utc::now();
        let expires_at = now.checked_add_signed(lease_duration).ok_or_else(|| {
            anyhow::anyhow!(
                "lease_secs value {ttl} is too large to compute a valid expiration timestamp"
            )
        })?;
        let lock = self
            .persist_locks
            .entry(id.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _guard = lock.lock().await;

        let Some(mut entry) = self.cache.get_mut(id) else {
            return Ok(None);
        };
        if !matches!(entry.status, TaskStatus::Pending) {
            return Ok(None);
        }
        let original_scheduler = entry.scheduler.clone();
        if let Some(owner) = entry.scheduler.owner.as_ref() {
            match owner.kind {
                SchedulerOwnerKind::Scheduler => {
                    // Pending rows owned by the local scheduler are unreachable today,
                    // but if one ever appears we must not let a runtime host steal it.
                    return Ok(None);
                }
                SchedulerOwnerKind::RuntimeHost => {
                    // RuntimeHost with live lease: the claim is still active.
                    if entry.scheduler.has_live_runtime_host_lease(now) {
                        return Ok(None);
                    }
                    // RuntimeHost with a stale lease: safe to clear and re-claim.
                    entry.scheduler.clear_to_queued();
                }
            }
        }

        entry.scheduler.claim_runtime_host(host_id, expires_at);
        let snapshot = entry.value().clone();
        drop(entry);
        if let Err(e) = self.db.update(&snapshot).await {
            if let Some(mut rollback) = self.cache.get_mut(id) {
                rollback.scheduler = original_scheduler;
            }
            return Err(e);
        }
        if let Some(mut entry) = self.cache.get_mut(id) {
            entry.value_mut().version = entry.value().version.saturating_add(1);
        }
        Ok(Some(crate::runtime_hosts::TaskClaimResult {
            task_id: id.clone(),
            lease_expires_at: expires_at.to_rfc3339(),
        }))
    }

    /// Clear a pending runtime-host lease from the authoritative scheduler state.
    ///
    /// Returns `true` only when the task still belonged to `host_id` and was
    /// rewritten back to the queued state.
    pub(crate) async fn release_runtime_host_claim(
        &self,
        id: &TaskId,
        host_id: &str,
    ) -> anyhow::Result<bool> {
        let lock = self
            .persist_locks
            .entry(id.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _guard = lock.lock().await;

        let Some(mut entry) = self.cache.get_mut(id) else {
            return Ok(false);
        };
        if !matches!(entry.status, TaskStatus::Pending) {
            return Ok(false);
        }
        if entry.scheduler.runtime_host_id() != Some(host_id) {
            return Ok(false);
        }

        let original_scheduler = entry.scheduler.clone();
        entry.scheduler.clear_to_queued();
        let snapshot = entry.value().clone();
        drop(entry);
        if let Err(e) = self.db.update(&snapshot).await {
            if let Some(mut rollback) = self.cache.get_mut(id) {
                rollback.scheduler = original_scheduler;
            }
            return Err(e);
        }
        if let Some(mut entry) = self.cache.get_mut(id) {
            entry.value_mut().version = entry.value().version.saturating_add(1);
        }
        Ok(true)
    }

    /// Release every pending task lease owned by `host_id`.
    pub(crate) async fn release_runtime_host_claims(
        &self,
        host_id: &str,
    ) -> anyhow::Result<Vec<TaskId>> {
        let candidate_ids: Vec<TaskId> = self
            .cache
            .iter()
            .filter_map(|entry| {
                let task = entry.value();
                (matches!(task.status, TaskStatus::Pending)
                    && task.scheduler.runtime_host_id() == Some(host_id))
                .then(|| task.id.clone())
            })
            .collect();

        let mut released = Vec::new();
        for task_id in candidate_ids {
            if self.release_runtime_host_claim(&task_id, host_id).await? {
                released.push(task_id);
            }
        }
        Ok(released)
    }
}
