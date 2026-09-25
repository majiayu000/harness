use super::*;
use harness_core::config::agents::AgentPermissionMode;

fn args_to_strings(args: &[OsString]) -> Vec<String> {
    args.iter()
        .map(|arg| arg.to_string_lossy().to_string())
        .collect()
}

fn test_agent() -> ClaudeCodeAgent {
    ClaudeCodeAgent::new(
        PathBuf::from("claude"),
        "test-model".to_string(),
        SandboxMode::DangerFullAccess,
    )
}

#[test]
fn explicit_full_profile_uses_dangerously_skip_permissions() {
    let req = AgentRequest {
        permission_mode: AgentPermissionMode::Full,
        allowed_tools: None,
        ..AgentRequest::default()
    };
    let args = args_to_strings(&test_agent().base_args(&req));

    assert!(args.contains(&"--dangerously-skip-permissions".to_string()));
    assert!(!args.contains(&"--allowedTools".to_string()));
}

#[test]
fn default_request_uses_standard_allowed_tools() {
    let args = args_to_strings(&test_agent().base_args(&AgentRequest::default()));

    assert!(args.contains(&"--allowedTools".to_string()));
    assert!(args.contains(&"--permission-mode".to_string()));
    assert!(!args.contains(&"--dangerously-skip-permissions".to_string()));
    let tools = args
        .iter()
        .skip_while(|arg| *arg != "--allowedTools")
        .nth(1)
        .cloned()
        .unwrap_or_default();
    assert_eq!(tools, "Read,Write,Edit,Bash");
}

#[test]
fn read_only_profile_uses_read_only_allowed_tools() {
    let req = AgentRequest {
        allowed_tools: Some(vec![
            "Read".to_string(),
            "Grep".to_string(),
            "Glob".to_string(),
        ]),
        ..AgentRequest::default()
    };
    let args = args_to_strings(&test_agent().base_args(&req));

    assert!(!args.contains(&"--dangerously-skip-permissions".to_string()));
    let tools = args
        .iter()
        .skip_while(|arg| *arg != "--allowedTools")
        .nth(1)
        .cloned()
        .unwrap_or_default();
    assert_eq!(tools, "Read,Grep,Glob");
}

#[test]
fn explicit_allowlist_overrides_full_permission_mode() {
    let req = AgentRequest {
        permission_mode: AgentPermissionMode::Full,
        allowed_tools: Some(vec!["Read".to_string()]),
        ..AgentRequest::default()
    };
    let args = args_to_strings(&test_agent().base_args(&req));

    assert!(!args.contains(&"--dangerously-skip-permissions".to_string()));
    assert!(args.contains(&"--allowedTools".to_string()));
    assert_eq!(
        args.iter().filter(|arg| *arg == "--allowedTools").count(),
        1
    );
}
