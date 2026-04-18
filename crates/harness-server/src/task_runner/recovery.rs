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
    use crate::task_runner::{TaskState, TaskStatus};
    use harness_core::types::TaskId as ConcreteTaskId;
    use std::sync::Arc;

    // Serialize tests that temporarily prepend a fake `gh` binary to PATH so
    // that concurrent tests don't see each other's PATH mutations.
    // tokio::sync::Mutex::const_new works as a process-global static.
    static GH_PATH_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// Write a shell script named `gh` to `dir` that prints `stdout` and exits
    /// with `exit_code`.  Marks the file executable on Unix.
    fn write_fake_gh(dir: &std::path::Path, stdout: &str, exit_code: i32) {
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' '{}'\nexit {}\n",
            stdout, exit_code
        );
        let path = dir.join("gh");
        std::fs::write(&path, &script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    async fn open_store(dir: &std::path::Path) -> Arc<super::super::store::TaskStore> {
        super::super::store::TaskStore::open(&dir.join("tasks.db"))
            .await
            .unwrap()
    }

    // ── validate_recovered_tasks behaviour tests ─────────────────────────────

    #[tokio::test]
    async fn validate_recovered_empty_cache_returns_early_no_callback() {
        let dir = tempfile::tempdir().unwrap();
        let store = open_store(dir.path()).await;
        let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let fired_clone = fired.clone();
        let cb: CompletionCallback = Arc::new(move |_| {
            fired_clone.store(true, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async {})
        });
        store.validate_recovered_tasks(Some(cb)).await;
        assert!(
            !fired.load(std::sync::atomic::Ordering::SeqCst),
            "callback must not fire when there are no pending PR tasks"
        );
    }

    #[tokio::test]
    async fn validate_recovered_unparseable_url_skips_task_leaves_pending() {
        let dir = tempfile::tempdir().unwrap();
        let store = open_store(dir.path()).await;
        let mut task = TaskState::new(ConcreteTaskId("t-bad-url".to_string()));
        task.pr_url = Some("not-a-github-pr-url".to_string());
        let task_id = task.id.clone();
        store.cache.insert(task_id.clone(), task);

        store.validate_recovered_tasks(None).await;

        assert!(
            matches!(store.get(&task_id).unwrap().status, TaskStatus::Pending),
            "task with unparseable PR URL must remain Pending"
        );
    }

    #[tokio::test]
    async fn validate_recovered_merged_pr_marks_task_done_invokes_callback() {
        let _path_lock = GH_PATH_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        write_fake_gh(dir.path(), "MERGED", 0);

        let store = open_store(dir.path()).await;
        let mut task = TaskState::new(ConcreteTaskId("t-merged".to_string()));
        task.pr_url = Some("https://github.com/owner/repo/pull/10".to_string());
        let task_id = task.id.clone();
        store.cache.insert(task_id.clone(), task);

        let cb_states: Arc<tokio::sync::Mutex<Vec<TaskState>>> = Default::default();
        let cb_clone = cb_states.clone();
        let cb: CompletionCallback = Arc::new(move |s| {
            let states = cb_clone.clone();
            Box::pin(async move {
                states.lock().await.push(s);
            })
        });

        let old_path = std::env::var("PATH").unwrap_or_default();
        // SAFETY: protected by GH_PATH_LOCK; no concurrent test modifies PATH.
        unsafe { std::env::set_var("PATH", format!("{}:{}", dir.path().display(), old_path)) };
        store.validate_recovered_tasks(Some(cb)).await;
        unsafe { std::env::set_var("PATH", &old_path) };

        assert!(
            matches!(store.get(&task_id).unwrap().status, TaskStatus::Done),
            "MERGED PR must transition task to Done"
        );
        assert_eq!(
            cb_states.lock().await.len(),
            1,
            "completion callback must be invoked exactly once for MERGED"
        );
    }

    #[tokio::test]
    async fn validate_recovered_closed_pr_marks_task_failed_invokes_callback() {
        let _path_lock = GH_PATH_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        write_fake_gh(dir.path(), "CLOSED", 0);

        let store = open_store(dir.path()).await;
        let mut task = TaskState::new(ConcreteTaskId("t-closed".to_string()));
        task.pr_url = Some("https://github.com/owner/repo/pull/11".to_string());
        let task_id = task.id.clone();
        store.cache.insert(task_id.clone(), task);

        let cb_states: Arc<tokio::sync::Mutex<Vec<TaskState>>> = Default::default();
        let cb_clone = cb_states.clone();
        let cb: CompletionCallback = Arc::new(move |s| {
            let states = cb_clone.clone();
            Box::pin(async move {
                states.lock().await.push(s);
            })
        });

        let old_path = std::env::var("PATH").unwrap_or_default();
        // SAFETY: protected by GH_PATH_LOCK; no concurrent test modifies PATH.
        unsafe { std::env::set_var("PATH", format!("{}:{}", dir.path().display(), old_path)) };
        store.validate_recovered_tasks(Some(cb)).await;
        unsafe { std::env::set_var("PATH", &old_path) };

        assert!(
            matches!(store.get(&task_id).unwrap().status, TaskStatus::Failed),
            "CLOSED PR must transition task to Failed"
        );
        assert_eq!(
            cb_states.lock().await.len(),
            1,
            "completion callback must be invoked exactly once for CLOSED"
        );
    }

    #[tokio::test]
    async fn validate_recovered_open_pr_leaves_task_pending_no_callback() {
        let _path_lock = GH_PATH_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        write_fake_gh(dir.path(), "OPEN", 0);

        let store = open_store(dir.path()).await;
        let mut task = TaskState::new(ConcreteTaskId("t-open".to_string()));
        task.pr_url = Some("https://github.com/owner/repo/pull/12".to_string());
        let task_id = task.id.clone();
        store.cache.insert(task_id.clone(), task);

        let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let fired_clone = fired.clone();
        let cb: CompletionCallback = Arc::new(move |_| {
            fired_clone.store(true, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async {})
        });

        let old_path = std::env::var("PATH").unwrap_or_default();
        // SAFETY: protected by GH_PATH_LOCK; no concurrent test modifies PATH.
        unsafe { std::env::set_var("PATH", format!("{}:{}", dir.path().display(), old_path)) };
        store.validate_recovered_tasks(Some(cb)).await;
        unsafe { std::env::set_var("PATH", &old_path) };

        assert!(
            matches!(store.get(&task_id).unwrap().status, TaskStatus::Pending),
            "OPEN PR must leave task Pending"
        );
        assert!(
            !fired.load(std::sync::atomic::Ordering::SeqCst),
            "callback must not fire for an OPEN PR"
        );
    }

    #[tokio::test]
    async fn validate_recovered_gh_nonzero_exit_leaves_task_pending() {
        let _path_lock = GH_PATH_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        write_fake_gh(dir.path(), "error: repository not found", 1);

        let store = open_store(dir.path()).await;
        let mut task = TaskState::new(ConcreteTaskId("t-gh-fail".to_string()));
        task.pr_url = Some("https://github.com/owner/repo/pull/13".to_string());
        let task_id = task.id.clone();
        store.cache.insert(task_id.clone(), task);

        let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let fired_clone = fired.clone();
        let cb: CompletionCallback = Arc::new(move |_| {
            fired_clone.store(true, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async {})
        });

        let old_path = std::env::var("PATH").unwrap_or_default();
        // SAFETY: protected by GH_PATH_LOCK; no concurrent test modifies PATH.
        unsafe { std::env::set_var("PATH", format!("{}:{}", dir.path().display(), old_path)) };
        store.validate_recovered_tasks(Some(cb)).await;
        unsafe { std::env::set_var("PATH", &old_path) };

        assert!(
            matches!(store.get(&task_id).unwrap().status, TaskStatus::Pending),
            "gh non-zero exit must leave task Pending"
        );
        assert!(
            !fired.load(std::sync::atomic::Ordering::SeqCst),
            "callback must not fire when gh exits non-zero"
        );
    }

    // ── parse_pr_url unit tests (pre-existing) ───────────────────────────────

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
