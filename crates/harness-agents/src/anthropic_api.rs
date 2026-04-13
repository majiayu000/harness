use std::fmt;

use async_trait::async_trait;
use harness_core::agent::{AgentRequest, AgentResponse, CodeAgent, StreamItem};
use harness_core::config::agents::AnthropicApiConfig;
use harness_core::types::{Capability, Item, TokenUsage};
use serde::{Deserialize, Serialize};

pub struct AnthropicApiAgent {
    pub api_key: String,
    pub base_url: String,
    pub default_model: String,
    pub max_tokens: u32,
    client: reqwest::Client,
}

impl fmt::Debug for AnthropicApiAgent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AnthropicApiAgent")
            .field("api_key", &"[REDACTED]")
            .field("base_url", &self.base_url)
            .field("default_model", &self.default_model)
            .field("max_tokens", &self.max_tokens)
            .finish()
    }
}

impl AnthropicApiAgent {
    pub fn new(api_key: String, base_url: String, default_model: String, max_tokens: u32) -> Self {
        Self {
            api_key,
            base_url,
            default_model,
            max_tokens,
            client: reqwest::Client::new(),
        }
    }

    pub fn from_config(api_key: String, config: &AnthropicApiConfig) -> Self {
        Self::new(
            api_key,
            config.base_url.clone(),
            config.default_model.clone(),
            config.max_tokens,
        )
    }

    fn build_request(&self, req: &AgentRequest) -> MessagesRequest {
        let model = req.model.as_deref().unwrap_or(&self.default_model);
        MessagesRequest {
            model: model.to_string(),
            max_tokens: self.max_tokens,
            messages: vec![Message {
                role: "user".to_string(),
                content: req.prompt.clone(),
            }],
        }
    }
}

#[derive(Serialize)]
struct MessagesRequest {
    model: String,
    max_tokens: u32,
    messages: Vec<Message>,
}

#[derive(Serialize, Deserialize)]
struct Message {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct MessagesResponse {
    content: Vec<ContentBlock>,
    model: String,
    usage: Usage,
}

#[derive(Deserialize)]
struct ContentBlock {
    text: Option<String>,
}

#[derive(Deserialize)]
struct Usage {
    input_tokens: u64,
    output_tokens: u64,
}

#[async_trait]
impl CodeAgent for AnthropicApiAgent {
    fn name(&self) -> &str {
        "anthropic-api"
    }

    fn capabilities(&self) -> Vec<Capability> {
        vec![Capability::Read]
    }

    async fn execute(&self, req: AgentRequest) -> harness_core::error::Result<AgentResponse> {
        let body = self.build_request(&req);

        let resp = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                harness_core::error::HarnessError::AgentExecution(format!(
                    "API request failed: {e}"
                ))
            })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(harness_core::error::HarnessError::AgentExecution(format!(
                "API returned {status}: {text}"
            )));
        }

        let data: MessagesResponse = resp.json().await.map_err(|e| {
            harness_core::error::HarnessError::AgentExecution(format!(
                "failed to parse response: {e}"
            ))
        })?;

        let output = data
            .content
            .iter()
            .filter_map(|b| b.text.as_deref())
            .collect::<Vec<_>>()
            .join("\n");

        Ok(AgentResponse {
            output,
            stderr: String::new(),
            items: Vec::new(),
            token_usage: TokenUsage {
                input_tokens: data.usage.input_tokens,
                output_tokens: data.usage.output_tokens,
                total_tokens: data.usage.input_tokens + data.usage.output_tokens,
                cost_usd: 0.0, // caller computes
            },
            model: data.model,
            exit_code: Some(0),
        })
    }

    async fn execute_stream(
        &self,
        req: AgentRequest,
        tx: tokio::sync::mpsc::Sender<StreamItem>,
    ) -> harness_core::error::Result<()> {
        let resp = self.execute(req).await?;
        crate::streaming::send_stream_item(
            &tx,
            StreamItem::ItemCompleted {
                item: Item::AgentReasoning {
                    content: resp.output.clone(),
                },
            },
            self.name(),
            "item_completed",
        )
        .await?;
        crate::streaming::send_stream_item(
            &tx,
            StreamItem::TokenUsage {
                usage: resp.token_usage,
            },
            self.name(),
            "token_usage",
        )
        .await?;
        crate::streaming::send_stream_item(&tx, StreamItem::Done, self.name(), "done").await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_request_uses_configured_max_tokens_and_default_model() {
        let agent = AnthropicApiAgent::new(
            "test-key".to_string(),
            "https://example.com".to_string(),
            "claude-default".to_string(),
            2048,
        );

        let request = AgentRequest {
            prompt: "hello".to_string(),
            ..AgentRequest::default()
        };
        let body = agent.build_request(&request);

        assert_eq!(body.model, "claude-default");
        assert_eq!(body.max_tokens, 2048);
        assert_eq!(body.messages.len(), 1);
        assert_eq!(body.messages[0].role, "user");
        assert_eq!(body.messages[0].content, "hello");
    }

    #[test]
    fn build_request_prefers_model_override() {
        let agent = AnthropicApiAgent::new(
            "test-key".to_string(),
            "https://example.com".to_string(),
            "claude-default".to_string(),
            1024,
        );

        let request = AgentRequest {
            prompt: "hello".to_string(),
            model: Some("claude-custom".to_string()),
            ..AgentRequest::default()
        };
        let body = agent.build_request(&request);

        assert_eq!(body.model, "claude-custom");
        assert_eq!(body.max_tokens, 1024);
    }

    #[test]
    fn debug_redacts_api_key() {
        let agent = AnthropicApiAgent::new(
            "sk-secret-key".to_string(),
            "https://api.anthropic.com".to_string(),
            "claude-default".to_string(),
            2048,
        );
        let debug_output = format!("{agent:?}");
        assert!(
            !debug_output.contains("sk-secret-key"),
            "api_key must not appear in Debug output"
        );
        assert!(
            debug_output.contains("[REDACTED]"),
            "Debug output must contain [REDACTED]"
        );
    }

    #[test]
    fn from_config_wires_all_anthropic_settings() {
        let config = AnthropicApiConfig {
            base_url: "https://api.anthropic.com".to_string(),
            default_model: "claude-sonnet-4-6".to_string(),
            max_tokens: 3072,
        };

        let agent = AnthropicApiAgent::from_config("test-key".to_string(), &config);

        assert_eq!(agent.api_key, "test-key");
        assert_eq!(agent.base_url, config.base_url);
        assert_eq!(agent.default_model, config.default_model);
        assert_eq!(agent.max_tokens, 3072);
    }
}
