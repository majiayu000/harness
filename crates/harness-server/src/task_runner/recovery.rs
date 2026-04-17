use super::store::TaskStore;
use super::types::{CompletionCallback, TaskId, TaskStatus};

/// Extract `(owner, repo, number)` from a GitHub PR URL.
///
/// Expects format: `https://github.com/{owner}/{repo}/pull/{number}[#...]`
fn parse_pr_url(pr_url: &str) -> Option<(String, String, u64)> {
    // Strip fragment (e.g. #discussion_r...)
    let url = pr_url.split('#').next().unwrap_or(pr_url);
    let parts: Vec<&str> = url.trim_end_matches('/').split('/').collect();
    // Expected: ["https:", "", "github.com", owner, repo, "pull", number]
    let pull_idx = parts.iter().rposition(|&p| p == "pull")?;
    if pull_idx + 1 >= parts.len() || pull_idx < 2 {
        return None;
    }
    let number: u64 = parts[pull_idx + 1].parse().ok()?;
    let repo = parts[pull_idx - 1].to_string();
    let owner = parts[pull_idx - 2].to_string();
    Some((owner, repo, number))
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
            let Some((owner, repo, number)) = parse_pr_url(&pr_url) else {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pr_url_standard() {
        let Some((owner, repo, number)) = parse_pr_url("https://github.com/acme/myrepo/pull/42")
        else {
            panic!("expected Some for standard GitHub PR URL");
        };
        assert_eq!(owner, "acme");
        assert_eq!(repo, "myrepo");
        assert_eq!(number, 42);
    }

    #[test]
    fn parse_pr_url_with_fragment() {
        let Some((owner, repo, number)) =
            parse_pr_url("https://github.com/acme/myrepo/pull/99#issuecomment-123")
        else {
            panic!("expected Some for PR URL with fragment");
        };
        assert_eq!(owner, "acme");
        assert_eq!(repo, "myrepo");
        assert_eq!(number, 99);
    }

    #[test]
    fn parse_pr_url_trailing_slash() {
        let Some((owner, repo, number)) = parse_pr_url("https://github.com/acme/myrepo/pull/7/")
        else {
            panic!("expected Some for PR URL with trailing slash");
        };
        assert_eq!(owner, "acme");
        assert_eq!(repo, "myrepo");
        assert_eq!(number, 7);
    }

    #[test]
    fn parse_pr_url_invalid_returns_none() {
        assert!(parse_pr_url("https://github.com/acme/myrepo").is_none());
        assert!(parse_pr_url("not-a-url").is_none());
        assert!(parse_pr_url("https://github.com/acme/myrepo/issues/1").is_none());
    }
}
