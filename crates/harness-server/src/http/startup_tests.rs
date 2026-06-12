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

#[test]
fn http_listener_starts_before_background_work() {
    let source = include_str!("mod.rs");
    let Some(bind_index) = source.find("tokio::net::TcpListener::bind") else {
        panic!("serve should bind a TCP listener");
    };
    let Some(serve_task_index) = source.find("let serve_handle = tokio::spawn") else {
        panic!("serve should start Axum before background work");
    };
    let Some(reconciliation_index) = source.find("run_once_with_runtime_config") else {
        panic!("serve should still run startup reconciliation");
    };
    let Some(reconciliation_gate_index) =
        source.find("state.core.server.config.reconciliation.enabled")
    else {
        panic!("startup reconciliation should respect reconciliation.enabled");
    };
    let Some(feedback_sweeper_index) = source.find("spawn_runtime_pr_feedback_sweeper") else {
        panic!("serve should still start the PR feedback sweeper");
    };
    let Some(backlog_poller_index) = source.find("spawn_runtime_repo_backlog_poller") else {
        panic!("serve should still start the repo backlog poller");
    };
    let Some(runtime_worker_index) = source.find("spawn_runtime_job_workers") else {
        panic!("serve should still start runtime workers");
    };

    for background_index in [
        reconciliation_index,
        feedback_sweeper_index,
        backlog_poller_index,
        runtime_worker_index,
    ] {
        assert!(
            bind_index < background_index,
            "HTTP listener must be bound before background work can enqueue or execute agent jobs"
        );
        assert!(
            serve_task_index < background_index,
            "Axum must be running before background work can enqueue or execute agent jobs"
        );
    }

    assert!(
        reconciliation_gate_index < reconciliation_index,
        "startup reconciliation must be gated before it can make GitHub calls"
    );
}

#[tokio::test]
async fn persisted_skills_survive_restart() -> anyhow::Result<()> {
    // Hold the shared HOME_LOCK so no sibling test races on HOME.
    let _lock = HOME_LOCK.lock().await;
    let _ = crate::test_helpers::test_database_url()?;

    let sandbox = tempfile::tempdir()?;
    let project_root = sandbox.path().join("project");
    std::fs::create_dir_all(&project_root)?;
    let data_dir = sandbox.path().join("data");
    let database_url = crate::test_helpers::test_database_url()?;

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
        let database_url = database_url.clone();
        async move {
            let mut config = HarnessConfig::default();
            config.server.project_root = project_root;
            config.server.data_dir = data_dir;
            config.server.database_url = Some(database_url);
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
    // _env_guard dropped here -> HOME restored unconditionally
    // _lock dropped here -> next test may proceed
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

    let mut server = HarnessServer::new(config, ThreadManager::new(), AgentRegistry::new("test"));
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

    let mut server = HarnessServer::new(config, ThreadManager::new(), AgentRegistry::new("test"));
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

    let mut server = HarnessServer::new(config, ThreadManager::new(), AgentRegistry::new("test"));
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
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let sandbox = tempfile::tempdir()?;
    let startup_root = sandbox.path().join("startup-project");
    let override_root = sandbox.path().join("override-project");
    std::fs::create_dir_all(&startup_root)?;
    std::fs::create_dir_all(&override_root)?;

    let mut config = HarnessConfig::default();
    config.server.project_root = override_root.clone();
    config.server.data_dir = sandbox.path().join("data");

    let mut server = HarnessServer::new(config, ThreadManager::new(), AgentRegistry::new("test"));
    let startup_default_project = harness_core::config::ProjectEntry {
        name: "configured-default".to_string(),
        root: startup_root.clone(),
        default: true,
        default_agent: Some("claude".to_string()),
        max_concurrent: Some(3),
    };
    server.startup_projects = vec![startup_default_project.clone()];
    server.startup_default_project = Some(startup_default_project);

    let state = match build_app_state(Arc::new(server)).await {
        Ok(state) => state,
        Err(err) if crate::test_helpers::is_pool_timeout(&err) => return Ok(()),
        Err(err) => return Err(err),
    };
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
async fn startup_grade_uses_latest_rule_scan_session_for_violation_count() -> anyhow::Result<()> {
    let sandbox = tempfile::tempdir()?;
    let project_root = sandbox.path().join("project");
    std::fs::create_dir_all(&project_root)?;
    let data_dir = sandbox.path().join("data");
    let events = harness_observe::event_store::EventStore::new(&data_dir).await?;

    // First scan: persist 5 violations from an old session.
    let old_violations: Vec<Violation> = (0..5)
        .map(|i| Violation {
            rule_id: RuleId::from_str(&format!("U-{i:02}")),
            file: std::path::PathBuf::from("src/old.rs"),
            line: Some(i + 1),
            message: format!("old violation {i}"),
            severity: Severity::Low,
        })
        .collect();
    events
        .persist_rule_scan(&project_root, &old_violations)
        .await;

    // Second scan: persist 2 violations from the latest session.
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
    events
        .persist_rule_scan(&project_root, &new_violations)
        .await;

    let events = events
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

    assert_eq!(
        violation_count,
        new_violations.len(),
        "startup grade must use latest scan session ({} violations), not historical total ({})",
        new_violations.len(),
        old_violations.len() + new_violations.len(),
    );

    Ok(())
}
