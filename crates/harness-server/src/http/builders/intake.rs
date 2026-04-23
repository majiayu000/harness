use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::{server::HarnessServer, task_runner};

use super::{engines::EnginesBundle, registry::RegistryBundle, storage::StorageBundle};

/// Outputs of the intake initialization phase.
pub(crate) struct IntakeBundle {
    pub task_queue: Arc<crate::task_queue::TaskQueue>,
    pub review_task_queue: Arc<crate::task_queue::TaskQueue>,
    pub feishu_intake: Option<Arc<crate::intake::feishu::FeishuIntake>>,
    /// GitHub pollers keyed as `"github:{owner/repo}"` for per-repo routing.
    /// The same `Arc` instances are shared with the completion callback.
    pub github_pollers: Vec<(String, Arc<dyn crate::intake::IntakeSource>)>,
    pub completion_callback: Option<task_runner::CompletionCallback>,
}

/// Initialize task queue, intake sources (Feishu, GitHub), quality trigger,
/// and the completion callback (including Q-value wrapper when available).
///
/// Depends on: `storage` (q_values), `engines` (gc_agent, events),
/// `registry` (project_registry) — must follow all three.
pub(crate) async fn build_intake(
    server: &Arc<HarnessServer>,
    storage: &StorageBundle,
    engines: &EnginesBundle,
    registry: &RegistryBundle,
    project_root: &Path,
    data_dir: &Path,
) -> anyhow::Result<IntakeBundle> {
    // ── Task queues ───────────────────────────────────────────────────────────
    let memory_pressure =
        server
            .config
            .concurrency
            .memory_pressure_threshold_mb
            .map(|threshold_mb| {
                let poll_secs = server.config.concurrency.memory_poll_interval_secs;
                tracing::info!(threshold_mb, poll_secs, "memory pressure monitor enabled");
                crate::memory_monitor::start(threshold_mb, poll_secs)
            });
    let issue_queue_config = runtime_issue_concurrency_config(server);
    let review_queue_config = runtime_review_concurrency_config(server, registry).await;
    let task_queue = Arc::new(crate::task_queue::TaskQueue::new_with_pressure(
        &issue_queue_config,
        memory_pressure.clone(),
    ));
    let review_task_queue = Arc::new(crate::task_queue::TaskQueue::new_with_pressure(
        &review_queue_config,
        memory_pressure,
    ));
    tracing::debug!(
        max_concurrent = issue_queue_config.max_concurrent_tasks,
        max_queue_size = issue_queue_config.max_queue_size,
        review_max_concurrent = review_queue_config.max_concurrent_tasks,
        "task queues initialized"
    );

    // ── Feishu intake ─────────────────────────────────────────────────────────
    let feishu_intake = server.config.intake.feishu.as_ref().and_then(|cfg| {
        if !cfg.enabled {
            return None;
        }
        if !crate::intake::feishu::has_verification_token(cfg) {
            tracing::error!(
                "intake: Feishu enabled but verification_token is missing; webhook will fail closed"
            );
            return None;
        }
        tracing::info!(
            trigger_keyword = %cfg.trigger_keyword,
            "intake: Feishu bot registered"
        );
        Some(Arc::new(crate::intake::feishu::FeishuIntake::new(
            cfg.clone(),
        )))
    });

    // ── GitHub pollers ────────────────────────────────────────────────────────
    // Build ALL GitHub pollers once. The same Arc instances are shared between
    // the completion callback and the orchestrator, so on_task_complete operates
    // on the live poller's dispatched map rather than a detached clone.
    // Keyed as "github:{owner/repo}" for per-repo routing in the callback;
    // a "github" fallback entry (first poller) supports tasks persisted before
    // this multi-repo routing was introduced.
    let mut github_pollers: Vec<(String, Arc<dyn crate::intake::IntakeSource>)> = Vec::new();
    if let Some(cfg) = server
        .config
        .intake
        .github
        .as_ref()
        .filter(|cfg| cfg.enabled)
    {
        for repo_cfg in cfg.effective_repos() {
            tracing::info!(
                repo = %repo_cfg.repo,
                label = %repo_cfg.label,
                "intake: GitHub Issues poller registered"
            );
            let key = format!("github:{}", repo_cfg.repo);
            let poller =
                crate::intake::github_issues::GitHubIssuesPoller::new(&repo_cfg, Some(data_dir))
                    .with_task_checker(storage.tasks.clone());
            match poller.reconcile_dispatched_with_store().await {
                Ok(pruned) if pruned > 0 => tracing::info!(
                    repo = %repo_cfg.repo,
                    pruned,
                    "intake: pruned stale GitHub dispatched entries at startup"
                ),
                Ok(_) => {}
                Err(e) => tracing::warn!(
                    repo = %repo_cfg.repo,
                    "intake: failed to reconcile GitHub dispatched entries at startup: {e}"
                ),
            }
            let poller = Arc::new(poller) as Arc<dyn crate::intake::IntakeSource>;
            github_pollers.push((key, poller));
        }
    }

    // ── Quality trigger ───────────────────────────────────────────────────────
    let quality_trigger = {
        let gc_cfg = &server.config.gc;
        let default_name = server
            .agent_registry
            .resolved_default_agent_name()
            .map(|s| s.to_owned());
        let all_names: Vec<String> = server
            .agent_registry
            .list()
            .iter()
            .map(|&s| s.to_owned())
            .collect();
        let challenger = all_names.iter().find_map(|name| {
            if Some(name.as_str()) == default_name.as_deref() {
                return None;
            }
            let agent = server.agent_registry.get(name)?;
            if agent.capabilities().iter().any(|c| {
                matches!(
                    c,
                    harness_core::types::Capability::Write
                        | harness_core::types::Capability::Execute
                )
            }) {
                None
            } else {
                Some(agent)
            }
        });
        Arc::new(crate::quality_trigger::QualityTrigger::new(
            engines.events.clone(),
            engines.gc_agent.clone(),
            server.agent_registry.clone(),
            project_root.to_path_buf(),
            gc_cfg.auto_gc_grades.clone(),
            gc_cfg.auto_gc_cooldown_secs,
            challenger,
            gc_cfg.auto_adopt,
            gc_cfg.auto_adopt_path_prefix.clone(),
            gc_cfg.gc_run_timeout_secs,
        ))
    };

    // ── Completion callback ───────────────────────────────────────────────────
    let completion_callback = crate::http::build_completion_callback(
        &feishu_intake,
        &github_pollers,
        server.config.agents.review.clone(),
        Some(quality_trigger),
        server.config.server.github_token.clone(),
        registry.issue_workflow_store.clone(),
    );

    // Wrap completion callback to record Q-value pipeline events and apply
    // backprop on every live task completion (Done/Failed).
    // Guard IDs are captured once at startup; they are stable after registration.
    let completion_callback = if let Some(ref qv) = storage.q_values {
        let qv = qv.clone();
        let inner = completion_callback;
        let cb: task_runner::CompletionCallback = Arc::new(move |state: task_runner::TaskState| {
            let qv = qv.clone();
            let inner = inner.clone();
            Box::pin(async move {
                let reward = match state.status {
                    task_runner::TaskStatus::Done => {
                        if state.pr_url.is_some() {
                            Some(crate::q_value_store::REWARD_MERGED)
                        } else {
                            None
                        }
                    }
                    task_runner::TaskStatus::Failed => Some(crate::q_value_store::REWARD_CLOSED),
                    task_runner::TaskStatus::Cancelled => {
                        Some(crate::q_value_store::REWARD_UNKNOWN_CLOSED)
                    }
                    _ => None,
                };
                if let Some(reward) = reward {
                    match qv.get_experiences_for_task(&state.id.0).await {
                        Ok(exp_ids) if !exp_ids.is_empty() => {
                            if let Err(e) = qv
                                .apply_q_update(
                                    &exp_ids,
                                    reward,
                                    crate::q_value_store::DEFAULT_ALPHA,
                                )
                                .await
                            {
                                tracing::warn!(
                                    task_id = %state.id.0,
                                    "q_value apply_q_update failed: {e}"
                                );
                            }
                        }
                        Ok(_) => {}
                        Err(e) => tracing::warn!(
                            task_id = %state.id.0,
                            "q_value get_experiences_for_task failed: {e}"
                        ),
                    }
                }
                if let Some(cb) = inner {
                    cb(state).await;
                }
            })
        });
        Some(cb)
    } else {
        completion_callback
    };

    Ok(IntakeBundle {
        task_queue,
        review_task_queue,
        feishu_intake,
        github_pollers,
        completion_callback,
    })
}

fn runtime_issue_concurrency_config(
    server: &HarnessServer,
) -> harness_core::config::misc::ConcurrencyConfig {
    let mut config = server.config.concurrency.clone();

    for project in &server.startup_projects {
        let Some(limit) = project.max_concurrent.map(|value| value as usize) else {
            continue;
        };
        let canonical = queue_project_key(&project.root);
        config.per_project.insert(canonical, limit);
    }

    config
}

async fn runtime_review_concurrency_config(
    server: &HarnessServer,
    registry: &RegistryBundle,
) -> harness_core::config::misc::ConcurrencyConfig {
    let mut config = server.config.concurrency.clone();
    config.max_concurrent_tasks = server.config.review.max_concurrent_tasks.max(1);
    config.per_project = server
        .startup_projects
        .iter()
        .map(|project| (queue_project_key(&project.root), 1usize))
        .collect();
    if config.per_project.is_empty() {
        config.per_project.insert(
            queue_project_key(&server.config.server.project_root),
            1usize,
        );
    }
    match registry.project_registry.list().await {
        Ok(projects) => {
            for project in projects.into_iter().filter(|project| project.active) {
                config
                    .per_project
                    .entry(queue_project_key(&project.root))
                    .or_insert(1usize);
            }
        }
        Err(e) => tracing::warn!(
            "intake: failed to pre-seed review queue limits from project registry: {e}"
        ),
    }
    config
}

fn queue_project_key(path: &Path) -> String {
    canonicalize_for_queue(path).to_string_lossy().into_owned()
}

fn canonicalize_for_queue(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{server::HarnessServer, thread_manager::ThreadManager};
    use harness_agents::registry::AgentRegistry;
    use harness_core::config::HarnessConfig;

    async fn make_minimal_bundles(
        dir: &std::path::Path,
    ) -> (
        Arc<HarnessServer>,
        StorageBundle,
        EnginesBundle,
        RegistryBundle,
    ) {
        let server = Arc::new(HarnessServer::new(
            HarnessConfig::default(),
            ThreadManager::new(),
            AgentRegistry::new("test"),
        ));
        let storage = crate::http::builders::storage::build_storage(dir)
            .await
            .expect("storage");
        let engines = crate::http::builders::engines::build_engines(&server, dir, dir)
            .await
            .expect("engines");
        let registry =
            crate::http::builders::registry::build_registry(&server, dir, dir, &storage.tasks)
                .await
                .expect("registry");
        (server, storage, engines, registry)
    }

    #[tokio::test]
    async fn no_intake_config_produces_empty_intake() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (server, storage, engines, registry) = make_minimal_bundles(dir.path()).await;
        let bundle = build_intake(
            &server,
            &storage,
            &engines,
            &registry,
            dir.path(),
            dir.path(),
        )
        .await
        .expect("build_intake");
        assert!(
            bundle.feishu_intake.is_none(),
            "feishu_intake should be None without config"
        );
        assert!(
            bundle.github_pollers.is_empty(),
            "github_pollers should be empty without config"
        );
    }

    #[test]
    fn runtime_issue_concurrency_uses_startup_project_limits() {
        let mut server = HarnessServer::new(
            HarnessConfig::default(),
            ThreadManager::new(),
            AgentRegistry::new("test"),
        );
        server.config.concurrency.max_concurrent_tasks = 6;
        server
            .config
            .concurrency
            .per_project
            .insert("/tmp/a".to_string(), 2);
        server.startup_projects = vec![
            harness_core::config::ProjectEntry {
                name: "a".to_string(),
                root: std::path::PathBuf::from("/tmp/a"),
                default: false,
                default_agent: None,
                max_concurrent: Some(4),
            },
            harness_core::config::ProjectEntry {
                name: "b".to_string(),
                root: std::path::PathBuf::from("/tmp/b"),
                default: false,
                default_agent: None,
                max_concurrent: Some(8),
            },
        ];

        let cfg = runtime_issue_concurrency_config(&server);

        assert_eq!(cfg.max_concurrent_tasks, 6);
        assert_eq!(cfg.per_project.len(), 2);
        assert_eq!(cfg.per_project.get("/tmp/a"), Some(&4));
        assert_eq!(cfg.per_project.get("/tmp/b"), Some(&8));
    }

    #[tokio::test]
    async fn runtime_review_concurrency_uses_dedicated_domain() {
        let mut server = HarnessServer::new(
            HarnessConfig::default(),
            ThreadManager::new(),
            AgentRegistry::new("test"),
        );
        server.config.review.max_concurrent_tasks = 3;
        server.startup_projects = vec![harness_core::config::ProjectEntry {
            name: "a".to_string(),
            root: std::path::PathBuf::from("/tmp/a"),
            default: false,
            default_agent: None,
            max_concurrent: Some(8),
        }];

        let dir = tempfile::tempdir().expect("tempdir");
        let (_, _, _, registry) = make_minimal_bundles(dir.path()).await;
        let cfg = runtime_review_concurrency_config(&server, &registry).await;

        assert_eq!(cfg.max_concurrent_tasks, 3);
        assert_eq!(cfg.per_project.get("/tmp/a"), Some(&1));
        assert!(cfg.per_project.values().all(|v| *v == 1));
    }

    #[tokio::test]
    async fn runtime_review_concurrency_falls_back_to_server_project_root() {
        let mut server = HarnessServer::new(
            HarnessConfig::default(),
            ThreadManager::new(),
            AgentRegistry::new("test"),
        );
        server.config.review.max_concurrent_tasks = 2;
        server.config.server.project_root = std::path::PathBuf::from("/tmp/fallback");

        let dir = tempfile::tempdir().expect("tempdir");
        let (_, _, _, registry) = make_minimal_bundles(dir.path()).await;
        let cfg = runtime_review_concurrency_config(&server, &registry).await;

        assert_eq!(cfg.max_concurrent_tasks, 2);
        assert_eq!(cfg.per_project.get("/tmp/fallback"), Some(&1));
    }

    #[tokio::test]
    async fn runtime_review_concurrency_includes_registry_projects() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (server, _storage, _engines, registry) = make_minimal_bundles(temp.path()).await;
        let runtime_project_root = temp.path().join("runtime-project");
        std::fs::create_dir_all(&runtime_project_root).expect("create runtime project");
        registry
            .project_registry
            .register(crate::project_registry::Project {
                id: "runtime-project".to_string(),
                root: runtime_project_root.clone(),
                name: Some("runtime-project".to_string()),
                default_agent: None,
                max_concurrent: None,
                active: true,
                created_at: chrono::Utc::now().to_rfc3339(),
            })
            .await
            .expect("register runtime project");

        let cfg = runtime_review_concurrency_config(&server, &registry).await;

        assert_eq!(
            cfg.per_project.get(
                &runtime_project_root
                    .canonicalize()
                    .expect("canonical runtime root")
                    .to_string_lossy()
                    .into_owned()
            ),
            Some(&1)
        );
    }
}
