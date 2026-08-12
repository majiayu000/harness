use crate::agent::{AgentPromptLayers, AgentRequest};
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

    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    let spawn_request = |layers: AgentPromptLayers| {
        let barrier = std::sync::Arc::clone(&barrier);
        std::thread::spawn(move || {
            let flattened = prompts::PromptParts {
                static_instructions: layers.static_instructions.clone(),
                context: layers.context.clone(),
                dynamic_payload: layers.dynamic_payload.clone(),
            }
            .to_prompt_string();
            let request = AgentRequest::from_prompt_layers(layers, PathBuf::from("/tmp/project"));
            barrier.wait();
            let system_prompt = request
                .claude_system_prompt()
                .map(|prompt| prompt.into_owned());
            let main_prompt = request.claude_main_prompt().into_owned();
            (flattened, request.prompt, system_prompt, main_prompt)
        })
    };

    let short_static_request = spawn_request(short_static);
    let long_static_request = spawn_request(long_static);
    barrier.wait();

    let flattened_prompt = "static\ncontext\ndynamic\n".to_string();
    let flattened_request = AgentRequest {
        prompt: flattened_prompt.clone(),
        project_root: PathBuf::from("/tmp/project"),
        ..AgentRequest::default()
    };
    assert_eq!(flattened_request.claude_system_prompt().as_deref(), None);
    assert_eq!(flattened_request.claude_main_prompt(), flattened_prompt);

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
            "static\ncontext\ndynamic\n".to_string(),
            Some("static\n".to_string()),
            "context\ndynamic\n".to_string(),
        )
    );
    assert_eq!(
        long_static_request,
        (
            "static\ncontext\ndynamic\n".to_string(),
            "static\ncontext\ndynamic\n".to_string(),
            Some("static\ncontext\n".to_string()),
            "dynamic\n".to_string(),
        )
    );
}

#[test]
fn agent_request_uses_same_claude_layer_split() {
    let request = AgentRequest {
        prompt: "static\ncontext\ndynamic\n".to_string(),
        prompt_layers: Some(AgentPromptLayers::new("static\n", "context\n", "dynamic\n")),
        project_root: PathBuf::from("/tmp/project"),
        permission_mode: crate::config::agents::AgentPermissionMode::Scoped,
        model: None,
        reasoning_effort: None,
        execution_phase: None,
        sandbox_mode: None,
        approval_policy: None,
        allowed_tools: None,
        max_budget_usd: None,
        context: vec![],
        timeout_secs: None,
        env_vars: HashMap::new(),
        capability_token: None,
    };

    assert_eq!(request.claude_system_prompt().as_deref(), Some("static\n"));
    assert_eq!(request.claude_main_prompt(), "context\ndynamic\n");
}

#[test]
fn default_agent_request_is_scoped_to_standard_tools() {
    let request = AgentRequest::default();

    assert!(!request.uses_dangerously_skip_permissions());
    assert_eq!(
        request.effective_permission_mode(),
        crate::config::agents::AgentPermissionMode::Scoped
    );
    assert_eq!(
        request.scoped_allowed_tools(),
        vec!["Read", "Write", "Edit", "Bash"]
    );
}

#[test]
fn explicit_full_agent_request_keeps_egress_when_tools_are_restricted() {
    let full = AgentRequest {
        permission_mode: crate::config::agents::AgentPermissionMode::Full,
        allowed_tools: None,
        ..AgentRequest::default()
    };
    assert!(full.uses_dangerously_skip_permissions());

    let restricted = AgentRequest {
        allowed_tools: Some(vec!["Read".to_string()]),
        ..full
    };
    assert!(!restricted.uses_dangerously_skip_permissions());
    assert_eq!(
        restricted.effective_permission_mode(),
        crate::config::agents::AgentPermissionMode::Full
    );
    assert_eq!(restricted.scoped_allowed_tools(), vec!["Read"]);
}

#[test]
fn legacy_missing_tool_allowlist_keeps_unrestricted_behavior() {
    let request = AgentRequest {
        allowed_tools: None,
        ..AgentRequest::default()
    };

    assert!(request.uses_dangerously_skip_permissions());
    assert_eq!(
        request.effective_permission_mode(),
        crate::config::agents::AgentPermissionMode::Full
    );
    assert_eq!(
        crate::agent::AgentEgressMode::resolve(request.effective_permission_mode(), &[]),
        crate::agent::AgentEgressMode::Unrestricted
    );
}

#[test]
fn full_egress_is_preserved_when_tools_are_explicitly_empty() {
    let request = AgentRequest {
        permission_mode: crate::config::agents::AgentPermissionMode::Full,
        allowed_tools: Some(Vec::new()),
        ..AgentRequest::default()
    };

    assert!(!request.uses_dangerously_skip_permissions());
    assert!(request.scoped_allowed_tools().is_empty());
    assert_eq!(
        crate::agent::AgentEgressMode::resolve(request.effective_permission_mode(), &[]),
        crate::agent::AgentEgressMode::Unrestricted
    );
}

#[test]
fn egress_mode_is_fail_closed_and_allowlist_driven() {
    use crate::agent::AgentEgressMode;
    use crate::config::agents::AgentPermissionMode;

    assert_eq!(
        AgentEgressMode::resolve(AgentPermissionMode::Scoped, &[]),
        AgentEgressMode::DenyAll
    );
    assert_eq!(
        AgentEgressMode::resolve(AgentPermissionMode::Full, &[]),
        AgentEgressMode::Unrestricted
    );
    let allowlist = vec!["api.openai.com".to_string()];
    for permission_mode in [AgentPermissionMode::Scoped, AgentPermissionMode::Full] {
        assert_eq!(
            AgentEgressMode::resolve(permission_mode, &allowlist),
            AgentEgressMode::FirstPartyProxy
        );
    }
}

#[test]
fn configured_policy_overrides_direct_request_permissions_and_isolation() {
    let mut config = crate::config::HarnessConfig::default();
    config.agents.capability_profile = crate::config::agents::CapabilityProfile::Full;
    config.isolation.default_tier = crate::config::isolation::IsolationTier::Container;
    config.isolation.network_allowlist = vec![
        " github.com ".to_string(),
        String::new(),
        "api.openai.com".to_string(),
    ];
    let mut request = AgentRequest::default();

    request.apply_configured_policy(&config);

    assert_eq!(
        request.permission_mode,
        crate::config::agents::AgentPermissionMode::Full
    );
    assert_eq!(request.allowed_tools, None);
    assert_eq!(
        request.env_vars.get(crate::agent::AGENT_ISOLATION_TIER_ENV),
        Some(&"container".to_string())
    );
    assert_eq!(
        request
            .env_vars
            .get(crate::agent::AGENT_NETWORK_ALLOWLIST_ENV),
        Some(&"github.com,api.openai.com".to_string())
    );
}

#[test]
fn spawn_control_env_inherits_only_declared_non_blank_image_settings() {
    let process_env = HashMap::from([
        (
            crate::agent::AGENT_CONTAINER_IMAGE_ENV.to_string(),
            "example/agent@sha256:test".to_string(),
        ),
        (
            crate::agent::AGENT_EGRESS_PROXY_IMAGE_ENV.to_string(),
            "example/proxy@sha256:test".to_string(),
        ),
        ("OPERATOR_SECRET".to_string(), "secret".to_string()),
    ]);
    let mut env_vars = HashMap::new();

    crate::agent::inherit_agent_spawn_control_env_with(&mut env_vars, |key| {
        process_env.get(key).cloned()
    });

    assert_eq!(env_vars.len(), 2);
    assert_eq!(
        env_vars.get(crate::agent::AGENT_CONTAINER_IMAGE_ENV),
        Some(&"example/agent@sha256:test".to_string())
    );
    assert_eq!(
        env_vars.get(crate::agent::AGENT_EGRESS_PROXY_IMAGE_ENV),
        Some(&"example/proxy@sha256:test".to_string())
    );
}
