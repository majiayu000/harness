use crate::{server::HarnessServer, task_runner};
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex};

use super::{
    builders, rate_limit,
    state::{
        AppState, ConcurrencyServices, CoreServices, EngineServices, IntakeServices,
        NotificationServices, ObservabilityServices,
    },
};

fn resolve_project_root(configured_root: &std::path::Path) -> anyhow::Result<std::path::PathBuf> {
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
pub(super) fn expand_tilde(path: &std::path::Path) -> std::path::PathBuf {
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

fn parse_external_id_number(prefix: &str, external_id: Option<&str>) -> Option<u64> {
    let value = external_id?;
    value.strip_prefix(prefix)?.parse::<u64>().ok()
}

/// Build an AppState with all stores. Used by both HTTP and stdio transports.
///
/// Initialization is split into five ordered phases — each phase's outputs
/// become inputs to the next.  Dependency edges:
///   storage → engines → registry (needs storage.tasks for orphan cleanup)
///   registry → intake  (needs engines.gc_agent + events, registry.project_registry)
///   intake   → services (needs interceptors wired to engines.rules + events)
pub async fn build_app_state(server: Arc<HarnessServer>) -> anyhow::Result<AppState> {
    let dir = expand_tilde(&server.config.server.data_dir);
    let project_root = resolve_project_root(&server.config.server.project_root)?;
    #[cfg(test)]
    let db_state_guard = Some(crate::test_helpers::acquire_db_state_guard().await);

    tracing::debug!(
        data_dir = %dir.display(),
        project_root = %project_root.display(),
        discovery_paths = ?server.config.rules.discovery_paths,
        builtin_path = ?server.config.rules.builtin_path,
        exec_policy_paths = ?server.config.rules.exec_policy_paths,
        requirements_path = ?server.config.rules.requirements_path,
        session_renewal_secs = server.config.observe.session_renewal_secs,
        log_retention_days = server.config.observe.log_retention_days,
        "config details (use RUST_LOG=debug to see)"
    );
    match server.config.server.github_webhook_secret.as_deref() {
        Some("") => {
            tracing::warn!(
                "server.github_webhook_secret is configured as empty string; refusing webhook requests until this is set to a non-empty value"
            );
        }
        None => {
            tracing::warn!(
                "server.github_webhook_secret is not configured; refusing webhook requests until this is set to a non-empty value"
            );
        }
        Some(_) => {}
    }

    // Phase 1: storage — dir validation (symlink check, chmod), task DB, q_value DB.
    let storage = builders::storage::build_storage_with_database_url(
        &dir,
        server.config.server.database_url.as_deref(),
    )
    .await?;

    // Phase 2: engines — rule engine, event store (+purge task), GC agent, skill store.
    // Depends on: storage (none directly, but must precede registry which uses storage.tasks).
    let engines = builders::engines::build_engines(&server, &dir, &project_root).await?;

    // Phase 3: registry — thread DB, plan DB + cache, project registry, workspace manager,
    // runtime state store.
    // Depends on: storage.tasks (orphan-worktree cleanup reads terminal task IDs).
    let registry =
        builders::registry::build_registry(&server, &dir, &project_root, &storage.tasks).await?;

    // Phase 4: intake — task queue, Feishu/GitHub pollers, quality trigger, completion callback
    // (including Q-value wrapper).
    // Depends on: engines.gc_agent + engines.events (quality trigger),
    //             registry.project_registry (unused directly but ordering is stable).
    let intake =
        builders::intake::build_intake(&server, &storage, &engines, &registry, &project_root, &dir)
            .await?;

    // Phase 5: services — interceptor stack, service layer, runtime host/project-cache
    // managers, snapshot restore, recovery validator spawn.
    // Depends on: engines.rules + engines.events (interceptors),
    //             registry.workspace_mgr + registry.project_registry (execution service),
    //             intake.task_queue + intake.completion_callback (execution service).
    let services = builders::services::build_services(
        &server,
        &storage,
        &engines,
        &registry,
        &intake,
        &project_root,
    )
    .await?;

    let configured_capacity = server.config.server.notification_broadcast_capacity;
    let notification_broadcast_capacity = configured_capacity.max(1);
    let notification_lag_log_every = server.config.server.notification_lag_log_every;
    if configured_capacity == 0 {
        tracing::warn!(
            "server.notification_broadcast_capacity=0 is invalid; falling back to capacity=1"
        );
    }

    let signal_rate_limit = server.config.server.signal_rate_limit_per_minute;
    let password_reset_rate_limit = server.config.server.password_reset_rate_limit_per_hour;
    let home_dir = std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| project_root.clone());

    let maintenance_active = {
        let in_window = server
            .config
            .maintenance_window
            .in_quiet_window(chrono::Utc::now());
        Arc::new(AtomicBool::new(in_window))
    };

    let review_store = {
        let review_db_path = harness_core::config::dirs::default_db_path(&dir, "reviews");
        match crate::review_store::ReviewStore::open_with_database_url(
            &review_db_path,
            server.config.server.database_url.as_deref(),
        )
        .await
        {
            Ok(store) => Some(Arc::new(store)),
            Err(e) => {
                tracing::warn!("review store init failed, reviews will not be persisted: {e}");
                None
            }
        }
    };

    // NOTE: add a matching entry here whenever a new optional store is added.
    let mut degraded_subsystems: Vec<&'static str> = Vec::new();
    if storage.q_values.is_none() {
        degraded_subsystems.push("q_value_store");
    }
    if registry.runtime_state_store.is_none() || services.snapshot_load_failed {
        degraded_subsystems.push("runtime_state_store");
    }
    if registry.workspace_mgr.is_none() {
        degraded_subsystems.push("workspace_manager");
    }
    if review_store.is_none() {
        degraded_subsystems.push("review_store");
    }

    Ok(AppState {
        core: CoreServices {
            server,
            project_root,
            home_dir,
            tasks: storage.tasks,
            thread_db: Some(registry.thread_db),
            plan_db: Some(registry.plan_db),
            plan_cache: registry.plan_cache,
            issue_workflow_store: registry.issue_workflow_store,
            project_workflow_store: registry.project_workflow_store,
            project_registry: Some(registry.project_registry),
            runtime_state_store: registry.runtime_state_store,
            q_values: storage.q_values,
            maintenance_active,
        },
        engines: EngineServices {
            skills: engines.skills,
            rules: engines.rules,
            gc_agent: engines.gc_agent,
        },
        observability: ObservabilityServices {
            events: engines.events,
            signal_rate_limiter: Arc::new(rate_limit::SignalRateLimiter::new(signal_rate_limit)),
            password_reset_rate_limiter: Arc::new(rate_limit::PasswordResetRateLimiter::new(
                password_reset_rate_limit,
            )),
            review_store,
        },
        concurrency: ConcurrencyServices {
            task_queue: intake.task_queue,
            review_task_queue: intake.review_task_queue,
            workspace_mgr: registry.workspace_mgr,
        },
        #[cfg(test)]
        _db_state_guard: db_state_guard,
        runtime_hosts: services.runtime_hosts,
        runtime_project_cache: services.runtime_project_cache,
        runtime_state_persist_lock: Mutex::new(()),
        runtime_state_dirty: AtomicBool::new(false),
        notifications: NotificationServices {
            notification_tx: broadcast::channel(notification_broadcast_capacity).0,
            notification_lagged_total: Arc::new(AtomicU64::new(0)),
            notification_lag_log_every,
            notify_tx: None,
            initializing: Arc::new(AtomicBool::new(false)),
            initialized: Arc::new(AtomicBool::new(false)),
            ws_shutdown_tx: broadcast::channel(1).0,
        },
        interceptors: services.interceptors,
        degraded_subsystems,
        intake: IntakeServices {
            feishu_intake: intake.feishu_intake,
            github_pollers: intake.github_pollers.into_iter().map(|(_, p)| p).collect(),
            completion_callback: intake.completion_callback,
        },
        project_svc: services.project_svc,
        task_svc: services.task_svc,
        execution_svc: services.execution_svc,
    })
}

pub(crate) fn build_completion_callback(
    feishu_intake: &Option<Arc<crate::intake::feishu::FeishuIntake>>,
    github_pollers: &[(String, Arc<dyn crate::intake::IntakeSource>)],
    review_config: harness_core::config::agents::AgentReviewConfig,
    quality_trigger: Option<Arc<crate::quality_trigger::QualityTrigger>>,
    config_github_token: Option<String>,
    issue_workflow_store: Option<Arc<harness_workflow::issue_lifecycle::IssueWorkflowStore>>,
) -> Option<task_runner::CompletionCallback> {
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
    Some(Arc::new(move |task: task_runner::TaskState| {
        let sources = sources.clone();
        let review_config = review_config.clone();
        let quality_trigger = quality_trigger.clone();
        let github_token = config_github_token.clone();
        let issue_workflow_store = issue_workflow_store.clone();
        Box::pin(async move {
            // Grade recent events and auto-trigger GC if quality is poor.
            if let Some(qt) = quality_trigger {
                let task_ctx = task.pr_url.as_ref().and_then(|pr| {
                    // Find the most recent implementation-affecting round.
                    // agent_review_fix rounds always store detail: None, so if
                    // the most recent such round is a fix round we have no
                    // usable diff for the final PR state — skip cross-review
                    // entirely to avoid judging stale initial-implement content
                    // and spuriously downgrading an already-fixed PR.
                    let last_impl_round = task
                        .rounds
                        .iter()
                        .rev()
                        .find(|r| r.action == "implement" || r.action == "agent_review_fix");
                    let diff = match last_impl_round {
                        Some(r) if r.action == "implement" => {
                            // Use the full implement-agent output as review context.
                            // The implementation prompt contract requires a PR_URL line
                            // but does not mandate unified-diff format, so accepting
                            // any non-empty detail avoids silently dropping valid rounds.
                            r.detail.clone().unwrap_or_default()
                        }
                        // agent_review_fix (detail always None) or no round at all
                        _ => String::new(),
                    };
                    if diff.is_empty() {
                        None
                    } else {
                        Some(crate::quality_trigger::TaskReviewContext {
                            diff,
                            pr_description: pr.clone(),
                        })
                    }
                });
                qt.check_and_maybe_trigger(task_ctx.as_ref()).await;
            }

            // Auto-trigger review bot comment when task completes with a PR URL.
            if review_config.review_bot_auto_trigger {
                if let task_runner::TaskStatus::Done = &task.status {
                    if let Some(pr_url) = task.pr_url.as_deref() {
                        if let Some((owner, repo, pr_num)) =
                            harness_core::prompts::parse_github_pr_url(pr_url)
                        {
                            let resolved_token =
                                crate::github_auth::resolve_github_token(github_token.as_deref());
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
                                        "review_bot_auto_trigger: GitHub token not configured; skipping"
                                    );
                                }
                            }
                        }
                    }
                }
            }

            if let (Some(workflows), Some(project_root)) =
                (issue_workflow_store.as_ref(), task.project_root.as_ref())
            {
                let project_id = project_root.to_string_lossy().into_owned();
                let task_error = task.error.as_deref();
                if let Some(issue_number) = task
                    .issue
                    .or_else(|| parse_external_id_number("issue:", task.external_id.as_deref()))
                {
                    let final_state = match task.status {
                        task_runner::TaskStatus::Done => {
                            if task.pr_url.is_none() {
                                Some(harness_workflow::issue_lifecycle::IssueLifecycleState::Done)
                            } else {
                                None
                            }
                        }
                        task_runner::TaskStatus::Failed => {
                            Some(harness_workflow::issue_lifecycle::IssueLifecycleState::Failed)
                        }
                        task_runner::TaskStatus::Cancelled => {
                            Some(harness_workflow::issue_lifecycle::IssueLifecycleState::Cancelled)
                        }
                        _ => None,
                    };
                    if let Some(final_state) = final_state {
                        if let Err(e) = workflows
                            .record_terminal_for_issue(
                                &project_id,
                                task.repo.as_deref(),
                                issue_number,
                                final_state,
                                task_error,
                            )
                            .await
                        {
                            tracing::warn!(
                                issue = issue_number,
                                task_id = ?task.id,
                                "issue workflow terminal update failed: {e}"
                            );
                        }
                    }
                } else if let Some(pr_number) = task
                    .pr_url
                    .as_deref()
                    .and_then(crate::http::parse_pr_num_from_url)
                    .or_else(|| parse_external_id_number("pr:", task.external_id.as_deref()))
                {
                    let success = matches!(task.status, task_runner::TaskStatus::Done);
                    if matches!(
                        task.status,
                        task_runner::TaskStatus::Done
                            | task_runner::TaskStatus::Failed
                            | task_runner::TaskStatus::Cancelled
                    ) {
                        if let Err(e) = workflows
                            .record_terminal_for_pr(
                                &project_id,
                                task.repo.as_deref(),
                                pr_number,
                                success,
                                matches!(task.status, task_runner::TaskStatus::Cancelled),
                                task_error,
                            )
                            .await
                        {
                            tracing::warn!(
                                pr = pr_number,
                                task_id = ?task.id,
                                "issue workflow PR terminal update failed: {e}"
                            );
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
            let summary = if task.status.is_success() {
                task.pr_url
                    .as_deref()
                    .map(|url| format!("PR: {url}"))
                    .unwrap_or_else(|| "Task completed.".to_string())
            } else if task.status.is_cancelled() {
                "Task cancelled.".to_string()
            } else if task.status.is_failure() {
                task.error.as_deref().unwrap_or("unknown error").to_string()
            } else {
                tracing::warn!(
                    task_id = ?task.id,
                    status = ?task.status,
                    "completion_callback: called with non-terminal status, skipping"
                );
                return;
            };
            let result = crate::intake::TaskCompletionResult {
                status: task.status.clone(),
                failure_kind: task.effective_failure_kind(),
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
