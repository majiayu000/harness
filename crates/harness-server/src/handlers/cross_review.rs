use crate::observation_compression::{
    compress_observation_for_prompt, RawObservationSink, TaskObservationCompressionSession,
};
use crate::task_runner::TaskId;
use crate::{http::AppState, validate_root};
use harness_core::{agent::AgentRequest, agent::CodeAgent, config::HarnessConfig};
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

    let result = match run_cross_review(
        primary,
        challenger,
        project_root,
        target,
        rounds,
        None,
        &state.core.server.config,
    )
    .await
    {
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
/// - `None`        → Use the operator-configured capability profile.
/// - `Some(tools)` → Restricted to the listed tools. Pass `Some(vec![])` to deny all tools
///   (read-only text review where all content is in the prompt).
pub async fn run_cross_review(
    primary: Arc<dyn CodeAgent>,
    challenger: Option<Arc<dyn CodeAgent>>,
    project_root: PathBuf,
    target: String,
    max_rounds: u32,
    allowed_tools: Option<Vec<String>>,
    config: &HarnessConfig,
) -> Result<CrossReviewResult, String> {
    run_cross_review_with_context(
        primary,
        challenger,
        project_root,
        target,
        max_rounds,
        allowed_tools,
        None,
        config,
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
    config: &HarnessConfig,
) -> Result<CrossReviewResult, String> {
    let safe_target = harness_core::prompts::wrap_external_data(&target);
    let primary_prompt = harness_core::prompts::cross_review::primary_review_prompt(&safe_target);

    let primary_request =
        configured_review_request(config, primary_prompt, project_root.clone(), &allowed_tools);
    let primary_resp = primary
        .execute(primary_request)
        .await
        .map_err(|e| e.to_string())?;
    let primary_review = primary_resp.output;

    let challenger = match challenger {
        None => {
            // Degraded single-model review: treat all ISSUE lines as
            // consensus, and never report a clean review as fully approved.
            let consensus_issues =
                harness_agents::output_parsing::extract_review_issues(&primary_review);
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
        harness_agents::output_parsing::extract_review_issues(&primary_review);
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

        let challenger_request = configured_review_request(
            config,
            challenge_prompt,
            project_root.clone(),
            &allowed_tools,
        );
        let resp = challenger
            .execute(challenger_request)
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

fn configured_review_request(
    config: &HarnessConfig,
    prompt: String,
    project_root: PathBuf,
    allowed_tools: &Option<Vec<String>>,
) -> AgentRequest {
    let mut request = AgentRequest {
        prompt,
        project_root,
        ..Default::default()
    };
    request.apply_configured_policy(config);
    if let Some(tools) = allowed_tools {
        request.allowed_tools = Some(tools.clone());
    }
    request
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
#[path = "cross_review_tests.rs"]
mod tests;
