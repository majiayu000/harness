//! Real `CompressModel` backed by the Anthropic messages API (GH1574 T005).
//!
//! Wraps [`AnthropicApiAgent`] so NAP-Lite compression reuses the existing
//! provider plumbing. Construction is config-driven and inert without an
//! active `[context.compression]` block plus an `ANTHROPIC_API_KEY`.

use std::sync::Arc;

use async_trait::async_trait;
use harness_core::agent::{AgentRequest, CodeAgent};
use harness_core::compress::{
    ActionSketch, CompressHint, CompressModel, ObservationCompressor, PromptCompressor,
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

    async fn ask(&self, prompt: String) -> Result<String, String> {
        let req = AgentRequest {
            prompt,
            model: Some(self.model.clone()),
            ..AgentRequest::default()
        };
        self.agent
            .execute(req)
            .await
            .map(|resp| resp.output)
            .map_err(|e| e.to_string())
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

#[async_trait]
impl CompressModel for ApiCompressModel {
    async fn summarize(&self, raw: &str, hint: &CompressHint) -> Result<String, String> {
        self.ask(summarize_prompt(raw, hint)).await
    }

    async fn sketch(&self, observation: &str, hint: &CompressHint) -> Result<ActionSketch, String> {
        let reply = self.ask(sketch_prompt(observation, hint)).await?;
        parse_sketch(&reply)
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

    fn hint() -> CompressHint {
        CompressHint {
            task_summary: "plan phase for issue #7".into(),
            source: ObsSource::AgentResult,
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
