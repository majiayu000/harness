use harness_core::{CodeAgent, HarnessError, TaskClassification, TaskComplexity};
use std::collections::HashMap;

pub struct AgentRegistry {
    agents: HashMap<String, Box<dyn CodeAgent>>,
    default_agent: String,
}

impl AgentRegistry {
    pub fn new(default_agent: impl Into<String>) -> Self {
        Self {
            agents: HashMap::new(),
            default_agent: default_agent.into(),
        }
    }

    pub fn register(&mut self, name: impl Into<String>, agent: Box<dyn CodeAgent>) {
        self.agents.insert(name.into(), agent);
    }

    pub fn get(&self, name: &str) -> Option<&dyn CodeAgent> {
        self.agents.get(name).map(|a| a.as_ref())
    }

    pub fn default_agent(&self) -> Option<&dyn CodeAgent> {
        self.get(&self.default_agent)
    }

    pub fn dispatch(&self, task: &TaskClassification) -> harness_core::Result<&dyn CodeAgent> {
        // Strategy: complex/critical tasks → prefer strongest agent
        // Simple tasks → default agent is fine
        let preferred = match task.complexity {
            TaskComplexity::Critical | TaskComplexity::Complex => {
                // Try claude first for complex tasks
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
