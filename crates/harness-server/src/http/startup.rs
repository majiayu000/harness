use std::sync::Arc;

pub(crate) fn resolve_project_root(
    configured_root: &std::path::Path,
) -> anyhow::Result<std::path::PathBuf> {
    let project_root = configured_root.canonicalize().map_err(|e| {
        anyhow::anyhow!(
            "invalid server.project_root '{}': {e}",
            configured_root.display()
        )
    })?;
    if !project_root.is_dir() {
        anyhow::bail!(
            "server.project_root is not a directory: {}",
            project_root.display()
        );
    }
    Ok(project_root)
}

/// Expand a leading `~/` or standalone `~` to the value of `$HOME`.
/// Returns the path unchanged when `~` is not present or `HOME` is unset.
pub(crate) fn expand_tilde(path: &std::path::Path) -> std::path::PathBuf {
    if let Some(s) = path.to_str() {
        if let Some(rest) = s.strip_prefix("~/") {
            if let Ok(home) = std::env::var("HOME") {
                return std::path::PathBuf::from(home).join(rest);
            }
        } else if s == "~" {
            if let Ok(home) = std::env::var("HOME") {
                return std::path::PathBuf::from(home);
            }
        }
    }
    path.to_path_buf()
}

pub(crate) fn build_completion_callback(
    feishu_intake: &Option<Arc<crate::intake::feishu::FeishuIntake>>,
    github_pollers: &[(String, Arc<dyn crate::intake::IntakeSource>)],
    review_config: harness_core::config::agents::AgentReviewConfig,
    quality_trigger: Option<Arc<crate::quality_trigger::QualityTrigger>>,
    config_github_token: Option<String>,
) -> Option<crate::task_runner::CompletionCallback> {
    let mut sources: std::collections::HashMap<String, Arc<dyn crate::intake::IntakeSource>> =
        std::collections::HashMap::new();
    // Insert each GitHub poller keyed by "github:{owner/repo}" for precise
    // per-repo routing. Also insert the first poller under the bare "github"
    // key as a backward-compat fallback for tasks that pre-date multi-repo
    // support and have task.repo == None.
    for (i, (key, poller)) in github_pollers.iter().enumerate() {
        sources.insert(key.clone(), poller.clone());
        if i == 0 {
            sources.insert("github".to_string(), poller.clone());
        }
    }
    if let Some(fi) = feishu_intake {
        let fi_source: Arc<dyn crate::intake::IntakeSource> = fi.clone();
        sources.insert(fi_source.name().to_string(), fi_source);
    }
    if sources.is_empty() && !review_config.review_bot_auto_trigger && quality_trigger.is_none() {
        return None;
    }
    let sources = Arc::new(sources);
    Some(Arc::new(move |task: crate::task_runner::TaskState| {
        let sources = sources.clone();
        let review_config = review_config.clone();
        let quality_trigger = quality_trigger.clone();
        let github_token = config_github_token.clone();
        Box::pin(async move {
            // Grade recent events and auto-trigger GC if quality is poor.
            if let Some(qt) = quality_trigger {
                qt.check_and_maybe_trigger().await;
            }

            // Auto-trigger review bot comment when task completes with a PR URL.
            if review_config.review_bot_auto_trigger {
                if let crate::task_runner::TaskStatus::Done = &task.status {
                    if let Some(pr_url) = task.pr_url.as_deref() {
                        if let Some((owner, repo, pr_num)) =
                            harness_core::prompts::parse_github_pr_url(pr_url)
                        {
                            let resolved_token = github_token
                                .or_else(|| std::env::var("GITHUB_TOKEN").ok())
                                .filter(|t| !t.is_empty());
                            match resolved_token {
                                Some(token) => {
                                    if let Err(e) = post_review_bot_comment(
                                        &owner,
                                        &repo,
                                        pr_num,
                                        &review_config.review_bot_command,
                                        &token,
                                    )
                                    .await
                                    {
                                        tracing::warn!(
                                            pr_url,
                                            "review_bot_auto_trigger: failed to post comment: {e}"
                                        );
                                    } else {
                                        tracing::info!(
                                            pr_url,
                                            comment = review_config.review_bot_command,
                                            "review bot comment posted"
                                        );
                                    }
                                }
                                _ => {
                                    tracing::warn!(
                                        pr_url,
                                        "review_bot_auto_trigger: GITHUB_TOKEN not set or empty; skipping"
                                    );
                                }
                            }
                        }
                    }
                }
            }

            // Intake source notification.
            let Some(source_name) = task.source.as_deref() else {
                return;
            };
            let Some(external_id) = task.external_id.as_deref() else {
                tracing::warn!(
                    task_id = ?task.id,
                    source = source_name,
                    "completion_callback: task missing external_id, skipping"
                );
                return;
            };
            // For GitHub tasks, route to the specific repo's poller using
            // "github:{owner/repo}". Fall back to the bare "github" key for
            // tasks persisted before multi-repo support (task.repo == None).
            let lookup_key = if source_name == "github" {
                task.repo
                    .as_ref()
                    .map(|r| format!("github:{r}"))
                    .unwrap_or_else(|| "github".to_string())
            } else {
                source_name.to_string()
            };
            let Some(source) = sources.get(&lookup_key) else {
                tracing::warn!(
                    task_id = ?task.id,
                    source = source_name,
                    lookup_key,
                    "completion_callback: intake source not found, skipping"
                );
                return;
            };
            let summary = match &task.status {
                crate::task_runner::TaskStatus::Done => task
                    .pr_url
                    .as_deref()
                    .map(|url| format!("PR: {url}"))
                    .unwrap_or_else(|| "Task completed.".to_string()),
                crate::task_runner::TaskStatus::Failed => {
                    task.error.as_deref().unwrap_or("unknown error").to_string()
                }
                _ => {
                    tracing::warn!(
                        task_id = ?task.id,
                        status = ?task.status,
                        "completion_callback: called with non-terminal status, skipping"
                    );
                    return;
                }
            };
            let result = crate::intake::TaskCompletionResult {
                status: task.status.clone(),
                pr_url: task.pr_url.clone(),
                error: task.error.clone(),
                summary,
            };
            if let Err(e) = source.on_task_complete(external_id, &result).await {
                tracing::warn!(
                    task_id = ?task.id,
                    source = source_name,
                    "on_task_complete failed: {e}"
                );
            }
        })
    }))
}

/// Post a comment to a GitHub PR via the Issues API.
async fn post_review_bot_comment(
    owner: &str,
    repo: &str,
    pr_number: u64,
    body: &str,
    github_token: &str,
) -> anyhow::Result<()> {
    let url = format!("https://api.github.com/repos/{owner}/{repo}/issues/{pr_number}/comments");
    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {github_token}"))
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", "harness-bot")
        .json(&serde_json::json!({ "body": body }))
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("GitHub API returned {status}: {text}");
    }
    Ok(())
}

/// Request body for `POST /signals`.
#[derive(serde::Deserialize)]
pub(crate) struct IngestSignalRequest {
    pub source: String,
    #[serde(default)]
    pub severity: Option<harness_core::types::Severity>,
    pub payload: serde_json::Value,
}

/// Infer severity from a GitHub webhook payload: CI failure → High, changes_requested → Medium.
pub(crate) fn infer_github_severity(
    payload: &serde_json::Value,
) -> Option<harness_core::types::Severity> {
    if let Some(obj) = payload.as_object() {
        // check_run completed with failure
        if let (Some(action), Some(check_run)) = (
            obj.get("action").and_then(|v| v.as_str()),
            obj.get("check_run"),
        ) {
            if action == "completed" {
                let conclusion = check_run
                    .get("conclusion")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if conclusion == "failure" {
                    return Some(harness_core::types::Severity::High);
                }
            }
        }
        // pull_request_review with changes_requested
        if let Some(review) = obj.get("review") {
            let state = review.get("state").and_then(|v| v.as_str()).unwrap_or("");
            if state.eq_ignore_ascii_case("changes_requested") {
                return Some(harness_core::types::Severity::Medium);
            }
        }
    }
    None
}

#[derive(serde::Deserialize)]
pub(crate) struct PasswordResetRequest {
    pub email: String,
}
