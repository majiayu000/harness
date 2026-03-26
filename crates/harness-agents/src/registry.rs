use harness_core::{AgentAdapter, CodeAgent, HarnessError, TaskClassification, TaskComplexity};
use std::collections::HashMap;
use std::sync::Arc;

pub struct AgentRegistry {
    agents: HashMap<String, Arc<dyn CodeAgent>>,
    default_agent: String,
}

impl AgentRegistry {
    pub fn new(default_agent: impl Into<String>) -> Self {
        Self {
            agents: HashMap::new(),
            default_agent: default_agent.into(),
        }
    }

    pub fn register(&mut self, name: impl Into<String>, agent: Arc<dyn CodeAgent>) {
        self.agents.insert(name.into(), agent);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn CodeAgent>> {
        self.agents.get(name).cloned()
    }

    pub fn default_agent(&self) -> Option<Arc<dyn CodeAgent>> {
        self.get(&self.default_agent)
    }

    pub fn dispatch(&self, task: &TaskClassification) -> harness_core::Result<Arc<dyn CodeAgent>> {
        let preferred = match task.complexity {
            TaskComplexity::Critical | TaskComplexity::Complex => self.get("codex"),
            _ => None,
        };

        preferred
            .or_else(|| self.default_agent())
            .ok_or_else(|| HarnessError::AgentNotFound("no agents registered".to_string()))
    }

    pub fn list(&self) -> Vec<&str> {
        self.agents.keys().map(|k| k.as_str()).collect()
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
        AgentEvent, AgentRequest, AgentResponse, Capability, CodeAgent, StreamItem,
        TaskClassification, TaskComplexity, TokenUsage, TurnRequest,
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

        async fn execute(&self, _req: AgentRequest) -> harness_core::Result<AgentResponse> {
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
        ) -> harness_core::Result<()> {
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
    fn dispatch_complex_task_prefers_codex() {
        let mut registry = registry_with_default();
        registry.register(
            "codex",
            Arc::new(StubAgent {
                agent_name: "codex",
            }),
        );

        let agent = registry
            .dispatch(&classification(TaskComplexity::Complex))
            .unwrap();
        assert_eq!(agent.name(), "codex");
    }

    #[test]
    fn dispatch_complex_task_falls_back_to_default_when_no_codex() {
        let mut registry = registry_with_default();
        registry.register(
            "anthropic-api",
            Arc::new(StubAgent {
                agent_name: "anthropic-api",
            }),
        );

        let agent = registry
            .dispatch(&classification(TaskComplexity::Complex))
            .unwrap();
        assert_eq!(agent.name(), "default-agent");
    }

    #[test]
    fn dispatch_simple_task_uses_default_agent() {
        let mut registry = registry_with_default();
        registry.register(
            "codex",
            Arc::new(StubAgent {
                agent_name: "codex",
            }),
        );

        let agent = registry
            .dispatch(&classification(TaskComplexity::Simple))
            .unwrap();
        assert_eq!(agent.name(), "default-agent");
    }

    #[test]
    fn dispatch_critical_task_prefers_codex() {
        let mut registry = registry_with_default();
        registry.register(
            "codex",
            Arc::new(StubAgent {
                agent_name: "codex",
            }),
        );

        let agent = registry
            .dispatch(&classification(TaskComplexity::Critical))
            .unwrap();
        assert_eq!(agent.name(), "codex");
    }

    #[test]
    fn dispatch_critical_task_falls_back_to_default_when_no_codex() {
        let mut registry = registry_with_default();
        registry.register(
            "anthropic-api",
            Arc::new(StubAgent {
                agent_name: "anthropic-api",
            }),
        );

        let agent = registry
            .dispatch(&classification(TaskComplexity::Critical))
            .unwrap();
        assert_eq!(agent.name(), "default-agent");
    }

    #[test]
    fn dispatch_medium_task_uses_default_agent() {
        let mut registry = registry_with_default();
        registry.register(
            "codex",
            Arc::new(StubAgent {
                agent_name: "codex",
            }),
        );

        let agent = registry
            .dispatch(&classification(TaskComplexity::Medium))
            .unwrap();
        assert_eq!(agent.name(), "default-agent");
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

    struct MockAdapter;

    #[async_trait::async_trait]
    impl AgentAdapter for MockAdapter {
        fn name(&self) -> &str {
            "mock"
        }

        async fn start_turn(
            &self,
            _req: TurnRequest,
            tx: tokio::sync::mpsc::Sender<AgentEvent>,
        ) -> harness_core::Result<()> {
            if let Err(e) = tx.send(AgentEvent::TurnStarted).await {
                tracing::debug!("stream channel closed: {e}");
            }
            if let Err(e) = tx
                .send(AgentEvent::TurnCompleted {
                    output: "mock done".into(),
                })
                .await
            {
                tracing::debug!("stream channel closed: {e}");
            }
            Ok(())
        }

        async fn interrupt(&self) -> harness_core::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn adapter_registry_register_and_get() {
        let mut registry = AdapterRegistry::new("mock");
        registry.register("mock", Arc::new(MockAdapter));
        assert!(registry.get("mock").is_some());
        assert!(registry.get("missing").is_none());
    }

    #[test]
    fn adapter_registry_default() {
        let mut registry = AdapterRegistry::new("mock");
        registry.register("mock", Arc::new(MockAdapter));
        let adapter = registry.default_adapter().unwrap();
        assert_eq!(adapter.name(), "mock");
    }

    #[test]
    fn adapter_registry_default_returns_none_when_unregistered() {
        let registry = AdapterRegistry::new("missing");
        assert!(registry.default_adapter().is_none());
    }

    #[test]
    fn adapter_registry_list() {
        let mut registry = AdapterRegistry::new("a");
        registry.register("a", Arc::new(MockAdapter));
        registry.register("b", Arc::new(MockAdapter));
        let mut names = registry.list();
        names.sort();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[tokio::test]
    async fn mock_adapter_produces_events() {
        let adapter = MockAdapter;
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let req = TurnRequest {
            prompt: "test".into(),
            project_root: std::path::PathBuf::from("."),
            model: None,
            allowed_tools: vec![],
            context: vec![],
            timeout_secs: None,
        };
        adapter.start_turn(req, tx).await.unwrap();

        let first = rx.recv().await.unwrap();
        assert!(matches!(first, AgentEvent::TurnStarted));

        let second = rx.recv().await.unwrap();
        match second {
            AgentEvent::TurnCompleted { output } => assert_eq!(output, "mock done"),
            other => panic!("expected TurnCompleted, got {other:?}"),
        }
    }
}
