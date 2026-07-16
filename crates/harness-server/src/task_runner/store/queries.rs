use super::*;

impl TaskSummaryFilter {
    pub fn matches_summary(&self, summary: &TaskSummary) -> bool {
        if !self.statuses.is_empty() && !self.statuses.contains(&summary.status) {
            return false;
        }
        if self.active && summary.status.is_terminal() {
            return false;
        }
        if !self.scheduler_states.is_empty()
            && !self
                .scheduler_states
                .contains(&summary.scheduler.authority_state)
        {
            return false;
        }
        if !self.kinds.is_empty() && !self.kinds.contains(&summary.task_kind) {
            return false;
        }
        if self
            .source
            .as_deref()
            .is_some_and(|source| summary.source.as_deref() != Some(source))
        {
            return false;
        }
        if self
            .repo
            .as_deref()
            .is_some_and(|repo| summary.repo.as_deref() != Some(repo))
        {
            return false;
        }
        if self
            .project
            .as_deref()
            .is_some_and(|project| summary.project.as_deref() != Some(project))
        {
            return false;
        }
        true
    }
}

fn task_summary_is_after_cursor(summary: &TaskSummary, cursor: &TaskSummaryPageCursor) -> bool {
    let Some(created_at) = summary
        .created_at
        .as_deref()
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc))
    else {
        return false;
    };
    created_at < cursor.created_at
        || (created_at == cursor.created_at && summary.id.as_str() < cursor.id.as_str())
}

impl TaskStore {
    pub fn get(&self, id: &TaskId) -> Option<TaskState> {
        record_task_runner_usage();
        self.cache.get(id).map(|r| r.value().clone())
    }

    /// Look up a task by ID, checking the in-memory cache first.
    /// Falls back to the database for terminal tasks that were evicted from
    /// the cache at startup (Done, Failed, Cancelled).
    /// Returns `Ok(None)` only when the ID is unknown in both cache and DB.
    /// Returns `Err` when the database query itself fails.
    pub async fn get_with_db_fallback(&self, id: &TaskId) -> anyhow::Result<Option<TaskState>> {
        record_task_runner_usage();
        if let Some(task) = self.cache.get(id) {
            return Ok(Some(task.value().clone()));
        }
        self.db.get(id.0.as_str()).await
    }

    /// Return `true` when the task exists in either the in-memory cache or the
    /// backing database.
    ///
    /// Used by intake reconciliation to detect stale persisted dispatch markers
    /// after a task-storage backend change (for example SQLite -> Postgres).
    pub async fn exists_with_db_fallback(&self, id: &TaskId) -> anyhow::Result<bool> {
        record_task_runner_usage();
        if self.cache.contains_key(id) {
            return Ok(true);
        }
        self.db.exists_by_id(id.0.as_str()).await
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
        record_task_runner_usage();
        self.db.list_stalled_tasks(stale_threshold, project).await
    }

    /// Return IDs of terminal tasks (Done, Failed, Cancelled) directly from the database.
    ///
    /// Used during startup for worktree cleanup. Only fetches task IDs to avoid
    /// deserializing the heavy `rounds` column for large historical datasets.
    pub async fn list_terminal_ids_from_db(&self) -> anyhow::Result<Vec<TaskId>> {
        record_task_runner_usage();
        let ids = self
            .db
            .list_ids_by_status(&["done", "failed", "cancelled"])
            .await?;
        Ok(ids.into_iter().map(harness_core::types::TaskId).collect())
    }

    pub fn count(&self) -> usize {
        record_task_runner_usage();
        self.cache.len()
    }

    pub(crate) fn postgres_pool(&self) -> sqlx::PgPool {
        self.db.postgres_pool()
    }

    #[cfg(test)]
    pub(crate) fn pool_for_test(&self) -> &sqlx::PgPool {
        self.db.pool_for_test()
    }

    /// Check whether an active (non-terminal) task already exists for the same
    /// project + external_id. Cache-first, DB fallback.
    pub async fn find_active_duplicate(
        &self,
        project_id: &str,
        external_id: &str,
    ) -> Option<TaskId> {
        record_task_runner_usage();
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
        record_task_runner_usage();
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
        record_task_runner_usage();
        self.cache.iter().map(|e| e.value().clone()).collect()
    }

    /// Return all tasks: active ones from cache (most current state) merged
    /// with terminal tasks from the database (Done, Failed, Cancelled).
    ///
    /// Cache entries take precedence over DB rows for the same task ID so that
    /// in-flight state is always reflected accurately.
    pub async fn list_all_with_terminal(&self) -> anyhow::Result<Vec<TaskState>> {
        record_task_runner_usage();
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
        record_task_runner_usage();
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

    /// Return active tasks as lightweight [`TaskSummary`] values.
    ///
    /// Fetches only non-terminal summary rows from the DB, then applies live cache
    /// overrides so recently terminal tasks are removed and in-flight state wins.
    pub async fn list_active_summaries(&self) -> anyhow::Result<Vec<TaskSummary>> {
        record_task_runner_usage();
        let mut by_id: std::collections::HashMap<TaskId, TaskSummary> = self
            .db
            .list_active_summaries()
            .await?
            .into_iter()
            .map(|s| (s.id.clone(), s))
            .collect();
        for entry in self.cache.iter() {
            let summary = entry.value().summary();
            if summary.status.is_terminal() {
                by_id.remove(&summary.id);
            } else {
                by_id.insert(entry.key().clone(), summary);
            }
        }
        Ok(by_id.into_values().collect())
    }

    pub async fn list_summaries_filtered_with_terminal(
        &self,
        filter: &TaskSummaryFilter,
    ) -> anyhow::Result<Vec<TaskSummary>> {
        record_task_runner_usage();
        let mut by_id: std::collections::HashMap<TaskId, TaskSummary> = self
            .db
            .list_summaries_filtered(filter)
            .await?
            .into_iter()
            .map(|summary| (summary.id.clone(), summary))
            .collect();
        for entry in self.cache.iter() {
            let summary = entry.value().summary();
            if filter.matches_summary(&summary) {
                by_id.insert(entry.key().clone(), summary);
            } else {
                by_id.remove(entry.key());
            }
        }
        Ok(by_id.into_values().collect())
    }

    pub async fn list_summaries_filtered_page_with_terminal(
        &self,
        filter: &TaskSummaryFilter,
        cursor: Option<&TaskSummaryPageCursor>,
        limit: usize,
    ) -> anyhow::Result<Vec<TaskSummary>> {
        record_task_runner_usage();
        let db_limit = limit.saturating_add(self.cache.len()).saturating_add(1);
        let mut by_id: std::collections::HashMap<TaskId, TaskSummary> = self
            .db
            .list_summaries_filtered_page(filter, cursor, db_limit)
            .await?
            .into_iter()
            .map(|summary| (summary.id.clone(), summary))
            .collect();
        for entry in self.cache.iter() {
            let summary = entry.value().summary();
            if filter.matches_summary(&summary)
                && cursor.is_none_or(|cursor| task_summary_is_after_cursor(&summary, cursor))
            {
                by_id.insert(entry.key().clone(), summary);
            } else {
                by_id.remove(entry.key());
            }
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
        record_task_runner_usage();
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

    /// Return the latest status for each `external_id` in a single project/repo namespace.
    ///
    /// The database query collapses historical attempts down to the newest row per
    /// issue. In-memory cache entries then override the DB result when the server
    /// is holding a newer status transition that has not been re-read from storage.
    pub async fn list_latest_external_statuses_by_project_and_repo(
        &self,
        project_id: &str,
        repo: Option<&str>,
    ) -> anyhow::Result<HashMap<String, (Option<String>, TaskStatus)>> {
        record_task_runner_usage();
        let mut by_external_id: HashMap<String, (Option<String>, TaskStatus)> = self
            .db
            .list_latest_external_statuses_by_project_and_repo(project_id, repo)
            .await?
            .into_iter()
            .filter_map(|(external_id, status, created_at)| {
                status
                    .parse::<TaskStatus>()
                    .ok()
                    .map(|parsed| (external_id, (created_at, parsed)))
            })
            .collect();
        for entry in self.cache.iter() {
            let task = entry.value();
            let same_project = task
                .project_root
                .as_ref()
                .map(|path| path.to_string_lossy() == project_id)
                .unwrap_or(false);
            let same_repo = match (repo, task.repo.as_deref()) {
                (Some(expected), Some(actual)) => expected == actual,
                (None, None) => true,
                _ => false,
            };
            if !same_project || !same_repo {
                continue;
            }
            let Some(external_id) = task.external_id.clone() else {
                continue;
            };
            let should_replace = by_external_id
                .get(&external_id)
                .map(|(existing_created_at, _)| {
                    latest_timestamp_at_least(
                        task.created_at.as_deref(),
                        existing_created_at.as_deref(),
                    )
                })
                .unwrap_or(true);
            if should_replace {
                by_external_id.insert(external_id, (task.created_at.clone(), task.status.clone()));
            }
        }

        Ok(by_external_id)
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
