//! Task-scoped NAP-Lite compression at agent-result → next-prompt seams (GH1574).
//!
//! The raw output always stays in the task store (checkpoints /
//! `state.plan_output`); compression only affects what is injected into the
//! next agent's prompt, so every failure path falls back to raw text.

#[cfg(test)]
use harness_agents::compress_model::build_observation_compressor;
use harness_agents::compress_model::build_seeded_observation_compressor;
use harness_core::compress::{
    CompressError, CompressHint, NapStatus, ObsSource, ObservationCompressor,
};
use harness_core::config::compression::CompressionConfig;
use std::sync::Arc;

use crate::task_runner::TaskId;

pub(crate) type TaskObservationCompressor = Arc<dyn ObservationCompressor>;

tokio::task_local! {
    static COMPLETION_OBSERVATION_SESSION: Arc<TaskObservationCompressionSession>;
}

/// A compressor whose sampling and breaker counters belong to exactly one task.
pub(crate) struct TaskObservationCompressionSession {
    compressor: TaskObservationCompressor,
}

impl TaskObservationCompressionSession {
    pub(crate) fn compressor(&self) -> &dyn ObservationCompressor {
        self.compressor.as_ref()
    }
}

pub(crate) fn start_task_observation_session(
    config: &CompressionConfig,
    task_id: &TaskId,
) -> Option<Arc<TaskObservationCompressionSession>> {
    build_task_observation_compressor(config, task_id)
        .map(|compressor| Arc::new(TaskObservationCompressionSession { compressor }))
}

pub(crate) async fn scope_completion_observation_session<F>(
    session: Arc<TaskObservationCompressionSession>,
    future: F,
) -> F::Output
where
    F: std::future::Future,
{
    COMPLETION_OBSERVATION_SESSION.scope(session, future).await
}

pub(crate) fn completion_observation_session() -> Option<Arc<TaskObservationCompressionSession>> {
    COMPLETION_OBSERVATION_SESSION.try_with(Arc::clone).ok()
}

#[cfg(test)]
pub(crate) fn test_task_observation_session(
    compressor: TaskObservationCompressor,
) -> Arc<TaskObservationCompressionSession> {
    Arc::new(TaskObservationCompressionSession { compressor })
}

#[async_trait::async_trait]
pub(crate) trait RawObservationSink: Send + Sync {
    async fn persist_raw(
        &self,
        task_id: &TaskId,
        turn: u32,
        artifact_type: &str,
        raw: &str,
    ) -> anyhow::Result<()>;
}

/// Build one compressor for the full task lifecycle so sampling and breaker
/// state accumulate across prompt-injection seams and transient retries.
///
/// GH-1574 still needs separate wiring for triage-to-plan, workflow-runtime,
/// and composer seams. Plan-to-implement and live-task cross-review share this
/// task-scoped handle.
pub(crate) fn build_task_observation_compressor(
    config: &CompressionConfig,
    task_id: &TaskId,
) -> Option<TaskObservationCompressor> {
    build_seeded_observation_compressor(config, task_id.as_str())
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
        ActionSketch, CompressModel, CompressModelError, CompressModelOutput, Compressed,
        CompressorCallUsage, CompressorUsage, PromptCompressor,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct StaticCompressor(&'static str);

    #[async_trait]
    impl ObservationCompressor for StaticCompressor {
        async fn compress(
            &self,
            obs: &str,
            _hint: &CompressHint,
        ) -> Result<Compressed, CompressError> {
            Ok(Compressed {
                text: self.0.to_string(),
                original_tokens: obs.len() as u32,
                compressed_tokens: 1,
                compressor_usage: CompressorUsage::default(),
                nap: NapStatus::SkippedSample,
            })
        }
    }

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

    #[tokio::test]
    async fn completion_scope_exposes_only_the_exact_session() {
        let old = test_task_observation_session(Arc::new(StaticCompressor("old")));
        let new = test_task_observation_session(Arc::new(StaticCompressor("new")));

        assert!(completion_observation_session().is_none());
        scope_completion_observation_session(Arc::clone(&old), async {
            assert!(Arc::ptr_eq(
                &old,
                &completion_observation_session().unwrap()
            ));
            scope_completion_observation_session(Arc::clone(&new), async {
                assert!(Arc::ptr_eq(
                    &new,
                    &completion_observation_session().unwrap()
                ));
            })
            .await;
            assert!(Arc::ptr_eq(
                &old,
                &completion_observation_session().unwrap()
            ));
        })
        .await;
        assert!(completion_observation_session().is_none());
    }

    #[tokio::test]
    async fn concurrent_completion_scopes_do_not_cross_sessions() {
        let first = test_task_observation_session(Arc::new(StaticCompressor("first")));
        let second = test_task_observation_session(Arc::new(StaticCompressor("second")));

        let first_scope = scope_completion_observation_session(Arc::clone(&first), async {
            tokio::task::yield_now().await;
            Arc::ptr_eq(&first, &completion_observation_session().unwrap())
        });
        let second_scope = scope_completion_observation_session(Arc::clone(&second), async {
            tokio::task::yield_now().await;
            Arc::ptr_eq(&second, &completion_observation_session().unwrap())
        });

        let (first_isolated, second_isolated) = tokio::join!(first_scope, second_scope);
        assert!(first_isolated);
        assert!(second_isolated);
        assert!(completion_observation_session().is_none());
    }
}
