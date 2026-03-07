use harness_agents::AgentRegistry;
use harness_core::HarnessConfig;
use harness_rules::exec_policy::{ExecDecision, MatchOptions};
use harness_server::{http::build_app_state, server::HarnessServer, thread_manager::ThreadManager};
use std::sync::Arc;

#[tokio::test]
async fn check_command_policy_startup_loads_exec_policy_and_requirements() -> anyhow::Result<()> {
    let sandbox = tempfile::tempdir()?;
    let project_root = sandbox.path().join("project-root");
    let policy_path = sandbox.path().join("policy.star");
    let requirements_path = sandbox.path().join("requirements.toml");
    std::fs::create_dir_all(&project_root)?;

    std::fs::write(
        &policy_path,
        r#"
prefix_rule(pattern = ["git", "status"], decision = "allow")
"#,
    )?;
    std::fs::write(
        &requirements_path,
        r#"
[rules]
[[rules.prefix_rules]]
pattern = [{ token = "rm" }, { token = "-rf" }]
decision = "forbidden"
"#,
    )?;

    let mut config = HarnessConfig::default();
    config.server.data_dir = sandbox.path().join("server-data");
    config.server.project_root = project_root;
    config.rules.exec_policy_paths = vec![policy_path];
    config.rules.requirements_path = Some(requirements_path);
    let server = Arc::new(HarnessServer::new(
        config,
        ThreadManager::new(),
        AgentRegistry::new("test"),
    ));
    let state = build_app_state(server).await?;
    let rules = state.rules.read().await;

    let allow_result = rules.check_command_policy(
        &["git".to_string(), "status".to_string()],
        &MatchOptions::default(),
    );
    assert_eq!(allow_result.decision, Some(ExecDecision::Allow));

    let forbidden_result = rules.check_command_policy(
        &["rm".to_string(), "-rf".to_string(), "/tmp/x".to_string()],
        &MatchOptions::default(),
    );
    assert_eq!(forbidden_result.decision, Some(ExecDecision::Forbidden));
    Ok(())
}
