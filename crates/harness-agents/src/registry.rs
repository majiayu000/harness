use harness_core::{CodeAgent, HarnessError, TaskClassification, TaskComplexity};
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
