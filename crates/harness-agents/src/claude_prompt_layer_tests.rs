use super::*;
use harness_core::agent::{AgentPromptLayers, AgentRequest};

#[test]
fn base_args_sends_static_layer_through_append_system_prompt() {
    let agent = ClaudeCodeAgent::new(
        PathBuf::from("claude"),
        "test-model".to_string(),
        SandboxMode::DangerFullAccess,
    );
    let req = AgentRequest {
        prompt: "static\ncontext\ndynamic\n".to_string(),
        prompt_layers: Some(AgentPromptLayers::new("static\n", "context\n", "dynamic\n")),
        ..AgentRequest::default()
    };

    let args: Vec<String> = agent
        .base_args(&req)
        .iter()
        .map(|arg| arg.to_string_lossy().to_string())
        .collect();

    assert_eq!(args[0], "-p");
    assert_eq!(args[1], "context\ndynamic\n");
    assert!(args
        .windows(2)
        .any(|window| window == ["--append-system-prompt", "static\n"]));
    assert!(args.contains(&"--exclude-dynamic-system-prompt-sections".to_string()));
}

#[test]
fn base_args_keeps_flattened_prompt_without_layers() {
    let agent = ClaudeCodeAgent::new(
        PathBuf::from("claude"),
        "test-model".to_string(),
        SandboxMode::DangerFullAccess,
    );
    let req = AgentRequest {
        prompt: "flat prompt".to_string(),
        ..AgentRequest::default()
    };

    let args: Vec<String> = agent
        .base_args(&req)
        .iter()
        .map(|arg| arg.to_string_lossy().to_string())
        .collect();

    assert_eq!(args[0], "-p");
    assert_eq!(args[1], "flat prompt");
    assert!(!args.contains(&"--append-system-prompt".to_string()));
    assert!(!args.contains(&"--exclude-dynamic-system-prompt-sections".to_string()));
}

#[test]
fn base_args_keeps_flattened_prompt_for_static_only_layers() {
    let agent = ClaudeCodeAgent::new(
        PathBuf::from("claude"),
        "test-model".to_string(),
        SandboxMode::DangerFullAccess,
    );
    let req = AgentRequest {
        prompt: "static only".to_string(),
        prompt_layers: Some(AgentPromptLayers::new("static only", "", "")),
        ..AgentRequest::default()
    };

    let args: Vec<String> = agent
        .base_args(&req)
        .iter()
        .map(|arg| arg.to_string_lossy().to_string())
        .collect();

    assert_eq!(args[0], "-p");
    assert_eq!(args[1], "static only");
    assert!(!args.contains(&"--append-system-prompt".to_string()));
    assert!(!args.contains(&"--exclude-dynamic-system-prompt-sections".to_string()));
}
