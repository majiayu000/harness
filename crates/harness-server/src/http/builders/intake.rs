use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::{project_registry::Project, server::HarnessServer, task_runner};

use super::{engines::EnginesBundle, registry::RegistryBundle, storage::StorageBundle};

/// Outputs of the intake initialization phase.
pub(crate) struct IntakeBundle {
    pub task_queue: Arc<crate::task_queue::TaskQueue>,
    pub review_task_queue: Arc<crate::task_queue::TaskQueue>,
    pub feishu_intake: Option<Arc<crate::intake::feishu::FeishuIntake>>,
    /// GitHub pollers keyed as `"github:{owner/repo}"` for per-repo routing.
    /// The same `Arc` instances are shared with the completion callback.
    pub github_pollers: Vec<(String, Arc<dyn crate::intake::IntakeSource>)>,
    pub legacy_github_fallback_enabled: bool,
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
    _data_dir: &Path,
) -> anyhow::Result<IntakeBundle> {
    let events = engines
        .events
        .as_ref()
        .expect("build_intake requires a ready event store")
        .clone();

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
    let issue_queue_config = runtime_issue_concurrency_config(server, registry).await;
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

    // ── GitHub backlog ownership ──────────────────────────────────────────────
    // GitHub issue polling is workflow-owned. The server does not register the
    // legacy GitHub poller; repo backlog scans are requested through runtime
    // commands and executed by agents.
    let github_pollers: Vec<(String, Arc<dyn crate::intake::IntakeSource>)> = Vec::new();
    let mut legacy_github_fallback_enabled = true;
    if let Some(cfg) = server
        .config
        .intake
        .github
        .as_ref()
        .filter(|cfg| cfg.enabled)
    {
        legacy_github_fallback_enabled = false;
        for repo_cfg in cfg.effective_repos() {
            if runtime_repo_backlog_owns_github_polling(project_root, &repo_cfg, registry) {
                tracing::info!(
                    repo = %repo_cfg.repo,
                    label = %repo_cfg.label,
                    "intake: GitHub issue polling owned by workflow runtime repo backlog"
                );
            } else {
                tracing::warn!(
                    repo = %repo_cfg.repo,
                    label = %repo_cfg.label,
                    "intake: GitHub issue polling disabled because workflow runtime repo backlog is unavailable or disabled"
                );
            }
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
            events.clone(),
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
        legacy_github_fallback_enabled,
        completion_callback,
    })
}

fn runtime_repo_backlog_owns_github_polling(
    fallback_project_root: &Path,
    repo_config: &harness_core::config::intake::GitHubRepoConfig,
    registry: &RegistryBundle,
) -> bool {
    if registry.workflow_runtime_store.is_none() {
        return false;
    }
    let project_root = repo_config
        .project_root
        .as_deref()
        .filter(|path| !path.trim().is_empty())
        .map(expand_home_path)
        .unwrap_or_else(|| fallback_project_root.to_path_buf());
    let workflow_cfg = harness_core::config::workflow::load_workflow_config(&project_root)
        .unwrap_or_else(|error| {
            tracing::warn!(
                project_root = %project_root.display(),
                "intake: failed to load WORKFLOW.md for runtime repo backlog handoff, using default config: {error}"
            );
            harness_core::config::workflow::WorkflowConfig::default()
        });
    workflow_cfg.repo_backlog.enabled
        && workflow_cfg.runtime_dispatch.enabled
        && workflow_cfg.runtime_worker.enabled
}

fn expand_home_path(path: &str) -> PathBuf {
    if path == "~" {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home);
        }
    }
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(path)
}

async fn runtime_issue_concurrency_config(
    server: &HarnessServer,
    registry: &RegistryBundle,
) -> harness_core::config::misc::ConcurrencyConfig {
    let registry_projects = load_registry_projects(registry).await;
    build_runtime_issue_concurrency_config(server, &registry_projects)
}

pub(crate) fn build_runtime_issue_concurrency_config(
    server: &HarnessServer,
    registry_projects: &[Project],
) -> harness_core::config::misc::ConcurrencyConfig {
    let mut config = server.config.concurrency.clone();
    config.per_project = effective_issue_project_limits(server, registry_projects);
    config
}

pub(crate) fn effective_issue_project_limits(
    server: &HarnessServer,
    registry_projects: &[Project],
) -> std::collections::HashMap<String, usize> {
    let mut per_project = canonicalized_config_limits(&server.config.concurrency.per_project);

    for project in registry_projects {
        let Some(limit) = project.max_concurrent.map(|value| value as usize) else {
            continue;
        };
        if !project.active {
            continue;
        }
        per_project.insert(queue_project_key(&project.root), limit);
    }

    for project in &server.startup_projects {
        let Some(limit) = project.max_concurrent.map(|value| value as usize) else {
            continue;
        };
        per_project.insert(queue_project_key(&project.root), limit);
    }

    per_project
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
    let Some(project_registry) = registry.project_registry.as_ref() else {
        return config;
    };
    match project_registry.list().await {
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

fn canonicalized_config_limits(
    limits: &std::collections::HashMap<String, usize>,
) -> std::collections::HashMap<String, usize> {
    limits
        .iter()
        .map(|(project, limit)| {
            let path = Path::new(project);
            let key = if path.is_absolute() {
                queue_project_key(path)
            } else {
                project.clone()
            };
            (key, *limit)
        })
        .collect()
}

async fn load_registry_projects(registry: &RegistryBundle) -> Vec<Project> {
    let Some(project_registry) = registry.project_registry.as_ref() else {
        return Vec::new();
    };
    match project_registry.list().await {
        Ok(projects) => projects,
        Err(e) => {
            tracing::warn!("intake: failed to load project registry for issue queue seeding: {e}");
            Vec::new()
        }
    }
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
        let registry = crate::http::builders::registry::build_registry(
            &server,
            dir,
            dir,
            storage.tasks.as_ref().expect("tasks store"),
        )
        .await
        .expect("registry");
        (server, storage, engines, registry)
    }

    fn registry_project(
        id: &str,
        root: std::path::PathBuf,
        max_concurrent: Option<u32>,
        active: bool,
    ) -> Project {
        Project {
            id: id.to_string(),
            root,
            name: Some(id.to_string()),
            default_agent: None,
            max_concurrent,
            active,
            created_at: chrono::Utc::now().to_rfc3339(),
        }
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

    #[tokio::test]
    async fn github_intake_is_owned_by_runtime_not_legacy_poller() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = HarnessConfig::default();
        config.intake.github = Some(harness_core::config::intake::GitHubIntakeConfig {
            enabled: true,
            repo: "owner/repo".to_string(),
            label: "harness".to_string(),
            ..Default::default()
        });
        let server = Arc::new(HarnessServer::new(
            config,
            ThreadManager::new(),
            AgentRegistry::new("test"),
        ));
        let storage = crate::http::builders::storage::build_storage(dir.path())
            .await
            .expect("storage");
        let engines =
            crate::http::builders::engines::build_engines(&server, dir.path(), dir.path())
                .await
                .expect("engines");
        let registry = crate::http::builders::registry::build_registry(
            &server,
            dir.path(),
            dir.path(),
            storage.tasks.as_ref().expect("tasks store"),
        )
        .await
        .expect("registry");

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
            bundle.github_pollers.is_empty(),
            "configured GitHub intake should not register legacy pollers"
        );
        assert!(
            !bundle.legacy_github_fallback_enabled,
            "configured GitHub intake should not fall back to legacy server polling"
        );
    }

    #[test]
    fn runtime_issue_concurrency_uses_registry_limits() {
        let mut server = HarnessServer::new(
            HarnessConfig::default(),
            ThreadManager::new(),
            AgentRegistry::new("test"),
        );
        server.config.concurrency.max_concurrent_tasks = 6;
        let cfg = build_runtime_issue_concurrency_config(
            &server,
            &[registry_project(
                "registry-only",
                std::path::PathBuf::from("/tmp/registry-only"),
                Some(5),
                true,
            )],
        );

        assert_eq!(cfg.max_concurrent_tasks, 6);
        assert_eq!(cfg.per_project.get("/tmp/registry-only"), Some(&5));
    }

    #[test]
    fn runtime_issue_concurrency_startup_projects_override_registry() {
        let mut server = HarnessServer::new(
            HarnessConfig::default(),
            ThreadManager::new(),
            AgentRegistry::new("test"),
        );
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

        let cfg = build_runtime_issue_concurrency_config(
            &server,
            &[
                registry_project("a", std::path::PathBuf::from("/tmp/a"), Some(3), true),
                registry_project("c", std::path::PathBuf::from("/tmp/c"), Some(9), true),
            ],
        );

        assert_eq!(cfg.per_project.len(), 3);
        assert_eq!(cfg.per_project.get("/tmp/a"), Some(&4));
        assert_eq!(cfg.per_project.get("/tmp/b"), Some(&8));
        assert_eq!(cfg.per_project.get("/tmp/c"), Some(&9));
    }

    #[test]
    fn runtime_issue_concurrency_ignores_inactive_and_unlimited_registry_projects() {
        let server = HarnessServer::new(
            HarnessConfig::default(),
            ThreadManager::new(),
            AgentRegistry::new("test"),
        );

        let cfg = build_runtime_issue_concurrency_config(
            &server,
            &[
                registry_project(
                    "inactive",
                    std::path::PathBuf::from("/tmp/inactive"),
                    Some(7),
                    false,
                ),
                registry_project(
                    "unlimited",
                    std::path::PathBuf::from("/tmp/unlimited"),
                    None,
                    true,
                ),
            ],
        );

        assert!(!cfg.per_project.contains_key("/tmp/inactive"));
        assert!(!cfg.per_project.contains_key("/tmp/unlimited"));
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
            .as_ref()
            .expect("project registry")
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
