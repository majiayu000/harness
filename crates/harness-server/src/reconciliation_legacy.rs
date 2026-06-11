use super::*;

/// Determine the current GitHub state for one candidate, consuming rate-limit budget.
pub(super) async fn resolve_github_state(
    candidate: &Candidate,
    rate: &mut RateLimiter,
    repo_slug_cache: &mut HashMap<PathBuf, Option<String>>,
    github_token: Option<&str>,
) -> GitHubState {
    // PR URL takes precedence; most candidates in `implementing` or `reviewing` have one.
    if let Some(pr_url) = &candidate.pr_url {
        rate.acquire().await;
        return fetch_pr_state_by_url(pr_url, github_token).await;
    }
    let repo_slug = resolve_repo_slug(candidate, repo_slug_cache).await;
    let Some(repo_slug) = repo_slug else {
        tracing::debug!(
            task_id = %candidate.id.0,
            "GitHub state check skipped because repository slug is unavailable"
        );
        return GitHubState::Unknown;
    };
    if let Some(pr_num) = candidate.pr_num_from_ext {
        rate.acquire().await;
        return fetch_pr_state_by_slug_with_token(&repo_slug, pr_num, github_token).await;
    }
    if let Some(issue_num) = candidate.issue_num {
        rate.acquire().await;
        return fetch_issue_state_with_token(&repo_slug, issue_num, github_token).await;
    }
    GitHubState::Unknown
}

pub(super) async fn resolve_repo_slug(
    candidate: &Candidate,
    repo_slug_cache: &mut HashMap<PathBuf, Option<String>>,
) -> Option<String> {
    match candidate.repo.as_deref() {
        Some(repo) if !repo.trim().is_empty() => Some(repo.to_string()),
        _ => match candidate.project_root.as_ref() {
            Some(project_root) => {
                if let Some(cached) = cached_repo_slug(repo_slug_cache, project_root) {
                    return cached.clone();
                }

                let detected =
                    crate::task_executor::pr_detection::detect_repo_slug(project_root).await;
                repo_slug_cache.insert(project_root.clone(), detected.clone());
                detected
            }
            None => None,
        },
    }
}

fn cached_repo_slug(
    repo_slug_cache: &HashMap<PathBuf, Option<String>>,
    project_root: &PathBuf,
) -> Option<Option<String>> {
    repo_slug_cache.get(project_root).cloned()
}

/// Apply a status transition to a task, returning `true` on success.
pub(super) async fn apply_transition(
    store: &Arc<TaskStore>,
    task_id: &TaskId,
    new_status: TaskStatus,
    reason: &str,
) -> bool {
    let result = mutate_and_persist(store, task_id, |s| {
        // TOCTOU guard: skip if already terminal.
        if s.status.is_terminal() {
            return;
        }
        tracing::info!(
            task_id = %task_id.0,
            from = s.status.as_ref(),
            to = new_status.as_ref(),
            reason,
            "reconciliation: applying transition"
        );
        s.status = new_status.clone();
        s.scheduler.mark_terminal(&new_status);
    })
    .await;

    match result {
        Ok(()) => true,
        Err(e) => {
            tracing::error!(task_id = %task_id.0, "reconciliation: persist failed: {e}");
            false
        }
    }
}
