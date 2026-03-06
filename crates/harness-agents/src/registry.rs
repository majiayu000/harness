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
            TaskComplexity::Critical | TaskComplexity::Complex => {
                self.get("claude").or_else(|| self.get("anthropic-api"))
            }
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
    use harness_core::{AgentEvent, TurnRequest};

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
            let _ = tx.send(AgentEvent::TurnStarted).await;
            let _ = tx
                .send(AgentEvent::TurnCompleted {
                    output: "mock done".into(),
                })
                .await;
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
