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
fn concurrent_similar_prompts_keep_explicit_layer_attribution() {
    let short_static = AgentPromptLayers::new("static\n", "context\n", "dynamic\n");
    let long_static = AgentPromptLayers::new("static\ncontext\n", "dynamic\n", "");
    assert_eq!(
        short_static.to_prompt_string(),
        long_static.to_prompt_string()
    );

    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let spawn_request = |layers: AgentPromptLayers| {
        let barrier = std::sync::Arc::clone(&barrier);
        std::thread::spawn(move || {
            barrier.wait();
            let request = AgentRequest::from_prompt_layers(layers, PathBuf::from("/tmp/project"));
            barrier.wait();
            let system_prompt = request
                .claude_system_prompt()
                .map(|prompt| prompt.into_owned());
            let main_prompt = request.claude_main_prompt().into_owned();
            (request.prompt, system_prompt, main_prompt)
        })
    };

    let short_static_request = spawn_request(short_static);
    let long_static_request = spawn_request(long_static);
    let short_static_request = match short_static_request.join() {
        Ok(request) => request,
        Err(payload) => std::panic::resume_unwind(payload),
    };
    let long_static_request = match long_static_request.join() {
        Ok(request) => request,
        Err(payload) => std::panic::resume_unwind(payload),
    };

    assert_eq!(
        short_static_request,
        (
            "static\ncontext\ndynamic\n".to_string(),
            Some("static\n".to_string()),
            "context\ndynamic\n".to_string(),
        )
    );
    assert_eq!(
        long_static_request,
        (
            "static\ncontext\ndynamic\n".to_string(),
            Some("static\ncontext\n".to_string()),
            "dynamic\n".to_string(),
        )
    );
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
