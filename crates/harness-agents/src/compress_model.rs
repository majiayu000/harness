//! Real `CompressModel` backed by the Anthropic messages API (GH1574 T005).
//!
//! Wraps [`AnthropicApiAgent`] so NAP-Lite compression reuses the existing
//! provider plumbing. Construction is config-driven and inert without an
//! active `[context.compression]` block plus an `ANTHROPIC_API_KEY`.

use std::sync::Arc;

use async_trait::async_trait;
use harness_core::agent::{AgentRequest, AgentResponse, CodeAgent};
use harness_core::compress::{
    ActionSketch, CompressHint, CompressModel, CompressModelError, CompressModelOutput,
    CompressorCallUsage, ObservationCompressor, PromptCompressor,
};
use harness_core::config::compression::CompressionConfig;

use crate::anthropic_api::AnthropicApiAgent;

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
/// Summaries and sketches are small; cap output well below agent turns.
const MAX_OUTPUT_TOKENS: u32 = 2_048;

pub struct ApiCompressModel {
    agent: AnthropicApiAgent,
    model: String,
}

impl ApiCompressModel {
    pub fn new(api_key: String, endpoint: String, model: String) -> Self {
        let base_url = if endpoint.trim().is_empty() {
            DEFAULT_BASE_URL.to_string()
        } else {
            endpoint
        };
        Self {
            agent: AnthropicApiAgent::new(api_key, base_url, model.clone(), MAX_OUTPUT_TOKENS),
            model,
        }
    }

    async fn ask(&self, prompt: String) -> Result<CompressModelOutput<String>, CompressModelError> {
        let req = AgentRequest {
            prompt,
            model: Some(self.model.clone()),
            ..AgentRequest::default()
        };
        self.agent
            .execute(req)
            .await
            .map(response_to_model_output)
            .map_err(|error| CompressModelError {
                message: error.to_string(),
                usage: None,
            })
    }
}

fn response_to_model_output(response: AgentResponse) -> CompressModelOutput<String> {
    CompressModelOutput {
        output: response.output,
        usage: CompressorCallUsage {
            input_tokens: response.token_usage.input_tokens,
            output_tokens: response.token_usage.output_tokens,
            model: response.model,
        },
    }
}

fn summarize_prompt(raw: &str, hint: &CompressHint) -> String {
    format!(
        "You compress an agent observation without changing what the agent \
         should do next. Task context: {task}\n\n\
         Rewrite the observation below as a compact rendition that preserves \
         every next-action-relevant fact: error messages, file paths, \
         commands, identifiers, counts, and outcomes. Drop repetition, \
         boilerplate, and progress noise. Output ONLY the compressed text, \
         no preamble.\n\n<observation>\n{raw}\n</observation>",
        task = hint.task_summary,
    )
}

fn sketch_prompt(observation: &str, hint: &CompressHint) -> String {
    format!(
        "Task context: {task}\n\nGiven the observation below, state the \
         single most likely next action as strict JSON with exactly these \
         keys: {{\"intent\": string, \"target_files\": [string], \
         \"command_class\": string}}. Output ONLY the JSON.\n\n\
         <observation>\n{observation}\n</observation>",
        task = hint.task_summary,
    )
}

/// Extract the first JSON object from a model reply that may wrap it in
/// code fences or prose.
fn parse_sketch(reply: &str) -> Result<ActionSketch, String> {
    let start = reply.find('{').ok_or("no JSON object in reply")?;
    let end = reply.rfind('}').ok_or("no closing brace in reply")?;
    if end < start {
        return Err("malformed JSON braces in reply".to_string());
    }
    serde_json::from_str(&reply[start..=end]).map_err(|e| e.to_string())
}

fn parse_sketch_output(
    reply: CompressModelOutput<String>,
) -> Result<CompressModelOutput<ActionSketch>, CompressModelError> {
    match parse_sketch(&reply.output) {
        Ok(output) => Ok(CompressModelOutput {
            output,
            usage: reply.usage,
        }),
        Err(message) => Err(CompressModelError {
            message,
            usage: Some(reply.usage),
        }),
    }
}

#[async_trait]
impl CompressModel for ApiCompressModel {
    async fn summarize(
        &self,
        raw: &str,
        hint: &CompressHint,
    ) -> Result<CompressModelOutput<String>, CompressModelError> {
        self.ask(summarize_prompt(raw, hint)).await
    }

    async fn sketch(
        &self,
        observation: &str,
        hint: &CompressHint,
    ) -> Result<CompressModelOutput<ActionSketch>, CompressModelError> {
        let reply = self.ask(sketch_prompt(observation, hint)).await?;
        parse_sketch_output(reply)
    }
}

/// Build the configured compressor, or `None` when compression is inactive
/// or no API key is present. Callers treat `None` as "pass raw through".
pub fn build_observation_compressor(
    config: &CompressionConfig,
) -> Option<Arc<dyn ObservationCompressor>> {
    if !config.is_active() {
        return None;
    }
    let api_key = std::env::var("ANTHROPIC_API_KEY").ok()?;
    if api_key.trim().is_empty() {
        return None;
    }
    let model = ApiCompressModel::new(api_key, config.endpoint.clone(), config.model.clone());
    Some(Arc::new(PromptCompressor::new(
        model,
        config.sample_rate,
        config.min_size_bytes,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::compress::ObsSource;
    use harness_core::types::TokenUsage;

    fn hint() -> CompressHint {
        CompressHint {
            task_summary: "plan phase for issue #7".into(),
            source: ObsSource::AgentResult,
        }
    }

    fn agent_response(output: &str) -> AgentResponse {
        AgentResponse {
            output: output.into(),
            stderr: String::new(),
            items: Vec::new(),
            token_usage: TokenUsage {
                input_tokens: 41,
                output_tokens: 7,
                total_tokens: 48,
                cost_usd: 12.34,
            },
            model: "provider-returned-model".into(),
            exit_code: Some(0),
        }
    }

    #[test]
    fn parse_sketch_accepts_bare_json() {
        let sketch = parse_sketch(
            r#"{"intent": "fix test", "target_files": ["src/lib.rs"], "command_class": "cargo_test"}"#,
        )
        .expect("parses");
        assert_eq!(sketch.intent, "fix test");
        assert_eq!(sketch.target_files, vec!["src/lib.rs"]);
    }

    #[test]
    fn parse_sketch_accepts_fenced_json_with_prose() {
        let reply = "Here is the action:\n```json\n{\"intent\": \"open pr\", \
                     \"target_files\": [], \"command_class\": \"gh_pr\"}\n```";
        let sketch = parse_sketch(reply).expect("parses");
        assert_eq!(sketch.command_class, "gh_pr");
        assert!(sketch.target_files.is_empty());
    }

    #[test]
    fn parse_sketch_rejects_non_json() {
        assert!(parse_sketch("no json here").is_err());
    }

    #[test]
    fn response_mapping_uses_provider_tokens_and_actual_model() {
        let output = response_to_model_output(agent_response("summary"));
        assert_eq!(output.output, "summary");
        assert_eq!(output.usage.input_tokens, 41);
        assert_eq!(output.usage.output_tokens, 7);
        assert_eq!(output.usage.model, "provider-returned-model");
    }

    #[test]
    fn sketch_parse_error_retains_completed_call_usage() {
        let reply = response_to_model_output(agent_response("not JSON"));
        let err = parse_sketch_output(reply).unwrap_err();
        assert!(err.message.contains("no JSON object"));
        assert_eq!(
            err.usage,
            Some(CompressorCallUsage {
                input_tokens: 41,
                output_tokens: 7,
                model: "provider-returned-model".into(),
            })
        );
    }

    #[test]
    fn prompts_embed_task_context_and_observation() {
        let h = hint();
        let s = summarize_prompt("raw obs", &h);
        assert!(s.contains("plan phase for issue #7"));
        assert!(s.contains("raw obs"));
        let k = sketch_prompt("raw obs", &h);
        assert!(k.contains("command_class"));
        assert!(k.contains("raw obs"));
    }

    #[test]
    fn builder_is_inert_without_active_config() {
        let config = CompressionConfig::default();
        assert!(build_observation_compressor(&config).is_none());
    }
}
