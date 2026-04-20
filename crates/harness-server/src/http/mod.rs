use crate::server::HarnessServer;
use std::net::SocketAddr;
use std::sync::Arc;

// Items re-exported into test scope via `use super::*` in tests.rs.
#[cfg(test)]
use crate::task_runner;
#[cfg(test)]
use axum::{
    extract::DefaultBodyLimit,
    http::StatusCode,
    routing::{get, post},
    Router,
};
#[cfg(test)]
use std::sync::atomic::{AtomicBool, AtomicU64};

pub(crate) mod auth;
pub(crate) mod background;
pub(crate) mod builders;
pub(crate) mod http_router;
pub(crate) mod init;
pub(crate) mod misc_routes;
pub(crate) mod rate_limit;
pub(crate) mod sse_routes;
pub(crate) mod state;
pub(crate) mod task_routes;

#[cfg(test)]
mod tests;

// Re-export all public symbols so callers using `crate::http::*` paths continue to work.
pub use init::build_app_state;
pub(crate) use init::build_completion_callback;
pub use state::{
    AppState, ConcurrencyServices, CoreServices, EngineServices, IntakeServices,
    NotificationServices, ObservabilityServices,
};

// Handler re-exports — moved to focused submodules, kept accessible via `crate::http::`.
pub(crate) use misc_routes::{
    get_task, get_task_artifacts, get_task_prompts, github_webhook, handle_rpc, health_check,
    ingest_signal, intake_status, list_tasks, password_reset, project_queue_stats,
};
pub(crate) use sse_routes::stream_task_sse;

/// Resolve the reviewer agent for independent agent review.
///
/// 1. If `config.reviewer_agent` is set and differs from implementor, use it.
/// 2. Otherwise, auto-select the first registered agent that isn't the implementor.
/// 3. If none found, return None (agent review will be skipped).
pub(crate) fn resolve_reviewer(
    registry: &harness_agents::registry::AgentRegistry,
    config: &harness_core::config::agents::AgentReviewConfig,
    implementor_name: &str,
) -> (
    Option<Arc<dyn harness_core::agent::CodeAgent>>,
    harness_core::config::agents::AgentReviewConfig,
) {
    if !config.enabled {
        return (None, config.clone());
    }

    // Explicit reviewer
    if !config.reviewer_agent.is_empty() {
        if config.reviewer_agent == implementor_name {
            tracing::warn!(
                "agents.review.reviewer_agent == implementor '{}', skipping agent review",
                implementor_name
            );
            return (None, config.clone());
        }
        if let Some(agent) = registry.get(&config.reviewer_agent) {
            return (Some(agent), config.clone());
        }
        tracing::warn!(
            "agents.review.reviewer_agent '{}' not registered, skipping agent review",
            config.reviewer_agent
        );
        return (None, config.clone());
    }

    // Auto-select: first agent != implementor
    for name in registry.list() {
        if name != implementor_name {
            if let Some(agent) = registry.get(name) {
                return (Some(agent), config.clone());
            }
        }
    }

    (None, config.clone())
}

/// Extract the PR number from a GitHub PR URL.
///
/// Handles:
/// - `.../pull/42`
/// - `.../pull/42/files`
/// - `.../pull/42#discussion_r...`
pub(crate) fn parse_pr_num_from_url(url: &str) -> Option<u64> {
    // Strip fragment first, then query string
    let url = url.split('#').next().unwrap_or(url);
    let url = url.split('?').next().unwrap_or(url);
    // Walk path segments looking for "pull", then parse the segment that follows
    let mut parts = url.split('/');
    while let Some(seg) = parts.next() {
        if seg == "pull" {
            return parts.next()?.parse::<u64>().ok();
        }
    }
    None
}

pub async fn serve(server: Arc<HarnessServer>, addr: SocketAddr) -> anyhow::Result<()> {
    tracing::info!("harness: HTTP server listening on {addr}");
    // Record true server start time before accepting any connections.
    crate::handlers::dashboard::SERVER_START.get_or_init(std::time::Instant::now);

    let state = Arc::new(build_app_state(server.clone()).await?);

    // Startup summary — one clean line instead of scattered logs.
    {
        let guard_count = state.engines.rules.read().await.guards().len();
        let skill_count = state.engines.skills.read().await.list().len();
        let task_count = state.core.tasks.list_all().len();
        tracing::info!(
            project = %state.core.project_root.display(),
            guards = guard_count,
            skills = skill_count,
            pending_tasks = task_count,
            "harness: ready"
        );
    }

    // Spawn background watcher for AwaitingDeps tasks.
    background::spawn_awaiting_deps_watcher(&state);

    // Re-dispatch tasks that were recovered to pending after server restart.
    // These had PRs when the server crashed and need their review loop re-started.
    background::spawn_pr_recovery(&state);

    // Re-dispatch tasks recovered from plan/triage checkpoints but without a PR.
    background::spawn_checkpoint_recovery(&state).await;

    let initial_grade = {
        let events = state
            .observability
            .events
            .query(&harness_core::types::EventFilters::default())
            .await
            .unwrap_or_default();
        // Use violations from the most recent scan (identified by the latest rule_scan session_id)
        // rather than all historical rule_check events, to avoid permanently depressing the grade.
        let violation_count = events
            .iter()
            .rev()
            .find(|e| e.hook == "rule_scan")
            .map(|scan| {
                events
                    .iter()
                    .filter(|e| e.hook == "rule_check" && e.session_id == scan.session_id)
                    .count()
            })
            .unwrap_or(0);
        harness_observe::quality::QualityGrader::grade(&events, violation_count).grade
    };
    crate::scheduler::Scheduler::from_grade(initial_grade).start(state.clone());
    // Pass the pre-built GitHub pollers from AppState to the orchestrator so
    // both share the same Arc instances and on_task_complete operates on the
    // live poller's dispatched map.
    let github_sources = state.intake.github_pollers.clone();
    crate::intake::build_orchestrator(
        &state.core.server.config.intake,
        Some(&init::expand_tilde(
            &state.core.server.config.server.data_dir,
        )),
        state.intake.feishu_intake.clone(),
        github_sources,
    )
    .start(state.clone());

    let app = http_router::build_router(state.clone());

    let listener = tokio::net::TcpListener::bind(addr).await?;
    let ws_shutdown_tx = state.notifications.ws_shutdown_tx.clone();
    let serve_result = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            tracing::info!("server shutting down: closing WebSocket connections");
            ws_shutdown_tx.send(()).ok();
        })
        .await;
    tracing::info!("server shutting down");
    state.observability.events.shutdown().await;
    serve_result?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::error!("failed to install Ctrl+C handler: {e}");
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(e) => tracing::error!("failed to install SIGTERM handler: {e}"),
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("received Ctrl+C"),
        _ = terminate => tracing::info!("received SIGTERM"),
    }
}

#[cfg(test)]
mod startup_tests {
    use super::build_app_state;
    use crate::{
        server::HarnessServer,
        test_helpers::{HomeGuard, HOME_LOCK},
        thread_manager::ThreadManager,
    };
    use harness_agents::registry::AgentRegistry;
    use harness_core::{
        config::HarnessConfig, types::EventFilters, types::RuleId, types::Severity,
        types::SkillLocation, types::Violation,
    };
    use std::sync::Arc;

    #[tokio::test]
    async fn persisted_skills_survive_restart() -> anyhow::Result<()> {
        // Hold the shared HOME_LOCK so no sibling test races on HOME.
        let _lock = HOME_LOCK.lock().await;

        let sandbox = tempfile::tempdir()?;
        let project_root = sandbox.path().join("project");
        std::fs::create_dir_all(&project_root)?;
        let data_dir = sandbox.path().join("data");

        // Redirect HOME to an empty sandbox directory so that
        // $HOME/.harness/skills/ cannot shadow the persisted skill under
        // data_dir, keeping the test isolated from machine state.
        let fake_home = sandbox.path().join("home");
        std::fs::create_dir_all(&fake_home)?;
        // SAFETY: HOME_LOCK is held above; HomeGuard::drop restores HOME
        // unconditionally, even when an assertion below panics.
        let _env_guard = unsafe { HomeGuard::set(&fake_home) };

        let startup = |project_root: &std::path::Path, data_dir: &std::path::Path| {
            let project_root = project_root.to_path_buf();
            let data_dir = data_dir.to_path_buf();
            async move {
                let mut config = HarnessConfig::default();
                config.server.project_root = project_root;
                config.server.data_dir = data_dir;
                let server = Arc::new(HarnessServer::new(
                    config,
                    ThreadManager::new(),
                    AgentRegistry::new("test"),
                ));
                build_app_state(server).await
            }
        };

        // First startup: create a skill so it gets persisted to disk.
        {
            let state = startup(&project_root, &data_dir).await?;
            let mut skills = state.engines.skills.write().await;
            skills.create("my-test-skill".to_string(), "# My test skill".to_string());
        }

        // Assert the skill file was physically written to data_dir/skills/
        // before the second startup, catching a broken persist_dir path early.
        let persisted_path = data_dir.join("skills").join("my-test-skill.md");
        assert!(
            persisted_path.exists(),
            "expected skill file to be written to {}",
            persisted_path.display()
        );

        // Second startup: verify the persisted skill is reloaded via discover().
        {
            let state = startup(&project_root, &data_dir).await?;
            let skills = state.engines.skills.read().await;
            let reloaded = skills
                .list()
                .iter()
                .find(|s| s.name == "my-test-skill")
                .ok_or_else(|| {
                    anyhow::anyhow!("expected persisted skill to be reloaded after restart")
                })?;
            // Skills persisted via store.create() are stored in data_dir/skills/
            // and loaded with SkillLocation::User so they can override same-named
            // builtins after restart (User priority > System priority).
            assert_eq!(
                reloaded.location,
                SkillLocation::User,
                "reloaded skill has location {:?}; expected User (data_dir/skills/)",
                reloaded.location
            );
        }

        Ok(())
        // _env_guard dropped here → HOME restored unconditionally
        // _lock dropped here → next test may proceed
    }

    #[tokio::test]
    async fn build_app_state_auto_registers_builtin_guard() -> anyhow::Result<()> {
        let _lock = HOME_LOCK.lock().await;
        let sandbox = tempfile::tempdir()?;
        let project_root = sandbox.path().join("project");
        std::fs::create_dir_all(&project_root)?;

        let mut config = HarnessConfig::default();
        config.server.project_root = project_root;
        config.server.data_dir = sandbox.path().join("data");

        let server = Arc::new(HarnessServer::new(
            config,
            ThreadManager::new(),
            AgentRegistry::new("test"),
        ));
        let state = build_app_state(server).await?;
        let rules = state.engines.rules.read().await;

        assert!(
            rules
                .guards()
                .iter()
                .any(|guard| guard.id.as_str() == harness_rules::engine::BUILTIN_BASELINE_GUARD_ID),
            "expected build_app_state to auto-register builtin guard"
        );
        Ok(())
    }

    #[tokio::test]
    async fn build_app_state_registers_startup_project_metadata() -> anyhow::Result<()> {
        let _lock = HOME_LOCK.lock().await;
        let sandbox = tempfile::tempdir()?;
        let project_root = sandbox.path().join("project");
        std::fs::create_dir_all(&project_root)?;

        let mut config = HarnessConfig::default();
        config.server.project_root = project_root.clone();
        config.server.data_dir = sandbox.path().join("data");

        let mut server =
            HarnessServer::new(config, ThreadManager::new(), AgentRegistry::new("test"));
        server.startup_projects = vec![harness_core::config::ProjectEntry {
            name: "named".to_string(),
            root: project_root.clone(),
            default: false,
            default_agent: Some("codex".to_string()),
            max_concurrent: Some(7),
        }];

        let state = build_app_state(Arc::new(server)).await?;
        let registry = state
            .core
            .project_registry
            .as_ref()
            .expect("project registry should be initialized");
        let canonical_id = project_root.canonicalize()?.to_string_lossy().into_owned();
        let project = registry
            .get(&canonical_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("startup project should be registered"))?;

        assert_eq!(project.default_agent.as_deref(), Some("codex"));
        assert_eq!(project.max_concurrent, Some(7));
        Ok(())
    }

    #[tokio::test]
    async fn build_app_state_registers_default_project_metadata_from_startup_entry(
    ) -> anyhow::Result<()> {
        let _lock = HOME_LOCK.lock().await;
        let sandbox = tempfile::tempdir()?;
        let project_root = sandbox.path().join("project");
        std::fs::create_dir_all(&project_root)?;

        let mut config = HarnessConfig::default();
        config.server.project_root = project_root.clone();
        config.server.data_dir = sandbox.path().join("data");

        let mut server =
            HarnessServer::new(config, ThreadManager::new(), AgentRegistry::new("test"));
        server.startup_projects = vec![harness_core::config::ProjectEntry {
            name: "configured-default".to_string(),
            root: project_root.clone(),
            default: true,
            default_agent: Some("claude".to_string()),
            max_concurrent: Some(3),
        }];

        let state = build_app_state(Arc::new(server)).await?;
        let registry = state
            .core
            .project_registry
            .as_ref()
            .expect("project registry should be initialized");
        let canonical_id = project_root.canonicalize()?.to_string_lossy().into_owned();
        let project = registry
            .get(&canonical_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("default project should be registered"))?;

        assert_eq!(project.root.canonicalize()?, project_root.canonicalize()?);
        assert_eq!(project.default_agent.as_deref(), Some("claude"));
        assert_eq!(project.max_concurrent, Some(3));
        Ok(())
    }

    #[tokio::test]
    async fn build_app_state_preserves_partial_startup_project_metadata() -> anyhow::Result<()> {
        let _lock = HOME_LOCK.lock().await;
        let sandbox = tempfile::tempdir()?;
        let project_root = sandbox.path().join("project");
        std::fs::create_dir_all(&project_root)?;

        let mut config = HarnessConfig::default();
        config.server.project_root = project_root.clone();
        config.server.data_dir = sandbox.path().join("data");

        let mut server =
            HarnessServer::new(config, ThreadManager::new(), AgentRegistry::new("test"));
        server.startup_projects = vec![harness_core::config::ProjectEntry {
            name: "partial".to_string(),
            root: project_root,
            default: false,
            default_agent: Some("claude".to_string()),
            max_concurrent: None,
        }];

        let state = build_app_state(Arc::new(server)).await?;
        let registry = state
            .core
            .project_registry
            .as_ref()
            .expect("project registry should be initialized");
        let canonical_id = sandbox
            .path()
            .join("project")
            .canonicalize()?
            .to_string_lossy()
            .into_owned();
        let project = registry
            .get(&canonical_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("startup project should be registered"))?;

        assert_eq!(project.default_agent.as_deref(), Some("claude"));
        assert_eq!(project.max_concurrent, None);
        Ok(())
    }

    #[tokio::test]
    async fn build_app_state_ignores_stale_default_project_metadata_for_different_root(
    ) -> anyhow::Result<()> {
        let _lock = HOME_LOCK.lock().await;
        let sandbox = tempfile::tempdir()?;
        let startup_root = sandbox.path().join("startup-project");
        let override_root = sandbox.path().join("override-project");
        std::fs::create_dir_all(&startup_root)?;
        std::fs::create_dir_all(&override_root)?;

        let mut config = HarnessConfig::default();
        config.server.project_root = override_root.clone();
        config.server.data_dir = sandbox.path().join("data");

        let mut server =
            HarnessServer::new(config, ThreadManager::new(), AgentRegistry::new("test"));
        let startup_default_project = harness_core::config::ProjectEntry {
            name: "configured-default".to_string(),
            root: startup_root.clone(),
            default: true,
            default_agent: Some("claude".to_string()),
            max_concurrent: Some(3),
        };
        server.startup_projects = vec![startup_default_project.clone()];
        server.startup_default_project = Some(startup_default_project);

        let state = build_app_state(Arc::new(server)).await?;
        let registry = state
            .core
            .project_registry
            .as_ref()
            .expect("project registry should be initialized");
        let canonical_id = override_root.canonicalize()?.to_string_lossy().into_owned();
        let project = registry
            .get(&canonical_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("default project should be registered"))?;

        assert_eq!(project.root.canonicalize()?, override_root.canonicalize()?);
        assert_eq!(project.default_agent, None);
        assert_eq!(project.max_concurrent, None);
        Ok(())
    }

    #[tokio::test]
    async fn startup_grade_uses_latest_rule_scan_session_for_violation_count() -> anyhow::Result<()>
    {
        let _lock = HOME_LOCK.lock().await;
        let sandbox = tempfile::tempdir()?;
        let project_root = sandbox.path().join("project");
        std::fs::create_dir_all(&project_root)?;
        let data_dir = sandbox.path().join("data");

        // Redirect HOME so build_app_state does not read from the real user home.
        let fake_home = sandbox.path().join("home");
        std::fs::create_dir_all(&fake_home)?;
        // SAFETY: HOME_LOCK is held above; HomeGuard::drop restores HOME unconditionally.
        let _env_guard = unsafe { HomeGuard::set(&fake_home) };

        let mut config = HarnessConfig::default();
        config.server.project_root = project_root.clone();
        config.server.data_dir = data_dir;
        let server = Arc::new(HarnessServer::new(
            config,
            ThreadManager::new(),
            AgentRegistry::new("test"),
        ));
        let state = build_app_state(server).await?;

        // First scan: persist 5 violations (old session — must NOT count at startup).
        let old_violations: Vec<Violation> = (0..5)
            .map(|i| Violation {
                rule_id: RuleId::from_str(&format!("U-{i:02}")),
                file: std::path::PathBuf::from("src/old.rs"),
                line: Some(i + 1),
                message: format!("old violation {i}"),
                severity: Severity::Low,
            })
            .collect();
        state
            .observability
            .events
            .persist_rule_scan(&project_root, &old_violations)
            .await;

        // Second scan: persist 2 violations (latest session — must be used for startup grade).
        let new_violations = vec![
            Violation {
                rule_id: RuleId::from_str("SEC-01"),
                file: std::path::PathBuf::from("src/lib.rs"),
                line: Some(1),
                message: "new critical violation".to_string(),
                severity: Severity::Critical,
            },
            Violation {
                rule_id: RuleId::from_str("SEC-02"),
                file: std::path::PathBuf::from("src/main.rs"),
                line: None,
                message: "another new violation".to_string(),
                severity: Severity::High,
            },
        ];
        state
            .observability
            .events
            .persist_rule_scan(&project_root, &new_violations)
            .await;

        // Replicate the exact startup grade logic from serve() (lines 687-697).
        let events = state
            .observability
            .events
            .query(&EventFilters::default())
            .await
            .unwrap_or_default();
        let violation_count = events
            .iter()
            .rev()
            .find(|e| e.hook == "rule_scan")
            .map(|scan| {
                events
                    .iter()
                    .filter(|e| e.hook == "rule_check" && e.session_id == scan.session_id)
                    .count()
            })
            .unwrap_or(0);

        // Must count only the latest scan session (2 violations), not historical total (7).
        assert_eq!(
            violation_count,
            new_violations.len(),
            "startup grade must use latest scan session ({} violations), not historical total ({})",
            new_violations.len(),
            old_violations.len() + new_violations.len(),
        );

        Ok(())
        // _env_guard dropped here → HOME restored unconditionally
        // _lock dropped here → next test may proceed
    }
}
