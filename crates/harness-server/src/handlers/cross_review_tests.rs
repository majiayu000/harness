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
            output: "CONFIRMED: Missing error handling\nFALSE-POSITIVE: Unbounded loop".to_string(),
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
    async fn compress(&self, obs: &str, _hint: &CompressHint) -> Result<Compressed, CompressError> {
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

#[test]
fn configured_review_request_preserves_policy_and_explicit_tool_scope() {
    let mut config = HarnessConfig::default();
    config.agents.capability_profile = harness_core::config::agents::CapabilityProfile::Full;
    config.isolation.default_tier = harness_core::config::isolation::IsolationTier::Container;
    config.isolation.network_allowlist = vec!["api.anthropic.com".to_string()];

    let request = configured_review_request(
        &config,
        "review".to_string(),
        PathBuf::from("/tmp/project"),
        &Some(Vec::new()),
    );

    assert_eq!(
        request.permission_mode,
        harness_core::config::agents::AgentPermissionMode::Full
    );
    assert_eq!(request.allowed_tools, Some(Vec::new()));
    assert_eq!(
        request
            .env_vars
            .get(harness_core::agent::AGENT_ISOLATION_TIER_ENV),
        Some(&"container".to_string())
    );
    assert_eq!(
        request
            .env_vars
            .get(harness_core::agent::AGENT_NETWORK_ALLOWLIST_ENV),
        Some(&"api.anthropic.com".to_string())
    );
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
        &HarnessConfig::default(),
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
        &HarnessConfig::default(),
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
        &HarnessConfig::default(),
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
        &HarnessConfig::default(),
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
        &HarnessConfig::default(),
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
        &HarnessConfig::default(),
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
        &HarnessConfig::default(),
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
        &HarnessConfig::default(),
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
        &HarnessConfig::default(),
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
        &HarnessConfig::default(),
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
