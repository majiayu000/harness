//! Launch-argument contract for deny-all-tools Codex turns (agent-contract
//! enforcement, Slice B).

use super::super::CodexAgent;
use harness_core::agent::AgentRequest;
use harness_core::config::agents::SandboxMode;
use std::path::PathBuf;

fn deny_all_request() -> AgentRequest {
    AgentRequest {
        prompt: "classify".to_string(),
        project_root: PathBuf::from("/tmp/project"),
        allowed_tools: Some(Vec::new()),
        sandbox_mode: Some(SandboxMode::ReadOnly),
        ..Default::default()
    }
}

fn args_of(agent: &CodexAgent, request: &AgentRequest) -> Vec<String> {
    agent
        .base_args(request)
        .iter()
        .map(|value| value.to_string_lossy().to_string())
        .collect()
}

#[test]
fn deny_all_tools_launch_disables_config_rules_and_persistence() {
    let agent = CodexAgent::new(PathBuf::from("codex"), SandboxMode::WorkspaceWrite);
    let args = args_of(&agent, &deny_all_request());

    assert!(args.iter().any(|arg| arg == "--ignore-user-config"));
    assert!(args.iter().any(|arg| arg == "--ignore-rules"));
    assert_eq!(
        args.iter().filter(|arg| *arg == "--ephemeral").count(),
        1,
        "deny-all launch is ephemeral exactly once"
    );
    // The declared sandbox still reaches codex itself.
    assert!(args.windows(2).any(|window| window == ["-s", "read-only"]));
}

#[test]
fn deny_all_tools_skips_outer_process_sandbox_but_keeps_codex_sandbox() {
    let agent = CodexAgent::new(PathBuf::from("codex"), SandboxMode::WorkspaceWrite);
    let request = deny_all_request();
    assert_eq!(
        agent.process_sandbox_mode(&request),
        SandboxMode::DangerFullAccess,
        "the codex process is not double-wrapped; codex applies the declared sandbox internally"
    );

    let ordinary = AgentRequest {
        prompt: "implement".to_string(),
        project_root: PathBuf::from("/tmp/project"),
        sandbox_mode: Some(SandboxMode::ReadOnly),
        ..Default::default()
    };
    assert_eq!(
        agent.process_sandbox_mode(&ordinary),
        SandboxMode::ReadOnly,
        "ordinary requests keep the outer process sandbox"
    );
}

#[test]
fn codex_exec_backend_claims_every_contract_capability() {
    use harness_core::agent::AgentBackend;
    let agent = CodexAgent::new(PathBuf::from("codex"), SandboxMode::WorkspaceWrite);
    let capabilities = agent.agent_contract_capabilities();
    assert!(
        capabilities.missing_for_enforcement().is_empty(),
        "codex exec is the first conforming backend: {capabilities:?}"
    );
}

#[test]
fn ordinary_launch_keeps_user_config_and_rules() {
    let agent = CodexAgent::new(PathBuf::from("codex"), SandboxMode::WorkspaceWrite);
    let request = AgentRequest {
        prompt: "implement".to_string(),
        project_root: PathBuf::from("/tmp/project"),
        allowed_tools: None,
        ..Default::default()
    };
    let args = args_of(&agent, &request);

    assert!(!args.iter().any(|arg| arg == "--ignore-user-config"));
    assert!(!args.iter().any(|arg| arg == "--ignore-rules"));
    assert!(!args.iter().any(|arg| arg == "--ephemeral"));
}
