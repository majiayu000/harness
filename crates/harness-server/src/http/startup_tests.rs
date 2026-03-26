use super::build_app_state;
use crate::{
    server::HarnessServer,
    test_helpers::{HomeGuard, HOME_LOCK},
    thread_manager::ThreadManager,
};
use harness_agents::AgentRegistry;
use harness_core::{EventFilters, HarnessConfig, RuleId, Severity, SkillLocation, Violation};
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
async fn startup_grade_uses_latest_rule_scan_session_for_violation_count() -> anyhow::Result<()> {
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
