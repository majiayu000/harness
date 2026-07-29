//! The single place an [`AgentRegistry`] is assembled from configuration.
//!
//! Every entry point — `serve`, `exec`, `gc`, and the MCP server — used to
//! hand-assemble its own registry, and the four copies had drifted: the
//! provider backpressure gate was wired only in `serve`, `reasoning_budget`
//! only in `serve` and `exec`, adapters only in `serve` and the MCP server,
//! and the `anthropic-api` backend was missing from the MCP server entirely.
//! Adding a backend or a config knob meant editing four files, and history
//! shows that did not happen.
//!
//! Anything that varies per entry point is a parameter here. Everything else
//! is applied identically by construction.

use crate::claude::ClaudeCodeAgent;
use crate::codex::CodexAgent;
use crate::codex_adapter::CodexAdapter;
use crate::provider_backpressure::ProviderBackpressureGate;
use crate::registry::{AdapterExecutionStrategy, AgentRegistry};
use async_trait::async_trait;
use harness_core::{
    agent::{AgentRequest, AgentResponse, CodeAgent, StreamItem},
    config::agents::{AgentsConfig, SandboxMode},
    types::Capability,
};
use std::sync::Arc;

/// Environment variable that supplies the Anthropic API key. The
/// `anthropic-api` backend is registered only when it is set.
const ANTHROPIC_API_KEY_ENV: &str = "ANTHROPIC_API_KEY";

/// An agent whose configuration is sealed at the canonical builder boundary.
///
/// The wrapped concrete agent stays private so downstream entry points can
/// execute it but cannot overwrite configured fields or call fluent setters.
struct ConfiguredAgent<A> {
    inner: A,
}

impl<A> ConfiguredAgent<A> {
    fn new(inner: A) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl<A: CodeAgent> CodeAgent for ConfiguredAgent<A> {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn capabilities(&self) -> Vec<Capability> {
        self.inner.capabilities()
    }

    async fn execute(&self, req: AgentRequest) -> harness_core::error::Result<AgentResponse> {
        self.inner.execute(req).await
    }

    async fn execute_stream(
        &self,
        req: AgentRequest,
        tx: tokio::sync::mpsc::Sender<StreamItem>,
    ) -> harness_core::error::Result<()> {
        self.inner.execute_stream(req, tx).await
    }
}

/// Builds the Claude backend with every configured knob applied.
///
/// Provider backpressure is derived internally so callers cannot replace the
/// configured gate.
pub fn claude_agent_from_config(
    config: &AgentsConfig,
    sandbox_mode: SandboxMode,
) -> impl CodeAgent {
    configured_claude_agent_from_config(config, sandbox_mode)
}

fn configured_claude_agent_from_config(
    config: &AgentsConfig,
    sandbox_mode: SandboxMode,
) -> ConfiguredAgent<ClaudeCodeAgent> {
    let gate = ProviderBackpressureGate::from_claude_config(&config.claude.provider_backpressure);
    claude_agent_from_config_with_gate(config, sandbox_mode, gate)
}

fn claude_agent_from_config_with_gate(
    config: &AgentsConfig,
    sandbox_mode: SandboxMode,
    gate: ProviderBackpressureGate,
) -> ConfiguredAgent<ClaudeCodeAgent> {
    let mut agent = ClaudeCodeAgent::new(
        config.claude.cli_path.clone(),
        config.claude.default_model.clone(),
        sandbox_mode,
    )
    .with_provider_backpressure_gate(gate)
    .with_stream_timeout(config.stream_timeout_secs);
    if let Some(budget) = config.claude.reasoning_budget.clone() {
        agent = agent.with_reasoning_budget(budget);
    }
    ConfiguredAgent::new(agent)
}

/// Builds the Codex backend with every configured knob applied.
pub fn codex_agent_from_config(config: &AgentsConfig, sandbox_mode: SandboxMode) -> impl CodeAgent {
    configured_codex_agent_from_config(config, sandbox_mode)
}

fn configured_codex_agent_from_config(
    config: &AgentsConfig,
    sandbox_mode: SandboxMode,
) -> ConfiguredAgent<CodexAgent> {
    ConfiguredAgent::new(
        CodexAgent::from_config(config.codex.clone(), sandbox_mode)
            .with_stream_timeout(config.stream_timeout_secs),
    )
}

/// Assembles the registry every entry point runs on.
///
/// `sandbox_mode` is a parameter rather than read from `config` because `exec`
/// resolves it per invocation from its CLI flags.
pub fn registry_from_config(
    config: &AgentsConfig,
    sandbox_mode: SandboxMode,
) -> anyhow::Result<AgentRegistry> {
    let mut registry = AgentRegistry::new(&config.default_agent);
    registry.set_complexity_preferences(config.complexity_preferred_agents.clone());

    registry.register(
        "claude",
        Arc::new(claude_agent_from_config_with_gate(
            config,
            sandbox_mode,
            ProviderBackpressureGate::from_claude_config(&config.claude.provider_backpressure),
        )),
    );

    registry.register(
        "codex",
        Arc::new(codex_agent_from_config(config, sandbox_mode)),
    );
    let codex_config = config.codex.clone();
    registry
        .register_adapter_factory_with_strategy(
            "codex",
            move || {
                Arc::new(CodexAdapter::from_config(
                    codex_config.clone(),
                    sandbox_mode,
                ))
            },
            AdapterExecutionStrategy::ExecuteTurns,
        )
        .map_err(|error| anyhow::anyhow!("failed to attach the codex adapter: {error}"))?;

    if let Ok(api_key) = std::env::var(ANTHROPIC_API_KEY_ENV) {
        registry.register(
            "anthropic-api",
            Arc::new(crate::anthropic_api::AnthropicApiAgent::from_config(
                api_key,
                &config.anthropic_api,
            )),
        );
    }

    Ok(registry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::error::HarnessError;
    use harness_core::types::ReasoningBudget;
    use std::num::NonZeroUsize;

    struct TestAgent;

    #[async_trait]
    impl CodeAgent for TestAgent {
        fn name(&self) -> &str {
            "test"
        }

        fn capabilities(&self) -> Vec<Capability> {
            vec![Capability::Read]
        }

        async fn execute(&self, req: AgentRequest) -> harness_core::error::Result<AgentResponse> {
            Err(HarnessError::AgentExecution(req.prompt))
        }

        async fn execute_stream(
            &self,
            req: AgentRequest,
            _tx: tokio::sync::mpsc::Sender<StreamItem>,
        ) -> harness_core::error::Result<()> {
            Err(HarnessError::AgentExecution(req.prompt))
        }
    }

    fn config() -> AgentsConfig {
        AgentsConfig {
            default_agent: "claude".to_string(),
            complexity_preferred_agents: vec!["codex".to_string(), "claude".to_string()],
            stream_timeout_secs: Some(120),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn configured_agent_delegates_only_the_code_agent_contract() {
        let agent = ConfiguredAgent::new(TestAgent);
        assert_eq!(agent.name(), "test");
        assert_eq!(agent.capabilities(), vec![Capability::Read]);

        let execute_error = agent
            .execute(AgentRequest {
                prompt: "execute".to_string(),
                ..Default::default()
            })
            .await
            .expect_err("the test agent should surface its execute result");
        assert!(execute_error.to_string().contains("execute"));

        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let stream_error = agent
            .execute_stream(
                AgentRequest {
                    prompt: "stream".to_string(),
                    ..Default::default()
                },
                tx,
            )
            .await
            .expect_err("the test agent should surface its stream result");
        assert!(stream_error.to_string().contains("stream"));
    }

    #[test]
    fn claude_backend_applies_the_configured_stream_timeout() {
        let agent = configured_claude_agent_from_config(&config(), SandboxMode::default());
        assert_eq!(agent.inner.stream_timeout_secs, Some(120));
    }

    #[test]
    fn claude_backend_applies_a_disabled_stream_timeout() {
        let mut config = config();
        config.stream_timeout_secs = None;
        let agent = configured_claude_agent_from_config(&config, SandboxMode::default());
        assert_eq!(
            agent.inner.stream_timeout_secs, None,
            "an explicitly disabled timeout must not fall back to the 3600s default"
        );
    }

    #[test]
    fn claude_backend_applies_the_configured_reasoning_budget() {
        let mut config = config();
        config.claude.reasoning_budget = Some(ReasoningBudget::default());
        let agent = configured_claude_agent_from_config(&config, SandboxMode::default());
        assert!(agent.inner.reasoning_budget.is_some());
    }

    #[test]
    fn claude_backend_applies_the_configured_provider_gate() {
        let mut config = config();
        config.claude.provider_backpressure.max_concurrent_sessions = NonZeroUsize::new(2);
        let agent = configured_claude_agent_from_config(&config, SandboxMode::default());
        assert!(
            agent.inner.provider_gate.is_enabled(),
            "a configured concurrency limit must reach the agent, not be dropped"
        );
    }

    #[test]
    fn codex_backend_applies_the_configured_stream_timeout() {
        let agent = configured_codex_agent_from_config(&config(), SandboxMode::default());
        assert_eq!(agent.inner.stream_timeout_secs, Some(120));
    }

    #[test]
    fn registry_registers_every_backend_and_adapter() {
        let registry =
            registry_from_config(&config(), SandboxMode::default()).expect("registry builds");
        let mut names = registry.list();
        names.sort_unstable();
        assert!(names.contains(&"claude"));
        assert!(names.contains(&"codex"));
        assert!(
            registry.get_adapter("claude").is_none(),
            "GH-1786 removed the unreachable ClaudeAdapter; claude has no adapter"
        );
        assert_eq!(
            registry.adapter_strategy("codex"),
            Some(AdapterExecutionStrategy::ExecuteTurns)
        );
        assert_eq!(registry.resolved_default_agent_name(), Some("claude"));
    }

    #[test]
    fn registry_honours_the_sandbox_mode_argument_over_the_config() {
        // `exec` resolves the sandbox mode from its own flags, so the argument
        // must win over `config.sandbox_mode`.
        let mut config = config();
        config.sandbox_mode = SandboxMode::ReadOnly;
        let agent = configured_claude_agent_from_config(&config, SandboxMode::WorkspaceWrite);
        assert_eq!(agent.inner.sandbox_mode, SandboxMode::WorkspaceWrite);
    }
}
