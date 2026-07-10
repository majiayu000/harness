//! NAP-Lite compression at the agent-result → next-prompt seam (GH1574).
//!
//! The raw output always stays in the task store (checkpoints /
//! `state.plan_output`); compression only affects what is injected into the
//! next agent's prompt, so every failure path falls back to raw text.

use harness_agents::compress_model::build_observation_compressor;
use harness_core::compress::{CompressError, CompressHint, NapStatus, ObsSource};
use harness_core::config::compression::CompressionConfig;

/// Compress a prior agent's output before injecting it into the next
/// phase's prompt. Returns the raw text unchanged when compression is
/// inactive, skipped, or fails.
///
/// The compressor handle is built per call, so the NAP circuit breaker
/// only aggregates within one call site invocation; a shared per-task
/// handle arrives with the triage-seam wiring (tasks.md handoff).
pub(crate) async fn compress_observation_for_prompt(
    config: &CompressionConfig,
    raw: &str,
    task_summary: &str,
) -> String {
    let Some(compressor) = build_observation_compressor(config) else {
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
                        "NAP verification failed; using raw observation"
                    );
                }
                nap => {
                    tracing::info!(
                        task_summary,
                        original_tokens = compressed.original_tokens,
                        compressed_tokens = compressed.compressed_tokens,
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
        Err(err) => {
            tracing::warn!(%err, task_summary, "compression failed; using raw observation");
            raw.to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn inactive_config_returns_raw_unchanged() {
        let raw = "x".repeat(4096);
        let out =
            compress_observation_for_prompt(&CompressionConfig::default(), &raw, "test").await;
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
}
