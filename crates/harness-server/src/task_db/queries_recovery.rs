//! Startup-recovery and event-replay write paths for `TaskDb`.
//!
//! These methods exclusively act on tasks that are in interrupted/transient
//! states at server start. They live in a separate impl block to keep
//! `queries_tasks.rs` under the project's per-file line ceiling.

use crate::http::parse_pr_num_from_url;
use crate::task_runner::{TaskSchedulerState, TaskStatus};
use harness_core::error::TaskDbDecodeError;

use super::types::{RecoveryResult, RecoveryRow};
use super::{TaskDb, TaskRecoveryWriteOutcome};

#[derive(sqlx::FromRow)]
struct RecoveryTaskSnapshot {
    status: String,
    pr_url: Option<String>,
    error: Option<String>,
    scheduler_state: String,
    version: i32,
}

enum RecoveryWriteIntent<'a> {
    TerminalReplay {
        status: TaskStatus,
        pr_url: Option<&'a str>,
        scheduler: &'a TaskSchedulerState,
    },
    PrReplay {
        pr_url: &'a str,
    },
    Resume {
        pr_url: Option<&'a str>,
        scheduler: &'a TaskSchedulerState,
    },
    Fail {
        error: &'a str,
        scheduler: &'a TaskSchedulerState,
    },
}

impl TaskDb {
    /// Apply event-replayed state to a task row (called during startup replay).
    pub async fn apply_replayed_state(
        &self,
        task_id: &str,
        pr_url: Option<&str>,
        terminal_status: Option<&str>,
    ) -> anyhow::Result<()> {
        self.apply_replayed_state_outcome(task_id, pr_url, terminal_status)
            .await?
            .applied_or_error()?;
        Ok(())
    }

    pub(crate) async fn apply_replayed_state_outcome(
        &self,
        task_id: &str,
        pr_url: Option<&str>,
        terminal_status: Option<&str>,
    ) -> anyhow::Result<TaskRecoveryWriteOutcome> {
        super::record_task_db_usage();
        if let Some(status) = terminal_status {
            let snapshot: Option<RecoveryTaskSnapshot> = sqlx::query_as(
                "SELECT status, pr_url, error, scheduler_state, version \
                 FROM tasks WHERE store_key = $1 AND id = $2",
            )
            .bind(&self.store_key)
            .bind(task_id)
            .fetch_optional(&self.pool)
            .await?;
            let Some(snapshot) = snapshot else {
                return Ok(TaskRecoveryWriteOutcome::Superseded);
            };
            let mut scheduler = decode_scheduler_state(task_id, &snapshot.scheduler_state)?;
            let task_status = status.parse::<TaskStatus>()?;
            if pr_url.is_some() && snapshot.pr_url.is_some() && pr_url != snapshot.pr_url.as_deref()
            {
                return Ok(TaskRecoveryWriteOutcome::Conflict {
                    action: "terminal replay",
                    task_id: task_id.to_string(),
                    expected_version: snapshot.version,
                    current_version: Some(snapshot.version),
                });
            }
            scheduler.mark_terminal(&task_status);
            let scheduler_json = serde_json::to_string(&scheduler)?;
            self.apply_terminal_replay_at_version(
                task_id,
                pr_url,
                &task_status,
                &scheduler,
                &scheduler_json,
                snapshot.version,
            )
            .await
        } else if let Some(url) = pr_url {
            let version: Option<i32> =
                sqlx::query_scalar("SELECT version FROM tasks WHERE store_key = $1 AND id = $2")
                    .bind(&self.store_key)
                    .bind(task_id)
                    .fetch_optional(&self.pool)
                    .await?;
            let Some(expected_version) = version else {
                return Ok(TaskRecoveryWriteOutcome::Superseded);
            };
            self.apply_pr_replay_at_version(task_id, url, expected_version)
                .await
        } else {
            Ok(TaskRecoveryWriteOutcome::Superseded)
        }
    }

    /// Recovery on server restart: resume or fail interrupted tasks.
    pub async fn recover_in_progress(&self) -> anyhow::Result<RecoveryResult> {
        super::record_task_db_usage();
        let rows = {
            let resumable = TaskStatus::resumable_statuses();
            let placeholders = Self::numbered_placeholders(2, resumable.len());
            let sql = format!(
                "SELECT t.id, t.status, t.turn, t.pr_url AS task_pr_url, t.scheduler_state, t.version, \
                        c.triage_output, c.plan_output, c.pr_url AS ck_pr_url \
                 FROM tasks t \
                 LEFT JOIN task_checkpoints c ON t.store_key = c.store_key AND t.id = c.task_id \
                 WHERE t.store_key = $1 AND t.status IN ({})",
                placeholders
            );
            let query = resumable.iter().fold(
                sqlx::query_as::<_, RecoveryRow>(&sql).bind(&self.store_key),
                |q, status| q.bind(*status),
            );
            query.fetch_all(&self.pool).await?
        };

        let mut result = RecoveryResult::default();

        for row in rows {
            let mut scheduler = decode_scheduler_state(&row.id, &row.scheduler_state)?;
            let effective_pr_url = row
                .task_pr_url
                .as_deref()
                .filter(|u| parse_pr_num_from_url(u).is_some())
                .or_else(|| {
                    row.ck_pr_url
                        .as_deref()
                        .filter(|u| parse_pr_num_from_url(u).is_some())
                });
            let has_pr = effective_pr_url.is_some();
            let has_plan = row.plan_output.is_some();
            let has_triage = row.triage_output.is_some();

            if has_pr || has_plan || has_triage {
                let reason = if has_pr {
                    format!(
                        "resumed after restart (was: {}, pr: {})",
                        row.status,
                        effective_pr_url.unwrap_or("checkpoint")
                    )
                } else if has_plan {
                    format!(
                        "resumed after restart (was: {}, plan checkpoint)",
                        row.status
                    )
                } else {
                    format!(
                        "resumed after restart (was: {}, triage checkpoint)",
                        row.status
                    )
                };
                scheduler.mark_recovering("startup-recovery");
                let task_pr_url_valid = row
                    .task_pr_url
                    .as_deref()
                    .map(|u| parse_pr_num_from_url(u).is_some())
                    .unwrap_or(false);
                let needs_pr_url_writeback = !task_pr_url_valid && effective_pr_url.is_some();
                let scheduler_json = serde_json::to_string(&scheduler)?;
                let outcome = if needs_pr_url_writeback {
                    let writeback_pr_url = effective_pr_url.ok_or_else(|| {
                        anyhow::anyhow!(
                            "startup recovery invariant violation: task {} requires PR writeback without an effective PR URL",
                            row.id
                        )
                    })?;
                    self.apply_checkpoint_resume_with_pr_at_version(
                        &row.id,
                        writeback_pr_url,
                        &scheduler,
                        &scheduler_json,
                        row.version,
                    )
                    .await?
                } else {
                    self.apply_checkpoint_resume_without_pr_at_version(
                        &row.id,
                        row.task_pr_url.as_deref(),
                        &scheduler,
                        &scheduler_json,
                        row.version,
                    )
                    .await?
                };
                if observe_recovery_outcome(&row.id, outcome)? {
                    result.resumed += 1;
                    if needs_pr_url_writeback {
                        tracing::info!(
                            task_id = %row.id,
                            was = %row.status,
                            pr_url = ?effective_pr_url,
                            "startup recovery: wrote back pr_url from checkpoint"
                        );
                    }
                    tracing::info!(
                        task_id = %row.id,
                        was = %row.status,
                        reason = %reason,
                        "startup recovery: resumed task"
                    );
                }
            } else {
                let err = format!(
                    "recovered after restart (was: {}, round: {}, pr: {})",
                    row.status,
                    row.turn,
                    row.task_pr_url.as_deref().unwrap_or("none")
                );
                scheduler.mark_terminal(&TaskStatus::Failed);
                let scheduler_json = serde_json::to_string(&scheduler)?;
                let outcome = self
                    .apply_no_checkpoint_failure_at_version(
                        &row.id,
                        &err,
                        &scheduler,
                        &scheduler_json,
                        row.version,
                    )
                    .await?;
                if observe_recovery_outcome(&row.id, outcome)? {
                    result.failed += 1;
                }
            }
        }

        let transient_failed_rows: Vec<(String, Option<String>, String, i32)> = sqlx::query_as(
            "SELECT id, error, scheduler_state, version FROM tasks \
             WHERE store_key = $1 \
               AND status = 'pending' \
               AND error LIKE 'retrying after transient failure%'",
        )
        .bind(&self.store_key)
        .fetch_all(&self.pool)
        .await?;
        for (id, error, scheduler_state, expected_version) in &transient_failed_rows {
            let mut scheduler = decode_scheduler_state(id, scheduler_state)?;
            scheduler.mark_terminal(&TaskStatus::Failed);
            let scheduler_json = serde_json::to_string(&scheduler)?;
            let err = format!(
                "recovered after restart (was: pending in transient retry): {}",
                error.as_deref().unwrap_or_default()
            );
            let outcome = self
                .apply_transient_failure_at_version(
                    id,
                    &err,
                    &scheduler,
                    &scheduler_json,
                    *expected_version,
                )
                .await?;
            if observe_recovery_outcome(id, outcome)? {
                result.transient_failed += 1;
            }
        }
        if result.transient_failed > 0 {
            tracing::info!(
                "startup recovery: failed {} task(s) that were pending mid-transient-retry",
                result.transient_failed
            );
        }
        Ok(result)
    }

    pub(super) async fn apply_terminal_replay_at_version(
        &self,
        task_id: &str,
        pr_url: Option<&str>,
        status: &TaskStatus,
        scheduler: &TaskSchedulerState,
        scheduler_json: &str,
        expected_version: i32,
    ) -> anyhow::Result<TaskRecoveryWriteOutcome> {
        let resumable = TaskStatus::resumable_statuses();
        let placeholders = Self::numbered_placeholders(7, resumable.len());
        let sql = format!(
            "UPDATE tasks SET status = $1, pr_url = COALESCE($2, pr_url), \
             scheduler_state = $3, updated_at = CURRENT_TIMESTAMP, \
             version = version + 1 \
             WHERE store_key = $4 AND id = $5 AND version = $6 AND status IN ({})",
            placeholders
        );
        let query = sqlx::query(&sql)
            .bind(status.as_str())
            .bind(pr_url)
            .bind(scheduler_json)
            .bind(&self.store_key)
            .bind(task_id)
            .bind(expected_version);
        let query = resumable
            .iter()
            .fold(query, |query, resumable| query.bind(*resumable));
        let rows_affected = query.execute(&self.pool).await?.rows_affected();
        self.finish_recovery_write(
            rows_affected,
            "terminal replay",
            task_id,
            expected_version,
            RecoveryWriteIntent::TerminalReplay {
                status: status.clone(),
                pr_url,
                scheduler,
            },
        )
        .await
    }

    pub(super) async fn apply_pr_replay_at_version(
        &self,
        task_id: &str,
        pr_url: &str,
        expected_version: i32,
    ) -> anyhow::Result<TaskRecoveryWriteOutcome> {
        let rows_affected = sqlx::query(
            "UPDATE tasks SET pr_url = $1, version = version + 1 \
             WHERE store_key = $2 AND id = $3 AND pr_url IS NULL AND version = $4",
        )
        .bind(pr_url)
        .bind(&self.store_key)
        .bind(task_id)
        .bind(expected_version)
        .execute(&self.pool)
        .await?
        .rows_affected();
        self.finish_recovery_write(
            rows_affected,
            "PR-only replay",
            task_id,
            expected_version,
            RecoveryWriteIntent::PrReplay { pr_url },
        )
        .await
    }

    pub(super) async fn apply_checkpoint_resume_with_pr_at_version(
        &self,
        task_id: &str,
        pr_url: &str,
        scheduler: &TaskSchedulerState,
        scheduler_json: &str,
        expected_version: i32,
    ) -> anyhow::Result<TaskRecoveryWriteOutcome> {
        let rows_affected = sqlx::query(
            "UPDATE tasks SET status = 'pending', error = NULL, pr_url = $1, \
             scheduler_state = $2, updated_at = CURRENT_TIMESTAMP, \
             version = version + 1 WHERE store_key = $3 AND id = $4 AND version = $5",
        )
        .bind(pr_url)
        .bind(scheduler_json)
        .bind(&self.store_key)
        .bind(task_id)
        .bind(expected_version)
        .execute(&self.pool)
        .await?
        .rows_affected();
        self.finish_recovery_write(
            rows_affected,
            "checkpoint resume with PR writeback",
            task_id,
            expected_version,
            RecoveryWriteIntent::Resume {
                pr_url: Some(pr_url),
                scheduler,
            },
        )
        .await
    }

    pub(super) async fn apply_checkpoint_resume_without_pr_at_version(
        &self,
        task_id: &str,
        pr_url: Option<&str>,
        scheduler: &TaskSchedulerState,
        scheduler_json: &str,
        expected_version: i32,
    ) -> anyhow::Result<TaskRecoveryWriteOutcome> {
        let rows_affected = sqlx::query(
            "UPDATE tasks SET status = 'pending', error = NULL, \
             scheduler_state = $1, updated_at = CURRENT_TIMESTAMP, \
             version = version + 1 WHERE store_key = $2 AND id = $3 AND version = $4",
        )
        .bind(scheduler_json)
        .bind(&self.store_key)
        .bind(task_id)
        .bind(expected_version)
        .execute(&self.pool)
        .await?
        .rows_affected();
        self.finish_recovery_write(
            rows_affected,
            "checkpoint resume without PR writeback",
            task_id,
            expected_version,
            RecoveryWriteIntent::Resume { pr_url, scheduler },
        )
        .await
    }

    pub(super) async fn apply_no_checkpoint_failure_at_version(
        &self,
        task_id: &str,
        error: &str,
        scheduler: &TaskSchedulerState,
        scheduler_json: &str,
        expected_version: i32,
    ) -> anyhow::Result<TaskRecoveryWriteOutcome> {
        let rows_affected = sqlx::query(
            "UPDATE tasks SET status = 'failed', error = $1, \
             scheduler_state = $2, updated_at = CURRENT_TIMESTAMP, \
             version = version + 1 WHERE store_key = $3 AND id = $4 AND version = $5",
        )
        .bind(error)
        .bind(scheduler_json)
        .bind(&self.store_key)
        .bind(task_id)
        .bind(expected_version)
        .execute(&self.pool)
        .await?
        .rows_affected();
        self.finish_recovery_write(
            rows_affected,
            "no-checkpoint failure",
            task_id,
            expected_version,
            RecoveryWriteIntent::Fail { error, scheduler },
        )
        .await
    }

    pub(super) async fn apply_transient_failure_at_version(
        &self,
        task_id: &str,
        error: &str,
        scheduler: &TaskSchedulerState,
        scheduler_json: &str,
        expected_version: i32,
    ) -> anyhow::Result<TaskRecoveryWriteOutcome> {
        let rows_affected = sqlx::query(
            "UPDATE tasks SET status = 'failed', error = $1, scheduler_state = $2, \
             updated_at = CURRENT_TIMESTAMP, version = version + 1 \
             WHERE store_key = $3 AND id = $4 AND version = $5",
        )
        .bind(error)
        .bind(scheduler_json)
        .bind(&self.store_key)
        .bind(task_id)
        .bind(expected_version)
        .execute(&self.pool)
        .await?
        .rows_affected();
        self.finish_recovery_write(
            rows_affected,
            "transient-retry failure",
            task_id,
            expected_version,
            RecoveryWriteIntent::Fail { error, scheduler },
        )
        .await
    }

    async fn finish_recovery_write(
        &self,
        rows_affected: u64,
        action: &'static str,
        task_id: &str,
        expected_version: i32,
        intent: RecoveryWriteIntent<'_>,
    ) -> anyhow::Result<TaskRecoveryWriteOutcome> {
        match rows_affected {
            1 => return Ok(TaskRecoveryWriteOutcome::Applied),
            0 => {}
            count => {
                anyhow::bail!(
                    "task recovery invariant violation: task {task_id}, action {action}, \
                     expected one affected row, got {count}"
                );
            }
        }

        let snapshot: Option<RecoveryTaskSnapshot> = sqlx::query_as(
            "SELECT status, pr_url, error, scheduler_state, version \
             FROM tasks WHERE store_key = $1 AND id = $2",
        )
        .bind(&self.store_key)
        .bind(task_id)
        .fetch_optional(&self.pool)
        .await?;
        let Some(snapshot) = snapshot else {
            return Ok(TaskRecoveryWriteOutcome::Superseded);
        };
        let current_version = Some(snapshot.version);
        let current_status = snapshot.status.parse::<TaskStatus>()?;
        let current_scheduler = decode_scheduler_state(task_id, &snapshot.scheduler_state)?;

        let superseded = match intent {
            RecoveryWriteIntent::TerminalReplay {
                status,
                pr_url,
                scheduler,
            } => {
                let pr_matches = pr_url.is_none() || snapshot.pr_url.as_deref() == pr_url;
                current_status == status && pr_matches && current_scheduler == *scheduler
            }
            RecoveryWriteIntent::PrReplay { pr_url } => snapshot.pr_url.as_deref() == Some(pr_url),
            RecoveryWriteIntent::Resume { pr_url, scheduler } => {
                let pr_matches = snapshot.pr_url.as_deref() == pr_url;
                let exact_target = current_status == TaskStatus::Pending
                    && snapshot.error.is_none()
                    && current_scheduler == *scheduler
                    && pr_matches;
                exact_target || (current_status.is_terminal() && pr_matches)
            }
            RecoveryWriteIntent::Fail { error, scheduler } => {
                let exact_target = current_status == TaskStatus::Failed
                    && snapshot.error.as_deref() == Some(error)
                    && current_scheduler == *scheduler;
                exact_target || current_status.is_terminal()
            }
        };

        if superseded {
            Ok(TaskRecoveryWriteOutcome::Superseded)
        } else {
            Ok(TaskRecoveryWriteOutcome::Conflict {
                action,
                task_id: task_id.to_string(),
                expected_version,
                current_version,
            })
        }
    }
}

pub(super) fn observe_recovery_outcome(
    task_id: &str,
    outcome: TaskRecoveryWriteOutcome,
) -> anyhow::Result<bool> {
    if matches!(outcome, TaskRecoveryWriteOutcome::Superseded) {
        tracing::debug!(
            task_id,
            "startup recovery: action superseded by authoritative durable state"
        );
    }
    outcome.applied_or_error()
}

fn decode_scheduler_state(
    task_id: &str,
    scheduler_state: &str,
) -> Result<TaskSchedulerState, TaskDbDecodeError> {
    serde_json::from_str::<TaskSchedulerState>(scheduler_state).map_err(|source| {
        TaskDbDecodeError::SchedulerStateDeserialize {
            task_id: task_id.to_string(),
            source,
        }
    })
}
