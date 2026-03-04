use std::sync::Arc;

use harness_agents::AgentRegistry;
use harness_core::{CodeAgent, HarnessConfig};

pub fn build_agent_registry(config: &HarnessConfig) -> AgentRegistry {
    let mut registry = AgentRegistry::new(config.agents.default_agent.clone());
    registry.register(
        "claude",
        Arc::new(harness_agents::claude::ClaudeCodeAgent::new(
            config.agents.claude.cli_path.clone(),
            config.agents.claude.default_model.clone(),
        )),
    );
    registry.register(
        "codex",
        Arc::new(harness_agents::codex::CodexAgent::new(
            config.agents.codex.cli_path.clone(),
        )),
    );

    if let Ok(api_key) = std::env::var("ANTHROPIC_API_KEY") {
        if !api_key.trim().is_empty() {
            registry.register(
                "anthropic-api",
                Arc::new(harness_agents::anthropic_api::AnthropicApiAgent::new(
                    api_key,
                    config.agents.anthropic_api.base_url.clone(),
                    config.agents.anthropic_api.default_model.clone(),
                )),
            );
        }
    }

    registry
}

pub fn resolve_agent(
    config: &HarnessConfig,
    requested: Option<&str>,
) -> anyhow::Result<Arc<dyn CodeAgent>> {
    let registry = build_agent_registry(config);
    let selected = requested.unwrap_or(config.agents.default_agent.as_str());

    registry.get(selected).ok_or_else(|| {
        let mut available = registry.list();
        available.sort_unstable();
        anyhow::anyhow!(
            "agent '{}' is not registered. available agents: {}",
            selected,
            available.join(", ")
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::HarnessConfig;

    #[test]
    fn registry_contains_claude_and_codex() {
        let config = HarnessConfig::default();
        let registry = build_agent_registry(&config);
        let agents = registry.list();
        assert!(agents.contains(&"claude"), "claude not registered");
        assert!(agents.contains(&"codex"), "codex not registered");
    }

    #[test]
    fn resolve_agent_returns_claude_by_name() {
        let config = HarnessConfig::default();
        let agent = resolve_agent(&config, Some("claude")).unwrap();
        assert_eq!(agent.name(), "claude");
    }

    #[test]
    fn resolve_agent_returns_codex_by_name() {
        let config = HarnessConfig::default();
        let agent = resolve_agent(&config, Some("codex")).unwrap();
        assert_eq!(agent.name(), "codex");
    }

    #[test]
    fn resolve_agent_defaults_to_config_default() {
        let config = HarnessConfig::default();
        let agent = resolve_agent(&config, None).unwrap();
        assert_eq!(agent.name(), "claude");
    }

    #[test]
    fn resolve_agent_unknown_name_lists_available_agents() {
        let config = HarnessConfig::default();
        let result = resolve_agent(&config, Some("unknown-agent"));
        let err = result.err().expect("expected an error for unknown agent");
        let msg = err.to_string();
        assert!(msg.contains("unknown-agent"), "error should name the requested agent");
        assert!(msg.contains("available agents:"), "error should list available agents");
        assert!(msg.contains("claude"), "available agents should include claude");
        assert!(msg.contains("codex"), "available agents should include codex");
    }
}
