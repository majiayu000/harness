use super::*;

impl TaskStore {
    /// Persist an artifact captured from agent output during task execution.
    pub(crate) async fn insert_artifact(
        &self,
        task_id: &TaskId,
        turn: u32,
        artifact_type: &str,
        content: &str,
    ) {
        if let Err(e) = self
            .db
            .insert_artifact(&task_id.0, turn, artifact_type, content)
            .await
        {
            tracing::warn!(task_id = %task_id.0, artifact_type, "failed to insert task artifact: {e}");
        }
    }

    /// Return all artifacts for a task ordered by insertion time.
    pub async fn list_artifacts(
        &self,
        task_id: &TaskId,
    ) -> anyhow::Result<Vec<crate::task_db::TaskArtifact>> {
        record_task_runner_usage();
        self.db.list_artifacts(&task_id.0).await
    }

    /// Persist a redacted agent prompt for a task turn (fire-and-forget wrapper).
    pub(crate) async fn save_prompt(
        &self,
        task_id: &TaskId,
        turn: u32,
        phase: &str,
        prompt: &str,
    ) -> anyhow::Result<()> {
        self.db
            .save_task_prompt(&task_id.0, turn, phase, prompt)
            .await
    }

    /// Return all persisted prompts for a task ordered by turn.
    pub async fn get_prompts(
        &self,
        task_id: &TaskId,
    ) -> anyhow::Result<Vec<crate::task_db::TaskPrompt>> {
        record_task_runner_usage();
        self.db.get_task_prompts(&task_id.0).await
    }

    /// Append a [`TaskEvent`] to the event log. No-op when the log is not open.
    pub(crate) fn log_event(&self, event: crate::event_replay::TaskEvent) {
        if let Some(ref log) = self.event_log {
            log.append(&event);
        }
    }

    pub(crate) async fn insert(&self, state: &TaskState) {
        let mut state = state.clone();
        state.reconcile_scheduler_with_status();
        self.persist_locks
            .entry(state.id.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())));
        self.cache.insert(state.id.clone(), state.clone());
        if let Err(e) = self.db.insert(&state).await {
            tracing::error!("task_db insert failed: {e}");
        }
        self.log_event(crate::event_replay::TaskEvent::Created {
            task_id: state.id.0.clone(),
            ts: crate::event_replay::now_ts(),
        });
    }

    /// Write a phase checkpoint for `task_id`. Checkpoint writes are non-fatal:
    /// callers should log the error rather than failing the task.
    pub(crate) async fn write_checkpoint(
        &self,
        task_id: &TaskId,
        triage_output: Option<&str>,
        plan_output: Option<&str>,
        pr_url: Option<&str>,
        last_phase: &str,
    ) -> anyhow::Result<()> {
        self.db
            .write_checkpoint(
                task_id.as_str(),
                triage_output,
                plan_output,
                pr_url,
                last_phase,
            )
            .await
    }

    /// Load the checkpoint for `task_id`, or `None` if no checkpoint exists.
    pub(crate) async fn load_checkpoint(
        &self,
        task_id: &TaskId,
    ) -> anyhow::Result<Option<crate::task_db::TaskCheckpoint>> {
        self.db.load_checkpoint(task_id.as_str()).await
    }

    /// Return pending tasks that have a plan or triage checkpoint but no `pr_url`.
    ///
    /// Used at startup to re-dispatch tasks recovered from plan/triage checkpoints
    /// that were not caught by the PR-based redispatch path.
    pub(crate) async fn pending_tasks_with_checkpoint(
        &self,
    ) -> anyhow::Result<Vec<(TaskState, crate::task_db::TaskCheckpoint)>> {
        self.db.pending_tasks_with_checkpoint().await
    }

    /// Return pending tasks that have no PR URL and no checkpoint row.
    pub(crate) async fn pending_orphan_tasks(&self) -> anyhow::Result<Vec<TaskState>> {
        self.db.pending_orphan_tasks().await
    }

    /// Overwrite `external_id` on an auto-fix task, even if one is already set.
    ///
    /// Used during streaming to implement "last sentinel wins" — the agent may
    /// emit multiple `CREATED_ISSUE=` lines as it self-corrects.  Updates both
    /// the DB and the in-memory cache so dedup reads are immediately consistent.
    pub(crate) async fn overwrite_external_id_auto_fix(
        &self,
        id: &TaskId,
        external_id: &str,
    ) -> anyhow::Result<()> {
        let lock = self
            .persist_locks
            .entry(id.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _guard = lock.lock().await;

        let expected_version = match self.cache.get(id) {
            Some(entry) => entry.version,
            None => match self.db.get_version_only(id.as_str()).await? {
                Some(version) => version,
                None => return Ok(()),
            },
        };
        self.db
            .overwrite_external_id_auto_fix(id.as_str(), external_id, expected_version)
            .await?;
        if let Some(mut entry) = self.cache.get_mut(id) {
            if entry.source.as_deref() == Some("auto-fix") {
                entry.external_id = Some(external_id.to_string());
                entry.version = entry.version.saturating_add(1);
            }
        }
        Ok(())
    }

    pub(crate) async fn persist(&self, id: &TaskId) -> anyhow::Result<()> {
        let lock = self
            .persist_locks
            .entry(id.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _guard = lock.lock().await;

        let snapshot = if let Some(mut state) = self.cache.get_mut(id) {
            state.value_mut().reconcile_scheduler_with_status();
            Some(state.value().clone())
        } else {
            None
        };
        if let Some(state) = snapshot {
            self.db.update(&state).await?;
            // The DB increments `version` atomically inside the UPDATE; mirror
            // that increment into the in-memory cache so the next persist()
            // sends a matching expected version instead of tripping the
            // optimistic-lock guard immediately on the second write.
            if let Some(mut entry) = self.cache.get_mut(id) {
                entry.value_mut().version = entry.value().version.saturating_add(1);
            }

            // Persist a checkpoint to the DB so that recover_in_progress() can
            // restore the task if the server is killed mid-flight. Only written
            // for statuses that can be resumed after restart.
            if state.status.is_resumable_after_restart() {
                let phase_str = format!("{:?}", state.phase);
                if let Err(e) = self
                    .db
                    .write_checkpoint(id.as_str(), None, None, state.pr_url.as_deref(), &phase_str)
                    .await
                {
                    tracing::warn!(
                        task_id = %id.0,
                        "checkpoint: failed to write DB checkpoint: {e}"
                    );
                }
            }
        }
        Ok(())
    }

    /// Update status in cache and DB without resetting `updated_at`.
    ///
    /// Used to roll back a cancelled status after a failed retry enqueue so that
    /// `list_stalled_tasks` still considers the task stale on the next tick instead
    /// of deferring by a full `stale_threshold_mins` window.
    pub(crate) async fn restore_status_preserve_staleness(
        &self,
        id: &TaskId,
        status: TaskStatus,
    ) -> anyhow::Result<()> {
        let lock = self
            .persist_locks
            .entry(id.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _guard = lock.lock().await;

        let mut scheduler_json = None;
        let mut expected_version = None;
        let mut rollback_state = None;
        if let Some(mut entry) = self.cache.get_mut(id) {
            rollback_state = Some((entry.status.clone(), entry.scheduler.clone()));
            entry.value_mut().status = status.clone();
            entry.value_mut().reconcile_scheduler_with_status();
            scheduler_json = Some(serde_json::to_string(&entry.value().scheduler)?);
            expected_version = Some(entry.value().version);
        }
        let expected_version = match expected_version {
            Some(version) => version,
            None => match self.db.get_version_only(id.as_str()).await? {
                Some(version) => version,
                None => return Ok(()),
            },
        };
        let default_scheduler_json =
            serde_json::to_string(&crate::task_runner::TaskSchedulerState::queued())?;
        if let Err(e) = self
            .db
            .update_status_only(
                id.0.as_str(),
                status.as_ref(),
                scheduler_json
                    .as_deref()
                    .unwrap_or(default_scheduler_json.as_str()),
                expected_version,
            )
            .await
        {
            if let Some((original_status, original_scheduler)) = rollback_state {
                if let Some(mut entry) = self.cache.get_mut(id) {
                    entry.value_mut().status = original_status;
                    entry.value_mut().scheduler = original_scheduler;
                }
            }
            return Err(e);
        }
        if let Some(mut entry) = self.cache.get_mut(id) {
            entry.value_mut().version = entry.value().version.saturating_add(1);
        }
        Ok(())
    }
}
