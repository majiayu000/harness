use super::agent_review::jaccard_word_similarity;
use super::helpers::{
    build_task_event, run_agent_streaming, run_on_error, run_post_execute, run_pre_execute,
    telemetry_for_timeout, update_status,
};
use crate::task_runner::{
    mutate_and_persist, CreateTaskRequest, RoundResult, TaskId, TaskStatus, TaskStore,
};
use chrono::{DateTime, Utc};
use harness_core::agent::{AgentRequest, AgentResponse, CodeAgent};
use harness_core::prompts;
use harness_core::tool_isolation::validate_tool_usage;
use harness_core::types::{Decision, ExecutionPhase, TurnFailure, TurnFailureKind};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::time::{sleep, Duration, Instant};

/// External state of a PR as observed via GitHub. Used to short-circuit the
/// review loop when a PR has been merged or closed outside of this task so the
/// loop does not keep invoking the reviewer agent against stale work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrExternalState {
    Open,
    Merged,
    Closed,
    Unknown,
}

#[derive(Debug, Deserialize)]
struct GitHubPullState {
    state: String,
    merged_at: Option<String>,
}

/// Query the GitHub REST API with a 10 s timeout and classify the result.
/// Returns [`PrExternalState::Unknown`] on any transient
/// failure so callers do not abort a healthy review loop because of a flaky
/// network call.
async fn fetch_pr_external_state(
    pr_num: u64,
    project: &Path,
    github_token: Option<&str>,
) -> PrExternalState {
    let Some(repo) = super::pr_detection::detect_repo_slug(project).await else {
        tracing::debug!(
            pr = pr_num,
            "PR state check skipped because repository slug is unavailable"
        );
        return PrExternalState::Unknown;
    };
    let client = reqwest::Client::new();
    let mut request = client
        .get(format!(
            "https://api.github.com/repos/{repo}/pulls/{pr_num}"
        ))
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .header(reqwest::header::USER_AGENT, "harness-server");
    if let Some(token) = crate::github_auth::resolve_github_token(github_token) {
        request = request.bearer_auth(token);
    }
    let response = match tokio::time::timeout(Duration::from_secs(10), request.send()).await {
        Ok(Ok(response)) if response.status().is_success() => response,
        Ok(Ok(response)) => {
            tracing::debug!(pr = pr_num, status = %response.status(), "GitHub PR state check failed");
            return PrExternalState::Unknown;
        }
        Ok(Err(e)) => {
            tracing::debug!(pr = pr_num, error = %e, "GitHub PR state check invocation error");
            return PrExternalState::Unknown;
        }
        Err(_) => {
            tracing::debug!(pr = pr_num, "GitHub PR state check timed out after 10s");
            return PrExternalState::Unknown;
        }
    };
    let Ok(state) = response.json::<GitHubPullState>().await else {
        tracing::debug!(pr = pr_num, "GitHub PR state response was not parseable");
        return PrExternalState::Unknown;
    };
    let merged_at_empty = state.merged_at.as_deref().unwrap_or("").trim().is_empty();
    match (state.state.as_str(), merged_at_empty) {
        ("open", _) => PrExternalState::Open,
        ("merged", _) => PrExternalState::Merged,
        // GitHub occasionally reports merged PRs as CLOSED with a non-empty
        // `mergedAt`; treat that as a merge so we do not cancel a completed
        // task by mistake.
        ("closed", false) => PrExternalState::Merged,
        ("closed", true) => PrExternalState::Closed,
        _ => PrExternalState::Unknown,
    }
}

/// Returns `true` when the trailing three entries of `counts` are all `Some`
/// and the issue count is not decreasing (`c >= a && c >= b`).
///
/// Used to detect convergence failure in the review loop so the caller can
/// switch to impasse (critical-only) mode.
fn issue_count_not_decreasing(counts: &[Option<u32>]) -> bool {
    if counts.len() < 3 {
        return false;
    }
    let tail = &counts[counts.len() - 3..];
    matches!(tail, [Some(a), Some(b), Some(c)] if c >= a && c >= b)
}

const MAX_CONSECUTIVE_WAITS: u32 = 10;
const CODEX_REVIEWER_NAME: &str = "chatgpt-codex-connector[bot]";
const CODEX_REVIEW_COMMAND: &str = "@codex";
const CODEX_APPROVAL_SIGNATURE: &str = "no major issues";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ReviewBotKey {
    Gemini,
    Codex,
}

impl ReviewBotKey {
    fn as_str(self) -> &'static str {
        match self {
            Self::Gemini => "gemini",
            Self::Codex => "codex",
        }
    }
}

#[derive(Debug, Clone)]
struct ReviewBotDescriptor {
    key: ReviewBotKey,
    reviewer_name: String,
    review_command: String,
}

impl ReviewBotDescriptor {
    fn from_chain_entry(
        entry: &str,
        review_config: &harness_core::config::agents::AgentReviewConfig,
    ) -> Option<Self> {
        match entry.trim().to_ascii_lowercase().as_str() {
            "gemini" => Some(Self {
                key: ReviewBotKey::Gemini,
                reviewer_name: review_config.reviewer_name.clone(),
                review_command: review_config.review_bot_command.clone(),
            }),
            "codex" => Some(Self {
                key: ReviewBotKey::Codex,
                reviewer_name: CODEX_REVIEWER_NAME.to_string(),
                review_command: CODEX_REVIEW_COMMAND.to_string(),
            }),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct GitHubGraphQlResponse<T> {
    data: Option<T>,
}

#[derive(Debug, Clone, Deserialize)]
struct PullRequestQueryData {
    repository: Option<PullRequestRepository>,
}

#[derive(Debug, Clone, Deserialize)]
struct PullRequestRepository {
    #[serde(rename = "pullRequest")]
    pull_request: Option<PullRequestGraphQl>,
}

#[derive(Debug, Clone, Deserialize)]
struct PullRequestGraphQl {
    commits: GraphQlConnection<CommitNode>,
    reviews: GraphQlConnection<ReviewNode>,
    comments: GraphQlConnection<CommentNode>,
    #[serde(rename = "reviewThreads")]
    review_threads: GraphQlConnection<ReviewThreadNode>,
}

#[derive(Debug, Clone, Deserialize)]
struct GraphQlConnection<T> {
    nodes: Vec<T>,
}

#[derive(Debug, Clone, Deserialize)]
struct CommitNode {
    commit: CommitDetails,
}

#[derive(Debug, Clone, Deserialize)]
struct CommitDetails {
    #[serde(rename = "committedDate")]
    committed_date: DateTime<Utc>,
    #[serde(rename = "statusCheckRollup")]
    status_check_rollup: Option<StatusCheckRollup>,
}

#[derive(Debug, Clone, Deserialize)]
struct StatusCheckRollup {
    state: String,
}

#[derive(Debug, Clone, Deserialize)]
struct ReviewNode {
    author: Option<ActorNode>,
    body: String,
    #[serde(rename = "submittedAt")]
    submitted_at: Option<DateTime<Utc>>,
    state: String,
}

#[derive(Debug, Clone, Deserialize)]
struct CommentNode {
    author: Option<ActorNode>,
    body: String,
    #[serde(rename = "createdAt")]
    created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Deserialize)]
struct ReviewThreadNode {
    #[serde(rename = "isResolved")]
    is_resolved: bool,
    #[serde(rename = "isOutdated")]
    is_outdated: bool,
    comments: GraphQlConnection<ThreadCommentNode>,
}

#[derive(Debug, Clone, Deserialize)]
struct ThreadCommentNode {
    author: Option<ActorNode>,
    body: String,
    #[serde(rename = "publishedAt")]
    published_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Deserialize)]
struct ActorNode {
    login: String,
}

#[derive(Debug, Clone)]
struct PullRequestSignals {
    latest_commit_at: DateTime<Utc>,
    ci_green: bool,
    latest_bot_activity_at: Option<DateTime<Utc>>,
    blocking_feedback: bool,
    bots: HashMap<ReviewBotKey, BotSignals>,
}

#[derive(Debug, Clone, Default)]
struct BotSignals {
    latest_review_at: Option<DateTime<Utc>>,
    latest_review_state: Option<String>,
    latest_review_body: Option<String>,
    latest_comment_at: Option<DateTime<Utc>>,
    latest_comment_body: Option<String>,
    reviewed_any_commit: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BotClassification {
    FreshApproval,
    ActionableFeedback,
    QuotaExhausted,
    Silent,
    NeverReviewed,
}

#[derive(Debug, Clone)]
struct ReviewFallbackState {
    tier: harness_workflow::issue_lifecycle::ReviewFallbackTier,
    trigger: harness_workflow::issue_lifecycle::ReviewFallbackTrigger,
    active_bot: Option<ReviewBotKey>,
}

#[derive(Debug, Clone)]
struct ReviewLoopDecision {
    active_bot: ReviewBotDescriptor,
    fallback: Option<ReviewFallbackState>,
    wait_for_bot: bool,
}

fn bot_fallback_chain(
    review_config: &harness_core::config::agents::AgentReviewConfig,
) -> Vec<ReviewBotDescriptor> {
    let mut chain: Vec<ReviewBotDescriptor> = review_config
        .fallback_chain
        .iter()
        .filter_map(|entry| ReviewBotDescriptor::from_chain_entry(entry, review_config))
        .collect();
    if chain.is_empty() {
        chain.push(ReviewBotDescriptor {
            key: ReviewBotKey::Gemini,
            reviewer_name: review_config.reviewer_name.clone(),
            review_command: review_config.review_bot_command.clone(),
        });
    }
    chain
}

fn bot_quota_exhausted(bot: ReviewBotKey, body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    match bot {
        ReviewBotKey::Gemini => prompts::is_quota_exhausted(body),
        ReviewBotKey::Codex => lower.contains("codex usage limits have been reached"),
    }
}

fn bot_approval_signal(bot: ReviewBotKey, body: &str) -> bool {
    match bot {
        ReviewBotKey::Gemini => false,
        ReviewBotKey::Codex => body.to_ascii_lowercase().contains(CODEX_APPROVAL_SIGNATURE),
    }
}

fn blocking_review_thread(thread: &ReviewThreadNode) -> bool {
    if thread.is_resolved || thread.is_outdated {
        return false;
    }
    thread.comments.nodes.iter().any(|comment| {
        let body = comment.body.to_ascii_lowercase();
        body.contains("critical") || body.contains("high")
    })
}

fn latest_comment_body_after_commit(
    bot: &BotSignals,
    latest_commit_at: DateTime<Utc>,
) -> Option<&str> {
    match (bot.latest_comment_at, bot.latest_comment_body.as_deref()) {
        (Some(at), Some(body)) if at >= latest_commit_at => Some(body),
        _ => None,
    }
}

fn classify_bot(
    descriptor: &ReviewBotDescriptor,
    bot: &BotSignals,
    latest_commit_at: DateTime<Utc>,
    ci_green: bool,
    blocking_feedback: bool,
    silence_rounds: u32,
    silence_rounds_threshold: u32,
    silence_min_minutes_after_commit: u32,
) -> BotClassification {
    if let Some(review_at) = bot.latest_review_at {
        if review_at >= latest_commit_at {
            let review_state = bot.latest_review_state.as_deref().unwrap_or_default();
            if review_state == "APPROVED"
                || bot
                    .latest_review_body
                    .as_deref()
                    .is_some_and(|body| bot_approval_signal(descriptor.key, body))
            {
                return BotClassification::FreshApproval;
            }
            if bot
                .latest_review_body
                .as_deref()
                .is_some_and(|body| bot_quota_exhausted(descriptor.key, body))
            {
                return BotClassification::QuotaExhausted;
            }
            return BotClassification::ActionableFeedback;
        }
    }
    if let Some(body) = latest_comment_body_after_commit(bot, latest_commit_at) {
        if bot_quota_exhausted(descriptor.key, body) {
            return BotClassification::QuotaExhausted;
        }
        if bot_approval_signal(descriptor.key, body) {
            return BotClassification::FreshApproval;
        }
        return BotClassification::ActionableFeedback;
    }
    if !bot.reviewed_any_commit {
        return BotClassification::NeverReviewed;
    }
    let commit_age_minutes = (Utc::now() - latest_commit_at).num_minutes();
    if silence_rounds >= silence_rounds_threshold
        && commit_age_minutes >= i64::from(silence_min_minutes_after_commit)
        && ci_green
        && !blocking_feedback
    {
        return BotClassification::Silent;
    }
    BotClassification::ActionableFeedback
}

async fn fetch_pull_request_signals(
    repo_slug: &str,
    pr_num: u64,
    github_token: Option<&str>,
) -> anyhow::Result<PullRequestSignals> {
    let Some((owner, repo)) = repo_slug.split_once('/') else {
        anyhow::bail!("invalid repo slug for review loop: {repo_slug}");
    };
    let query = r#"
        query ReviewLoopSignals($owner: String!, $repo: String!, $pr: Int!) {
          repository(owner: $owner, name: $repo) {
            pullRequest(number: $pr) {
              commits(last: 1) {
                nodes {
                  commit {
                    committedDate
                    statusCheckRollup {
                      state
                    }
                  }
                }
              }
              reviews(last: 50) {
                nodes {
                  author { login }
                  body
                  submittedAt
                  state
                }
              }
              comments(last: 50) {
                nodes {
                  author { login }
                  body
                  createdAt
                }
              }
              reviewThreads(last: 50) {
                nodes {
                  isResolved
                  isOutdated
                  comments(last: 10) {
                    nodes {
                      author { login }
                      body
                      publishedAt
                    }
                  }
                }
              }
            }
          }
        }
    "#;

    let client = reqwest::Client::new();
    let mut request = client
        .post("https://api.github.com/graphql")
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .header(reqwest::header::USER_AGENT, "harness-server")
        .json(&serde_json::json!({
            "query": query,
            "variables": {
                "owner": owner,
                "repo": repo,
                "pr": pr_num as i64,
            }
        }));
    if let Some(token) = crate::github_auth::resolve_github_token(github_token) {
        request = request.bearer_auth(token);
    }
    let response = tokio::time::timeout(Duration::from_secs(10), request.send()).await??;
    if !response.status().is_success() {
        anyhow::bail!(
            "GitHub review signal query failed with status {}",
            response.status()
        );
    }
    let parsed = response
        .json::<GitHubGraphQlResponse<PullRequestQueryData>>()
        .await?;
    let pr = parsed
        .data
        .and_then(|data| data.repository)
        .and_then(|repo| repo.pull_request)
        .ok_or_else(|| anyhow::anyhow!("GitHub review signal query returned no PR data"))?;
    let latest_commit = pr
        .commits
        .nodes
        .last()
        .ok_or_else(|| anyhow::anyhow!("PR has no commits"))?;
    let latest_commit_at = latest_commit.commit.committed_date;
    let ci_green = latest_commit
        .commit
        .status_check_rollup
        .as_ref()
        .is_some_and(|rollup| rollup.state == "SUCCESS");

    let mut bots = HashMap::from([
        (ReviewBotKey::Gemini, BotSignals::default()),
        (ReviewBotKey::Codex, BotSignals::default()),
    ]);

    for review in pr.reviews.nodes {
        let Some(author) = review.author.as_ref().map(|author| author.login.as_str()) else {
            continue;
        };
        let key = if author == CODEX_REVIEWER_NAME {
            ReviewBotKey::Codex
        } else if author.ends_with("[bot]") {
            ReviewBotKey::Gemini
        } else {
            continue;
        };
        let entry = bots.entry(key).or_default();
        entry.reviewed_any_commit = true;
        if let Some(submitted_at) = review.submitted_at {
            let replace = entry
                .latest_review_at
                .map(|current| submitted_at >= current)
                .unwrap_or(true);
            if replace {
                entry.latest_review_at = Some(submitted_at);
                entry.latest_review_state = Some(review.state);
                entry.latest_review_body = Some(review.body);
            }
        }
    }

    for comment in pr.comments.nodes {
        let Some(author) = comment.author.as_ref().map(|author| author.login.as_str()) else {
            continue;
        };
        let key = match author {
            CODEX_REVIEWER_NAME => Some(ReviewBotKey::Codex),
            _ if author.ends_with("[bot]") => Some(ReviewBotKey::Gemini),
            _ => None,
        };
        let Some(key) = key else { continue };
        let entry = bots.entry(key).or_default();
        let replace = entry
            .latest_comment_at
            .map(|current| comment.created_at >= current)
            .unwrap_or(true);
        if replace {
            entry.latest_comment_at = Some(comment.created_at);
            entry.latest_comment_body = Some(comment.body);
        }
    }

    let blocking_feedback = pr.review_threads.nodes.iter().any(blocking_review_thread);
    let mut latest_bot_activity_at: Option<DateTime<Utc>> = None;
    for thread in pr.review_threads.nodes {
        for comment in thread.comments.nodes {
            let Some(author) = comment.author.as_ref().map(|author| author.login.as_str()) else {
                continue;
            };
            if !author.ends_with("[bot]") {
                continue;
            }
            let published_at = comment.published_at.unwrap_or(latest_commit_at);
            latest_bot_activity_at = Some(
                latest_bot_activity_at
                    .map(|current: DateTime<Utc>| current.max(published_at))
                    .unwrap_or(published_at),
            );
        }
    }
    for bot in bots.values() {
        if let Some(at) = bot.latest_review_at.or(bot.latest_comment_at) {
            latest_bot_activity_at = Some(
                latest_bot_activity_at
                    .map(|current: DateTime<Utc>| current.max(at))
                    .unwrap_or(at),
            );
        }
    }

    Ok(PullRequestSignals {
        latest_commit_at,
        ci_green,
        latest_bot_activity_at,
        blocking_feedback,
        bots,
    })
}

async fn post_review_bot_comment(
    repo_slug: &str,
    pr_num: u64,
    body: &str,
    github_token: Option<&str>,
) -> anyhow::Result<()> {
    let Some((owner, repo)) = repo_slug.split_once('/') else {
        anyhow::bail!("invalid repo slug for review bot comment: {repo_slug}");
    };
    let mut request = reqwest::Client::new()
        .post(format!(
            "https://api.github.com/repos/{owner}/{repo}/issues/{pr_num}/comments"
        ))
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .header(reqwest::header::USER_AGENT, "harness-server")
        .json(&serde_json::json!({ "body": body }));
    if let Some(token) = crate::github_auth::resolve_github_token(github_token) {
        request = request.bearer_auth(token);
    }
    let response = tokio::time::timeout(Duration::from_secs(10), request.send()).await??;
    if response.status().is_success() {
        Ok(())
    } else {
        anyhow::bail!(
            "GitHub review bot comment failed with status {}",
            response.status()
        )
    }
}

fn decide_review_loop_action(
    chain: &[ReviewBotDescriptor],
    signals: &PullRequestSignals,
    silence_rounds: u32,
    review_config: &harness_core::config::agents::AgentReviewConfig,
) -> anyhow::Result<ReviewLoopDecision> {
    let primary = chain
        .first()
        .ok_or_else(|| anyhow::anyhow!("review fallback chain is empty"))?;
    let primary_signals = signals.bots.get(&primary.key).cloned().unwrap_or_default();
    let primary_classification = classify_bot(
        primary,
        &primary_signals,
        signals.latest_commit_at,
        signals.ci_green,
        signals.blocking_feedback,
        silence_rounds,
        review_config.silence_rounds_threshold,
        review_config.silence_min_minutes_after_commit,
    );
    if primary_classification != BotClassification::QuotaExhausted {
        return Ok(ReviewLoopDecision {
            active_bot: primary.clone(),
            fallback: None,
            wait_for_bot: primary_classification == BotClassification::NeverReviewed,
        });
    }

    let secondary = chain
        .iter()
        .skip(1)
        .find(|bot| bot.key == ReviewBotKey::Codex)
        .cloned()
        .unwrap_or_else(|| primary.clone());
    let secondary_key = secondary.key;
    let secondary_signals = signals
        .bots
        .get(&secondary.key)
        .cloned()
        .unwrap_or_default();
    let secondary_classification = classify_bot(
        &secondary,
        &secondary_signals,
        signals.latest_commit_at,
        signals.ci_green,
        signals.blocking_feedback,
        silence_rounds,
        review_config.silence_rounds_threshold,
        review_config.silence_min_minutes_after_commit,
    );
    let fallback_b = Some(ReviewFallbackState {
        tier: harness_workflow::issue_lifecycle::ReviewFallbackTier::B,
        trigger: harness_workflow::issue_lifecycle::ReviewFallbackTrigger::GeminiQuota,
        active_bot: Some(secondary.key),
    });
    match secondary_classification {
        BotClassification::QuotaExhausted => Ok(ReviewLoopDecision {
            active_bot: secondary,
            fallback: Some(ReviewFallbackState {
                tier: harness_workflow::issue_lifecycle::ReviewFallbackTier::C,
                trigger: harness_workflow::issue_lifecycle::ReviewFallbackTrigger::AllBotsQuota,
                active_bot: Some(secondary_key),
            }),
            wait_for_bot: false,
        }),
        BotClassification::Silent => Ok(ReviewLoopDecision {
            active_bot: secondary,
            fallback: Some(ReviewFallbackState {
                tier: harness_workflow::issue_lifecycle::ReviewFallbackTier::C,
                trigger: harness_workflow::issue_lifecycle::ReviewFallbackTrigger::Silence,
                active_bot: Some(secondary_key),
            }),
            wait_for_bot: false,
        }),
        BotClassification::NeverReviewed => Ok(ReviewLoopDecision {
            active_bot: secondary,
            fallback: fallback_b,
            wait_for_bot: true,
        }),
        _ => Ok(ReviewLoopDecision {
            active_bot: secondary,
            fallback: fallback_b,
            wait_for_bot: false,
        }),
    }
}

fn update_silence_rounds(
    current_rounds: u32,
    last_activity_at: Option<DateTime<Utc>>,
    next_activity_at: Option<DateTime<Utc>>,
) -> (u32, Option<DateTime<Utc>>) {
    if next_activity_at != last_activity_at {
        (0, next_activity_at)
    } else {
        (current_rounds.saturating_add(1), last_activity_at)
    }
}

/// Execute the external review bot wait loop.
///
/// Polls the PR for review bot feedback, handles LGTM/FIXED/WAITING responses,
/// runs the test gate on LGTM acceptance, and manages convergence tracking.
/// Returns `Ok(())` when the task is complete (status already persisted).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_review_loop(
    store: &TaskStore,
    task_id: &TaskId,
    agent: &dyn CodeAgent,
    review_config: &harness_core::config::agents::AgentReviewConfig,
    project_config: &harness_core::config::project::ProjectConfig,
    issue_workflow_store: Option<&harness_workflow::issue_lifecycle::IssueWorkflowStore>,
    project_root: &Path,
    req: &CreateTaskRequest,
    events: &Arc<harness_observe::event_store::EventStore>,
    interceptors: &Arc<Vec<Arc<dyn harness_core::interceptor::TurnInterceptor>>>,
    context_items: &[harness_core::types::ContextItem],
    project: &Path,
    cargo_env: &HashMap<String, String>,
    pr_url: Option<String>,
    pr_num: u64,
    effective_max_turns: Option<u32>,
    _effective_max_rounds: u32,
    wait_secs: u64,
    max_rounds: u32,
    agent_pushed_commit: bool,
    rebase_pushed: bool,
    turn_timeout: Duration,
    turns_used: &mut u32,
    turns_used_acc: &mut u32,
    task_start: Instant,
    repo_slug: String,
    jaccard_threshold: f64,
    github_token: Option<&str>,
) -> anyhow::Result<()> {
    let review_phase_start = Instant::now();

    // `prev_fixed` tracks whether a commit was pushed that hasn't been re-reviewed yet.
    // Starts true if the implementation phase pushed a commit, and is set to true again
    // whenever a review round produces FIXED (agent commits + pushes). This drives the
    // freshness check in every round after a fix commit, not just round 1.
    let mut prev_fixed = agent_pushed_commit || rebase_pushed;

    // Convergence tracking: detect when issue count stops decreasing.
    let mut issue_counts: Vec<Option<u32>> = Vec::new();
    let mut impasse = false;

    // Carries test gate failure output from a rejected LGTM into the next review round.
    let mut pending_test_failure: Option<String> = None;
    // Issue 2 fix: track whether any LGTM was ever rejected by the test gate
    // without a subsequent clean LGTM+pass. `pending_test_failure.take()` clears
    // the Option at the start of each round (to avoid re-showing the same output),
    // but the graduation check must still know a test gate rejection occurred.
    let mut lgtm_test_gate_rejected = false;

    // Tracks the most recent non-waiting review output for Jaccard loop detection.
    let mut prev_review_output: Option<String> = None;

    let mut silence_rounds = 0u32;
    let mut last_bot_activity_at = None;
    let bot_chain = bot_fallback_chain(review_config);

    // Review loop.
    // Use an explicit counter so WAITING responses don't consume a round — `continue`
    // inside a `for` loop would silently advance the iterator even without a real review.
    let mut round: u32 = 1;
    let mut waiting_count: u32 = 1;
    while round <= max_rounds {
        // Cheap pre-flight: check the PR's external state before invoking the
        // reviewer agent. If the PR has already been merged or closed outside
        // of this task (operator merged manually, another agent closed it,
        // webhook finalised it) the remaining rounds would only burn tokens
        // against stale work — short-circuit to the appropriate terminal
        // status. Unknown/transient failures fall through and continue the
        // normal review flow.
        match fetch_pr_external_state(pr_num, project, github_token).await {
            PrExternalState::Merged => {
                tracing::info!(
                    task_id = %task_id,
                    pr = pr_num,
                    round,
                    "review loop exit: PR merged externally"
                );
                mutate_and_persist(store, task_id, |s| {
                    s.status = TaskStatus::Done;
                    s.turn = round;
                    s.error = Some(
                        "PR merged externally; review loop exited without waiting for LGTM"
                            .to_string(),
                    );
                })
                .await?;
                store.log_event(crate::event_replay::TaskEvent::Completed {
                    task_id: task_id.0.clone(),
                    ts: crate::event_replay::now_ts(),
                });
                tracing::info!(
                    task_id = %task_id,
                    phase = "reviewing",
                    elapsed_secs = review_phase_start.elapsed().as_secs(),
                    "phase_completed"
                );
                tracing::info!(
                    task_id = %task_id,
                    status = "done",
                    turns = round,
                    pr_url = pr_url.as_deref().unwrap_or(""),
                    total_elapsed_secs = task_start.elapsed().as_secs(),
                    "task_completed"
                );
                return Ok(());
            }
            PrExternalState::Closed => {
                tracing::info!(
                    task_id = %task_id,
                    pr = pr_num,
                    round,
                    "review loop exit: PR closed externally without merge"
                );
                mutate_and_persist(store, task_id, |s| {
                    s.status = TaskStatus::Cancelled;
                    s.turn = round;
                    s.error =
                        Some("PR closed externally without merge; review loop exited".to_string());
                })
                .await?;
                return Ok(());
            }
            PrExternalState::Open | PrExternalState::Unknown => {}
        }

        let mut decision = ReviewLoopDecision {
            active_bot: bot_chain
                .first()
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("review fallback chain is empty"))?,
            fallback: None,
            wait_for_bot: false,
        };
        match fetch_pull_request_signals(&repo_slug, pr_num, github_token).await {
            Ok(signals) => {
                (silence_rounds, last_bot_activity_at) = update_silence_rounds(
                    silence_rounds,
                    last_bot_activity_at,
                    signals.latest_bot_activity_at,
                );
                decision =
                    decide_review_loop_action(&bot_chain, &signals, silence_rounds, review_config)?;
                if let Some(fallback) = &decision.fallback {
                    let event = build_task_event(
                        task_id,
                        round,
                        "review",
                        "pr_review_fallback",
                        Decision::Warn,
                        Some(format!(
                            "tier={:?}; trigger={:?}; active_bot={}",
                            fallback.tier,
                            fallback.trigger,
                            fallback
                                .active_bot
                                .map(ReviewBotKey::as_str)
                                .unwrap_or("none")
                        )),
                        Some(format!("pr={pr_num}")),
                        None,
                        None,
                        None,
                    );
                    if let Err(error) = events.log(&event).await {
                        tracing::warn!("failed to log pr_review_fallback event: {error}");
                    }
                }
                if decision.fallback.as_ref().is_some_and(|fallback| {
                    fallback.tier == harness_workflow::issue_lifecycle::ReviewFallbackTier::C
                }) {
                    let fallback = decision.fallback.expect("tier C fallback");
                    let detail = format!(
                        "Review fallback tier {} via {}",
                        match fallback.tier {
                            harness_workflow::issue_lifecycle::ReviewFallbackTier::A => "A",
                            harness_workflow::issue_lifecycle::ReviewFallbackTier::B => "B",
                            harness_workflow::issue_lifecycle::ReviewFallbackTier::C => "C",
                        },
                        match fallback.trigger {
                            harness_workflow::issue_lifecycle::ReviewFallbackTrigger::GeminiQuota => {
                                "gemini_quota"
                            }
                            harness_workflow::issue_lifecycle::ReviewFallbackTrigger::CodexQuota => {
                                "codex_quota"
                            }
                            harness_workflow::issue_lifecycle::ReviewFallbackTrigger::AllBotsQuota => {
                                "all_bots_quota"
                            }
                            harness_workflow::issue_lifecycle::ReviewFallbackTrigger::Silence => {
                                "silence"
                            }
                        }
                    );
                    mutate_and_persist(store, task_id, |s| {
                        s.status = TaskStatus::Done;
                        s.turn = round;
                        s.error = Some(detail.clone());
                        s.rounds.push(RoundResult::new(
                            round,
                            "review",
                            "ready_to_merge",
                            Some(detail.clone()),
                            None,
                            None,
                        ));
                    })
                    .await?;
                    if let Some(workflows) = issue_workflow_store {
                        let project_id = project_root.to_string_lossy().into_owned();
                        let snapshot = harness_workflow::issue_lifecycle::ReviewFallbackSnapshot {
                            tier: fallback.tier,
                            trigger: fallback.trigger,
                            active_bot: fallback.active_bot.map(|bot| bot.as_str().to_string()),
                            activated_at: Utc::now(),
                        };
                        let _ = workflows
                            .record_ready_to_merge_with_fallback(
                                &project_id,
                                req.repo.as_deref(),
                                pr_num,
                                Some(&detail),
                                snapshot,
                            )
                            .await;
                    }
                    store.log_event(crate::event_replay::TaskEvent::Completed {
                        task_id: task_id.0.clone(),
                        ts: crate::event_replay::now_ts(),
                    });
                    return Ok(());
                }
                if decision.wait_for_bot {
                    if decision.active_bot.key == ReviewBotKey::Codex {
                        if let Err(error) = post_review_bot_comment(
                            &repo_slug,
                            pr_num,
                            &decision.active_bot.review_command,
                            github_token,
                        )
                        .await
                        {
                            tracing::warn!(pr = pr_num, "failed to summon Codex reviewer: {error}");
                        }
                    }
                    tracing::info!(
                        pr = pr_num,
                        active_bot = decision.active_bot.key.as_str(),
                        silence_rounds,
                        "waiting for review bot activity"
                    );
                    waiting_count = waiting_count.saturating_add(1);
                    if waiting_count >= MAX_CONSECUTIVE_WAITS {
                        round += 1;
                        waiting_count = 0;
                    }
                    update_status(store, task_id, TaskStatus::Waiting, waiting_count).await?;
                    sleep(Duration::from_secs(wait_secs)).await;
                    continue;
                }
            }
            Err(error) => {
                tracing::warn!(
                    pr = pr_num,
                    error = %error,
                    "review loop: GitHub signal fetch failed, falling back to prompt-driven review"
                );
            }
        }

        update_status(store, task_id, TaskStatus::Reviewing, round).await?;

        let base_prompt = prompts::review_prompt(
            req.issue,
            pr_num,
            round,
            prev_fixed,
            &decision.active_bot.review_command,
            &decision.active_bot.reviewer_name,
            &repo_slug,
            impasse,
        );
        // If the previous LGTM was rejected by the test gate, prepend the failure
        // output so the agent has context for why re-work is needed.
        let round_prompt = if let Some(failure) = pending_test_failure.take() {
            prompts::test_gate_failure_prompt(&failure, &base_prompt)
        } else {
            base_prompt
        };

        let prompt_built_at = Utc::now();
        let check_req = AgentRequest {
            prompt: round_prompt,
            project_root: project.to_path_buf(),
            context: context_items.to_vec(),
            execution_phase: Some(ExecutionPhase::Execution),
            env_vars: cargo_env.clone(),
            ..Default::default()
        };
        let check_req = run_pre_execute(interceptors, check_req).await?;

        if let Some(max) = effective_max_turns {
            if *turns_used >= max {
                let msg = format!(
                    "Turn budget exhausted: used {} of {} allowed turns",
                    turns_used, max
                );
                tracing::warn!(task_id = %task_id, turns_used, max, "turn budget exhausted in review loop");
                mutate_and_persist(store, task_id, |s| {
                    s.status = TaskStatus::Failed;
                    s.error = Some(msg.clone());
                })
                .await?;
                return Ok(());
            }
        }
        let review_started_at = Utc::now();
        let resp = tokio::time::timeout(
            turn_timeout,
            run_agent_streaming(
                agent,
                check_req.clone(),
                task_id,
                store,
                round,
                prompt_built_at,
                review_started_at,
            ),
        )
        .await;
        *turns_used += 1;
        *turns_used_acc = *turns_used;
        let resp = match resp {
            Ok(Ok(success)) => {
                let r = success.response;
                let tool_violations = validate_tool_usage(
                    &r.output,
                    check_req.allowed_tools.as_deref().unwrap_or(&[]),
                );
                if !tool_violations.is_empty() {
                    let msg = format!(
                        "Tool isolation violation in review check round {round}: agent used disallowed tools: [{}]",
                        tool_violations.join(", ")
                    );
                    tracing::warn!(
                        round,
                        ?tool_violations,
                        "review check: agent used tools outside allowed list"
                    );
                    run_on_error(interceptors, &check_req, &msg).await;
                    return Err(anyhow::anyhow!("{msg}"));
                }
                if let Some(val_err) = run_post_execute(interceptors, &check_req, &r).await {
                    tracing::error!(
                        round,
                        error = %val_err,
                        "post-execute validation failed in review check; will re-enter review"
                    );
                    pending_test_failure =
                        Some(format!("Post-execution validation failed:\n{val_err}"));
                }
                (r, success.telemetry)
            }
            Ok(Err(failure)) => {
                // Quota/billing failures are not retryable — break immediately instead of
                // burning remaining review rounds on repeated errors.
                // Do NOT activate the global rate-limit circuit breaker: the reviewer
                // agent is configured independently from the implementation agent and a
                // depleted reviewer account must not stall unrelated implementation tasks.
                if matches!(
                    failure.failure.kind,
                    TurnFailureKind::Quota | TurnFailureKind::Billing
                ) {
                    tracing::error!(round, error = %failure.error, "quota/billing failure during review — aborting review loop");
                    run_on_error(interceptors, &check_req, &failure.error.to_string()).await;
                    mutate_and_persist(store, task_id, |s| {
                        s.status = TaskStatus::Failed;
                        s.error = Some(failure.error.to_string());
                        s.rounds.push(RoundResult::new(
                            round,
                            "review",
                            match failure.failure.kind {
                                TurnFailureKind::Quota => "quota_exhausted",
                                TurnFailureKind::Billing => "billing_failed",
                                TurnFailureKind::Upstream => "upstream_failure",
                                _ => "failed",
                            },
                            None,
                            Some(failure.telemetry.clone()),
                            Some(failure.failure.clone()),
                        ));
                    })
                    .await?;
                    let event = build_task_event(
                        task_id,
                        round,
                        "review",
                        "pr_review",
                        Decision::Block,
                        Some("review round failed".to_string()),
                        Some(format!("pr={pr_num}")),
                        Some(failure.telemetry),
                        Some(failure.failure),
                        None,
                    );
                    if let Err(error) = events.log(&event).await {
                        tracing::warn!("failed to log pr_review event: {error}");
                    }
                    return Ok(());
                }
                run_on_error(interceptors, &check_req, &failure.error.to_string()).await;
                mutate_and_persist(store, task_id, |s| {
                    s.rounds.push(RoundResult::new(
                        round,
                        "review",
                        "failed",
                        None,
                        Some(failure.telemetry.clone()),
                        Some(failure.failure.clone()),
                    ));
                })
                .await?;
                let event = build_task_event(
                    task_id,
                    round,
                    "review",
                    "pr_review",
                    Decision::Block,
                    Some("review round failed".to_string()),
                    Some(format!("pr={pr_num}")),
                    Some(failure.telemetry),
                    Some(failure.failure),
                    None,
                );
                if let Err(error) = events.log(&event).await {
                    tracing::warn!("failed to log pr_review event: {error}");
                }
                return Err(failure.error.into());
            }
            Err(_) => {
                let msg = format!(
                    "Review check round {round} timed out after {}s",
                    turn_timeout.as_secs()
                );
                run_on_error(interceptors, &check_req, &msg).await;
                let telemetry =
                    telemetry_for_timeout(prompt_built_at, review_started_at, Utc::now(), None);
                let failure = TurnFailure {
                    kind: TurnFailureKind::Timeout,
                    provider: Some(agent.name().to_string()),
                    upstream_status: None,
                    message: Some(msg.clone()),
                    body_excerpt: None,
                };
                mutate_and_persist(store, task_id, |s| {
                    s.rounds.push(RoundResult::new(
                        round,
                        "review",
                        "timeout",
                        None,
                        Some(telemetry.clone()),
                        Some(failure.clone()),
                    ));
                })
                .await?;
                let event = build_task_event(
                    task_id,
                    round,
                    "review",
                    "pr_review",
                    Decision::Block,
                    Some("review round timed out".to_string()),
                    Some(format!("pr={pr_num}")),
                    Some(telemetry),
                    Some(failure),
                    None,
                );
                if let Err(error) = events.log(&event).await {
                    tracing::warn!("failed to log pr_review event: {error}");
                }
                return Err(anyhow::anyhow!("{msg}"));
            }
        };

        let (AgentResponse { output, stderr, .. }, review_telemetry) = resp;

        if !stderr.is_empty() {
            tracing::warn!(round, stderr = %stderr, "agent stderr during review check");
        }

        let raw_lgtm = prompts::is_lgtm(&output);
        let waiting = prompts::is_waiting(&output);
        // If post-execute validation failed this round, block LGTM acceptance even
        // if the reviewer approved — the local validator caught an issue that must be
        // fixed before the PR can be marked done.
        let lgtm = raw_lgtm && pending_test_failure.is_none();
        let fixed = !lgtm && !waiting;

        // Parse issue count before the Jaccard check so loop detection can distinguish
        // genuine forward progress (decreasing issue count) from true stuck loops.
        let current_issues = prompts::parse_issue_count(&output);

        // Jaccard loop detection: two consecutive non-waiting, non-LGTM outputs that are
        // too similar indicate the reviewer is stuck repeating itself without making progress.
        // Skip when the raw output is an approval: repeated LGTM is legitimate convergence
        // (e.g., reviewer approves again after a test-gate previously blocked acceptance).
        if !waiting && !raw_lgtm {
            if let Some(ref prev) = prev_review_output {
                let score = jaccard_word_similarity(prev, &output);
                if score >= jaccard_threshold {
                    // If the issue count decreased since the previous round, the agent is making
                    // genuine progress despite similar output structure (e.g., one fix per round
                    // with a consistent report format). Only abort on true stagnation.
                    let last_count = issue_counts.last().and_then(|x| *x);
                    let progressing = matches!(
                        (last_count, current_issues),
                        (Some(prev_n), Some(curr_n)) if curr_n < prev_n
                    );
                    if !progressing {
                        let msg = format!(
                            "review loop detected: output similarity {score:.2} >= threshold {jaccard_threshold:.2}"
                        );
                        tracing::warn!(task_id = %task_id, round, score, jaccard_threshold, "jaccard loop detected in review");
                        mutate_and_persist(store, task_id, |s| {
                            s.status = TaskStatus::Failed;
                            s.error = Some(msg.clone());
                        })
                        .await?;
                        return Ok(());
                    }
                }
            }
            prev_review_output = Some(output.clone());
        } else if raw_lgtm {
            // A test-gate-rejected LGTM still resets the comparison baseline so the
            // next genuine non-LGTM review is not incorrectly compared against a
            // pre-LGTM review and falsely flagged as a loop.
            prev_review_output = None;
        }

        if !waiting {
            issue_counts.push(current_issues);
        }

        // Convergence check: if the last 3 rounds all reported issues and the
        // count is not decreasing, enter impasse mode (critical-only fixes).
        if !impasse && issue_count_not_decreasing(&issue_counts) {
            impasse = true;
            let tail = &issue_counts[issue_counts.len() - 3..];
            if let [Some(a), Some(b), Some(c)] = tail {
                tracing::warn!(
                    task_id = %task_id,
                    round,
                    issues = %format_args!("[{a}, {b}, {c}]"),
                    "review loop impasse detected: issue count not decreasing"
                );
            }
        }

        // WAITING means the active review bot has not yet posted a fresh review.
        // Don't consume a round — just sleep and retry without incrementing.
        // Cap consecutive waits to prevent infinite loops when the bot never responds.
        if waiting {
            if waiting_count >= MAX_CONSECUTIVE_WAITS {
                tracing::warn!(
                    round,
                    waiting_count,
                    "PR #{pr_num} review bot has not responded after {MAX_CONSECUTIVE_WAITS} waits; \
                     consuming a round to prevent infinite loop"
                );
                round += 1;
                waiting_count = 0;
            } else {
                tracing::info!(
                    round,
                    waiting_count,
                    "PR #{pr_num} review bot has not responded yet; sleeping without consuming round"
                );
                waiting_count += 1;
                update_status(store, task_id, TaskStatus::Waiting, waiting_count).await?;
                sleep(Duration::from_secs(wait_secs)).await;
                continue;
            }
        }

        let result_label = if lgtm { "lgtm" } else { "fixed" };
        mutate_and_persist(store, task_id, |s| {
            s.rounds.push(RoundResult::new(
                round,
                "review",
                result_label,
                None,
                Some(review_telemetry.clone()),
                None,
            ));
        })
        .await?;

        // Emit RoundCompleted for crash recovery.
        store.log_event(crate::event_replay::TaskEvent::RoundCompleted {
            task_id: task_id.0.clone(),
            ts: crate::event_replay::now_ts(),
            round,
            result: result_label.to_string(),
        });

        // Log pr_review event for observability and GC signal detection.
        let event = build_task_event(
            task_id,
            round,
            "review",
            "pr_review",
            if lgtm {
                Decision::Complete
            } else {
                Decision::Warn
            },
            Some(format!("round {round}: {result_label}")),
            Some(format!("pr={pr_num}")),
            Some(review_telemetry.clone()),
            None,
            Some(output.clone()),
        );
        if let Err(e) = events.log(&event).await {
            tracing::warn!("failed to log pr_review event: {e}");
        }

        if lgtm {
            // Hard gate: run the project's tests before accepting LGTM.
            // This prevents agents from gaming the review by manipulating tests
            // rather than fixing code (OpenAI "Monitoring Reasoning Models", 2026).
            match super::run_test_gate(
                project,
                &project_config.validation.pre_push,
                project_config.validation.test_gate_timeout_secs,
                cargo_env,
            )
            .await
            {
                Ok(()) => {
                    tracing::info!("PR #{pr_num} approved at round {round}");
                    mutate_and_persist(store, task_id, |s| {
                        s.status = TaskStatus::Done;
                        s.turn = round.saturating_add(1);
                    })
                    .await?;
                    store.log_event(crate::event_replay::TaskEvent::Completed {
                        task_id: task_id.0.clone(),
                        ts: crate::event_replay::now_ts(),
                    });
                    tracing::info!(
                        task_id = %task_id,
                        phase = "reviewing",
                        elapsed_secs = review_phase_start.elapsed().as_secs(),
                        "phase_completed"
                    );
                    tracing::info!(
                        task_id = %task_id,
                        status = "done",
                        turns = round.saturating_add(1),
                        pr_url = pr_url.as_deref().unwrap_or(""),
                        total_elapsed_secs = task_start.elapsed().as_secs(),
                        "task_completed"
                    );
                    return Ok(());
                }
                Err(output) => {
                    tracing::warn!(
                        task_id = %task_id,
                        round,
                        "LGTM rejected: tests failed in test gate, re-entering review round {}",
                        round.saturating_add(1)
                    );
                    lgtm_test_gate_rejected = true;
                    pending_test_failure = Some(output);
                }
            }
        }

        // Track whether this round produced a fix so the next round enforces
        // freshness checks against new reviewer feedback.
        // Also treat a test gate rejection as a "fixed" round — the agent needs
        // to push a fix and get a fresh review before LGTM can be accepted.
        prev_fixed = fixed || pending_test_failure.is_some();
        tracing::info!("PR #{pr_num} fixed at round {round}; waiting for bot re-review");
        if round < max_rounds {
            waiting_count += 1;
            update_status(store, task_id, TaskStatus::Waiting, waiting_count).await?;
            sleep(Duration::from_secs(wait_secs)).await;
        }
        round += 1;
    }

    // Graduated exit: if the last round had few issues remaining, mark as done
    // with a warning instead of outright failure — the PR is likely mergeable.
    // But never graduate when the test gate was still failing on the last round:
    // a PR whose tests consistently fail must not be approved.
    let last_issue_count = issue_counts.iter().rev().find_map(|c| *c);
    // Never graduate when a LGTM was rejected by the test gate — the pending_test_failure
    // Option was already cleared by take() at round start, so we track rejection separately.
    let graduated = last_issue_count.is_some_and(|n| n <= 2) && !lgtm_test_gate_rejected;

    if graduated {
        tracing::info!(
            task_id = %task_id,
            remaining_issues = last_issue_count.unwrap_or(0),
            "review loop exhausted but few issues remain — marking done with warning"
        );
        mutate_and_persist(store, task_id, |s| {
            s.status = TaskStatus::Done;
            s.turn = max_rounds.saturating_add(1);
            s.error = Some(format!(
                "Graduated: LGTM not received after {} rounds but only {} issues remain.",
                max_rounds,
                last_issue_count.unwrap_or(0),
            ));
        })
        .await?;
    } else {
        mutate_and_persist(store, task_id, |s| {
            s.status = TaskStatus::Failed;
            s.turn = max_rounds.saturating_add(1);
            s.error = Some(format!(
                "Task did not receive LGTM after {} review rounds (last issues: {}).",
                max_rounds,
                last_issue_count.map_or("unknown".to_string(), |n| n.to_string()),
            ));
        })
        .await?;
    }
    tracing::info!(
        task_id = %task_id,
        phase = "reviewing",
        elapsed_secs = review_phase_start.elapsed().as_secs(),
        "phase_completed"
    );
    tracing::info!(
        task_id = %task_id,
        status = if graduated { "done" } else { "failed" },
        turns = max_rounds.saturating_add(1),
        pr_url = pr_url.as_deref().unwrap_or(""),
        total_elapsed_secs = task_start.elapsed().as_secs(),
        "task_completed"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn descriptor(key: ReviewBotKey) -> ReviewBotDescriptor {
        match key {
            ReviewBotKey::Gemini => ReviewBotDescriptor {
                key,
                reviewer_name: "gemini-code-assist[bot]".to_string(),
                review_command: "/gemini review".to_string(),
            },
            ReviewBotKey::Codex => ReviewBotDescriptor {
                key,
                reviewer_name: CODEX_REVIEWER_NAME.to_string(),
                review_command: CODEX_REVIEW_COMMAND.to_string(),
            },
        }
    }

    fn bot_signals() -> BotSignals {
        BotSignals::default()
    }

    fn review_config() -> harness_core::config::agents::AgentReviewConfig {
        harness_core::config::agents::AgentReviewConfig::default()
    }

    fn signals_with(primary: BotSignals, secondary: BotSignals) -> PullRequestSignals {
        let latest_commit_at = Utc::now() - chrono::Duration::minutes(45);
        let mut bots = HashMap::new();
        bots.insert(ReviewBotKey::Gemini, primary);
        bots.insert(ReviewBotKey::Codex, secondary);
        PullRequestSignals {
            latest_commit_at,
            ci_green: true,
            latest_bot_activity_at: None,
            blocking_feedback: false,
            bots,
        }
    }

    #[test]
    fn classifies_gemini_quota_body() {
        let mut bot = bot_signals();
        bot.reviewed_any_commit = true;
        bot.latest_comment_at = Some(Utc::now());
        bot.latest_comment_body =
            Some("You have reached your daily quota limit for Gemini Code Assist.".to_string());
        let classification = classify_bot(
            &descriptor(ReviewBotKey::Gemini),
            &bot,
            Utc::now() - chrono::Duration::minutes(5),
            true,
            false,
            0,
            3,
            30,
        );
        assert_eq!(classification, BotClassification::QuotaExhausted);
    }

    #[test]
    fn classifies_codex_quota_body() {
        let mut bot = bot_signals();
        bot.reviewed_any_commit = true;
        bot.latest_comment_at = Some(Utc::now());
        bot.latest_comment_body =
            Some("Codex usage limits have been reached for this account.".to_string());
        let classification = classify_bot(
            &descriptor(ReviewBotKey::Codex),
            &bot,
            Utc::now() - chrono::Duration::minutes(5),
            true,
            false,
            0,
            3,
            30,
        );
        assert_eq!(classification, BotClassification::QuotaExhausted);
    }

    #[test]
    fn decides_tier_c_when_both_bots_are_quota_exhausted() {
        let mut gemini = bot_signals();
        gemini.reviewed_any_commit = true;
        gemini.latest_comment_at = Some(Utc::now());
        gemini.latest_comment_body =
            Some("You have reached your daily quota limit for Gemini Code Assist.".to_string());
        let mut codex = bot_signals();
        codex.reviewed_any_commit = true;
        codex.latest_comment_at = Some(Utc::now());
        codex.latest_comment_body =
            Some("Codex usage limits have been reached for this account.".to_string());
        let decision = decide_review_loop_action(
            &bot_fallback_chain(&review_config()),
            &signals_with(gemini, codex),
            0,
            &review_config(),
        )
        .expect("decision");
        assert_eq!(decision.active_bot.key, ReviewBotKey::Codex);
        let fallback = decision.fallback.expect("fallback");
        assert_eq!(
            fallback.tier,
            harness_workflow::issue_lifecycle::ReviewFallbackTier::C
        );
        assert_eq!(
            fallback.trigger,
            harness_workflow::issue_lifecycle::ReviewFallbackTrigger::AllBotsQuota
        );
    }

    #[test]
    fn classifies_silence_when_all_guards_are_met() {
        let mut bot = bot_signals();
        bot.reviewed_any_commit = true;
        bot.latest_review_at = Some(Utc::now() - chrono::Duration::minutes(60));
        bot.latest_review_state = Some("COMMENTED".to_string());
        let classification = classify_bot(
            &descriptor(ReviewBotKey::Codex),
            &bot,
            Utc::now() - chrono::Duration::minutes(45),
            true,
            false,
            3,
            3,
            30,
        );
        assert_eq!(classification, BotClassification::Silent);
    }

    #[test]
    fn silence_rounds_reset_on_new_bot_activity() {
        let initial_activity = Some(Utc::now() - chrono::Duration::minutes(10));
        let new_activity = Some(Utc::now());
        assert_eq!(
            update_silence_rounds(2, initial_activity, new_activity),
            (0, new_activity)
        );
    }

    #[test]
    fn silence_is_blocked_by_red_ci() {
        let mut bot = bot_signals();
        bot.reviewed_any_commit = true;
        bot.latest_review_at = Some(Utc::now() - chrono::Duration::minutes(60));
        let classification = classify_bot(
            &descriptor(ReviewBotKey::Codex),
            &bot,
            Utc::now() - chrono::Duration::minutes(45),
            false,
            false,
            3,
            3,
            30,
        );
        assert_eq!(classification, BotClassification::ActionableFeedback);
    }

    #[test]
    fn silence_is_blocked_by_unresolved_high_feedback() {
        let mut bot = bot_signals();
        bot.reviewed_any_commit = true;
        bot.latest_review_at = Some(Utc::now() - chrono::Duration::minutes(60));
        let classification = classify_bot(
            &descriptor(ReviewBotKey::Codex),
            &bot,
            Utc::now() - chrono::Duration::minutes(45),
            true,
            true,
            3,
            3,
            30,
        );
        assert_eq!(classification, BotClassification::ActionableFeedback);
    }

    #[test]
    fn silence_is_blocked_when_no_prior_bot_review_exists() {
        let classification = classify_bot(
            &descriptor(ReviewBotKey::Codex),
            &bot_signals(),
            Utc::now() - chrono::Duration::minutes(45),
            true,
            false,
            3,
            3,
            30,
        );
        assert_eq!(classification, BotClassification::NeverReviewed);
    }

    #[test]
    fn normal_fresh_gemini_path_stays_primary() {
        let mut gemini = bot_signals();
        gemini.reviewed_any_commit = true;
        gemini.latest_review_at = Some(Utc::now());
        gemini.latest_review_state = Some("APPROVED".to_string());
        let decision = decide_review_loop_action(
            &bot_fallback_chain(&review_config()),
            &signals_with(gemini, bot_signals()),
            0,
            &review_config(),
        )
        .expect("decision");
        assert_eq!(decision.active_bot.key, ReviewBotKey::Gemini);
        assert!(decision.fallback.is_none());
        assert!(!decision.wait_for_bot);
    }
}
