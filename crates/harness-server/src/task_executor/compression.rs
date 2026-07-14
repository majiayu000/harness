//! NAP-Lite compression at the agent-result → next-prompt seam (GH1574).
//!
//! The raw output always stays in the task store (checkpoints /
//! `state.plan_output`); compression only affects what is injected into the
//! next agent's prompt, so every failure path falls back to raw text.

use harness_agents::compress_model::build_observation_compressor;
use harness_core::compress::{
    CompressError, CompressHint, NapStatus, ObsSource, ObservationCompressor,
};
use harness_core::config::compression::CompressionConfig;
use std::sync::Arc;

pub(crate) type TaskObservationCompressor = Arc<dyn ObservationCompressor>;

/// Build one compressor for the full task lifecycle so sampling and breaker
/// state accumulate across prompt-injection seams and transient retries.
///
/// GH-1574 still needs separate wiring for triage-to-plan, cross-review,
/// workflow-runtime, and composer seams; this legacy path currently injects
/// compression only at plan-to-implement (including replan retries).
pub(crate) fn build_task_observation_compressor(
    config: &CompressionConfig,
) -> Option<TaskObservationCompressor> {
    build_observation_compressor(config)
}

/// Compress a prior agent's output before injecting it into the next
/// phase's prompt. Returns the raw text unchanged when compression is
/// inactive, skipped, or fails.
///
pub(crate) async fn compress_observation_for_prompt(
    compressor: Option<&dyn ObservationCompressor>,
    raw: &str,
    task_summary: &str,
) -> String {
    let Some(compressor) = compressor else {
        return raw.to_string();
    };
    let hint = CompressHint {
        task_summary: task_summary.to_string(),
        source: ObsSource::AgentResult,
    };
    match compressor.compress(raw, &hint).await {
        Ok(compressed) => {
            match compressed.nap {
                NapStatus::Failed { .. } => {
                    // Compressed.text already carries the raw fallback.
                    tracing::warn!(
                        task_summary,
                        original_tokens = compressed.original_tokens,
                        compressor_provider_input_tokens = compressed.compressor_usage.input_tokens(),
                        compressor_provider_output_tokens = compressed.compressor_usage.output_tokens(),
                        compressor_models = ?compressed.compressor_usage.models(),
                        "NAP verification failed; using raw observation"
                    );
                }
                nap => {
                    tracing::info!(
                        task_summary,
                        original_tokens = compressed.original_tokens,
                        compressed_tokens = compressed.compressed_tokens,
                        compressor_provider_input_tokens = compressed.compressor_usage.input_tokens(),
                        compressor_provider_output_tokens = compressed.compressor_usage.output_tokens(),
                        compressor_models = ?compressed.compressor_usage.models(),
                        nap = ?nap,
                        "observation compressed for prompt injection"
                    );
                }
            }
            compressed.text
        }
        Err(CompressError::TooSmall { .. }) => raw.to_string(),
        Err(err @ CompressError::BreakerOpen { .. }) => {
            tracing::error!(%err, task_summary, "compression bypassed by circuit breaker");
            raw.to_string()
        }
        Err(CompressError::Model { message, usage }) => {
            tracing::warn!(
                error = %message,
                task_summary,
                compressor_provider_input_tokens = usage.input_tokens(),
                compressor_provider_output_tokens = usage.output_tokens(),
                compressor_models = ?usage.models(),
                "compressor model failed; using raw observation"
            );
            raw.to_string()
        }
        Err(err) => {
            tracing::warn!(%err, task_summary, "compression failed; using raw observation");
            raw.to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use harness_core::compress::{
        ActionSketch, CompressModel, CompressModelError, CompressModelOutput, CompressorCallUsage,
        PromptCompressor,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn missing_compressor_returns_raw_unchanged() {
        let raw = "x".repeat(4096);
        let out = compress_observation_for_prompt(None, &raw, "test").await;
        assert_eq!(out, raw);
    }

    #[test]
    fn active_config_without_api_key_builds_no_compressor() {
        let config = CompressionConfig {
            enabled: true,
            model: "claude-haiku-4-5-20251001".into(),
            ..Default::default()
        };
        // No env mutation: only assert the builder contract when the key is
        // absent from the environment; skip silently when a real key exists.
        if std::env::var("ANTHROPIC_API_KEY").is_err() {
            assert!(build_observation_compressor(&config).is_none());
        }
    }

    struct DisagreeingModel {
        summaries: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl CompressModel for DisagreeingModel {
        async fn summarize(
            &self,
            _obs: &str,
            _hint: &CompressHint,
        ) -> Result<CompressModelOutput<String>, CompressModelError> {
            self.summaries.fetch_add(1, Ordering::Relaxed);
            Ok(CompressModelOutput {
                output: "compressed observation".to_string(),
                usage: CompressorCallUsage {
                    input_tokens: 10,
                    output_tokens: 2,
                    model: "test-compressor".to_string(),
                },
            })
        }

        async fn sketch(
            &self,
            observation: &str,
            _hint: &CompressHint,
        ) -> Result<CompressModelOutput<ActionSketch>, CompressModelError> {
            let compressed = observation == "compressed observation";
            Ok(CompressModelOutput {
                output: ActionSketch {
                    intent: if compressed { "stop" } else { "continue" }.to_string(),
                    target_files: vec![if compressed { "b.rs" } else { "a.rs" }.to_string()],
                    command_class: if compressed { "none" } else { "cargo_test" }.to_string(),
                },
                usage: CompressorCallUsage {
                    input_tokens: 5,
                    output_tokens: 1,
                    model: "test-compressor".to_string(),
                },
            })
        }
    }

    #[tokio::test]
    async fn shared_handle_accumulates_nap_failures_and_opens_breaker() {
        let summaries = Arc::new(AtomicUsize::new(0));
        let compressor = Arc::new(PromptCompressor::new(
            DisagreeingModel {
                summaries: Arc::clone(&summaries),
            },
            1.0,
            0,
        ));
        let raw = "raw observation";

        for _ in 0..5 {
            let output =
                compress_observation_for_prompt(Some(compressor.as_ref()), raw, "task").await;
            assert_eq!(output, raw);
        }
        let after_breaker =
            compress_observation_for_prompt(Some(compressor.as_ref()), raw, "task").await;

        assert_eq!(after_breaker, raw);
        assert_eq!(compressor.nap_checked(), 5);
        assert_eq!(compressor.nap_failed(), 5);
        assert_eq!(summaries.load(Ordering::Relaxed), 5);
    }
}
