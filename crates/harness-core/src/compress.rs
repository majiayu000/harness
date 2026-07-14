//! Behavior-preserving observation compression (GH1574, NAP-Lite).
//!
//! Compresses observation-class content (subagent results, tool-output
//! artifacts) through a small model while verifying, by sampling, that the
//! compressed text induces the same next action as the raw text
//! (Next-Action Preservation, CoACT arXiv:2607.02911).
//!
//! Rules, skills, contracts, and exec-plan items are never compressed here;
//! callers own that boundary. Raw text must remain recoverable by the
//! caller (artifact path recorded before replacement).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use thiserror::Error;

/// Minimum NAP checks before the failure-rate circuit breaker may trip.
const BREAKER_MIN_CHECKS: u64 = 5;
/// NAP failure rate above which compression is bypassed for the handle.
const BREAKER_FAILURE_RATE: f64 = 0.15;

/// Where an observation came from; steers the compressor prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObsSource {
    AgentResult,
    ComposeDegradation,
}

/// Caller-supplied framing for a compression request.
#[derive(Debug, Clone)]
pub struct CompressHint {
    /// Short task framing injected into the compressor prompt.
    pub task_summary: String,
    pub source: ObsSource,
}

/// NAP verification outcome for one compression.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum NapStatus {
    /// Sampled and the next-action sketches agreed.
    Verified,
    /// Not selected by the verification sampler.
    SkippedSample,
    /// Sketches disagreed; the caller must use raw text.
    Failed { fell_back: bool },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Compressed {
    pub text: String,
    /// Byte-based estimate for the raw observation, not provider usage.
    pub original_tokens: u32,
    /// Byte-based estimate for the returned text, not provider usage.
    pub compressed_tokens: u32,
    /// Provider-reported usage for every completed compressor model call.
    #[serde(default)]
    pub compressor_usage: CompressorUsage,
    pub nap: NapStatus,
}

/// Provider-reported usage for one completed compressor model call.
///
/// Pricing is intentionally absent: the compressor does not have an approved
/// pricing oracle, and [`crate::types::TokenUsage::cost_usd`] must not be
/// inferred here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompressorCallUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Actual model identifier returned by the provider.
    pub model: String,
}

/// Provider-reported usage accumulated across completed compressor calls.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompressorUsage {
    pub calls: Vec<CompressorCallUsage>,
}

impl CompressorUsage {
    /// Sum provider-reported input tokens without wrapping on overflow.
    pub fn input_tokens(&self) -> u64 {
        self.calls
            .iter()
            .fold(0, |total, call| total.saturating_add(call.input_tokens))
    }

    /// Sum provider-reported output tokens without wrapping on overflow.
    pub fn output_tokens(&self) -> u64 {
        self.calls
            .iter()
            .fold(0, |total, call| total.saturating_add(call.output_tokens))
    }

    /// Actual provider-returned model identifier for each completed call.
    pub fn models(&self) -> Vec<&str> {
        self.calls.iter().map(|call| call.model.as_str()).collect()
    }

    fn push(&mut self, call: CompressorCallUsage) {
        self.calls.push(call);
    }
}

/// Successful compressor model output paired with provider-reported usage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompressModelOutput<T> {
    pub output: T,
    pub usage: CompressorCallUsage,
}

/// Compressor model failure, optionally after a provider call completed.
///
/// `usage` is present for post-processing failures such as invalid sketch
/// JSON, because the provider completed and reported usage for that call.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{message}")]
pub struct CompressModelError {
    pub message: String,
    pub usage: Option<CompressorCallUsage>,
}

#[derive(Debug, Error)]
pub enum CompressError {
    /// Input below the configured minimum; caller keeps raw text. A skip,
    /// not a failure.
    #[error("observation below min_size_bytes ({size} < {min})")]
    TooSmall { size: usize, min: usize },
    /// The failure-rate circuit breaker is open; caller keeps raw text.
    /// Callers must surface this at error level once per handle.
    #[error("compression bypassed: NAP failure rate {rate:.2} over {checks} checks")]
    BreakerOpen { rate: f64, checks: u64 },
    #[error("compressor model error: {message}")]
    Model {
        message: String,
        usage: CompressorUsage,
    },
    /// The model returned a sketch that could not be parsed.
    #[error("unparseable next-action sketch: {0}")]
    BadSketch(String),
}

/// Structured next-action sketch used for NAP comparison.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionSketch {
    pub intent: String,
    #[serde(default)]
    pub target_files: Vec<String>,
    pub command_class: String,
}

impl ActionSketch {
    /// Field-level agreement: intent and command_class compare
    /// case-insensitively; target_files compare as sets.
    pub fn agreement(&self, other: &ActionSketch) -> u8 {
        let mut agree = 0u8;
        if self.intent.eq_ignore_ascii_case(&other.intent) {
            agree += 1;
        }
        if self
            .command_class
            .eq_ignore_ascii_case(&other.command_class)
        {
            agree += 1;
        }
        let mut a: Vec<&str> = self.target_files.iter().map(String::as_str).collect();
        let mut b: Vec<&str> = other.target_files.iter().map(String::as_str).collect();
        a.sort_unstable();
        b.sort_unstable();
        if a == b {
            agree += 1;
        }
        agree
    }
}

/// Model access needed by the compressor. Implementations live where the
/// provider plumbing lives (harness-agents); tests use mocks.
#[async_trait]
pub trait CompressModel: Send + Sync {
    /// Return a compressed rendition of `raw` for the given task framing.
    async fn summarize(
        &self,
        raw: &str,
        hint: &CompressHint,
    ) -> Result<CompressModelOutput<String>, CompressModelError>;

    /// Return a next-action sketch given an observation and task framing.
    async fn sketch(
        &self,
        observation: &str,
        hint: &CompressHint,
    ) -> Result<CompressModelOutput<ActionSketch>, CompressModelError>;
}

fn collect_model_output<T>(
    result: Result<CompressModelOutput<T>, CompressModelError>,
    usage: &mut CompressorUsage,
) -> Result<T, CompressError> {
    match result {
        Ok(output) => {
            usage.push(output.usage);
            Ok(output.output)
        }
        Err(error) => {
            if let Some(call) = error.usage {
                usage.push(call);
            }
            Err(CompressError::Model {
                message: error.message,
                usage: std::mem::take(usage),
            })
        }
    }
}

/// Compression entry point used by the seams.
#[async_trait]
pub trait ObservationCompressor: Send + Sync {
    async fn compress(&self, obs: &str, hint: &CompressHint) -> Result<Compressed, CompressError>;
}

/// Decides which compressions get NAP-verified. Deterministic by default so
/// replays are stable; no RNG dependency.
pub trait NapSampler: Send + Sync {
    fn should_verify(&self, seq: u64) -> bool;
}

/// Verifies every `n`-th compression (1-based). `EveryN::from_rate(0.10)`
/// verifies every 10th call.
#[derive(Debug, Clone, Copy)]
pub struct EveryN(pub u64);

impl EveryN {
    pub fn from_rate(rate: f64) -> Self {
        if rate <= 0.0 {
            // Rate 0 disables verification entirely.
            return Self(u64::MAX);
        }
        let n = (1.0 / rate.min(1.0)).round() as u64;
        Self(n.max(1))
    }
}

impl NapSampler for EveryN {
    fn should_verify(&self, seq: u64) -> bool {
        self.0 != u64::MAX && seq.is_multiple_of(self.0)
    }
}

/// Applies the configured verification rate with a seed-selected phase.
///
/// The seed is mapped with 64-bit FNV-1a, whose constants and byte-wise
/// algorithm are fixed here to keep replay behavior stable across processes
/// and Rust releases. This hash is only for deterministic sampling, not
/// security. The phase gives each call the configured selection probability
/// across distinct task/request IDs while systematic sampling keeps the
/// long-run rate close to the configured value for each handle.
#[derive(Debug, Clone, Copy)]
pub struct SeededRateSampler {
    rate_units: u64,
    phase_units: u64,
}

const SAMPLING_UNITS: u64 = 1_u64 << 53;

impl SeededRateSampler {
    pub fn from_rate(rate: f64, seed: &str) -> Self {
        let rate = if rate.is_nan() {
            0.0
        } else {
            rate.clamp(0.0, 1.0)
        };
        // Use 53-bit fixed-point units so arbitrary f64 rates retain their
        // configured precision without floating-point sequence drift.
        let rate_units = (rate * SAMPLING_UNITS as f64).round() as u64;
        let phase_units = stable_seed_hash(seed) >> 11;
        Self {
            rate_units,
            phase_units,
        }
    }
}

impl NapSampler for SeededRateSampler {
    fn should_verify(&self, seq: u64) -> bool {
        if seq == 0 || self.rate_units == 0 {
            return false;
        }
        if self.rate_units >= SAMPLING_UNITS {
            return true;
        }

        let scale = u128::from(SAMPLING_UNITS);
        let rate = u128::from(self.rate_units);
        let phase = u128::from(self.phase_units);
        let current = (u128::from(seq) * rate + phase) / scale;
        let previous = (u128::from(seq - 1) * rate + phase) / scale;
        current > previous
    }
}

fn stable_seed_hash(seed: &str) -> u64 {
    const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

    seed.bytes().fold(FNV_OFFSET_BASIS, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(FNV_PRIME)
    })
}

/// Token estimate mirroring `BytesDivFourEstimator` in harness-context.
/// Canonical core-level helper; downstream crates with private variants
/// (harness-workflow memory_retrieval) can migrate to this.
pub fn estimate_tokens(content: &str) -> u32 {
    if content.is_empty() {
        0
    } else {
        ((content.len() as u32) / 4).max(1)
    }
}

/// Prompt-based compressor generic over the model client. Inert unless the
/// caller constructed it from an enabled config with a model id.
pub struct PromptCompressor<M: CompressModel, S: NapSampler = EveryN> {
    model: M,
    sampler: S,
    min_size_bytes: usize,
    seq: AtomicU64,
    nap_checked: AtomicU64,
    nap_failed: AtomicU64,
}

impl<M: CompressModel> PromptCompressor<M, EveryN> {
    pub fn new(model: M, sample_rate: f64, min_size_bytes: usize) -> Self {
        Self::with_sampler(model, EveryN::from_rate(sample_rate), min_size_bytes)
    }
}

impl<M: CompressModel> PromptCompressor<M, SeededRateSampler> {
    /// Construct a compressor whose deterministic verification phase is
    /// derived from a stable task/request identifier.
    pub fn new_seeded(model: M, sample_rate: f64, min_size_bytes: usize, seed: &str) -> Self {
        Self::with_sampler(
            model,
            SeededRateSampler::from_rate(sample_rate, seed),
            min_size_bytes,
        )
    }
}

impl<M: CompressModel, S: NapSampler> PromptCompressor<M, S> {
    pub fn with_sampler(model: M, sampler: S, min_size_bytes: usize) -> Self {
        Self {
            model,
            sampler,
            min_size_bytes,
            seq: AtomicU64::new(0),
            nap_checked: AtomicU64::new(0),
            nap_failed: AtomicU64::new(0),
        }
    }

    pub fn nap_checked(&self) -> u64 {
        self.nap_checked.load(Ordering::Relaxed)
    }

    pub fn nap_failed(&self) -> u64 {
        self.nap_failed.load(Ordering::Relaxed)
    }

    fn breaker_state(&self) -> Option<(f64, u64)> {
        let checked = self.nap_checked.load(Ordering::Relaxed);
        if checked < BREAKER_MIN_CHECKS {
            return None;
        }
        let failed = self.nap_failed.load(Ordering::Relaxed);
        let rate = failed as f64 / checked as f64;
        (rate > BREAKER_FAILURE_RATE).then_some((rate, checked))
    }
}

#[async_trait]
impl<M: CompressModel, S: NapSampler> ObservationCompressor for PromptCompressor<M, S> {
    async fn compress(&self, obs: &str, hint: &CompressHint) -> Result<Compressed, CompressError> {
        if obs.len() < self.min_size_bytes {
            return Err(CompressError::TooSmall {
                size: obs.len(),
                min: self.min_size_bytes,
            });
        }
        if let Some((rate, checks)) = self.breaker_state() {
            return Err(CompressError::BreakerOpen { rate, checks });
        }

        let mut compressor_usage = CompressorUsage::default();
        let text =
            collect_model_output(self.model.summarize(obs, hint).await, &mut compressor_usage)?;
        let original_tokens = estimate_tokens(obs);
        let compressed_tokens = estimate_tokens(&text);

        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        if !self.sampler.should_verify(seq) {
            return Ok(Compressed {
                text,
                original_tokens,
                compressed_tokens,
                compressor_usage,
                nap: NapStatus::SkippedSample,
            });
        }

        let raw_sketch =
            collect_model_output(self.model.sketch(obs, hint).await, &mut compressor_usage)?;
        let compressed_sketch =
            collect_model_output(self.model.sketch(&text, hint).await, &mut compressor_usage)?;
        self.nap_checked.fetch_add(1, Ordering::Relaxed);

        if raw_sketch.agreement(&compressed_sketch) >= 2 {
            Ok(Compressed {
                text,
                original_tokens,
                compressed_tokens,
                compressor_usage,
                nap: NapStatus::Verified,
            })
        } else {
            self.nap_failed.fetch_add(1, Ordering::Relaxed);
            // Fall back to raw: the caller receives the original text so
            // nothing behavior-bearing is lost, and the manifest records
            // the failure.
            Ok(Compressed {
                text: obs.to_string(),
                original_tokens,
                compressed_tokens: original_tokens,
                compressor_usage,
                nap: NapStatus::Failed { fell_back: true },
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeModel {
        summary: String,
        raw_sketch: ActionSketch,
        compressed_sketch: ActionSketch,
        compressed_sketch_error: Option<CompressModelError>,
    }

    impl FakeModel {
        fn agreeing(summary: &str) -> Self {
            let sketch = ActionSketch {
                intent: "fix failing test".into(),
                target_files: vec!["src/lib.rs".into()],
                command_class: "cargo_test".into(),
            };
            Self {
                summary: summary.into(),
                raw_sketch: sketch.clone(),
                compressed_sketch: sketch,
                compressed_sketch_error: None,
            }
        }

        fn disagreeing(summary: &str) -> Self {
            let mut this = Self::agreeing(summary);
            this.compressed_sketch = ActionSketch {
                intent: "open a PR".into(),
                target_files: vec!["README.md".into()],
                command_class: "gh_pr".into(),
            };
            this
        }

        fn failing_after_summary_and_raw_sketch(summary: &str) -> Self {
            let mut this = Self::agreeing(summary);
            this.compressed_sketch_error = Some(CompressModelError {
                message: "invalid sketch JSON".into(),
                usage: Some(call_usage(4, 1, "compressed-sketch-model")),
            });
            this
        }

        fn request_failing_after_summary_and_raw_sketch(summary: &str) -> Self {
            let mut this = Self::agreeing(summary);
            this.compressed_sketch_error = Some(CompressModelError {
                message: "provider request failed".into(),
                usage: None,
            });
            this
        }
    }

    fn call_usage(input_tokens: u64, output_tokens: u64, model: &str) -> CompressorCallUsage {
        CompressorCallUsage {
            input_tokens,
            output_tokens,
            model: model.into(),
        }
    }

    #[async_trait]
    impl CompressModel for FakeModel {
        async fn summarize(
            &self,
            _raw: &str,
            _hint: &CompressHint,
        ) -> Result<CompressModelOutput<String>, CompressModelError> {
            Ok(CompressModelOutput {
                output: self.summary.clone(),
                usage: call_usage(10, 2, "summary-model"),
            })
        }

        async fn sketch(
            &self,
            observation: &str,
            _hint: &CompressHint,
        ) -> Result<CompressModelOutput<ActionSketch>, CompressModelError> {
            if observation == self.summary {
                let usage = call_usage(4, 1, "compressed-sketch-model");
                if let Some(error) = &self.compressed_sketch_error {
                    return Err(error.clone());
                }
                Ok(CompressModelOutput {
                    output: self.compressed_sketch.clone(),
                    usage,
                })
            } else {
                Ok(CompressModelOutput {
                    output: self.raw_sketch.clone(),
                    usage: call_usage(6, 1, "raw-sketch-model"),
                })
            }
        }
    }

    fn hint() -> CompressHint {
        CompressHint {
            task_summary: "queue drain".into(),
            source: ObsSource::AgentResult,
        }
    }

    fn raw_obs() -> String {
        "test output line\n".repeat(200)
    }

    #[tokio::test]
    async fn small_observation_is_skipped() {
        let c = PromptCompressor::new(FakeModel::agreeing("s"), 1.0, 2048);
        let err = c.compress("short", &hint()).await.unwrap_err();
        assert!(matches!(err, CompressError::TooSmall { .. }));
    }

    #[tokio::test]
    async fn verified_compression_returns_summary() {
        let c = PromptCompressor::new(FakeModel::agreeing("summary text"), 1.0, 16);
        let out = c.compress(&raw_obs(), &hint()).await.unwrap();
        assert_eq!(out.text, "summary text");
        assert_eq!(out.nap, NapStatus::Verified);
        assert!(out.compressed_tokens < out.original_tokens);
        assert_eq!(out.compressor_usage.calls.len(), 3);
        assert_eq!(out.compressor_usage.input_tokens(), 20);
        assert_eq!(out.compressor_usage.output_tokens(), 4);
        assert_eq!(c.nap_checked(), 1);
    }

    #[tokio::test]
    async fn sampler_skips_unsampled_calls() {
        // Verify every 2nd call: call 1 skipped, call 2 verified.
        let c = PromptCompressor::with_sampler(FakeModel::agreeing("summary text"), EveryN(2), 16);
        let first = c.compress(&raw_obs(), &hint()).await.unwrap();
        assert_eq!(first.nap, NapStatus::SkippedSample);
        assert_eq!(first.compressor_usage.calls.len(), 1);
        assert_eq!(first.compressor_usage.models(), vec!["summary-model"]);
        let second = c.compress(&raw_obs(), &hint()).await.unwrap();
        assert_eq!(second.nap, NapStatus::Verified);
        assert_eq!(second.compressor_usage.calls.len(), 3);
    }

    #[test]
    fn seeded_sampler_is_stable_and_distributes_first_calls() {
        let decisions: Vec<bool> = (0..1_000)
            .map(|seed| SeededRateSampler::from_rate(0.1, &format!("task-{seed}")).should_verify(1))
            .collect();
        let repeated: Vec<bool> = (0..1_000)
            .map(|seed| SeededRateSampler::from_rate(0.1, &format!("task-{seed}")).should_verify(1))
            .collect();
        let selected = decisions.iter().filter(|decision| **decision).count();

        assert_eq!(decisions, repeated);
        assert!(
            (80..=120).contains(&selected),
            "expected about 100 sampled task seeds, got {selected}"
        );
    }

    #[test]
    fn seeded_sampler_honors_arbitrary_rates() {
        for rate in [0.1, 0.6, 0.75] {
            let sampler = SeededRateSampler::from_rate(rate, "task-1574");
            let selected = (1..=10_000)
                .filter(|seq| sampler.should_verify(*seq))
                .count();
            let expected = (10_000.0 * rate) as usize;
            assert!(selected.abs_diff(expected) <= 1, "rate={rate}");
        }

        assert!(!SeededRateSampler::from_rate(0.0, "task-1574").should_verify(1));
        assert!(SeededRateSampler::from_rate(2.0, "task-1574").should_verify(1));
        assert!(!SeededRateSampler::from_rate(f64::NAN, "task-1574").should_verify(1));
    }

    #[tokio::test]
    async fn nap_mismatch_falls_back_to_raw() {
        let c = PromptCompressor::new(FakeModel::disagreeing("bad summary"), 1.0, 16);
        let obs = raw_obs();
        let out = c.compress(&obs, &hint()).await.unwrap();
        assert_eq!(out.text, obs);
        assert_eq!(out.nap, NapStatus::Failed { fell_back: true });
        assert_eq!(out.compressor_usage.calls.len(), 3);
        assert_eq!(c.nap_checked(), 1);
        assert_eq!(c.nap_failed(), 1);
    }

    #[tokio::test]
    async fn later_post_processing_failure_retains_completed_and_failed_call_usage() {
        let c = PromptCompressor::new(
            FakeModel::failing_after_summary_and_raw_sketch("summary text"),
            1.0,
            16,
        );
        let err = c.compress(&raw_obs(), &hint()).await.unwrap_err();
        let CompressError::Model { message, usage } = err else {
            panic!("expected model error");
        };
        assert_eq!(message, "invalid sketch JSON");
        assert_eq!(usage.calls.len(), 3);
        assert_eq!(usage.input_tokens(), 20);
        assert_eq!(usage.output_tokens(), 4);
        assert_eq!(
            usage.models(),
            vec![
                "summary-model",
                "raw-sketch-model",
                "compressed-sketch-model"
            ]
        );
        assert_eq!(c.nap_checked(), 0);
    }

    #[tokio::test]
    async fn later_request_failure_retains_only_completed_call_usage() {
        let c = PromptCompressor::new(
            FakeModel::request_failing_after_summary_and_raw_sketch("summary text"),
            1.0,
            16,
        );
        let err = c.compress(&raw_obs(), &hint()).await.unwrap_err();
        let CompressError::Model { message, usage } = err else {
            panic!("expected model error");
        };
        assert_eq!(message, "provider request failed");
        assert_eq!(usage.calls.len(), 2);
        assert_eq!(usage.input_tokens(), 16);
        assert_eq!(usage.output_tokens(), 3);
        assert_eq!(usage.models(), vec!["summary-model", "raw-sketch-model"]);
        assert_eq!(c.nap_checked(), 0);
    }

    #[tokio::test]
    async fn breaker_opens_after_sustained_failures() {
        let c = PromptCompressor::new(FakeModel::disagreeing("bad"), 1.0, 16);
        let obs = raw_obs();
        for _ in 0..5 {
            let out = c.compress(&obs, &hint()).await.unwrap();
            assert_eq!(out.nap, NapStatus::Failed { fell_back: true });
        }
        // 5 checks, 100% failure: breaker must now be open.
        let err = c.compress(&obs, &hint()).await.unwrap_err();
        assert!(matches!(err, CompressError::BreakerOpen { .. }));
    }

    #[tokio::test]
    async fn rate_zero_disables_verification() {
        let c = PromptCompressor::new(FakeModel::disagreeing("s"), 0.0, 16);
        let out = c.compress(&raw_obs(), &hint()).await.unwrap();
        assert_eq!(out.nap, NapStatus::SkippedSample);
    }

    #[test]
    fn sketch_agreement_is_field_level() {
        let a = ActionSketch {
            intent: "Fix Failing Test".into(),
            target_files: vec!["b.rs".into(), "a.rs".into()],
            command_class: "cargo_test".into(),
        };
        let b = ActionSketch {
            intent: "fix failing test".into(),
            target_files: vec!["a.rs".into(), "b.rs".into()],
            command_class: "different".into(),
        };
        assert_eq!(a.agreement(&b), 2);
    }
}
