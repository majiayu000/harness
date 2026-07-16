use crate::task_db::TaskDb;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use futures::StreamExt;
use harness_core::agent::StreamItem;
use harness_core::db::PgStoreContext;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, Mutex, RwLock};

use super::metrics::{DashboardCounts, LlmMetricsInputs, ProjectCounts};
use super::state::{SchedulerAuthorityState, SchedulerOwnerKind, TaskState, TaskSummary};
use super::types::{TaskId, TaskKind, TaskStatus, TaskTerminalOutcome};
use super::CompletionCallback;

mod projections;
mod queries;
mod runtime_control;
mod runtime_hosts;
mod startup;

/// Broadcast channel capacity for per-task stream events.
/// When the buffer is full, the oldest events are dropped for lagging receivers.
const TASK_STREAM_CAPACITY: usize = 512;
const RECOVERED_PR_VIEW_TIMEOUT: Duration = Duration::from_secs(10);
const RECOVERED_PR_VALIDATION_CONCURRENCY: usize = 8;

fn record_task_runner_usage() {
    harness_core::usage_probe::record_usage(
        harness_core::usage_probe::UsageProbeSurface::TaskRunner,
    );
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RecoveredPrCandidate {
    task_id: TaskId,
    pr_url: String,
}

#[derive(Debug)]
struct RecoveredPrStatusUpdate {
    task_id: TaskId,
    pr_url: String,
    state: String,
    status: TaskStatus,
}

pub struct TaskStore {
    pub(crate) cache: DashMap<TaskId, TaskState>,
    pub(super) db: TaskDb,
    persist_locks: DashMap<TaskId, Arc<Mutex<()>>>,
    /// Per-task broadcast channels for real-time stream forwarding to SSE clients.
    stream_txs: DashMap<TaskId, broadcast::Sender<StreamItem>>,
    /// Per-task abort handles for cooperative cancellation via Tokio task abort.
    abort_handles: DashMap<TaskId, tokio::task::AbortHandle>,
    /// Global circuit breaker: when the CLI account-level limit is hit,
    /// all tasks pause until this instant passes.
    rate_limit_until: RwLock<Option<tokio::time::Instant>>,
    /// Append-only JSONL event log for crash recovery. `None` if the file
    /// could not be opened (best-effort; server still starts without it).
    pub(crate) event_log: Option<Arc<crate::event_replay::TaskEventLog>>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TaskSummaryFilter {
    pub statuses: Vec<TaskStatus>,
    pub scheduler_states: Vec<SchedulerAuthorityState>,
    pub kinds: Vec<TaskKind>,
    pub source: Option<String>,
    pub repo: Option<String>,
    pub project: Option<String>,
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskSummaryPageCursor {
    pub created_at: DateTime<Utc>,
    pub id: String,
}

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

    /// Validate recovered pending tasks by checking their GitHub PR state via `gh`.
    ///
    /// Spawned as a background task from `http.rs` after the completion callback is
    /// built, so it does not block server startup. For each pending task that has a
    /// `pr_url`, fetches the current PR state with a per-call timeout:
    /// - MERGED → mark Done  (completion_callback invoked)
    /// - CLOSED → mark Failed (completion_callback invoked so intake sources can
    ///   remove the issue from their `dispatched` map and allow retry)
    /// - OPEN   → leave as Pending
    ///
    /// GitHub API failures are treated as transient network errors; the task is left
    /// Pending so it will be retried normally.
    pub async fn validate_recovered_tasks(&self, completion_callback: Option<CompletionCallback>) {
        self.validate_recovered_tasks_with_github(completion_callback, None)
            .await;
    }

    pub async fn validate_recovered_tasks_with_token(
        &self,
        completion_callback: Option<CompletionCallback>,
        github_token: Option<&str>,
    ) {
        self.validate_recovered_tasks_with_github(completion_callback, github_token)
            .await;
    }

    async fn validate_recovered_tasks_with_github(
        &self,
        completion_callback: Option<CompletionCallback>,
        github_token: Option<&str>,
    ) {
        let candidates = self.collect_recovered_pr_candidates();

        if candidates.is_empty() {
            return;
        }

        tracing::info!(
            "startup: validating {} recovered pending task(s) with PR URLs",
            candidates.len()
        );

        // Probe PR states concurrently, but keep persistence and callbacks
        // serialized so terminal-state side effects preserve their current order.
        let mut results = futures::stream::iter(candidates)
            .map(|candidate| check_recovered_pr_state(candidate, github_token))
            .buffer_unordered(RECOVERED_PR_VALIDATION_CONCURRENCY);

        while let Some(result) = results.next().await {
            let Some(RecoveredPrStatusUpdate {
                task_id,
                pr_url,
                state,
                status,
            }) = result
            else {
                continue;
            };

            if let Some(mut entry) = self.cache.get_mut(&task_id) {
                entry.status = status;
            }
            // Persist before invoking the callback. If persist fails the task
            // remains `pending` in SQLite; firing the callback anyway would push
            // external state (Feishu notifications, GitHub intake cleanup) into a
            // terminal state while the DB still thinks the task is pending. On
            // the next restart the same task would be recovered and trigger the
            // same side-effects again (state split). Skip the callback so the
            // task can be safely retried on the next restart.
            if let Err(e) = self.persist(&task_id).await {
                tracing::error!(
                    task_id = %task_id.0,
                    "failed to persist PR state update: {e}; skipping completion callback to avoid state split"
                );
                continue;
            }
            tracing::info!(
                task_id = %task_id.0,
                pr_url,
                "startup recovery: PR state {state} → task status updated"
            );
            if let Some(cb) = &completion_callback {
                if let Some(final_state) = self.get(&task_id) {
                    cb(final_state).await;
                }
            }
        }
    }

    fn collect_recovered_pr_candidates(&self) -> Vec<RecoveredPrCandidate> {
        self.cache
            .iter()
            .filter_map(|entry| recovered_pr_candidate(entry.value()))
            .collect()
    }
}

fn recovered_pr_candidate(task: &TaskState) -> Option<RecoveredPrCandidate> {
    if !matches!(task.status, TaskStatus::Pending) {
        return None;
    }
    let pr_url = task.pr_url.as_ref()?.clone();
    if super::spawn::parse_pr_url(&pr_url).is_none() {
        tracing::warn!(
            task_id = %task.id.0,
            pr_url,
            "could not parse PR URL; leaving pending"
        );
        return None;
    }
    Some(RecoveredPrCandidate {
        task_id: task.id.clone(),
        pr_url,
    })
}

async fn check_recovered_pr_state(
    candidate: RecoveredPrCandidate,
    github_token: Option<&str>,
) -> Option<RecoveredPrStatusUpdate> {
    let RecoveredPrCandidate { task_id, pr_url } = candidate;
    let Some((owner, repo, pr_number)) = super::spawn::parse_pr_url(&pr_url) else {
        tracing::warn!(
            task_id = %task_id.0,
            pr_url,
            "could not parse PR URL during startup recovery; leaving pending"
        );
        return None;
    };
    let client = reqwest::Client::new();
    let mut request = client
        .get(format!(
            "https://api.github.com/repos/{owner}/{repo}/pulls/{pr_number}"
        ))
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .header(reqwest::header::USER_AGENT, "harness-server");
    if let Some(token) = crate::github_auth::resolve_github_token(github_token) {
        request = request.bearer_auth(token);
    }
    let response = match tokio::time::timeout(RECOVERED_PR_VIEW_TIMEOUT, request.send()).await {
        Err(_elapsed) => {
            tracing::warn!(
                task_id = %task_id.0,
                pr_url,
                "GitHub PR lookup timed out after 10s; leaving pending"
            );
            return None;
        }
        Ok(Err(e)) => {
            tracing::warn!(
                task_id = %task_id.0,
                pr_url,
                "GitHub PR lookup error: {e}; leaving pending"
            );
            return None;
        }
        Ok(Ok(response)) => response,
    };

    if !response.status().is_success() {
        tracing::warn!(
            task_id = %task_id.0,
            pr_url,
            status = %response.status(),
            "GitHub PR lookup failed; leaving pending"
        );
        return None;
    }

    let body = match response.json::<serde_json::Value>().await {
        Ok(body) => body,
        Err(e) => {
            tracing::warn!(
                task_id = %task_id.0,
                pr_url,
                "GitHub PR lookup response parse failed: {e}; leaving pending"
            );
            return None;
        }
    };
    let state = body
        .get("state")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let merged = body
        .get("merged_at")
        .and_then(|value| value.as_str())
        .is_some_and(|value| !value.trim().is_empty());
    let new_status = match (state.as_str(), merged) {
        ("closed", true) => Some(TaskStatus::Done),
        ("closed", false) => Some(TaskStatus::Failed),
        _ => None,
    };

    if let Some(status) = new_status {
        Some(RecoveredPrStatusUpdate {
            task_id,
            pr_url,
            state: if merged {
                "MERGED".to_string()
            } else {
                state.to_ascii_uppercase()
            },
            status,
        })
    } else {
        tracing::info!(
            task_id = %task_id.0,
            pr_url,
            "startup recovery: PR state {state} → leaving pending"
        );
        None
    }
}

pub async fn update_status(
    store: &TaskStore,
    task_id: &TaskId,
    status: TaskStatus,
    turn: u32,
) -> anyhow::Result<()> {
    // Terminal-state preservation: once a task has reached Done, Failed, or
    // Cancelled, refuse to transition it to any other status. This prevents
    // late-running execution paths (e.g. a task that was cancelled while
    // waiting for a queue permit) from resurrecting a terminal task to
    // Triaging / Implementing / Reviewing or overwriting one terminal state
    // with another. Issue #1046.
    if let Some(existing) = store.get(task_id) {
        if existing.status.is_terminal() && existing.status != status {
            tracing::warn!(
                task_id = %task_id.0,
                current = existing.status.as_ref(),
                attempted = status.as_ref(),
                "refusing to overwrite terminal task status"
            );
            return Ok(());
        }
    }
    let status_str = status.as_ref().to_string();
    // Track whether the inner closure actually applied the status change.
    // When the inner re-check refuses (race: another writer marked terminal
    // between the snapshot above and the exclusive cache lock), `mutate_and_persist`
    // still persists the row but the durable status does not change. The
    // StatusChanged event must reflect that — logging it for a status that was
    // never written would put the event log out of sync with the durable row.
    let applied = std::sync::atomic::AtomicBool::new(false);
    mutate_and_persist(store, task_id, |s| {
        if s.status.is_terminal() && s.status != status {
            return;
        }
        s.status = status.clone();
        s.turn = turn;
        match status {
            TaskStatus::Pending => s.scheduler.clear_to_queued(),
            TaskStatus::AwaitingDeps => {
                s.scheduler = crate::task_runner::TaskSchedulerState::awaiting_dependencies()
            }
            TaskStatus::Done | TaskStatus::Failed | TaskStatus::Cancelled => {
                s.scheduler.mark_terminal(&status)
            }
            _ => {
                if matches!(
                    s.scheduler.authority_state,
                    crate::task_runner::SchedulerAuthorityState::Queued
                        | crate::task_runner::SchedulerAuthorityState::Recovering
                        | crate::task_runner::SchedulerAuthorityState::RetryBackoff
                        | crate::task_runner::SchedulerAuthorityState::AwaitingDependencies
                ) {
                    s.scheduler.claim_scheduler("local-scheduler");
                }
            }
        }
        applied.store(true, std::sync::atomic::Ordering::Relaxed);
    })
    .await?;
    if applied.load(std::sync::atomic::Ordering::Relaxed) {
        store.log_event(crate::event_replay::TaskEvent::StatusChanged {
            task_id: task_id.0.clone(),
            ts: crate::event_replay::now_ts(),
            status: status_str,
            turn,
        });
    } else {
        tracing::debug!(
            task_id = %task_id.0,
            attempted = status_str,
            "update_status skipped: inner guard refused (terminal-state preservation); not logging StatusChanged"
        );
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalTransition {
    Applied,
    AlreadyTerminal(TaskStatus),
    MissingTask,
}

impl TerminalTransition {
    pub fn applied(&self) -> bool {
        matches!(self, Self::Applied)
    }
}

pub async fn mark_terminal_once(
    store: &TaskStore,
    id: &TaskId,
    outcome: TaskTerminalOutcome,
) -> anyhow::Result<TerminalTransition> {
    let lock = store
        .persist_locks
        .entry(id.clone())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone();
    let _guard = lock.lock().await;

    let mut rollback_state = None;
    let mut state_to_persist = None;
    let mut event = None;
    let transition = if let Some(mut entry) = store.cache.get_mut(id) {
        if entry.status.is_terminal() {
            TerminalTransition::AlreadyTerminal(entry.status.clone())
        } else {
            let original = entry.value().clone();
            let status = outcome.status();
            let reason = outcome.reason_string();
            entry.apply_terminal_outcome_once(outcome);
            let attempted = entry.value().clone();
            state_to_persist = Some(attempted.clone());
            rollback_state = Some((original, attempted));
            event = Some(match status {
                TaskStatus::Done => crate::event_replay::TaskEvent::Completed {
                    task_id: id.0.clone(),
                    ts: crate::event_replay::now_ts(),
                },
                TaskStatus::Failed => crate::event_replay::TaskEvent::Failed {
                    task_id: id.0.clone(),
                    ts: crate::event_replay::now_ts(),
                    reason: reason.unwrap_or_else(|| "failed".to_string()),
                },
                TaskStatus::Cancelled => crate::event_replay::TaskEvent::Cancelled {
                    task_id: id.0.clone(),
                    ts: crate::event_replay::now_ts(),
                    reason,
                },
                _ => unreachable!("terminal outcome produced non-terminal status"),
            });
            TerminalTransition::Applied
        }
    } else {
        TerminalTransition::MissingTask
    };

    if transition.applied() {
        let Some(state) = state_to_persist else {
            anyhow::bail!("terminal transition was applied without a persisted task snapshot");
        };
        if let Err(error) = store.db.update(&state).await {
            if let Some((original, attempted)) = rollback_state {
                if let Some(mut entry) = store.cache.get_mut(id) {
                    if task_states_match_for_rollback(entry.value(), &attempted) {
                        *entry.value_mut() = original;
                    } else {
                        tracing::warn!(
                            task_id = %id.0,
                            "mark_terminal_once rollback skipped because cache changed after failed persist"
                        );
                    }
                }
            }
            return Err(error);
        }
        if let Some(mut entry) = store.cache.get_mut(id) {
            entry.value_mut().version = entry.value().version.saturating_add(1);
        }
        if let Some(event) = event {
            store.log_event(event);
        }
    }

    Ok(transition)
}

/// Mutate a task in the cache then persist to SQLite.
pub async fn mutate_and_persist(
    store: &TaskStore,
    id: &TaskId,
    f: impl FnOnce(&mut TaskState),
) -> anyhow::Result<()> {
    let mut rollback_state = None;
    if let Some(mut entry) = store.cache.get_mut(id) {
        let original = entry.value().clone();
        f(entry.value_mut());
        entry.value_mut().reconcile_scheduler_with_status();
        let attempted = entry.value().clone();
        rollback_state = Some((original, attempted));
    }
    if let Err(error) = store.persist(id).await {
        if let Some((original, attempted)) = rollback_state {
            if let Some(mut entry) = store.cache.get_mut(id) {
                if task_states_match_for_rollback(entry.value(), &attempted) {
                    *entry.value_mut() = original;
                } else {
                    tracing::warn!(
                        task_id = %id.0,
                        "mutate_and_persist rollback skipped because cache changed after failed persist"
                    );
                }
            }
        }
        return Err(error);
    }
    Ok(())
}

fn task_states_match_for_rollback(current: &TaskState, attempted: &TaskState) -> bool {
    match (
        serde_json::to_value(current),
        serde_json::to_value(attempted),
    ) {
        (Ok(current), Ok(attempted)) => current == attempted,
        (Err(error), _) | (_, Err(error)) => {
            tracing::warn!("mutate_and_persist rollback comparison failed: {error}");
            false
        }
    }
}

fn latest_timestamp_at_least(candidate: Option<&str>, existing: Option<&str>) -> bool {
    match (candidate, existing) {
        (Some(candidate), Some(existing)) => candidate >= existing,
        (Some(_), None) => true,
        (None, Some(_)) => false,
        (None, None) => true,
    }
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod store_tests;
