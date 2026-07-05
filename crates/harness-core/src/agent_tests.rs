use crate::agent::{AgentPromptLayers, AgentRequest, TurnRequest};
use crate::prompts;
use std::path::PathBuf;

#[test]
fn agent_request_from_prompt_layers_keeps_flattened_fallback() {
    let layers = AgentPromptLayers::new("static\n", "context\n", "dynamic\n");

    let request = AgentRequest::from_prompt_layers(layers.clone(), PathBuf::from("/tmp/project"));

    assert_eq!(request.prompt, "static\ncontext\ndynamic\n");
    assert_eq!(request.prompt_layers, Some(layers));
}

#[test]
fn claude_prompt_helpers_split_static_from_main_prompt() {
    let request = AgentRequest::from_prompt_layers(
        AgentPromptLayers::new("static instructions\n", "context\n", "dynamic\n"),
        PathBuf::from("/tmp/project"),
    );

    assert_eq!(
        request.claude_system_prompt().as_deref(),
        Some("static instructions\n")
    );
    assert_eq!(request.claude_main_prompt(), "context\ndynamic\n");
    assert_eq!(request.prompt, "static instructions\ncontext\ndynamic\n");
}

#[test]
fn claude_prompt_helpers_fallback_to_flattened_prompt_without_layers() {
    let request = AgentRequest {
        prompt: "flat prompt".to_string(),
        project_root: PathBuf::from("/tmp/project"),
        ..AgentRequest::default()
    };

    assert_eq!(request.claude_system_prompt().as_deref(), None);
    assert_eq!(request.claude_main_prompt(), "flat prompt");
}

#[test]
fn claude_prompt_helpers_do_not_split_static_only_layers() {
    let request = AgentRequest::from_prompt_layers(
        AgentPromptLayers::new("static only\n", "", ""),
        PathBuf::from("/tmp/project"),
    );

    assert_eq!(request.claude_system_prompt().as_deref(), None);
    assert_eq!(request.claude_main_prompt(), "static only\n");
}

#[test]
fn agent_request_recovers_layers_from_registered_flattened_prompt_with_runtime_additions() {
    let flattened =
        prompts::implement_from_issue(1471, None, Some("follow the spec")).to_prompt_string();
    let request = AgentRequest {
        prompt: format!("Constitution\n\n{flattened}\n\n## Available Skills\n- review"),
        project_root: PathBuf::from("/tmp/project"),
        ..AgentRequest::default()
    };

    let Some(system_prompt) = request.claude_system_prompt() else {
        panic!("registered PromptParts should provide a Claude system prompt");
    };
    let main_prompt = request.claude_main_prompt();

    assert!(system_prompt.contains("Senior Engineer"));
    assert!(!system_prompt.contains("follow the spec"));
    assert!(main_prompt.contains("Constitution"));
    assert!(main_prompt.contains("follow the spec"));
    assert!(main_prompt.contains("## Available Skills"));
}

#[test]
fn turn_request_uses_same_claude_layer_split() {
    let request = TurnRequest {
        prompt: "static\ncontext\ndynamic\n".to_string(),
        prompt_layers: Some(AgentPromptLayers::new("static\n", "context\n", "dynamic\n")),
        project_root: PathBuf::from("/tmp/project"),
        model: None,
        reasoning_effort: None,
        execution_phase: None,
        sandbox_mode: None,
        approval_policy: None,
        allowed_tools: vec![],
        context: vec![],
        timeout_secs: None,
        capability_token: None,
    };

    assert_eq!(request.claude_system_prompt(), Some("static\n"));
    assert_eq!(request.claude_main_prompt(), "context\ndynamic\n");
}
