use harness_core::{
    agent::AgentAdapter, agent::CodeAgent, agent::TaskClassification, agent::TaskComplexity,
    error::HarnessError, types::Capability,
};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

type AdapterFactory = Arc<dyn Fn() -> Arc<dyn AgentAdapter> + Send + Sync>;

#[derive(Clone)]
pub struct RegisteredAgentDescriptor {
    pub name: String,
    pub code_agent: Arc<dyn CodeAgent>,
    adapter_factory: Option<AdapterFactory>,
    pub registration_index: usize,
    pub capabilities: Vec<Capability>,
    pub is_default: bool,
    pub complexity_preferred: bool,
}

impl RegisteredAgentDescriptor {
    pub fn code_agent(&self) -> Arc<dyn CodeAgent> {
        self.code_agent.clone()
    }

    pub fn adapter(&self) -> Option<Arc<dyn AgentAdapter>> {
        self.adapter_factory.as_ref().map(|factory| factory())
    }
}

fn singleton_adapter_factory(adapter: Arc<dyn AgentAdapter>) -> AdapterFactory {
    Arc::new(move || adapter.clone())
}

pub struct AgentRegistry {
    descriptors: HashMap<String, RegisteredAgentDescriptor>,
    registration_order: Vec<String>,
    default_agent: String,
    complexity_preferred_agents: Vec<String>,
}

impl AgentRegistry {
    pub fn new(default_agent: impl Into<String>) -> Self {
        Self {
            descriptors: HashMap::new(),
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
        self.refresh_metadata();
    }

    pub fn register(&mut self, name: impl Into<String>, agent: Arc<dyn CodeAgent>) {
        self.register_with_adapter(name, agent, None);
    }

    pub fn register_with_adapter(
        &mut self,
        name: impl Into<String>,
        agent: Arc<dyn CodeAgent>,
        adapter: Option<Arc<dyn AgentAdapter>>,
    ) {
        let name = name.into();
        let registration_index = self.ensure_registration_slot(&name);
        self.descriptors.insert(
            name.clone(),
            RegisteredAgentDescriptor {
                name,
                code_agent: agent.clone(),
                adapter_factory: adapter.map(singleton_adapter_factory),
                registration_index,
                capabilities: agent.capabilities(),
                is_default: false,
                complexity_preferred: false,
            },
        );
        self.refresh_metadata();
    }

    pub fn register_adapter(&mut self, name: impl Into<String>, adapter: Arc<dyn AgentAdapter>) {
        let name = name.into();
        if let Some(existing) = self.descriptors.get_mut(&name) {
            existing.adapter_factory = Some(singleton_adapter_factory(adapter));
            self.refresh_metadata();
        }
    }

    pub fn register_with_adapter_factory(
        &mut self,
        name: impl Into<String>,
        agent: Arc<dyn CodeAgent>,
        adapter_factory: Option<AdapterFactory>,
    ) {
        let name = name.into();
        let registration_index = self.ensure_registration_slot(&name);
        self.descriptors.insert(
            name.clone(),
            RegisteredAgentDescriptor {
                name,
                code_agent: agent.clone(),
                adapter_factory,
                registration_index,
                capabilities: agent.capabilities(),
                is_default: false,
                complexity_preferred: false,
            },
        );
        self.refresh_metadata();
    }

    pub fn register_adapter_factory(
        &mut self,
        name: impl Into<String>,
        adapter_factory: AdapterFactory,
    ) {
        let name = name.into();
        if let Some(existing) = self.descriptors.get_mut(&name) {
            existing.adapter_factory = Some(adapter_factory);
            self.refresh_metadata();
        }
    }

    pub fn register_with_fresh_adapter<A, F>(
        &mut self,
        name: impl Into<String>,
        agent: Arc<dyn CodeAgent>,
        adapter_factory: F,
    ) where
        A: AgentAdapter + 'static,
        F: Fn() -> A + Send + Sync + 'static,
    {
        self.register_with_adapter_factory(
            name,
            agent,
            Some(Arc::new(move || {
                Arc::new(adapter_factory()) as Arc<dyn AgentAdapter>
            })),
        );
    }

    pub fn register_fresh_adapter<A, F>(&mut self, name: impl Into<String>, adapter_factory: F)
    where
        A: AgentAdapter + 'static,
        F: Fn() -> A + Send + Sync + 'static,
    {
        self.register_adapter_factory(
            name,
            Arc::new(move || Arc::new(adapter_factory()) as Arc<dyn AgentAdapter>),
        );
    }

    pub fn descriptor(&self, name: &str) -> Option<&RegisteredAgentDescriptor> {
        self.descriptors.get(name)
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn CodeAgent>> {
        self.get_code_agent(name)
    }

    pub fn get_code_agent(&self, name: &str) -> Option<Arc<dyn CodeAgent>> {
        self.descriptor(name)
            .map(RegisteredAgentDescriptor::code_agent)
    }

    pub fn get_adapter(&self, name: &str) -> Option<Arc<dyn AgentAdapter>> {
        self.descriptor(name)
            .and_then(RegisteredAgentDescriptor::adapter)
    }

    pub fn resolved_default_agent_name(&self) -> Option<&str> {
        let configured = self.default_agent.trim();
        if !configured.is_empty()
            && !configured.eq_ignore_ascii_case("auto")
            && self.descriptors.contains_key(configured)
            && self.descriptor_has_code_agent(configured)
        {
            return Some(configured);
        }
        self.registration_order
            .iter()
            .find(|name| self.descriptor_has_code_agent(name))
            .map(String::as_str)
    }

    pub fn default_agent(&self) -> Option<Arc<dyn CodeAgent>> {
        self.resolved_default_agent_name()
            .and_then(|name| self.get_code_agent(name))
    }

    pub fn default_descriptor(&self) -> Option<&RegisteredAgentDescriptor> {
        self.resolved_default_agent_name()
            .and_then(|name| self.descriptor(name))
    }

    pub fn dispatch(
        &self,
        task: &TaskClassification,
    ) -> harness_core::error::Result<Arc<dyn CodeAgent>> {
        self.dispatch_descriptor(task)
            .map(|descriptor| descriptor.code_agent())
    }

    pub fn dispatch_descriptor(
        &self,
        task: &TaskClassification,
    ) -> harness_core::error::Result<&RegisteredAgentDescriptor> {
        let preferred = match task.complexity {
            TaskComplexity::Critical | TaskComplexity::Complex => {
                self.complexity_preferred_agents.iter().find_map(|name| {
                    self.descriptor(name)
                        .filter(|_| self.descriptor_has_code_agent(name))
                })
            }
            _ => None,
        };

        preferred
            .or_else(|| self.default_descriptor())
            .ok_or_else(|| HarnessError::AgentNotFound("no agents registered".to_string()))
    }

    pub fn list(&self) -> Vec<&str> {
        self.list_names()
    }

    pub fn list_names(&self) -> Vec<&str> {
        self.registration_order
            .iter()
            .map(|name| name.as_str())
            .collect()
    }

    pub fn list_descriptors(&self) -> Vec<&RegisteredAgentDescriptor> {
        self.registration_order
            .iter()
            .filter_map(|name| self.descriptor(name))
            .collect()
    }

    fn ensure_registration_slot(&mut self, name: &str) -> usize {
        if let Some(index) = self
            .registration_order
            .iter()
            .position(|existing| existing == name)
        {
            index
        } else {
            let index = self.registration_order.len();
            self.registration_order.push(name.to_string());
            index
        }
    }

    fn descriptor_has_code_agent(&self, name: &str) -> bool {
        self.descriptors.contains_key(name)
    }

    fn refresh_metadata(&mut self) {
        let default_name = self.resolved_default_agent_name().map(str::to_string);
        let preferred: HashSet<&str> = self
            .complexity_preferred_agents
            .iter()
            .map(String::as_str)
            .collect();

        for name in &self.registration_order {
            if let Some(descriptor) = self.descriptors.get_mut(name) {
                descriptor.is_default = default_name.as_deref() == Some(name.as_str());
                descriptor.complexity_preferred = preferred.contains(name.as_str());
                descriptor.capabilities = descriptor.code_agent.capabilities();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::{
        agent::AgentEvent, agent::AgentRequest, agent::AgentResponse, agent::CodeAgent,
        agent::StreamItem, agent::TaskClassification, agent::TaskComplexity, agent::TurnRequest,
        types::TokenUsage,
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
    fn execution_only_descriptor_has_no_adapter() {
        let mut registry = AgentRegistry::new("mock");
        registry.register("mock", Arc::new(StubAgent { agent_name: "mock" }));

        let descriptor = registry.descriptor("mock").unwrap();
        assert_eq!(descriptor.name, "mock");
        assert_eq!(descriptor.code_agent().name(), "mock");
        assert!(descriptor.adapter().is_none());
    }

    #[test]
    fn register_with_adapter_roundtrip() {
        let mut registry = AgentRegistry::new("mock");
        registry.register_with_adapter(
            "mock",
            Arc::new(StubAgent { agent_name: "mock" }),
            Some(Arc::new(StubAdapter {
                adapter_name: "mock",
            })),
        );

        let descriptor = registry.descriptor("mock").unwrap();
        assert_eq!(descriptor.name, "mock");
        assert_eq!(descriptor.code_agent().name(), "mock");
        assert_eq!(descriptor.adapter().unwrap().name(), "mock");
    }

    #[test]
    fn register_adapter_without_agent_is_ignored() {
        let mut registry = AgentRegistry::new("mock");
        registry.register_adapter(
            "missing",
            Arc::new(StubAdapter {
                adapter_name: "missing",
            }),
        );

        assert!(registry.descriptor("missing").is_none());
    }

    #[test]
    fn list_names_contains_each_registered_agent_once() {
        let mut registry = AgentRegistry::new("a");
        registry.register("a", Arc::new(StubAgent { agent_name: "a" }));
        registry.register("b", Arc::new(StubAgent { agent_name: "b" }));
        registry.register("a", Arc::new(StubAgent { agent_name: "a" }));

        assert_eq!(registry.list_names(), vec!["a", "b"]);
    }

    #[test]
    fn dispatch_descriptor_returns_same_descriptor_used_for_adapter_lookup() {
        let mut registry = AgentRegistry::new("mock");
        registry.register_with_adapter(
            "mock",
            Arc::new(StubAgent { agent_name: "mock" }),
            Some(Arc::new(StubAdapter {
                adapter_name: "mock",
            })),
        );

        let descriptor = registry
            .dispatch_descriptor(&classification(TaskComplexity::Simple))
            .unwrap();
        assert_eq!(descriptor.code_agent().name(), "mock");
        assert_eq!(descriptor.adapter().unwrap().name(), "mock");
    }

    #[test]
    fn list_descriptors_reflects_runnable_agents_exactly_once() {
        let mut registry = AgentRegistry::new("a");
        registry.register("a", Arc::new(StubAgent { agent_name: "a" }));
        registry.register_with_adapter(
            "b",
            Arc::new(StubAgent { agent_name: "b" }),
            Some(Arc::new(StubAdapter { adapter_name: "b" })),
        );
        registry.register("a", Arc::new(StubAgent { agent_name: "a" }));

        let names: Vec<&str> = registry
            .list_descriptors()
            .into_iter()
            .map(|descriptor| descriptor.name.as_str())
            .collect();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[test]
    fn descriptor_metadata_tracks_registration_order() {
        let mut registry = AgentRegistry::new("a");
        registry.register("a", Arc::new(StubAgent { agent_name: "a" }));
        registry.register("b", Arc::new(StubAgent { agent_name: "b" }));

        assert_eq!(registry.descriptor("a").unwrap().registration_index, 0);
        assert_eq!(registry.descriptor("b").unwrap().registration_index, 1);
    }

    #[test]
    fn register_adapter_adds_adapter_to_existing_descriptor() {
        let mut registry = AgentRegistry::new("mock");
        registry.register("mock", Arc::new(StubAgent { agent_name: "mock" }));
        registry.register_adapter(
            "mock",
            Arc::new(StubAdapter {
                adapter_name: "mock",
            }),
        );

        assert_eq!(registry.get_adapter("mock").unwrap().name(), "mock");
    }

    #[test]
    fn register_with_adapter_replaces_existing_descriptor_without_duplication() {
        let mut registry = AgentRegistry::new("mock");
        registry.register("mock", Arc::new(StubAgent { agent_name: "mock" }));
        registry.register_with_adapter(
            "mock",
            Arc::new(StubAgent { agent_name: "mock" }),
            Some(Arc::new(StubAdapter {
                adapter_name: "mock",
            })),
        );

        assert_eq!(registry.list_names(), vec!["mock"]);
        assert_eq!(registry.get_adapter("mock").unwrap().name(), "mock");
    }

    #[test]
    fn list_descriptors_preserves_registration_order() {
        let mut registry = AgentRegistry::new("a");
        registry.register("a", Arc::new(StubAgent { agent_name: "a" }));
        registry.register("b", Arc::new(StubAgent { agent_name: "b" }));

        let names: Vec<&str> = registry
            .list_descriptors()
            .into_iter()
            .map(|descriptor| descriptor.name.as_str())
            .collect();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[test]
    fn metadata_marks_default_and_complexity_preference() {
        let mut registry = AgentRegistry::new("alpha");
        registry.register(
            "alpha",
            Arc::new(StubAgent {
                agent_name: "alpha",
            }),
        );
        registry.register("beta", Arc::new(StubAgent { agent_name: "beta" }));
        registry.set_complexity_preferences(vec!["beta".to_string()]);

        let alpha = registry.descriptor("alpha").unwrap();
        let beta = registry.descriptor("beta").unwrap();
        assert!(alpha.is_default);
        assert!(!alpha.complexity_preferred);
        assert!(!beta.is_default);
        assert!(beta.complexity_preferred);
    }
}
