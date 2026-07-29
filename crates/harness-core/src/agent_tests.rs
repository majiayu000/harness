use crate::agent::{AgentPromptLayers, AgentRequest, TurnRequest};
use crate::prompts;
use std::collections::HashMap;
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
fn agent_request_does_not_infer_layers_from_flattened_prompt() {
    let flattened =
        prompts::implement_from_issue(1471, None, Some("follow the spec")).to_prompt_string();
    let prompt = format!("Constitution\n\n{flattened}\n\n## Available Skills\n- review");
    let request = AgentRequest {
        prompt: prompt.clone(),
        project_root: PathBuf::from("/tmp/project"),
        ..AgentRequest::default()
    };

    assert_eq!(request.claude_system_prompt().as_deref(), None);
    assert_eq!(request.claude_main_prompt(), prompt);
}

#[test]
fn flattened_prompt_without_layers_does_not_cross_associate_with_similar_prompt() {
    let registered_flattened = prompts::PromptParts {
        static_instructions: "shared static instructions\n".to_string(),
        context: "shared request context\n".to_string(),
        dynamic_payload: "shared dynamic payload\n".to_string(),
    }
    .to_prompt_string();
    let similar_prompt = format!("{registered_flattened}runtime-specific suffix\n");
    let request = AgentRequest {
        prompt: similar_prompt.clone(),
        project_root: PathBuf::from("/tmp/project"),
        ..AgentRequest::default()
    };

    assert_eq!(request.claude_system_prompt().as_deref(), None);
    assert_eq!(request.claude_main_prompt(), similar_prompt);
}

#[test]
fn explicit_layers_keep_similar_prompts_test_isolated() {
    let short_layers = AgentPromptLayers::new("static\n", "context\n", "dynamic\n");
    let long_layers = AgentPromptLayers::new("static\ncontext\n", "dynamic\n", "runtime\n");

    let short_request =
        AgentRequest::from_prompt_layers(short_layers, PathBuf::from("/tmp/project"));
    let long_request = AgentRequest::from_prompt_layers(long_layers, PathBuf::from("/tmp/project"));

    assert_eq!(
        short_request.claude_system_prompt().as_deref(),
        Some("static\n")
    );
    assert_eq!(short_request.claude_main_prompt(), "context\ndynamic\n");
    assert_eq!(
        long_request.claude_system_prompt().as_deref(),
        Some("static\ncontext\n")
    );
    assert_eq!(long_request.claude_main_prompt(), "dynamic\nruntime\n");
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
        allowed_tools: None,
        context: vec![],
        timeout_secs: None,
        env_vars: HashMap::new(),
        capability_token: None,
    };

    assert_eq!(request.claude_system_prompt(), Some("static\n"));
    assert_eq!(request.claude_main_prompt(), "context\ndynamic\n");
}
