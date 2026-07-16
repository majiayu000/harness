use super::*;

impl TaskStore {
    /// Return the `pr_url` of the most recently created Done task, ordered by `created_at DESC`
    /// from the database (stable ordering, unlike the in-memory DashMap cache).
    pub async fn latest_done_pr_url(&self) -> Option<String> {
        record_task_runner_usage();
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
        record_task_runner_usage();
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
        record_task_runner_usage();
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
        record_task_runner_usage();
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
        record_task_runner_usage();
        match self.db.count_done_failed_by_project().await {
            Ok((global_done, global_failed, global_stalled, rows)) => {
                let by_project = rows
                    .into_iter()
                    .map(|(project, done, failed, stalled)| {
                        (
                            project,
                            ProjectCounts {
                                done,
                                failed,
                                stalled,
                            },
                        )
                    })
                    .collect();
                DashboardCounts {
                    global_done,
                    global_failed,
                    global_stalled,
                    by_project,
                }
            }
            Err(e) => {
                tracing::warn!("count_for_dashboard: SQL aggregation failed: {e}");
                DashboardCounts {
                    global_done: 0,
                    global_failed: 0,
                    global_stalled: 0,
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
        record_task_runner_usage();
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
        record_task_runner_usage();
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
        record_task_runner_usage();
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
        record_task_runner_usage();
        self.cache
            .iter()
            .filter(|e| e.value().parent_id.as_ref() == Some(parent_id))
            .map(|e| e.value().clone())
            .collect()
    }
}
