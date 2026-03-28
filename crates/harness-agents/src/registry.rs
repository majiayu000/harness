use harness_core::{
    agent::AgentAdapter, agent::CodeAgent, agent::TaskClassification, agent::TaskComplexity,
    error::HarnessError,
};
use std::collections::HashMap;
use std::sync::Arc;

pub struct AgentRegistry {
    agents: HashMap<String, Arc<dyn CodeAgent>>,
    registration_order: Vec<String>,
    default_agent: String,
    complexity_preferred_agents: Vec<String>,
}

impl AgentRegistry {
    pub fn new(default_agent: impl Into<String>) -> Self {
        Self {
            agents: HashMap::new(),
            registration_order: Vec::new(),
            default_agent: default_agent.into(),
            complexity_preferred_agents: Vec::new(),
        }
    }

    pub fn set_complexity_preferences(&mut self, preferred_agents: Vec<String>) {
        self.complexity_preferred_agents = preferred_agents
            .into_iter()
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty())
            .collect();
    }

    pub fn register(&mut self, name: impl Into<String>, agent: Arc<dyn CodeAgent>) {
        let name = name.into();
        if !self.agents.contains_key(&name) {
            self.registration_order.push(name.clone());
        }
        self.agents.insert(name, agent);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn CodeAgent>> {
        self.agents.get(name).cloned()
    }

    pub fn resolved_default_agent_name(&self) -> Option<&str> {
        let configured = self.default_agent.trim();
        if !configured.is_empty()
            && !configured.eq_ignore_ascii_case("auto")
            && self.agents.contains_key(configured)
        {
            return Some(configured);
        }
        self.registration_order
            .iter()
            .find(|name| self.agents.contains_key(name.as_str()))
            .map(String::as_str)
    }

    pub fn default_agent(&self) -> Option<Arc<dyn CodeAgent>> {
        self.resolved_default_agent_name()
            .and_then(|name| self.get(name))
    }

    pub fn dispatch(
        &self,
        task: &TaskClassification,
    ) -> harness_core::error::Result<Arc<dyn CodeAgent>> {
        let preferred = match task.complexity {
            TaskComplexity::Critical | TaskComplexity::Complex => self
                .complexity_preferred_agents
                .iter()
                .find_map(|name| self.get(name)),
            _ => None,
        };

        preferred
            .or_else(|| self.default_agent())
            .ok_or_else(|| HarnessError::AgentNotFound("no agents registered".to_string()))
    }

    pub fn list(&self) -> Vec<&str> {
        self.registration_order.iter().map(|k| k.as_str()).collect()
    }
}

/// Registry for streaming `AgentAdapter` implementations.
///
/// Coexists with `AgentRegistry` — when an adapter is available for a given
/// agent name, the task executor prefers it over the legacy `CodeAgent` path.
pub struct AdapterRegistry {
    adapters: HashMap<String, Arc<dyn AgentAdapter>>,
    default_adapter: String,
}

impl AdapterRegistry {
    pub fn new(default_adapter: impl Into<String>) -> Self {
        Self {
            adapters: HashMap::new(),
            default_adapter: default_adapter.into(),
        }
    }

    pub fn register(&mut self, name: impl Into<String>, adapter: Arc<dyn AgentAdapter>) {
        self.adapters.insert(name.into(), adapter);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn AgentAdapter>> {
        self.adapters.get(name).cloned()
    }

    pub fn default_adapter(&self) -> Option<Arc<dyn AgentAdapter>> {
        self.get(&self.default_adapter)
    }

    pub fn list(&self) -> Vec<&str> {
        self.adapters.keys().map(|k| k.as_str()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::{
        agent::AgentEvent, agent::AgentRequest, agent::AgentResponse, agent::CodeAgent,
        agent::StreamItem, agent::TaskClassification, agent::TaskComplexity, agent::TurnRequest,
        types::Capability, types::TokenUsage,
    };

    struct StubAgent {
        agent_name: &'static str,
    }

    #[async_trait::async_trait]
    impl CodeAgent for StubAgent {
        fn name(&self) -> &str {
            self.agent_name
        }

        fn capabilities(&self) -> Vec<Capability> {
            vec![]
        }

        async fn execute(&self, _req: AgentRequest) -> harness_core::error::Result<AgentResponse> {
            Ok(AgentResponse {
                output: String::new(),
                stderr: String::new(),
                items: vec![],
                token_usage: TokenUsage {
                    input_tokens: 0,
                    output_tokens: 0,
                    total_tokens: 0,
                    cost_usd: 0.0,
                },
                model: self.agent_name.to_string(),
                exit_code: Some(0),
            })
        }

        async fn execute_stream(
            &self,
            _req: AgentRequest,
            _tx: tokio::sync::mpsc::Sender<StreamItem>,
        ) -> harness_core::error::Result<()> {
            Ok(())
        }
    }

    fn classification(complexity: TaskComplexity) -> TaskClassification {
        TaskClassification {
            complexity,
            language: None,
            requires_write: false,
            requires_network: false,
        }
    }

    fn registry_with_default() -> AgentRegistry {
        let mut registry = AgentRegistry::new("default-agent");
        registry.register(
            "default-agent",
            Arc::new(StubAgent {
                agent_name: "default-agent",
            }),
        );
        registry
    }

    #[test]
    fn dispatch_complex_task_prefers_configured_complexity_agent() {
        let mut registry = registry_with_default();
        registry.register(
            "worker-x",
            Arc::new(StubAgent {
                agent_name: "worker-x",
            }),
        );
        registry.set_complexity_preferences(vec!["worker-x".to_string()]);

        let agent = registry
            .dispatch(&classification(TaskComplexity::Complex))
            .unwrap();
        assert_eq!(agent.name(), "worker-x");
    }

    #[test]
    fn dispatch_complex_task_falls_back_to_default_when_preferred_missing() {
        let mut registry = registry_with_default();
        registry.register(
            "anthropic-api",
            Arc::new(StubAgent {
                agent_name: "anthropic-api",
            }),
        );
        registry.set_complexity_preferences(vec!["not-registered".to_string()]);

        let agent = registry
            .dispatch(&classification(TaskComplexity::Complex))
            .unwrap();
        assert_eq!(agent.name(), "default-agent");
    }

    #[test]
    fn dispatch_simple_task_uses_default_agent() {
        let mut registry = registry_with_default();
        registry.register(
            "worker-x",
            Arc::new(StubAgent {
                agent_name: "worker-x",
            }),
        );
        registry.set_complexity_preferences(vec!["worker-x".to_string()]);

        let agent = registry
            .dispatch(&classification(TaskComplexity::Simple))
            .unwrap();
        assert_eq!(agent.name(), "default-agent");
    }

    #[test]
    fn dispatch_critical_task_prefers_configured_complexity_agent() {
        let mut registry = registry_with_default();
        registry.register(
            "worker-x",
            Arc::new(StubAgent {
                agent_name: "worker-x",
            }),
        );
        registry.set_complexity_preferences(vec!["worker-x".to_string()]);

        let agent = registry
            .dispatch(&classification(TaskComplexity::Critical))
            .unwrap();
        assert_eq!(agent.name(), "worker-x");
    }

    #[test]
    fn auto_default_uses_first_registered_agent() {
        let mut registry = AgentRegistry::new("auto");
        registry.register(
            "alpha",
            Arc::new(StubAgent {
                agent_name: "alpha",
            }),
        );
        registry.register("beta", Arc::new(StubAgent { agent_name: "beta" }));

        let agent = registry.default_agent().unwrap();
        assert_eq!(agent.name(), "alpha");
    }

    #[test]
    fn unknown_default_falls_back_to_first_registered_agent() {
        let mut registry = AgentRegistry::new("ghost");
        registry.register(
            "alpha",
            Arc::new(StubAgent {
                agent_name: "alpha",
            }),
        );

        let agent = registry.default_agent().unwrap();
        assert_eq!(agent.name(), "alpha");
    }

    #[test]
    fn dispatch_returns_error_when_no_agents_registered() {
        let registry = AgentRegistry::new("missing");
        let result = registry.dispatch(&classification(TaskComplexity::Simple));
        assert!(result.is_err());
    }

    #[test]
    fn anthropic_api_agent_is_registered_and_retrievable() {
        let mut registry = AgentRegistry::new("anthropic-api");
        registry.register(
            "anthropic-api",
            Arc::new(StubAgent {
                agent_name: "anthropic-api",
            }),
        );

        let agent = registry.get("anthropic-api");
        assert!(agent.is_some());
        assert_eq!(agent.unwrap().name(), "anthropic-api");
    }

    struct StubAdapter {
        adapter_name: &'static str,
    }

    #[async_trait::async_trait]
    impl AgentAdapter for StubAdapter {
        fn name(&self) -> &str {
            self.adapter_name
        }

        async fn start_turn(
            &self,
            _req: TurnRequest,
            _tx: tokio::sync::mpsc::Sender<AgentEvent>,
        ) -> harness_core::error::Result<()> {
            Ok(())
        }

        async fn interrupt(&self) -> harness_core::error::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn adapter_registry_register_and_get_roundtrip() {
        let mut registry = AdapterRegistry::new("mock");
        registry.register(
            "mock",
            Arc::new(StubAdapter {
                adapter_name: "mock",
            }),
        );

        let adapter = registry.get("mock");
        assert!(adapter.is_some());
        assert_eq!(adapter.unwrap().name(), "mock");
    }

    #[test]
    fn adapter_registry_default_adapter_uses_configured_name() {
        let mut registry = AdapterRegistry::new("mock");
        registry.register(
            "mock",
            Arc::new(StubAdapter {
                adapter_name: "mock",
            }),
        );

        let adapter = registry.default_adapter();
        assert!(adapter.is_some());
        assert_eq!(adapter.unwrap().name(), "mock");
    }

    #[test]
    fn adapter_registry_default_adapter_missing_returns_none() {
        let registry = AdapterRegistry::new("missing");
        assert!(registry.default_adapter().is_none());
    }

    #[test]
    fn adapter_registry_list_returns_registered_names() {
        let mut registry = AdapterRegistry::new("a");
        registry.register("a", Arc::new(StubAdapter { adapter_name: "a" }));
        registry.register("b", Arc::new(StubAdapter { adapter_name: "b" }));

        let mut names = registry.list();
        names.sort_unstable();
        assert_eq!(names, vec!["a", "b"]);
    }
}
