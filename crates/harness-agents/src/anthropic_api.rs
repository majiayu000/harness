use async_trait::async_trait;
use harness_core::{
    AgentRequest, AgentResponse, Capability, CodeAgent, Item, StreamItem, TokenUsage,
};
use serde::{Deserialize, Serialize};

pub struct AnthropicApiAgent {
    pub api_key: String,
    pub base_url: String,
    pub default_model: String,
    pub max_tokens: u32,
    client: reqwest::Client,
}

impl AnthropicApiAgent {
    pub fn new(api_key: String, base_url: String, default_model: String) -> Self {
        let default_max_tokens = harness_core::AnthropicApiConfig::default().max_tokens;
        Self::new_with_max_tokens(api_key, base_url, default_model, default_max_tokens)
    }

    pub fn new_with_max_tokens(
        api_key: String,
        base_url: String,
        default_model: String,
        max_tokens: u32,
    ) -> Self {
        Self {
            api_key,
            base_url,
            default_model,
            max_tokens,
            client: reqwest::Client::new(),
        }
    }

    fn build_messages_request(&self, req: &AgentRequest) -> MessagesRequest {
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

    async fn execute(&self, req: AgentRequest) -> harness_core::Result<AgentResponse> {
        let body = self.build_messages_request(&req);

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
                harness_core::HarnessError::AgentExecution(format!("API request failed: {e}"))
            })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(harness_core::HarnessError::AgentExecution(format!(
                "API returned {status}: {text}"
            )));
        }

        let data: MessagesResponse = resp.json().await.map_err(|e| {
            harness_core::HarnessError::AgentExecution(format!("failed to parse response: {e}"))
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
    ) -> harness_core::Result<()> {
        let resp = self.execute(req).await?;
        let _ = tx
            .send(StreamItem::ItemCompleted {
                item: Item::AgentReasoning {
                    content: resp.output.clone(),
                },
            })
            .await;
        let _ = tx
            .send(StreamItem::TokenUsage {
                usage: resp.token_usage,
            })
            .await;
        let _ = tx.send(StreamItem::Done).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_messages_request_uses_configured_max_tokens() {
        let agent = AnthropicApiAgent::new_with_max_tokens(
            "key".into(),
            "https://api.anthropic.com".into(),
            "claude-sonnet".into(),
            2048,
        );
        let req = AgentRequest {
            prompt: "hello".into(),
            ..Default::default()
        };

        let body = agent.build_messages_request(&req);
        assert_eq!(body.max_tokens, 2048);
    }

    #[test]
    fn build_messages_request_uses_request_model_override() {
        let agent = AnthropicApiAgent::new_with_max_tokens(
            "key".into(),
            "https://api.anthropic.com".into(),
            "default-model".into(),
            4096,
        );
        let req = AgentRequest {
            prompt: "hello".into(),
            model: Some("override-model".into()),
            ..Default::default()
        };

        let body = agent.build_messages_request(&req);
        assert_eq!(body.model, "override-model");
    }
}
