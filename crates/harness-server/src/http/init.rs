use super::types::{
    AppState, ConcurrencyServices, CoreServices, EngineServices, IntakeServices,
    NotificationServices, ObservabilityServices,
};
use crate::{server::HarnessServer, task_runner};
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex};

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
    let storage = super::builders::storage::build_storage(&dir).await?;

    // Phase 2: engines — rule engine, event store (+purge task), GC agent, skill store.
    // Depends on: storage (none directly, but must precede registry which uses storage.tasks).
    let engines = super::builders::engines::build_engines(&server, &dir, &project_root).await?;

    // Phase 3: registry — thread DB, plan DB + cache, project registry, workspace manager,
    // runtime state store.
    // Depends on: storage.tasks (orphan-worktree cleanup reads terminal task IDs).
    let registry =
        super::builders::registry::build_registry(&server, &dir, &project_root, &storage.tasks)
            .await?;

    // Phase 4: intake — task queue, Feishu/GitHub pollers, quality trigger, completion callback
    // (including Q-value wrapper).
    // Depends on: engines.gc_agent + engines.events (quality trigger),
    //             registry.project_registry (unused directly but ordering is stable).
    let intake = super::builders::intake::build_intake(
        &server,
        &storage,
        &engines,
        &registry,
        &project_root,
        &dir,
    )
    .await?;

    // Phase 5: services — interceptor stack, service layer, runtime host/project-cache
    // managers, snapshot restore, recovery validator spawn.
    // Depends on: engines.rules + engines.events (interceptors),
    //             registry.workspace_mgr + registry.project_registry (execution service),
    //             intake.task_queue + intake.completion_callback (execution service).
    let services = super::builders::services::build_services(
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

    Ok(AppState {
        core: CoreServices {
            server,
            project_root,
            home_dir,
            tasks: storage.tasks,
            thread_db: Some(registry.thread_db),
            plan_db: Some(registry.plan_db),
            plan_cache: registry.plan_cache,
            project_registry: Some(registry.project_registry),
            runtime_state_store: registry.runtime_state_store,
            q_values: storage.q_values,
        },
        engines: EngineServices {
            skills: engines.skills,
            rules: engines.rules,
            gc_agent: engines.gc_agent,
        },
        observability: ObservabilityServices {
            events: engines.events,
            signal_rate_limiter: Arc::new(super::rate_limit::SignalRateLimiter::new(
                signal_rate_limit,
            )),
            password_reset_rate_limiter: Arc::new(
                super::rate_limit::PasswordResetRateLimiter::new(password_reset_rate_limit),
            ),
            review_store: {
                let review_db_path = harness_core::config::dirs::default_db_path(&dir, "reviews");
                match crate::review_store::ReviewStore::open(&review_db_path).await {
                    Ok(store) => Some(Arc::new(store)),
                    Err(e) => {
                        tracing::warn!(
                            "review store init failed, reviews will not be persisted: {e}"
                        );
                        None
                    }
                }
            },
        },
        concurrency: ConcurrencyServices {
            task_queue: intake.task_queue,
            workspace_mgr: registry.workspace_mgr,
        },
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
                            let resolved_token = github_token
                                .or_else(|| std::env::var("GITHUB_TOKEN").ok())
                                .filter(|t| !t.is_empty());
                            match resolved_token {
                                Some(token) => {
                                    if let Err(e) = super::helpers::post_review_bot_comment(
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
