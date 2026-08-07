use crate::observation_compression::{
    compress_observation_for_prompt, RawObservationSink, TaskObservationCompressionSession,
};
use crate::task_runner::TaskId;
use crate::{http::AppState, validate_root};
use harness_core::{agent::AgentRequest, agent::CodeAgent};
use harness_protocol::{methods::RpcResponse, methods::INTERNAL_ERROR};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::Arc;

const DEFAULT_MAX_ROUNDS: u32 = 3;
const PRIMARY_RAW_ARTIFACT_TYPE: &str = "cross_review_primary_raw";

pub(crate) struct CrossReviewCompressionContext {
    task_id: TaskId,
    turn: u32,
    session: Arc<TaskObservationCompressionSession>,
    raw_sink: Arc<dyn RawObservationSink>,
}

impl CrossReviewCompressionContext {
    pub(crate) fn new(
        task_id: TaskId,
        turn: u32,
        session: Arc<TaskObservationCompressionSession>,
        raw_sink: Arc<dyn RawObservationSink>,
    ) -> Self {
        Self {
            task_id,
            turn,
            session,
            raw_sink,
        }
    }
}

/// Whether the challenger round ran under a distinct model or the review
/// degraded to a single model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CrossReviewMode {
    CrossModel,
    SingleModelDegraded,
}

/// Fail-closed verdict. Serializes as the legacy uppercase strings for RPC
/// compatibility; `ApprovedDegraded` and `ProtocolFailure` are new wires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CrossReviewVerdict {
    /// Cross-model review, tags parsed, no consensus issues.
    Approved,
    /// Single-model degraded review with no issues — never reported as
    /// `Approved` so callers can tell the review lacked a challenger.
    ApprovedDegraded,
    NotConverged,
    /// Challenger reply followed none of the protocol tags; the round is
    /// unparseable and must not read as approval.
    ProtocolFailure,
}

impl CrossReviewVerdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            CrossReviewVerdict::Approved => "APPROVED",
            CrossReviewVerdict::ApprovedDegraded => "APPROVED_DEGRADED",
            CrossReviewVerdict::NotConverged => "NOT_CONVERGED",
            CrossReviewVerdict::ProtocolFailure => "PROTOCOL_FAILURE",
        }
    }
}

/// A challenger round whose reply carried no protocol tag at all.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtocolFailure {
    pub round: u32,
    /// Bounded excerpt of the offending reply.
    pub excerpt: String,
}

/// Maximum chars of a protocol-failure reply kept in `ProtocolFailure`.
const PROTOCOL_FAILURE_EXCERPT_MAX: usize = 280;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrossReviewResult {
    pub mode: CrossReviewMode,
    pub primary_agent_id: String,
    pub challenger_agent_id: Option<String>,
    pub primary_review: String,
    pub challenger_review: String,
    pub consensus_issues: Vec<String>,
    pub contested_issues: Vec<String>,
    pub rounds: u32,
    pub final_verdict: CrossReviewVerdict,
    pub protocol_failure: Option<ProtocolFailure>,
}

pub async fn cross_review(
    state: &AppState,
    id: Option<serde_json::Value>,
    project_root: PathBuf,
    target: String,
    max_rounds: Option<u32>,
) -> RpcResponse {
    let project_root = validate_root!(&project_root, id, &state.core.home_dir);

    let primary = match state.core.server.agent_registry.default_agent() {
        Some(a) => a,
        None => return RpcResponse::error(id, INTERNAL_ERROR, "no agent registered"),
    };

    let challenger = distinct_challenger(&primary, state.core.server.agent_registry.get("codex"));
    let rounds = max_rounds.unwrap_or(DEFAULT_MAX_ROUNDS);

    let result =
        match run_cross_review(primary, challenger, project_root, target, rounds, None).await {
            Ok(r) => r,
            Err(e) => return RpcResponse::error(id, INTERNAL_ERROR, e),
        };

    match serde_json::to_value(&result) {
        Ok(v) => RpcResponse::success(id, v),
        Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
    }
}

/// Core cross-review orchestration logic, exposed for testing.
///
/// Flow:
/// 1. Primary agent reviews `target`.
/// 2. If no challenger, degrade gracefully to single-model result.
/// 3. Challenger iterates up to `max_rounds - 1` times, classifying each issue as
///    CONFIRMED, FALSE-POSITIVE, or MISSED.
/// 4. Returns APPROVED when no consensus issues remain; NOT_CONVERGED after max rounds.
///
/// `allowed_tools` controls agent execution permissions:
/// - `None`        → Full profile (`--dangerously-skip-permissions`). Use for interactive calls.
/// - `Some(tools)` → Restricted to the listed tools. Pass `Some(vec![])` to deny all tools
///   (read-only text review where all content is in the prompt).
pub async fn run_cross_review(
    primary: Arc<dyn CodeAgent>,
    challenger: Option<Arc<dyn CodeAgent>>,
    project_root: PathBuf,
    target: String,
    max_rounds: u32,
    allowed_tools: Option<Vec<String>>,
) -> Result<CrossReviewResult, String> {
    run_cross_review_with_context(
        primary,
        challenger,
        project_root,
        target,
        max_rounds,
        allowed_tools,
        None,
    )
    .await
}

pub(crate) async fn run_cross_review_with_context(
    primary: Arc<dyn CodeAgent>,
    challenger: Option<Arc<dyn CodeAgent>>,
    project_root: PathBuf,
    target: String,
    max_rounds: u32,
    allowed_tools: Option<Vec<String>>,
    compression: Option<&CrossReviewCompressionContext>,
) -> Result<CrossReviewResult, String> {
    let safe_target = harness_core::prompts::wrap_external_data(&target);
    let primary_prompt = harness_core::prompts::cross_review::primary_review_prompt(&safe_target);

    let primary_resp = primary
        .execute(AgentRequest {
            prompt: primary_prompt,
            project_root: project_root.clone(),
            allowed_tools: allowed_tools.clone(),
            ..Default::default()
        })
        .await
        .map_err(|e| e.to_string())?;
    let primary_review = primary_resp.output;

    let challenger = match challenger {
        None => {
            // Degraded single-model review: treat all ISSUE lines as
            // consensus, and never report a clean review as fully approved.
            let consensus_issues = harness_core::prompts::extract_review_issues(&primary_review);
            let verdict = if consensus_issues.is_empty() {
                CrossReviewVerdict::ApprovedDegraded
            } else {
                CrossReviewVerdict::NotConverged
            };
            return Ok(CrossReviewResult {
                mode: CrossReviewMode::SingleModelDegraded,
                primary_agent_id: primary.id(),
                challenger_agent_id: None,
                primary_review,
                challenger_review: String::new(),
                consensus_issues,
                contested_issues: Vec::new(),
                rounds: 1,
                final_verdict: verdict,
                protocol_failure: None,
            });
        }
        Some(c) => c,
    };
    let challenger_agent_id = challenger.id();

    let mut challenger_review = String::new();
    let mut rounds_done = 1u32;
    let mut consensus_issues: Vec<String> =
        harness_core::prompts::extract_review_issues(&primary_review);
    let primary_for_challenger = if max_rounds <= 1 {
        Cow::Borrowed(primary_review.as_str())
    } else {
        match compression {
            Some(context) => {
                match context
                    .raw_sink
                    .persist_raw(
                        &context.task_id,
                        context.turn,
                        PRIMARY_RAW_ARTIFACT_TYPE,
                        &primary_review,
                    )
                    .await
                {
                    Ok(()) => Cow::Owned(
                        compress_observation_for_prompt(
                            Some(context.session.compressor()),
                            &primary_review,
                            &format!("cross-review primary output for task {}", context.task_id.0),
                        )
                        .await,
                    ),
                    Err(error) => {
                        tracing::error!(
                            task_id = %context.task_id.0,
                            turn = context.turn,
                            %error,
                            "raw cross-review primary output was not persisted; bypassing compression"
                        );
                        Cow::Borrowed(primary_review.as_str())
                    }
                }
            }
            None => Cow::Borrowed(primary_review.as_str()),
        }
    };
    let safe_primary = harness_core::prompts::wrap_external_data(primary_for_challenger.as_ref());

    for _ in 0..max_rounds.saturating_sub(1) {
        rounds_done += 1;

        // Rebuild prompt each round with outstanding issues from previous round
        let outstanding = if consensus_issues.is_empty() {
            String::new()
        } else {
            let items: String = consensus_issues
                .iter()
                .map(|i| format!("- {i}"))
                .collect::<Vec<_>>()
                .join("\n");
            format!("\n\nOutstanding issues from previous round:\n{items}")
        };

        let challenge_prompt =
            harness_core::prompts::cross_review::challenger_prompt(&safe_primary, &outstanding);

        let resp = challenger
            .execute(AgentRequest {
                prompt: challenge_prompt,
                project_root: project_root.clone(),
                allowed_tools: allowed_tools.clone(),
                ..Default::default()
            })
            .await
            .map_err(|e| e.to_string())?;
        challenger_review = resp.output;

        consensus_issues = extract_tagged(&challenger_review, "CONFIRMED")
            .into_iter()
            .chain(extract_tagged(&challenger_review, "MISSED"))
            .collect();
        let contested = extract_tagged(&challenger_review, "FALSE-POSITIVE");

        // Fail closed (GH-1767): a challenger reply with no protocol tag at
        // all is unparseable, not an approval. A reply carrying only
        // FALSE-POSITIVE tags remains a valid approving round.
        if consensus_issues.is_empty()
            && contested.is_empty()
            && !has_any_tag_prefix(&challenger_review)
        {
            return Ok(CrossReviewResult {
                mode: CrossReviewMode::CrossModel,
                primary_agent_id: primary.id(),
                challenger_agent_id: Some(challenger_agent_id.clone()),
                primary_review,
                challenger_review: challenger_review.clone(),
                consensus_issues: Vec::new(),
                contested_issues: Vec::new(),
                rounds: rounds_done,
                final_verdict: CrossReviewVerdict::ProtocolFailure,
                protocol_failure: Some(ProtocolFailure {
                    round: rounds_done,
                    excerpt: bounded_excerpt(&challenger_review),
                }),
            });
        }

        if consensus_issues.is_empty() {
            return Ok(CrossReviewResult {
                mode: CrossReviewMode::CrossModel,
                primary_agent_id: primary.id(),
                challenger_agent_id: Some(challenger_agent_id.clone()),
                primary_review,
                challenger_review,
                consensus_issues: Vec::new(),
                contested_issues: contested,
                rounds: rounds_done,
                final_verdict: CrossReviewVerdict::Approved,
                protocol_failure: None,
            });
        }
    }

    let consensus_issues: Vec<String> = extract_tagged(&challenger_review, "CONFIRMED")
        .into_iter()
        .chain(extract_tagged(&challenger_review, "MISSED"))
        .collect();
    let contested_issues = extract_tagged(&challenger_review, "FALSE-POSITIVE");

    // `rounds_done == 1` means the loop never ran (max_rounds <= 1): no
    // challenger reply exists to judge, so this is neither an approval nor a
    // protocol failure — fall through to NOT_CONVERGED.
    if rounds_done > 1 && consensus_issues.is_empty() && contested_issues.is_empty() {
        if !has_any_tag_prefix(&challenger_review) {
            return Ok(CrossReviewResult {
                mode: CrossReviewMode::CrossModel,
                primary_agent_id: primary.id(),
                challenger_agent_id: Some(challenger_agent_id),
                primary_review,
                challenger_review: challenger_review.clone(),
                consensus_issues: Vec::new(),
                contested_issues: Vec::new(),
                rounds: rounds_done,
                final_verdict: CrossReviewVerdict::ProtocolFailure,
                protocol_failure: Some(ProtocolFailure {
                    round: rounds_done,
                    excerpt: bounded_excerpt(&challenger_review),
                }),
            });
        }
        // Tags were present but all bodies were empty: parseable reply with
        // no findings — an approving round, not a failure.
        return Ok(CrossReviewResult {
            mode: CrossReviewMode::CrossModel,
            primary_agent_id: primary.id(),
            challenger_agent_id: Some(challenger_agent_id),
            primary_review,
            challenger_review,
            consensus_issues: Vec::new(),
            contested_issues: Vec::new(),
            rounds: rounds_done,
            final_verdict: CrossReviewVerdict::Approved,
            protocol_failure: None,
        });
    }

    Ok(CrossReviewResult {
        mode: CrossReviewMode::CrossModel,
        primary_agent_id: primary.id(),
        challenger_agent_id: Some(challenger_agent_id),
        primary_review,
        challenger_review,
        consensus_issues,
        contested_issues,
        rounds: rounds_done,
        final_verdict: CrossReviewVerdict::NotConverged,
        protocol_failure: None,
    })
}

/// Identity guard (GH-1767): a challenger resolving to the same agent
/// identity as the primary is no challenger at all — degrade instead of
/// "reviewing" with a single model twice.
fn distinct_challenger(
    primary: &Arc<dyn CodeAgent>,
    challenger: Option<Arc<dyn CodeAgent>>,
) -> Option<Arc<dyn CodeAgent>> {
    challenger.filter(|candidate| candidate.id() != primary.id())
}

/// True when any line carries one of the three protocol tag prefixes, even
/// with an empty body (which `extract_tagged` filters out).
fn has_any_tag_prefix(output: &str) -> bool {
    output.lines().map(str::trim).any(|line| {
        line.starts_with("CONFIRMED:")
            || line.starts_with("MISSED:")
            || line.starts_with("FALSE-POSITIVE:")
    })
}

fn bounded_excerpt(reply: &str) -> String {
    let mut chars = reply.chars();
    let mut excerpt: String = chars.by_ref().take(PROTOCOL_FAILURE_EXCERPT_MAX).collect();
    if chars.next().is_some() {
        excerpt.push('…');
    }
    excerpt
}

fn extract_tagged(output: &str, tag: &str) -> Vec<String> {
    let prefix = format!("{tag}:");
    output
        .lines()
        .filter_map(|l| {
            l.trim()
                .strip_prefix(prefix.as_str())
                .map(|s| s.trim().to_string())
        })
        .filter(|s| !s.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::agent::{AgentRequest, AgentResponse, CodeAgent, StreamItem};
    use harness_core::compress::{
        CompressError, CompressHint, Compressed, CompressorUsage, NapStatus, ObservationCompressor,
    };
    use harness_core::error::Result as HarnessResult;
    use harness_core::types::{Capability, TokenUsage};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Mutex;
    use tokio::sync::mpsc::Sender;

    struct PrimaryMock;

    #[async_trait::async_trait]
    impl CodeAgent for PrimaryMock {
        fn name(&self) -> &str {
            "primary"
        }
        fn capabilities(&self) -> Vec<Capability> {
            vec![]
        }
        async fn execute(&self, _req: AgentRequest) -> HarnessResult<AgentResponse> {
            Ok(AgentResponse {
                output: "ISSUE: Missing error handling\nISSUE: Unbounded loop".to_string(),
                stderr: String::new(),
                items: vec![],
                token_usage: TokenUsage::default(),
                model: "mock".to_string(),
                exit_code: Some(0),
            })
        }
        async fn execute_stream(
            &self,
            _req: AgentRequest,
            _tx: Sender<StreamItem>,
        ) -> HarnessResult<()> {
            Ok(())
        }
    }

    struct ChallengerMock;

    #[async_trait::async_trait]
    impl CodeAgent for ChallengerMock {
        fn name(&self) -> &str {
            "challenger"
        }
        fn capabilities(&self) -> Vec<Capability> {
            vec![]
        }
        async fn execute(&self, _req: AgentRequest) -> HarnessResult<AgentResponse> {
            Ok(AgentResponse {
                output: "CONFIRMED: Missing error handling\nFALSE-POSITIVE: Unbounded loop"
                    .to_string(),
                stderr: String::new(),
                items: vec![],
                token_usage: TokenUsage::default(),
                model: "mock".to_string(),
                exit_code: Some(0),
            })
        }
        async fn execute_stream(
            &self,
            _req: AgentRequest,
            _tx: Sender<StreamItem>,
        ) -> HarnessResult<()> {
            Ok(())
        }
    }

    struct CapturingChallenger {
        prompts: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl CodeAgent for CapturingChallenger {
        fn name(&self) -> &str {
            "capturing-challenger"
        }
        fn capabilities(&self) -> Vec<Capability> {
            vec![]
        }
        async fn execute(&self, req: AgentRequest) -> HarnessResult<AgentResponse> {
            self.prompts.lock().unwrap().push(req.prompt);
            Ok(AgentResponse {
                output: "FALSE-POSITIVE: Missing error handling".to_string(),
                stderr: String::new(),
                items: vec![],
                token_usage: TokenUsage::default(),
                model: "mock".to_string(),
                exit_code: Some(0),
            })
        }
        async fn execute_stream(
            &self,
            _req: AgentRequest,
            _tx: Sender<StreamItem>,
        ) -> HarnessResult<()> {
            Ok(())
        }
    }

    struct RecordingCompressor {
        calls: Arc<AtomicUsize>,
        persisted: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl ObservationCompressor for RecordingCompressor {
        async fn compress(
            &self,
            obs: &str,
            _hint: &CompressHint,
        ) -> Result<Compressed, CompressError> {
            assert!(self.persisted.load(Ordering::SeqCst));
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(Compressed {
                text: "COMPRESSED PRIMARY".to_string(),
                original_tokens: obs.len() as u32,
                compressed_tokens: 2,
                compressor_usage: CompressorUsage::default(),
                nap: NapStatus::SkippedSample,
            })
        }
    }

    struct RecordingSink {
        fail: bool,
        persisted: Arc<AtomicBool>,
        raw: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl RawObservationSink for RecordingSink {
        async fn persist_raw(
            &self,
            _task_id: &TaskId,
            _turn: u32,
            _artifact_type: &str,
            raw: &str,
        ) -> anyhow::Result<()> {
            if self.fail {
                anyhow::bail!("injected persistence failure");
            }
            self.raw.lock().unwrap().push(raw.to_string());
            self.persisted.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    struct LgtmMock;

    #[async_trait::async_trait]
    impl CodeAgent for LgtmMock {
        fn name(&self) -> &str {
            "lgtm"
        }
        fn capabilities(&self) -> Vec<Capability> {
            vec![]
        }
        async fn execute(&self, _req: AgentRequest) -> HarnessResult<AgentResponse> {
            Ok(AgentResponse {
                output: "LGTM".to_string(),
                stderr: String::new(),
                items: vec![],
                token_usage: TokenUsage::default(),
                model: "mock".to_string(),
                exit_code: Some(0),
            })
        }
        async fn execute_stream(
            &self,
            _req: AgentRequest,
            _tx: Sender<StreamItem>,
        ) -> HarnessResult<()> {
            Ok(())
        }
    }

    fn proj_dir() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("cr-test-")
            .tempdir()
            .expect("create temp dir")
    }

    #[tokio::test]
    async fn two_agents_extract_consensus_and_contested() {
        let proj = proj_dir();
        let result = run_cross_review(
            Arc::new(PrimaryMock),
            Some(Arc::new(ChallengerMock)),
            proj.path().to_path_buf(),
            "fn foo() {}".to_string(),
            3,
            None,
        )
        .await
        .expect("run_cross_review should succeed");

        assert_eq!(result.consensus_issues, vec!["Missing error handling"]);
        assert_eq!(result.contested_issues, vec!["Unbounded loop"]);
        assert_eq!(result.final_verdict, CrossReviewVerdict::NotConverged);
        assert_eq!(result.mode, CrossReviewMode::CrossModel);
        assert_eq!(result.primary_agent_id, "primary");
        assert_eq!(result.challenger_agent_id.as_deref(), Some("challenger"));
        assert!(result.protocol_failure.is_none());
        assert!(result.rounds >= 1);
    }

    #[tokio::test]
    async fn single_agent_graceful_degradation() {
        let proj = proj_dir();
        let result = run_cross_review(
            Arc::new(PrimaryMock),
            None,
            proj.path().to_path_buf(),
            "fn foo() {}".to_string(),
            3,
            None,
        )
        .await
        .expect("single-agent should succeed");

        assert_eq!(result.rounds, 1);
        assert_eq!(result.challenger_review, "");
        assert_eq!(
            result.consensus_issues,
            vec!["Missing error handling", "Unbounded loop"]
        );
        assert_eq!(result.final_verdict, CrossReviewVerdict::NotConverged);
        assert_eq!(result.mode, CrossReviewMode::SingleModelDegraded);
        assert_eq!(result.challenger_agent_id, None);
    }

    #[tokio::test]
    async fn approved_when_no_issues() {
        let proj = proj_dir();
        let result = run_cross_review(
            Arc::new(LgtmMock),
            None,
            proj.path().to_path_buf(),
            "fn foo() {}".to_string(),
            3,
            None,
        )
        .await
        .expect("lgtm path should succeed");

        // A clean single-model review degrades; it is never full approval.
        assert_eq!(result.final_verdict, CrossReviewVerdict::ApprovedDegraded);
        assert_eq!(result.mode, CrossReviewMode::SingleModelDegraded);
        assert!(result.consensus_issues.is_empty());
    }

    #[tokio::test]
    async fn live_task_persists_raw_before_compressing_challenger_input() {
        let proj = proj_dir();
        let task_id = TaskId::from_str("cross-review-success");
        let calls = Arc::new(AtomicUsize::new(0));
        let persisted = Arc::new(AtomicBool::new(false));
        let raw = Arc::new(Mutex::new(Vec::new()));
        let prompts = Arc::new(Mutex::new(Vec::new()));
        let session = crate::observation_compression::test_task_observation_session(Arc::new(
            RecordingCompressor {
                calls: Arc::clone(&calls),
                persisted: Arc::clone(&persisted),
            },
        ));
        let context = CrossReviewCompressionContext::new(
            task_id,
            7,
            session,
            Arc::new(RecordingSink {
                fail: false,
                persisted,
                raw: Arc::clone(&raw),
            }),
        );

        let result = run_cross_review_with_context(
            Arc::new(PrimaryMock),
            Some(Arc::new(CapturingChallenger {
                prompts: Arc::clone(&prompts),
            })),
            proj.path().to_path_buf(),
            "target".to_string(),
            2,
            Some(vec![]),
            Some(&context),
        )
        .await
        .unwrap();

        let expected_raw = "ISSUE: Missing error handling\nISSUE: Unbounded loop";
        assert_eq!(raw.lock().unwrap().as_slice(), &[expected_raw]);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(prompts.lock().unwrap()[0].contains("COMPRESSED PRIMARY"));
        assert_eq!(result.primary_review, expected_raw);
    }

    #[tokio::test]
    async fn persistence_failure_bypasses_compressor_and_injects_raw() {
        let proj = proj_dir();
        let task_id = TaskId::from_str("cross-review-persist-failure");
        let calls = Arc::new(AtomicUsize::new(0));
        let persisted = Arc::new(AtomicBool::new(false));
        let prompts = Arc::new(Mutex::new(Vec::new()));
        let session = crate::observation_compression::test_task_observation_session(Arc::new(
            RecordingCompressor {
                calls: Arc::clone(&calls),
                persisted: Arc::clone(&persisted),
            },
        ));
        let context = CrossReviewCompressionContext::new(
            task_id,
            8,
            session,
            Arc::new(RecordingSink {
                fail: true,
                persisted,
                raw: Arc::new(Mutex::new(Vec::new())),
            }),
        );

        let result = run_cross_review_with_context(
            Arc::new(PrimaryMock),
            Some(Arc::new(CapturingChallenger {
                prompts: Arc::clone(&prompts),
            })),
            proj.path().to_path_buf(),
            "target".to_string(),
            2,
            Some(vec![]),
            Some(&context),
        )
        .await
        .unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(prompts.lock().unwrap()[0].contains("ISSUE: Missing error handling"));
        assert_eq!(
            result.primary_review,
            "ISSUE: Missing error handling\nISSUE: Unbounded loop"
        );
    }

    #[tokio::test]
    async fn single_round_skips_unused_persistence_and_compression() {
        let proj = proj_dir();
        let task_id = TaskId::from_str("cross-review-single-round");
        let calls = Arc::new(AtomicUsize::new(0));
        let persisted = Arc::new(AtomicBool::new(false));
        let raw = Arc::new(Mutex::new(Vec::new()));
        let prompts = Arc::new(Mutex::new(Vec::new()));
        let session = crate::observation_compression::test_task_observation_session(Arc::new(
            RecordingCompressor {
                calls: Arc::clone(&calls),
                persisted: Arc::clone(&persisted),
            },
        ));
        let context = CrossReviewCompressionContext::new(
            task_id,
            9,
            session,
            Arc::new(RecordingSink {
                fail: false,
                persisted,
                raw: Arc::clone(&raw),
            }),
        );

        run_cross_review_with_context(
            Arc::new(PrimaryMock),
            Some(Arc::new(CapturingChallenger {
                prompts: Arc::clone(&prompts),
            })),
            proj.path().to_path_buf(),
            "target".to_string(),
            1,
            Some(vec![]),
            Some(&context),
        )
        .await
        .unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(raw.lock().unwrap().is_empty());
        assert!(prompts.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn raw_wrapper_keeps_challenger_input_uncompressed() {
        let proj = proj_dir();
        let prompts = Arc::new(Mutex::new(Vec::new()));

        let result = run_cross_review(
            Arc::new(PrimaryMock),
            Some(Arc::new(CapturingChallenger {
                prompts: Arc::clone(&prompts),
            })),
            proj.path().to_path_buf(),
            "target".to_string(),
            2,
            Some(vec![]),
        )
        .await
        .unwrap();

        assert!(prompts.lock().unwrap()[0].contains("ISSUE: Missing error handling"));
        assert_eq!(
            result.primary_review,
            "ISSUE: Missing error handling\nISSUE: Unbounded loop"
        );
    }
    struct TaglessChallenger;

    #[async_trait::async_trait]
    impl CodeAgent for TaglessChallenger {
        fn name(&self) -> &str {
            "tagless-challenger"
        }
        fn capabilities(&self) -> Vec<Capability> {
            vec![]
        }
        async fn execute(&self, _req: AgentRequest) -> HarnessResult<AgentResponse> {
            Ok(AgentResponse {
                output: "The primary review looks mostly fine to me.".to_string(),
                stderr: String::new(),
                items: vec![],
                token_usage: TokenUsage::default(),
                model: "mock".to_string(),
                exit_code: Some(0),
            })
        }
        async fn execute_stream(
            &self,
            _req: AgentRequest,
            _tx: Sender<StreamItem>,
        ) -> HarnessResult<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn tagless_challenger_reply_is_protocol_failure_not_approval() {
        let proj = proj_dir();
        let result = run_cross_review(
            Arc::new(PrimaryMock),
            Some(Arc::new(TaglessChallenger)),
            proj.path().to_path_buf(),
            "fn foo() {}".to_string(),
            3,
            None,
        )
        .await
        .expect("protocol failure is a verdict, not an error");

        assert_eq!(result.final_verdict, CrossReviewVerdict::ProtocolFailure);
        assert_eq!(result.mode, CrossReviewMode::CrossModel);
        let failure = result.protocol_failure.expect("failure detail recorded");
        assert_eq!(failure.round, 2);
        assert!(failure.excerpt.contains("looks mostly fine"));
        assert!(result.consensus_issues.is_empty());
    }

    #[tokio::test]
    async fn false_positive_only_reply_is_a_valid_approving_round() {
        let proj = proj_dir();
        let result = run_cross_review(
            Arc::new(PrimaryMock),
            Some(Arc::new(CapturingChallenger {
                prompts: Arc::new(Mutex::new(Vec::new())),
            })),
            proj.path().to_path_buf(),
            "fn foo() {}".to_string(),
            3,
            None,
        )
        .await
        .expect("false-positive-only round should succeed");

        assert_eq!(result.final_verdict, CrossReviewVerdict::Approved);
        assert_eq!(result.contested_issues, vec!["Missing error handling"]);
        assert!(result.protocol_failure.is_none());
    }

    #[tokio::test]
    async fn single_round_with_challenger_is_not_a_protocol_failure() {
        let proj = proj_dir();
        let result = run_cross_review(
            Arc::new(PrimaryMock),
            Some(Arc::new(ChallengerMock)),
            proj.path().to_path_buf(),
            "fn foo() {}".to_string(),
            1,
            None,
        )
        .await
        .expect("single-round review should succeed");

        assert_eq!(result.rounds, 1);
        assert_eq!(result.final_verdict, CrossReviewVerdict::NotConverged);
        assert!(result.protocol_failure.is_none());
    }

    #[test]
    fn identity_guard_drops_same_identity_challenger() {
        let primary: Arc<dyn CodeAgent> = Arc::new(PrimaryMock);
        let same: Arc<dyn CodeAgent> = Arc::new(PrimaryMock);
        let distinct: Arc<dyn CodeAgent> = Arc::new(ChallengerMock);

        assert!(distinct_challenger(&primary, Some(same)).is_none());
        assert!(distinct_challenger(&primary, Some(distinct)).is_some());
        assert!(distinct_challenger(&primary, None).is_none());
    }

    #[test]
    fn verdict_serializes_as_legacy_uppercase_strings() {
        assert_eq!(
            serde_json::to_value(CrossReviewVerdict::Approved).unwrap(),
            "APPROVED"
        );
        assert_eq!(
            serde_json::to_value(CrossReviewVerdict::ApprovedDegraded).unwrap(),
            "APPROVED_DEGRADED"
        );
        assert_eq!(
            serde_json::to_value(CrossReviewVerdict::NotConverged).unwrap(),
            "NOT_CONVERGED"
        );
        assert_eq!(
            serde_json::to_value(CrossReviewVerdict::ProtocolFailure).unwrap(),
            "PROTOCOL_FAILURE"
        );
    }
}
