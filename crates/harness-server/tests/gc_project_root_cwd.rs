use harness_agents::AgentRegistry;
use harness_core::{GuardId, HarnessConfig, Language};
use harness_server::{
    handlers::gc::gc_run,
    http::build_app_state,
    server::HarnessServer,
    thread_manager::ThreadManager,
};
use std::sync::Arc;

struct CwdGuard(std::path::PathBuf);

impl CwdGuard {
    fn switch_to(path: &std::path::Path) -> anyhow::Result<Self> {
        let original = std::env::current_dir()?;
        std::env::set_current_dir(path)?;
        Ok(Self(original))
    }
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.0);
    }
}

#[tokio::test]
async fn gc_run_uses_configured_project_root_across_cwd() -> anyhow::Result<()> {
    let sandbox = tempfile::tempdir()?;
    let project_root = sandbox.path().join("project-root");
    let other_cwd = sandbox.path().join("other-cwd");
    let capture_file = sandbox.path().join("captured-root.txt");
    let guard_script = sandbox.path().join("capture-root.sh");
    std::fs::create_dir_all(&project_root)?;
    std::fs::create_dir_all(&other_cwd)?;
    std::fs::write(
        &guard_script,
        format!(
            "#!/usr/bin/env bash\nprintf '%s' \"$1\" > \"{}\"\n",
            capture_file.display()
        ),
    )?;

    let harness_db = sandbox.path().join("harness.db");
    let previous_harness_db = std::env::var_os("HARNESS_DB");
    std::env::set_var("HARNESS_DB", &harness_db);

    let mut config = HarnessConfig::default();
    config.server.project_root = project_root.clone();
    let server = Arc::new(HarnessServer::new(
        config,
        ThreadManager::new(),
        AgentRegistry::new("test"),
    ));
    let state = build_app_state(server).await?;
    {
        let mut rules = state.rules.write().await;
        rules.register_guard(harness_rules::engine::Guard {
            id: GuardId::from_str("TEST-GUARD"),
            script_path: guard_script,
            language: Language::Common,
            rules: vec![],
        });
    }

    let _cwd_guard = CwdGuard::switch_to(&other_cwd)?;
    let _response = gc_run(&state, Some(serde_json::json!(1))).await;

    match previous_harness_db {
        Some(v) => std::env::set_var("HARNESS_DB", v),
        None => std::env::remove_var("HARNESS_DB"),
    }

    let scanned_root = std::fs::read_to_string(&capture_file)?;
    assert_eq!(scanned_root, project_root.canonicalize()?.display().to_string());
    Ok(())
}
