use harness_core::{
    agent::AgentAdapter, agent::CodeAgent, agent::TaskClassification, agent::TaskComplexity,
    error::HarnessError,
};
use std::collections::HashMap;
use std::sync::Arc;

/// Bundles an agent executor with an optional interactive adapter.
///
/// A descriptor is created with `adapter: None` on plain `register()` calls.
/// Call `register_adapter()` to attach an adapter after the agent is registered.
pub struct AgentDescriptor {
    pub agent: Arc<dyn CodeAgent>,
    pub adapter: Option<Arc<dyn AgentAdapter>>,
}

pub struct AgentRegistry {
    entries: HashMap<String, AgentDescriptor>,
    registration_order: Vec<String>,
    default_agent: String,
    complexity_preferred_agents: Vec<String>,
}

impl AgentRegistry {
    pub fn new(default_agent: impl Into<String>) -> Self {
        Self {
            entries: HashMap::new(),
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
        // Preserve any previously attached adapter so that hot-reload cannot
        // accidentally clear the adapter by re-registering the agent under the
        // same name (the old split-registry design had this property naturally).
        let existing_adapter = self.entries.get(&name).and_then(|d| d.adapter.clone());
        if !self.entries.contains_key(&name) {
            self.registration_order.push(name.clone());
        }
        self.entries.insert(
            name,
            AgentDescriptor {
                agent,
                adapter: existing_adapter,
            },
        );
    }

    /// Attach an adapter to an already-registered agent.
    ///
    /// Returns `Err(HarnessError::AgentNotFound)` if the agent name is unknown,
    /// so misconfiguration fails loudly instead of silently (U-23).
    pub fn register_adapter(
        &mut self,
        name: &str,
        adapter: Arc<dyn AgentAdapter>,
    ) -> harness_core::error::Result<()> {
        match self.entries.get_mut(name) {
            Some(descriptor) => {
                descriptor.adapter = Some(adapter);
                Ok(())
            }
            None => Err(HarnessError::AgentNotFound(format!(
                "cannot register adapter: agent '{}' not found",
                name
            ))),
        }
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn CodeAgent>> {
        self.entries.get(name).map(|d| d.agent.clone())
    }

    /// Return the adapter registered for the given agent name, if any.
    pub fn get_adapter(&self, name: &str) -> Option<Arc<dyn AgentAdapter>> {
        self.entries.get(name).and_then(|d| d.adapter.clone())
    }

    pub fn resolved_default_agent_name(&self) -> Option<&str> {
        let configured = self.default_agent.trim();
        if !configured.is_empty()
            && !configured.eq_ignore_ascii_case("auto")
            && self.entries.contains_key(configured)
        {
            return Some(configured);
        }
        self.registration_order
            .iter()
            .find(|name| self.entries.contains_key(name.as_str()))
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
    fn register_adapter_and_get_roundtrip() {
        let mut registry = AgentRegistry::new("mock");
        registry.register("mock", Arc::new(StubAgent { agent_name: "mock" }));
        registry
            .register_adapter(
                "mock",
                Arc::new(StubAdapter {
                    adapter_name: "mock",
                }),
            )
            .unwrap();

        let adapter = registry.get_adapter("mock");
        assert!(adapter.is_some());
        assert_eq!(adapter.unwrap().name(), "mock");
    }

    #[test]
    fn register_adapter_on_unknown_agent_returns_error() {
        let mut registry = AgentRegistry::new("");
        let result = registry.register_adapter(
            "ghost",
            Arc::new(StubAdapter {
                adapter_name: "ghost",
            }),
        );
        assert!(matches!(result, Err(HarnessError::AgentNotFound(_))));
    }

    #[test]
    fn get_adapter_returns_none_when_not_registered() {
        let mut registry = AgentRegistry::new("mock");
        registry.register("mock", Arc::new(StubAgent { agent_name: "mock" }));

        assert!(registry.get_adapter("mock").is_none());
    }

    #[test]
    fn register_creates_descriptor_with_no_adapter() {
        let mut registry = AgentRegistry::new("mock");
        registry.register("mock", Arc::new(StubAgent { agent_name: "mock" }));

        assert!(registry.get_adapter("mock").is_none());
        assert!(registry.get("mock").is_some());
    }

    #[test]
    fn dispatch_regression_returns_descriptor_agent() {
        let mut registry = AgentRegistry::new("mock");
        registry.register("mock", Arc::new(StubAgent { agent_name: "mock" }));
        registry
            .register_adapter(
                "mock",
                Arc::new(StubAdapter {
                    adapter_name: "mock",
                }),
            )
            .unwrap();

        // dispatch() must still return the CodeAgent, not the adapter
        let agent = registry
            .dispatch(&classification(TaskComplexity::Simple))
            .unwrap();
        assert_eq!(agent.name(), "mock");
    }

    #[test]
    fn re_register_preserves_existing_adapter() {
        let mut registry = AgentRegistry::new("mock");
        registry.register("mock", Arc::new(StubAgent { agent_name: "mock" }));
        registry
            .register_adapter(
                "mock",
                Arc::new(StubAdapter {
                    adapter_name: "mock",
                }),
            )
            .unwrap();

        // Re-registering with a new agent must not clear the adapter.
        registry.register(
            "mock",
            Arc::new(StubAgent {
                agent_name: "mock-v2",
            }),
        );

        let adapter = registry.get_adapter("mock");
        assert!(
            adapter.is_some(),
            "adapter must survive re-registration of the same agent name"
        );
        assert_eq!(adapter.unwrap().name(), "mock");
        // The agent itself must be updated.
        assert_eq!(registry.get("mock").unwrap().name(), "mock-v2");
    }

    #[test]
    fn list_returns_registered_agent_names_in_order() {
        let mut registry = AgentRegistry::new("a");
        registry.register("a", Arc::new(StubAgent { agent_name: "a" }));
        registry.register("b", Arc::new(StubAgent { agent_name: "b" }));

        let names = registry.list();
        assert_eq!(names, vec!["a", "b"]);
    }
}
