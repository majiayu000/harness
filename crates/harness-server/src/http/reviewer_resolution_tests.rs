use super::resolve_reviewer;
use harness_agents::registry::AgentRegistry;
use harness_core::{
    agent::{AgentRequest, AgentResponse, CodeAgent, StreamItem},
    config::agents::AgentReviewConfig,
    types::{Capability, TokenUsage},
};
use std::sync::Arc;

struct StaticAgent {
    name: &'static str,
}

#[async_trait::async_trait]
impl CodeAgent for StaticAgent {
    fn name(&self) -> &str {
        self.name
    }

    fn capabilities(&self) -> Vec<Capability> {
        Vec::new()
    }

    async fn execute(&self, _req: AgentRequest) -> harness_core::error::Result<AgentResponse> {
        Ok(AgentResponse {
            output: "APPROVED".to_string(),
            stderr: String::new(),
            items: Vec::new(),
            token_usage: TokenUsage::default(),
            model: self.name.to_string(),
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

#[test]
fn explicit_reviewer_can_match_implementor() {
    let mut registry = AgentRegistry::new("codex");
    registry.register("codex", Arc::new(StaticAgent { name: "codex" }));
    let config = AgentReviewConfig {
        enabled: true,
        reviewer_agent: "codex".to_string(),
        ..AgentReviewConfig::default()
    };

    let (reviewer, _) = resolve_reviewer(&registry, &config, "codex");

    assert_eq!(reviewer.as_ref().map(|agent| agent.name()), Some("codex"));
}

#[test]
fn auto_select_still_avoids_implementor() {
    let mut registry = AgentRegistry::new("codex");
    registry.register("codex", Arc::new(StaticAgent { name: "codex" }));
    let config = AgentReviewConfig {
        enabled: true,
        reviewer_agent: String::new(),
        ..AgentReviewConfig::default()
    };

    let (reviewer, _) = resolve_reviewer(&registry, &config, "codex");

    assert!(reviewer.is_none());
}
