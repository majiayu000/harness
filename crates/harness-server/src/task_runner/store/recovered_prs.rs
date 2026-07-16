use super::*;

const RECOVERED_PR_VIEW_TIMEOUT: Duration = Duration::from_secs(10);
const RECOVERED_PR_VALIDATION_CONCURRENCY: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RecoveredPrCandidate {
    pub(super) task_id: TaskId,
    pub(super) pr_url: String,
}

#[derive(Debug)]
struct RecoveredPrStatusUpdate {
    task_id: TaskId,
    pr_url: String,
    state: String,
    status: TaskStatus,
}

impl TaskStore {
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

pub(super) fn recovered_pr_candidate(task: &TaskState) -> Option<RecoveredPrCandidate> {
    if !matches!(task.status, TaskStatus::Pending) {
        return None;
    }
    let pr_url = task.pr_url.as_ref()?.clone();
    if super::super::spawn::parse_pr_url(&pr_url).is_none() {
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
    let Some((owner, repo, pr_number)) = super::super::spawn::parse_pr_url(&pr_url) else {
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
