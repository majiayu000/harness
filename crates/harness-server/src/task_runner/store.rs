use crate::task_db::TaskDb;
use dashmap::DashMap;
use harness_core::agent::StreamItem;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex, RwLock};

use super::metrics::{DashboardCounts, LlmMetricsInputs, ProjectCounts};
use super::state::{TaskState, TaskSummary};
use super::types::{TaskId, TaskStatus};
use super::CompletionCallback;

/// Broadcast channel capacity for per-task stream events.
/// When the buffer is full, the oldest events are dropped for lagging receivers.
const TASK_STREAM_CAPACITY: usize = 512;

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

impl TaskStore {
    pub async fn open(db_path: &std::path::Path) -> anyhow::Result<Arc<Self>> {
        let db = TaskDb::open(db_path).await?;

        // 1. Event replay: runs BEFORE recover_in_progress so event-sourced
        //    data (pr_url, terminal status) wins over checkpoint data.
        let event_log_path = db_path
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .join("task-events.jsonl");
        if let Err(e) = crate::event_replay::replay_and_recover(&db, &event_log_path).await {
            tracing::warn!("startup: event replay failed (non-fatal): {e}");
        }

        // 2. Legacy checkpoint-based recovery as fallback.
        let recovery = db.recover_in_progress().await?;
        if recovery.resumed > 0 {
            tracing::info!(
                "startup recovery: resumed {} task(s) from checkpoint",
                recovery.resumed
            );
        }
        if recovery.failed > 0 {
            tracing::warn!(
                "startup recovery: marked {} interrupted task(s) as failed (no fresh checkpoint)",
                recovery.failed
            );
        }

        // 3. Open the event log for appending during this server session.
        let event_log = match crate::event_replay::TaskEventLog::open(&event_log_path) {
            Ok(log) => {
                tracing::debug!("task event log: {}", event_log_path.display());
                Some(Arc::new(log))
            }
            Err(e) => {
                tracing::warn!(
                    "failed to open task event log at {}: {e}",
                    event_log_path.display()
                );
                None
            }
        };

        let cache = DashMap::new();
        let persist_locks = DashMap::new();
        // Only load active (non-terminal) tasks into the in-memory cache to prevent
        // unbounded memory growth from historical completed tasks.
        let active_statuses = &[
            "pending",
            "awaiting_deps",
            "implementing",
            "agent_review",
            "waiting",
            "reviewing",
        ];
        for task in db.list_by_status(active_statuses).await? {
            persist_locks.insert(task.id.clone(), Arc::new(Mutex::new(())));
            cache.insert(task.id.clone(), task);
        }
        let store = Arc::new(Self {
            cache,
            db,
            persist_locks,
            stream_txs: DashMap::new(),
            abort_handles: DashMap::new(),
            rate_limit_until: RwLock::new(None),
            event_log,
        });
        Ok(store)
    }

    pub fn get(&self, id: &TaskId) -> Option<TaskState> {
        self.cache.get(id).map(|r| r.value().clone())
    }

    /// Look up a task by ID, checking the in-memory cache first.
    /// Falls back to the database for terminal tasks that were evicted from
    /// the cache at startup (Done, Failed, Cancelled).
    /// Returns `Ok(None)` only when the ID is unknown in both cache and DB.
    /// Returns `Err` when the database query itself fails.
    pub async fn get_with_db_fallback(&self, id: &TaskId) -> anyhow::Result<Option<TaskState>> {
        if let Some(task) = self.cache.get(id) {
            return Ok(Some(task.value().clone()));
        }
        self.db.get(id.0.as_str()).await
    }

    /// Return the status of a dependency task with a single DB lookup.
    ///
    /// Checks the in-memory cache first; falls back to a lightweight
    /// `SELECT status` query (no `rounds` JSON decode) for terminal tasks
    /// evicted from the startup cache.  Returns `None` when the task is
    /// unknown in both cache and DB, or when the DB call fails.
    pub(crate) async fn dep_status(&self, id: &TaskId) -> Option<TaskStatus> {
        if let Some(task) = self.cache.get(id) {
            return Some(task.status.clone());
        }
        match self.db.get_status_only(id.0.as_str()).await {
            Ok(Some(s)) => s.parse::<TaskStatus>().ok(),
            Ok(None) => None,
            Err(e) => {
                tracing::warn!(task_id = %id.0, "DB error fetching dep status: {e}; treating as absent");
                None
            }
        }
    }

    /// Return tasks that are in an active status and have not been updated for
    /// at least `stale_threshold`.  Delegates to [`TaskDb::list_stalled_tasks`].
    pub async fn list_stalled_tasks(
        &self,
        stale_threshold: std::time::Duration,
        project: Option<&str>,
    ) -> anyhow::Result<Vec<TaskState>> {
        self.db.list_stalled_tasks(stale_threshold, project).await
    }

    /// Return IDs of terminal tasks (Done, Failed, Cancelled) directly from the database.
    ///
    /// Used during startup for worktree cleanup. Only fetches task IDs to avoid
    /// deserializing the heavy `rounds` column for large historical datasets.
    pub async fn list_terminal_ids_from_db(&self) -> anyhow::Result<Vec<TaskId>> {
        let ids = self
            .db
            .list_ids_by_status(&["done", "failed", "cancelled"])
            .await?;
        Ok(ids.into_iter().map(harness_core::types::TaskId).collect())
    }

    pub fn count(&self) -> usize {
        self.cache.len()
    }

    /// Check whether an active (non-terminal) task already exists for the same
    /// project + external_id. Cache-first, DB fallback.
    pub async fn find_active_duplicate(
        &self,
        project_id: &str,
        external_id: &str,
    ) -> Option<TaskId> {
        let mut found_terminal_in_cache = false;
        for entry in self.cache.iter() {
            let task = entry.value();
            let same_key = task.external_id.as_deref() == Some(external_id)
                && task
                    .project_root
                    .as_ref()
                    .map(|p| p.to_string_lossy() == project_id)
                    .unwrap_or(false);
            if !same_key {
                continue;
            }
            if !matches!(
                task.status,
                TaskStatus::Done | TaskStatus::Failed | TaskStatus::Cancelled
            ) {
                return Some(task.id.clone());
            }
            // Cache has a terminal match — skip DB fallback since cache is
            // more authoritative and the DB row may be stale.
            found_terminal_in_cache = true;
        }
        if found_terminal_in_cache {
            return None;
        }
        match self.db.find_active_duplicate(project_id, external_id).await {
            Ok(Some(id)) => Some(harness_core::types::TaskId(id)),
            Ok(None) => None,
            Err(e) => {
                tracing::warn!("dedup: DB lookup failed: {e}");
                None
            }
        }
    }

    /// Check whether a `done` task with a PR URL already exists for the same
    /// project + external_id. Cache-first, DB fallback. Fail-open on DB errors.
    pub async fn find_terminal_pr_duplicate(
        &self,
        project_id: &str,
        external_id: &str,
    ) -> Option<(TaskId, String)> {
        for entry in self.cache.iter() {
            let task = entry.value();
            let same_key = task.external_id.as_deref() == Some(external_id)
                && task
                    .project_root
                    .as_ref()
                    .map(|p| p.to_string_lossy() == project_id)
                    .unwrap_or(false);
            if same_key && matches!(task.status, TaskStatus::Done) {
                if let Some(ref url) = task.pr_url {
                    return Some((task.id.clone(), url.clone()));
                }
            }
        }
        match self.db.find_terminal_with_pr(project_id, external_id).await {
            Ok(Some((id, url))) => Some((harness_core::types::TaskId(id), url)),
            Ok(None) => None,
            Err(e) => {
                tracing::warn!("dedup: terminal PR DB lookup failed: {e}");
                None
            }
        }
    }

    /// Return all tasks currently in the in-memory cache.
    ///
    /// **Semantic note**: since startup only loads active (non-terminal) tasks
    /// into the cache, this method returns only Pending, AwaitingDeps,
    /// Implementing, AgentReview, Waiting, and Reviewing tasks.
    /// To look up a specific completed task use [`get_with_db_fallback`].
    /// To enumerate all tasks including historical ones use [`list_all_with_terminal`].
    pub fn list_all(&self) -> Vec<TaskState> {
        self.cache.iter().map(|e| e.value().clone()).collect()
    }

    /// Return all tasks: active ones from cache (most current state) merged
    /// with terminal tasks from the database (Done, Failed, Cancelled).
    ///
    /// Cache entries take precedence over DB rows for the same task ID so that
    /// in-flight state is always reflected accurately.
    pub async fn list_all_with_terminal(&self) -> anyhow::Result<Vec<TaskState>> {
        // DB is the authoritative record of all tasks ever created.
        let mut by_id: std::collections::HashMap<TaskId, TaskState> = self
            .db
            .list()
            .await?
            .into_iter()
            .map(|t| (t.id.clone(), t))
            .collect();
        // Override with cache values — these carry the live in-flight state.
        for entry in self.cache.iter() {
            by_id.insert(entry.key().clone(), entry.value().clone());
        }
        Ok(by_id.into_values().collect())
    }

    /// Return all tasks as lightweight [`TaskSummary`] values.
    ///
    /// Fetches summary columns from the DB (skipping the heavy `rounds` field),
    /// then overrides with live cache entries so in-flight state is accurate.
    /// Use this in the `/tasks` list endpoint instead of `list_all_with_terminal`.
    pub async fn list_all_summaries_with_terminal(&self) -> anyhow::Result<Vec<TaskSummary>> {
        let mut by_id: std::collections::HashMap<TaskId, TaskSummary> = self
            .db
            .list_summaries()
            .await?
            .into_iter()
            .map(|s| (s.id.clone(), s))
            .collect();
        for entry in self.cache.iter() {
            by_id.insert(entry.key().clone(), entry.value().summary());
        }
        Ok(by_id.into_values().collect())
    }

    /// Return `(TaskId, TaskStatus)` pairs for all tasks without deserializing `rounds`.
    ///
    /// Hot-path callers (skill governance, token usage attribution) that only need
    /// task status should use this instead of `list_all_with_terminal`.
    pub async fn list_all_statuses_with_terminal(
        &self,
    ) -> anyhow::Result<HashMap<TaskId, TaskStatus>> {
        let mut by_id: HashMap<TaskId, TaskStatus> = self
            .db
            .list_id_status()
            .await?
            .into_iter()
            .filter_map(|(id, status)| {
                status
                    .parse::<TaskStatus>()
                    .ok()
                    .map(|s| (harness_core::types::TaskId(id), s))
            })
            .collect();
        for entry in self.cache.iter() {
            by_id.insert(entry.key().clone(), entry.value().status.clone());
        }
        Ok(by_id)
    }

    /// Run `f` only if the task still exists and is live-`pending`.
    ///
    /// Holding the mutable cache guard across `f` prevents a concurrent status
    /// transition from changing this task out of `pending` between the check
    /// and a follow-up side effect such as runtime-host lease insertion.
    pub(crate) fn with_task_if_pending<R>(&self, id: &TaskId, f: impl FnOnce() -> R) -> Option<R> {
        let entry = self.cache.get_mut(id)?;
        if !matches!(entry.status, TaskStatus::Pending) {
            return None;
        }
        Some(f())
    }

    /// Return the `pr_url` of the most recently created Done task, ordered by `created_at DESC`
    /// from the database (stable ordering, unlike the in-memory DashMap cache).
    pub async fn latest_done_pr_url(&self) -> Option<String> {
        match self.db.latest_done_pr_url().await {
            Ok(url) => url,
            Err(e) => {
                tracing::warn!("failed to query latest done PR URL: {e}");
                None
            }
        }
    }

    /// Return the `pr_url` of the most recent Done task for a specific project root path.
    pub async fn latest_done_pr_url_by_project(&self, project: &str) -> Option<String> {
        match self.db.latest_done_pr_url_by_project(project).await {
            Ok(url) => url,
            Err(e) => {
                tracing::warn!("failed to query latest done PR URL for project {project}: {e}");
                None
            }
        }
    }

    /// Fetch the latest done PR URL for every project in a single DB query.
    /// Returns a map from project root path string to PR URL.
    pub async fn latest_done_pr_urls_all_projects(&self) -> HashMap<String, String> {
        match self.db.latest_done_pr_urls_all_projects().await {
            Ok(map) => map,
            Err(e) => {
                tracing::warn!("failed to bulk-query latest done PR URLs: {e}");
                HashMap::new()
            }
        }
    }

    /// Return all inflight sibling tasks on the same project, excluding `exclude_id`.
    ///
    /// Used by `run_task` to build sibling-awareness context before starting implementation,
    /// so each agent knows what other agents are working on and avoids touching their files.
    /// Only tasks that have `project_root` set (populated at spawn time) are considered.
    pub fn list_siblings(&self, project: &std::path::Path, exclude_id: &TaskId) -> Vec<TaskState> {
        self.cache
            .iter()
            .filter(|e| {
                let task = e.value();
                &task.id != exclude_id
                    && (matches!(task.status, TaskStatus::Pending) || task.status.is_inflight())
                    && task.project_root.as_deref() == Some(project)
            })
            .map(|e| e.value().clone())
            .collect()
    }

    /// Compute global and per-project done/failed counts via SQL aggregation.
    ///
    /// Delegates to `TaskDb` so the count scales with the database engine rather
    /// than requiring an O(N) scan of the in-memory cache, which grows unboundedly
    /// as tasks accumulate. Uses the `idx_tasks_project_status_updated` index.
    pub async fn count_for_dashboard(&self) -> DashboardCounts {
        match self.db.count_done_failed_by_project().await {
            Ok((global_done, global_failed, rows)) => {
                let by_project = rows
                    .into_iter()
                    .map(|(project, done, failed)| (project, ProjectCounts { done, failed }))
                    .collect();
                DashboardCounts {
                    global_done,
                    global_failed,
                    by_project,
                }
            }
            Err(e) => {
                tracing::warn!("count_for_dashboard: SQL aggregation failed: {e}");
                DashboardCounts {
                    global_done: 0,
                    global_failed: 0,
                    by_project: HashMap::new(),
                }
            }
        }
    }

    /// Count tasks that completed (`done`) with `updated_at >= since`.
    ///
    /// Forwards to [`TaskDb::count_done_since`]; used by the system overview
    /// to compute merged-in-last-24h without materialising full task rows.
    pub async fn count_done_since(&self, since: chrono::DateTime<chrono::Utc>) -> u64 {
        match self.db.count_done_since(since).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("count_done_since: SQL aggregation failed: {e}");
                0
            }
        }
    }

    /// Per-(project, hour) done counts since `since`. Forwards to
    /// [`TaskDb::done_per_project_hour_since`] and returns an empty vec on
    /// error so callers can degrade gracefully.
    pub async fn done_per_project_hour_since(
        &self,
        since: chrono::DateTime<chrono::Utc>,
    ) -> Vec<(String, String, u64)> {
        match self.db.done_per_project_hour_since(since).await {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!("done_per_project_hour_since: SQL aggregation failed: {e}");
                Vec::new()
            }
        }
    }

    /// Collect lightweight inputs for LLM metrics computation.
    ///
    /// Both **turn counts** and **first-token latencies** are collected in two
    /// bounded phases so the dashboard poll remains O(1) regardless of total
    /// task history size:
    ///
    /// 1. **Cache phase** — iterate `DashMap` refs in-place (no full `TaskState`
    ///    clone).  Records the IDs seen so phase 2 can deduplicate.
    /// 2. **DB phase** — bounded SQL queries (`LIMIT 500`, `ORDER BY updated_at DESC`)
    ///    return only scalar columns (`turn`, `first_token_latency_ms`) for the
    ///    500 most-recently-completed terminal tasks not already counted from cache.
    ///    This covers the idle-queue restart case without an unbounded table scan.
    ///
    /// Rounds whose `result` is `"resumed_checkpoint"` are skipped in both
    /// sources: they are synthetic markers with no real latency data.
    pub async fn collect_llm_metrics_inputs(&self) -> LlmMetricsInputs {
        // Phase 1: iterate cache refs in-place; never clone full TaskState.
        // Collect both turn counts and latencies in a single pass, and track
        // IDs seen so we can deduplicate against the DB queries in phase 2.
        //
        // Only terminal (done/failed) tasks are counted so the cache phase matches
        // DB phase semantics: both sources use the same status set.  Non-terminal
        // task IDs are still inserted into `cache_ids` so the DB deduplication
        // logic remains correct (we never double-count a task that transitions to
        // terminal while the DB query is in-flight).
        let mut cache_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut turn_counts: Vec<u32> = Vec::new();
        let mut first_token_latencies: Vec<u64> = Vec::new();
        for entry in self.cache.iter() {
            let task = entry.value();
            cache_ids.insert(entry.key().0.clone());

            // Skip non-terminal tasks to match the DB phase (done/failed only).
            if !matches!(task.status, TaskStatus::Done | TaskStatus::Failed) {
                continue;
            }

            if task.turn > 0 {
                turn_counts.push(task.turn);
            }

            // Skip the synthetic resumed_checkpoint round injected at recovery time,
            // then find the first round that actually received a response token.
            // Using find_map (instead of find + and_then) means transient_retry rounds
            // whose first_token_latency_ms is None are also skipped, so a task with
            // one or more transient failures before a successful round is still included
            // in p50 using the latency of that successful round.
            if let Some(latency) = task
                .rounds
                .iter()
                .filter(|r| r.result != "resumed_checkpoint")
                .find_map(|r| r.first_token_latency_ms)
            {
                first_token_latencies.push(latency);
            }
        }

        // Phase 2a: bounded DB query for terminal turn counts not in cache.
        // After restart with idle queue the cache is empty; without this, turn
        // counts would be zero even though history is persisted in the DB.
        // `list_terminal_turn_counts` fetches only `id` and `turn` (no rounds
        // blob) and is bounded to 500 rows ordered by updated_at DESC.
        match self.db.list_terminal_turn_counts().await {
            Ok(rows) => {
                for (id, turn) in rows {
                    if !cache_ids.contains(&id) && turn > 0 {
                        turn_counts.push(turn as u32);
                    }
                }
            }
            Err(e) => {
                tracing::warn!("collect_llm_metrics_inputs: DB turn counts query failed: {e}");
            }
        }

        // Phase 2b: bounded DB query for terminal latencies not in cache.
        // `list_terminal_first_token_latencies_ms` extracts the scalar in SQL via
        // json_extract (no full rounds blob in Rust memory) and is bounded to the
        // 500 most-recently-completed terminal tasks so each dashboard poll stays O(1).
        match self.db.list_terminal_first_token_latencies_ms().await {
            Ok(rows) => {
                for (id, latency_opt) in rows {
                    if cache_ids.contains(&id) {
                        // Already counted via cache iteration above.
                        continue;
                    }
                    if let Some(ms) = latency_opt {
                        first_token_latencies.push(ms as u64);
                    }
                }
            }
            Err(e) => {
                tracing::warn!("collect_llm_metrics_inputs: DB terminal latency query failed: {e}");
            }
        }

        LlmMetricsInputs {
            turn_counts,
            first_token_latencies,
        }
    }

    /// Return all cached tasks whose `parent_id` matches the given ID.
    /// Reconstructs the child list from in-memory state; does not require
    /// `subtask_ids` to be persisted on the parent.
    pub fn list_children(&self, parent_id: &TaskId) -> Vec<TaskState> {
        self.cache
            .iter()
            .filter(|e| e.value().parent_id.as_ref() == Some(parent_id))
            .map(|e| e.value().clone())
            .collect()
    }

    /// Register a broadcast channel for a task's stream. Call once before task execution starts.
    pub(crate) fn register_task_stream(&self, id: &TaskId) {
        let (tx, _rx) = broadcast::channel(TASK_STREAM_CAPACITY);
        self.stream_txs.insert(id.clone(), tx);
    }

    /// Subscribe to a task's active stream. Returns `None` if no stream is registered.
    pub fn subscribe_task_stream(&self, id: &TaskId) -> Option<broadcast::Receiver<StreamItem>> {
        self.stream_txs.get(id).map(|tx| tx.subscribe())
    }

    /// Publish a [`StreamItem`] to all current subscribers of a task stream.
    /// No-op when no stream is registered. Dropping the send result is intentional:
    /// `SendError` only occurs when there are no active receivers, which is normal
    /// when no SSE client is connected (backpressure: oldest events dropped on lag).
    pub(crate) fn publish_stream_item(&self, id: &TaskId, item: StreamItem) {
        if let Some(tx) = self.stream_txs.get(id) {
            if let Err(e) = tx.send(item) {
                tracing::trace!(task_id = %id.0, "stream publish dropped (no receivers): {e}");
            }
        }
    }

    /// Remove the task's stream channel after execution completes.
    pub(crate) fn close_task_stream(&self, id: &TaskId) {
        self.stream_txs.remove(id);
    }

    /// Store the abort handle for a running task so it can be cancelled later.
    pub(crate) fn store_abort_handle(&self, id: &TaskId, handle: tokio::task::AbortHandle) {
        self.abort_handles.insert(id.clone(), handle);
    }

    /// Remove and discard the abort handle when the task finishes normally.
    pub(crate) fn remove_abort_handle(&self, id: &TaskId) {
        self.abort_handles.remove(id);
    }

    /// Abort the running task's Tokio future, if one is registered.
    /// The `kill_on_drop(true)` flag on child processes ensures the CLI subprocess
    /// is also killed when the future is dropped after abort.
    /// Returns `true` if an abort handle was found and triggered.
    pub fn abort_task(&self, id: &TaskId) -> bool {
        if let Some(handle) = self.abort_handles.get(id) {
            handle.abort();
            true
        } else {
            false
        }
    }

    /// Activate the global rate-limit circuit breaker. All tasks will pause
    /// before their next agent call until `duration` elapses.
    pub async fn set_rate_limit(&self, duration: std::time::Duration) {
        let until = tokio::time::Instant::now() + duration;
        *self.rate_limit_until.write().await = Some(until);
        tracing::warn!(
            pause_secs = duration.as_secs(),
            "rate-limit circuit breaker activated — all tasks paused"
        );
    }

    /// Wait if the global rate-limit circuit breaker is active.
    /// Returns only after the deadline is None or already past. Loops to handle
    /// the case where a concurrent `set_rate_limit()` extends the deadline while
    /// this task is sleeping — without looping, the task would proceed after the
    /// original deadline and burn turns / trigger extra 429s.
    pub async fn wait_for_rate_limit(&self) {
        loop {
            let deadline = { *self.rate_limit_until.read().await };
            let Some(until) = deadline else { return };
            let now = tokio::time::Instant::now();
            if now >= until {
                // Deadline already passed. Clear it if unchanged; if a concurrent
                // setter extended the deadline, re-loop to wait for the new value.
                let mut wl = self.rate_limit_until.write().await;
                if *wl == Some(until) {
                    *wl = None;
                    return;
                }
                // Deadline was extended concurrently — fall through to re-loop.
                continue;
            }
            let remaining = until - now;
            tracing::info!(
                remaining_secs = remaining.as_secs(),
                "task waiting for rate-limit circuit breaker to clear"
            );
            tokio::time::sleep_until(until).await;
            // Only clear the breaker if it still holds the same deadline we
            // snapshotted. A concurrent set_rate_limit() call may have extended
            // the deadline during our sleep; preserving the new deadline lets
            // the next loop iteration wait for the extension.
            {
                let mut wl = self.rate_limit_until.write().await;
                if *wl == Some(until) {
                    *wl = None;
                }
            }
            // Loop again: re-read the deadline in case it was extended.
        }
    }

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
        self.db.get_task_prompts(&task_id.0).await
    }

    /// Append a [`TaskEvent`] to the event log. No-op when the log is not open.
    pub(crate) fn log_event(&self, event: crate::event_replay::TaskEvent) {
        if let Some(ref log) = self.event_log {
            log.append(&event);
        }
    }

    pub(crate) async fn insert(&self, state: &TaskState) {
        self.persist_locks
            .entry(state.id.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())));
        self.cache.insert(state.id.clone(), state.clone());
        if let Err(e) = self.db.insert(state).await {
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
        self.db
            .overwrite_external_id_auto_fix(id.as_str(), external_id)
            .await?;
        if let Some(mut entry) = self.cache.get_mut(id) {
            if entry.source.as_deref() == Some("auto-fix") {
                entry.external_id = Some(external_id.to_string());
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

        let snapshot = self.cache.get(id).map(|state| state.value().clone());
        if let Some(state) = snapshot {
            self.db.update(&state).await?;

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
        if let Some(mut entry) = self.cache.get_mut(id) {
            entry.value_mut().status = status.clone();
        }
        self.db
            .update_status_only(id.0.as_str(), status.as_ref())
            .await
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
    /// `gh` CLI failures are treated as transient network errors; the task is left
    /// Pending so it will be retried normally.
    pub async fn validate_recovered_tasks(&self, completion_callback: Option<CompletionCallback>) {
        let candidates: Vec<(TaskId, String)> = self
            .cache
            .iter()
            .filter_map(|e| {
                let task = e.value();
                if matches!(task.status, TaskStatus::Pending) {
                    task.pr_url
                        .as_ref()
                        .map(|url| (task.id.clone(), url.clone()))
                } else {
                    None
                }
            })
            .collect();

        if candidates.is_empty() {
            return;
        }

        tracing::info!(
            "startup: validating {} recovered pending task(s) with PR URLs",
            candidates.len()
        );

        for (task_id, pr_url) in candidates {
            let Some((owner, repo, number)) = super::spawn::parse_pr_url(&pr_url) else {
                tracing::warn!(
                    task_id = %task_id.0,
                    pr_url,
                    "could not parse PR URL; leaving pending"
                );
                continue;
            };

            let pr_ref = format!("{owner}/{repo}#{number}");
            // kill_on_drop(true) ensures the child process is killed when the
            // timeout future is dropped, preventing zombie `gh` processes during
            // startup when many tasks are recovered concurrently.
            let mut cmd = tokio::process::Command::new("gh");
            cmd.args(["pr", "view", &pr_ref, "--json", "state", "--jq", ".state"])
                .kill_on_drop(true);
            let gh_result =
                tokio::time::timeout(std::time::Duration::from_secs(10), cmd.output()).await;

            let output = match gh_result {
                Err(_elapsed) => {
                    tracing::warn!(
                        task_id = %task_id.0,
                        pr_url,
                        "gh pr view timed out after 10s; leaving pending"
                    );
                    continue;
                }
                Ok(Err(e)) => {
                    tracing::warn!(
                        task_id = %task_id.0,
                        pr_url,
                        "gh CLI error: {e}; leaving pending"
                    );
                    continue;
                }
                Ok(Ok(out)) => out,
            };

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                tracing::warn!(
                    task_id = %task_id.0,
                    pr_url,
                    "gh pr view failed: {stderr}; leaving pending"
                );
                continue;
            }

            let state = String::from_utf8_lossy(&output.stdout)
                .trim()
                .to_uppercase();
            let new_status = match state.as_str() {
                "MERGED" => Some(TaskStatus::Done),
                "CLOSED" => Some(TaskStatus::Failed),
                _ => None,
            };

            if let Some(status) = new_status {
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
            } else {
                tracing::info!(
                    task_id = %task_id.0,
                    pr_url,
                    "startup recovery: PR state {state} → leaving pending"
                );
            }
        }
    }
}

pub async fn update_status(
    store: &TaskStore,
    task_id: &TaskId,
    status: TaskStatus,
    turn: u32,
) -> anyhow::Result<()> {
    let status_str = status.as_ref().to_string();
    mutate_and_persist(store, task_id, |s| {
        s.status = status;
        s.turn = turn;
    })
    .await?;
    store.log_event(crate::event_replay::TaskEvent::StatusChanged {
        task_id: task_id.0.clone(),
        ts: crate::event_replay::now_ts(),
        status: status_str,
        turn,
    });
    Ok(())
}

/// Mutate a task in the cache then persist to SQLite.
pub async fn mutate_and_persist(
    store: &TaskStore,
    id: &TaskId,
    f: impl FnOnce(&mut TaskState),
) -> anyhow::Result<()> {
    if let Some(mut entry) = store.cache.get_mut(id) {
        f(entry.value_mut());
    }
    store.persist(id).await
}

#[cfg(test)]
mod tests {
    use super::super::state::TaskState;
    use super::*;

    #[tokio::test]
    async fn task_stream_subscribe_and_publish() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
        let id = harness_core::types::TaskId("stream-test".to_string());

        // No stream registered yet.
        assert!(
            store.subscribe_task_stream(&id).is_none(),
            "subscribe before register should return None"
        );

        store.register_task_stream(&id);
        let mut rx = store
            .subscribe_task_stream(&id)
            .ok_or_else(|| anyhow::anyhow!("subscribe after register should succeed"))?;

        store.publish_stream_item(
            &id,
            harness_core::agent::StreamItem::MessageDelta {
                text: "hello\n".into(),
            },
        );
        store.publish_stream_item(&id, harness_core::agent::StreamItem::Done);

        let item1 = rx.recv().await?;
        let item2 = rx.recv().await?;
        assert!(matches!(
            item1,
            harness_core::agent::StreamItem::MessageDelta { .. }
        ));
        assert!(matches!(item2, harness_core::agent::StreamItem::Done));

        // After close_task_stream the channel sender is dropped.
        store.close_task_stream(&id);
        assert!(
            store.subscribe_task_stream(&id).is_none(),
            "subscribe after close should return None"
        );
        Ok(())
    }

    #[tokio::test]
    async fn task_stream_backpressure_drops_oldest_on_lag() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
        let id = harness_core::types::TaskId("backpressure-test".to_string());

        store.register_task_stream(&id);
        let mut rx = store
            .subscribe_task_stream(&id)
            .ok_or_else(|| anyhow::anyhow!("subscribe should succeed after register"))?;

        // Publish more items than TASK_STREAM_CAPACITY to trigger lag.
        for i in 0..(TASK_STREAM_CAPACITY + 10) as u64 {
            store.publish_stream_item(
                &id,
                harness_core::agent::StreamItem::MessageDelta {
                    text: format!("line {i}\n"),
                },
            );
        }

        // Receiver should see RecvError::Lagged on overflow.
        let result = rx.recv().await;
        assert!(
            result.is_err(),
            "expected Lagged error after overflow, got: {:?}",
            result
        );
        Ok(())
    }

    #[tokio::test]
    async fn list_children_returns_subtasks_for_parent() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

        let parent_id = harness_core::types::TaskId("parent-task".to_string());
        let parent = TaskState::new(parent_id.clone());
        store.insert(&parent).await;

        let mut child1 = TaskState::new(harness_core::types::TaskId("child-1".to_string()));
        child1.parent_id = Some(parent_id.clone());
        store.insert(&child1).await;

        let mut child2 = TaskState::new(harness_core::types::TaskId("child-2".to_string()));
        child2.parent_id = Some(parent_id.clone());
        store.insert(&child2).await;

        // Unrelated task.
        store
            .insert(&TaskState::new(harness_core::types::TaskId(
                "other".to_string(),
            )))
            .await;

        let children = store.list_children(&parent_id);
        assert_eq!(children.len(), 2);
        assert!(children
            .iter()
            .all(|c| c.parent_id.as_ref() == Some(&parent_id)));

        let no_children =
            store.list_children(&harness_core::types::TaskId("nonexistent".to_string()));
        assert!(no_children.is_empty());
        Ok(())
    }

    #[test]
    fn test_task_state_new() {
        let id = super::TaskId::new();
        let state = TaskState::new(id);
        assert!(matches!(state.status, TaskStatus::Pending));
        assert_eq!(state.turn, 0);
        assert!(state.pr_url.is_none());
        assert!(state.project_root.is_none());
        assert!(state.issue.is_none());
        assert!(state.description.is_none());
    }

    #[tokio::test]
    async fn list_siblings_returns_active_tasks_for_same_project() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
        let project = std::path::PathBuf::from("/repo/project");
        let other_project = std::path::PathBuf::from("/repo/other");

        let current_id = harness_core::types::TaskId("current".to_string());
        let mut current = TaskState::new(current_id.clone());
        current.project_root = Some(project.clone());
        current.status = TaskStatus::Implementing;
        store.insert(&current).await;

        // Sibling on same project in Implementing status.
        let mut sibling1 = TaskState::new(harness_core::types::TaskId("sibling-1".to_string()));
        sibling1.project_root = Some(project.clone());
        sibling1.status = TaskStatus::Implementing;
        sibling1.issue = Some(77);
        sibling1.description = Some("fix unwrap in s3.rs".to_string());
        store.insert(&sibling1).await;

        // Sibling on same project in Pending status.
        let mut sibling2 = TaskState::new(harness_core::types::TaskId("sibling-2".to_string()));
        sibling2.project_root = Some(project.clone());
        sibling2.status = TaskStatus::Pending;
        store.insert(&sibling2).await;

        // Task on a different project — must not appear.
        let mut other = TaskState::new(harness_core::types::TaskId("other-project".to_string()));
        other.project_root = Some(other_project.clone());
        other.status = TaskStatus::Implementing;
        store.insert(&other).await;

        // Done task on same project — must not appear.
        let mut done = TaskState::new(harness_core::types::TaskId("done-task".to_string()));
        done.project_root = Some(project.clone());
        done.status = TaskStatus::Done;
        store.insert(&done).await;

        let siblings = store.list_siblings(&project, &current_id);
        let sibling_ids: Vec<&str> = siblings.iter().map(|s| s.id.0.as_str()).collect();
        assert_eq!(
            siblings.len(),
            2,
            "expected 2 siblings, got: {sibling_ids:?}"
        );
        assert!(
            siblings.iter().all(|s| s.id != current_id),
            "current task must be excluded"
        );
        assert!(siblings
            .iter()
            .all(|s| s.project_root.as_deref() == Some(project.as_path())));

        // One sibling on `other_project`.
        let other_project_siblings = store.list_siblings(&other_project, &current_id);
        assert_eq!(other_project_siblings.len(), 1);

        Ok(())
    }

    #[test]
    fn task_status_semantics_are_centralized() {
        let cases = [
            (
                TaskStatus::Pending,
                false,
                false,
                false,
                false,
                false,
                false,
            ),
            (
                TaskStatus::AwaitingDeps,
                false,
                false,
                false,
                false,
                false,
                false,
            ),
            (
                TaskStatus::Implementing,
                false,
                true,
                true,
                false,
                false,
                false,
            ),
            (
                TaskStatus::AgentReview,
                false,
                true,
                true,
                false,
                false,
                false,
            ),
            (TaskStatus::Waiting, false, true, true, false, false, false),
            (
                TaskStatus::Reviewing,
                false,
                true,
                true,
                false,
                false,
                false,
            ),
            (TaskStatus::Done, true, false, false, true, false, false),
            (TaskStatus::Failed, true, false, false, false, true, false),
            (
                TaskStatus::Cancelled,
                true,
                false,
                false,
                false,
                false,
                true,
            ),
        ];

        for (status, terminal, inflight, resumable, success, failure, cancelled) in cases {
            assert_eq!(status.is_terminal(), terminal, "{status:?} terminal");
            assert_eq!(status.is_inflight(), inflight, "{status:?} inflight");
            assert_eq!(
                status.is_resumable_after_restart(),
                resumable,
                "{status:?} resumable"
            );
            assert_eq!(status.is_success(), success, "{status:?} success");
            assert_eq!(status.is_failure(), failure, "{status:?} failure");
            assert_eq!(status.is_cancelled(), cancelled, "{status:?} cancelled");
        }

        assert_eq!(
            TaskStatus::terminal_statuses(),
            &["done", "failed", "cancelled"]
        );
        assert_eq!(
            TaskStatus::resumable_statuses(),
            &["implementing", "agent_review", "waiting", "reviewing"]
        );
    }

    #[tokio::test]
    async fn count_by_project_empty() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
        assert!(store.count_for_dashboard().await.by_project.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn count_by_project_none_root_excluded() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

        let mut task = TaskState::new(harness_core::types::TaskId("no-root".to_string()));
        task.status = TaskStatus::Done;
        // project_root stays None
        store.insert(&task).await;

        assert!(
            store.count_for_dashboard().await.by_project.is_empty(),
            "tasks with no project_root must not appear in per-project counts"
        );
        Ok(())
    }

    #[tokio::test]
    async fn count_by_project_groups_correctly() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

        let root_a = std::path::PathBuf::from("/projects/alpha");
        let root_b = std::path::PathBuf::from("/projects/beta");

        for (id, root, status) in [
            ("a1", &root_a, TaskStatus::Done),
            ("a2", &root_a, TaskStatus::Done),
            ("a3", &root_a, TaskStatus::Failed),
            ("a4", &root_a, TaskStatus::Cancelled),
            ("b1", &root_b, TaskStatus::Done),
            ("b2", &root_b, TaskStatus::Failed),
            ("b3", &root_b, TaskStatus::Failed),
            ("b4", &root_b, TaskStatus::Cancelled),
        ] {
            let mut task = TaskState::new(harness_core::types::TaskId(id.to_string()));
            task.status = status;
            task.project_root = Some(root.clone());
            store.insert(&task).await;
        }

        let counts = store.count_for_dashboard().await.by_project;
        let key_a = root_a.to_string_lossy().into_owned();
        let key_b = root_b.to_string_lossy().into_owned();

        assert!(counts.contains_key(&key_a), "alpha counts missing");
        assert_eq!(counts[&key_a].done, 2, "alpha done");
        assert_eq!(counts[&key_a].failed, 1, "alpha failed");

        assert!(counts.contains_key(&key_b), "beta counts missing");
        assert_eq!(counts[&key_b].done, 1, "beta done");
        assert_eq!(counts[&key_b].failed, 2, "beta failed");
        Ok(())
    }

    #[tokio::test]
    async fn count_by_project_excludes_cancelled_from_failed_totals() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

        let root = std::path::PathBuf::from("/projects/alpha");
        for (id, status) in [
            ("done", TaskStatus::Done),
            ("failed", TaskStatus::Failed),
            ("cancelled", TaskStatus::Cancelled),
        ] {
            let mut task = TaskState::new(harness_core::types::TaskId(id.to_string()));
            task.status = status;
            task.project_root = Some(root.clone());
            store.insert(&task).await;
        }

        let counts = store.count_for_dashboard().await;
        assert_eq!(counts.global_done, 1);
        assert_eq!(counts.global_failed, 1);
        let key = root.to_string_lossy().into_owned();
        assert_eq!(counts.by_project[&key].done, 1);
        assert_eq!(counts.by_project[&key].failed, 1);
        Ok(())
    }

    // --- rate-limit circuit-breaker tests ---

    #[tokio::test]
    async fn wait_for_rate_limit_no_op_when_none() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            store.wait_for_rate_limit(),
        )
        .await
        .map_err(|_| {
            anyhow::anyhow!("wait_for_rate_limit must return immediately when no limit is active")
        })?;
        Ok(())
    }

    #[tokio::test]
    async fn rate_limit_cleared_after_deadline() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
        store
            .set_rate_limit(std::time::Duration::from_millis(50))
            .await;
        store.wait_for_rate_limit().await;
        // After the limit expires, a subsequent call must return immediately.
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            store.wait_for_rate_limit(),
        )
        .await
        .map_err(|_| anyhow::anyhow!("rate limit must be cleared after its deadline passes"))?;
        Ok(())
    }
}
