use std::path::PathBuf;

use super::types::CreateTaskRequest;

pub(crate) fn summarize_request_description(req: &CreateTaskRequest) -> Option<String> {
    req.issue.map(|n| format!("issue #{n}")).or_else(|| {
        req.prompt.as_ref().map(|p| {
            let s = p.trim();
            let cutoff = s.char_indices().nth(80).map(|(i, _)| i).unwrap_or(s.len());
            if cutoff < s.len() {
                format!("{}...", &s[..cutoff])
            } else {
                s.to_string()
            }
        })
    })
}

pub(crate) async fn fill_missing_repo_from_project(req: &mut CreateTaskRequest) {
    if req.repo.is_some() {
        return;
    }
    let Some(project) = req.project.as_deref() else {
        return;
    };
    req.repo = crate::task_executor::pr_detection::detect_repo_slug(project).await;
}

/// Detect the main git worktree root using a blocking subprocess call.
/// Must be called via `tokio::task::spawn_blocking` in async contexts.
pub(crate) fn detect_main_worktree() -> PathBuf {
    std::process::Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| {
            s.lines()
                .next()
                .and_then(|line| line.strip_prefix("worktree "))
                .map(|p| PathBuf::from(p.trim()))
        })
        .unwrap_or_else(|| {
            tracing::warn!(
                "detect_main_worktree: could not detect git worktree root, falling back to '.'"
            );
            PathBuf::from(".")
        })
}

pub(crate) fn describe_detect_main_worktree_join_error(
    join_err: &tokio::task::JoinError,
) -> String {
    if join_err.is_panic() {
        format!("detect_main_worktree panicked: {join_err}")
    } else if join_err.is_cancelled() {
        format!("detect_main_worktree was cancelled: {join_err}")
    } else {
        format!("detect_main_worktree failed: {join_err}")
    }
}

pub(crate) async fn resolve_project_root_with(
    requested_project: Option<PathBuf>,
    detect_worktree: impl FnOnce() -> PathBuf + Send + 'static,
) -> anyhow::Result<PathBuf> {
    match requested_project {
        Some(project) => Ok(project),
        None => tokio::task::spawn_blocking(detect_worktree)
            .await
            .map_err(|join_err| {
                let reason = describe_detect_main_worktree_join_error(&join_err);
                tracing::error!("{reason}");
                anyhow::anyhow!("{reason}")
            }),
    }
}

/// Resolve and canonicalize the project root so the caller can obtain a stable
/// semaphore key before acquiring the concurrency permit.
///
/// When `project` is `None` the main git worktree is detected automatically.
/// Symlinks, relative paths and the `None` sentinel all converge to the same
/// canonical `PathBuf`, preventing the same repository from landing in
/// different per-project buckets due to path aliasing.
///
/// This function only resolves symlinks — it does NOT enforce the HOME-boundary
/// restriction applied by `validate_project_root`. Full validation still
/// happens inside `spawn_task` once the task is running.
pub(crate) async fn resolve_canonical_project(project: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    let raw = resolve_project_root_with(project, detect_main_worktree).await?;
    // Best-effort canonicalize: if the path doesn't exist yet (e.g. in tests
    // using a path that will be created later) fall back to the raw path so
    // we at least get a consistent string key.
    Ok(raw.canonicalize().unwrap_or(raw))
}

/// Return `true` when a free-text prompt is complex enough to require a Plan phase
/// before implementation.
///
/// Heuristic: prompt longer than 200 words OR contains 3 or more file-path-like
/// tokens (sequences containing `/` or ending in a recognised source extension).
pub(crate) fn prompt_requires_plan(prompt: &str) -> bool {
    let word_count = prompt.split_whitespace().count();
    if word_count > 200 {
        return true;
    }
    let file_path_count = prompt
        .split_whitespace()
        .filter(|tok| {
            // Exclude XML/HTML tags — `</foo>` contains '/' but is not a file path.
            // System prompts wrap user data in `<external_data>...</external_data>`
            // which would otherwise trigger false positives.
            let is_xml_tag = tok.starts_with('<');
            if is_xml_tag {
                return false;
            }
            tok.contains('/') || {
                let lower = tok.to_lowercase();
                lower.ends_with(".rs")
                    || lower.ends_with(".ts")
                    || lower.ends_with(".tsx")
                    || lower.ends_with(".go")
                    || lower.ends_with(".py")
                    || lower.ends_with(".toml")
                    || lower.ends_with(".json")
            }
        })
        .count();
    file_path_count >= 3
}

pub(crate) fn is_non_decomposable_prompt_source(source: Option<&str>) -> bool {
    matches!(source, Some("periodic_review") | Some("sprint_planner"))
}
