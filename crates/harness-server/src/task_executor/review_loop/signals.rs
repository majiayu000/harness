use chrono::{DateTime, Utc};
use harness_core::prompts;
use serde::Deserialize;
use std::collections::HashMap;
use tokio::time::{sleep, Duration, Instant};

pub(super) const CODEX_REVIEWER_NAME: &str = "chatgpt-codex-connector[bot]";
pub(super) const CODEX_REVIEW_COMMAND: &str = "@codex";
const CODEX_APPROVAL_SIGNATURE: &str = "no major issues";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum ReviewBotKey {
    Gemini,
    Codex,
}

impl ReviewBotKey {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Gemini => "gemini",
            Self::Codex => "codex",
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct ReviewBotDescriptor {
    pub(super) key: ReviewBotKey,
    pub(super) reviewer_name: String,
    pub(super) review_command: String,
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
pub(super) struct PullRequestSignals {
    pub(super) latest_commit_at: DateTime<Utc>,
    pub(super) ci_state: Option<String>,
    pub(super) ci_green: bool,
    pub(super) latest_bot_activity_at: Option<DateTime<Utc>>,
    pub(super) blocking_feedback: bool,
    pub(super) bots: HashMap<ReviewBotKey, BotSignals>,
}

#[derive(Debug, Clone, Default)]
pub(super) struct BotSignals {
    pub(super) latest_review_at: Option<DateTime<Utc>>,
    pub(super) latest_review_state: Option<String>,
    pub(super) latest_review_body: Option<String>,
    pub(super) latest_comment_at: Option<DateTime<Utc>>,
    pub(super) latest_comment_body: Option<String>,
    pub(super) reviewed_any_commit: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BotClassification {
    FreshApproval,
    ActionableFeedback,
    QuotaExhausted,
    Silent,
    NeverReviewed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PrCheckRollupState {
    Success,
    Pending(String),
    Failed(String),
}

#[derive(Debug, Clone)]
pub(super) struct ReviewFallbackState {
    pub(super) tier: harness_workflow::issue_lifecycle::ReviewFallbackTier,
    pub(super) trigger: harness_workflow::issue_lifecycle::ReviewFallbackTrigger,
    pub(super) active_bot: Option<ReviewBotKey>,
}

#[derive(Debug, Clone)]
pub(super) struct ReviewLoopDecision {
    pub(super) active_bot: ReviewBotDescriptor,
    pub(super) fallback: Option<ReviewFallbackState>,
    pub(super) wait_for_bot: bool,
}

pub(super) fn quota_trigger(
    bot: ReviewBotKey,
) -> harness_workflow::issue_lifecycle::ReviewFallbackTrigger {
    match bot {
        ReviewBotKey::Gemini => {
            harness_workflow::issue_lifecycle::ReviewFallbackTrigger::GeminiQuota
        }
        ReviewBotKey::Codex => harness_workflow::issue_lifecycle::ReviewFallbackTrigger::CodexQuota,
    }
}

pub(super) fn bot_fallback_chain(
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

pub(super) fn review_bot_key_for_author(
    chain: &[ReviewBotDescriptor],
    author: &str,
) -> Option<ReviewBotKey> {
    chain
        .iter()
        .find(|descriptor| descriptor.reviewer_name.eq_ignore_ascii_case(author))
        .map(|descriptor| descriptor.key)
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

pub(super) fn record_bot_comment_signal(
    bot: &mut BotSignals,
    body: String,
    created_at: DateTime<Utc>,
) {
    bot.reviewed_any_commit = true;
    let replace = bot
        .latest_comment_at
        .map(|current| created_at >= current)
        .unwrap_or(true);
    if replace {
        bot.latest_comment_at = Some(created_at);
        bot.latest_comment_body = Some(body);
    }
}

pub(super) fn classify_bot(
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

pub(super) fn classify_pr_check_rollup_state(state: Option<&str>) -> PrCheckRollupState {
    let Some(state) = state.map(str::trim).filter(|state| !state.is_empty()) else {
        return PrCheckRollupState::Pending("no statusCheckRollup state yet".to_string());
    };
    match state.to_ascii_uppercase().as_str() {
        "SUCCESS" => PrCheckRollupState::Success,
        "FAILURE" | "ERROR" => PrCheckRollupState::Failed(state.to_string()),
        _ => PrCheckRollupState::Pending(state.to_string()),
    }
}

pub(super) async fn fetch_pull_request_signals(
    repo_slug: &str,
    pr_num: u64,
    github_token: Option<&str>,
    chain: &[ReviewBotDescriptor],
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
    let ci_state = latest_commit
        .commit
        .status_check_rollup
        .as_ref()
        .map(|rollup| rollup.state.clone());
    let ci_green = matches!(
        classify_pr_check_rollup_state(ci_state.as_deref()),
        PrCheckRollupState::Success
    );

    let mut bots = HashMap::from([
        (ReviewBotKey::Gemini, BotSignals::default()),
        (ReviewBotKey::Codex, BotSignals::default()),
    ]);

    for review in pr.reviews.nodes {
        let Some(author) = review.author.as_ref().map(|author| author.login.as_str()) else {
            continue;
        };
        let Some(key) = review_bot_key_for_author(chain, author) else {
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
        let Some(key) = review_bot_key_for_author(chain, author) else {
            continue;
        };
        let entry = bots.entry(key).or_default();
        record_bot_comment_signal(entry, comment.body, comment.created_at);
    }

    let blocking_feedback = pr.review_threads.nodes.iter().any(blocking_review_thread);
    let mut latest_bot_activity_at: Option<DateTime<Utc>> = None;
    for thread in pr.review_threads.nodes {
        for comment in thread.comments.nodes {
            let Some(author) = comment.author.as_ref().map(|author| author.login.as_str()) else {
                continue;
            };
            if review_bot_key_for_author(chain, author).is_none() {
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
        ci_state,
        ci_green,
        latest_bot_activity_at,
        blocking_feedback,
        bots,
    })
}

pub(crate) async fn verify_pr_checks_green(
    repo_slug: &str,
    pr_num: u64,
    github_token: Option<&str>,
    timeout_secs: u64,
    poll_interval_secs: u64,
) -> Result<(), String> {
    let github_token = crate::github_auth::resolve_github_token(github_token).ok_or_else(|| {
        "GitHub token is required to verify PR checks when hosted review is disabled; \
         set server.github_token, GITHUB_TOKEN, or GH_TOKEN"
            .to_string()
    })?;
    let timeout_secs = timeout_secs.max(1);
    let poll_interval = Duration::from_secs(poll_interval_secs.clamp(1, 60));
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        let signals = fetch_pull_request_signals(repo_slug, pr_num, Some(&github_token), &[])
            .await
            .map_err(|error| {
                format!("GitHub PR check-status lookup failed for {repo_slug}#{pr_num}: {error}")
            })?;
        match classify_pr_check_rollup_state(signals.ci_state.as_deref()) {
            PrCheckRollupState::Success => return Ok(()),
            PrCheckRollupState::Failed(state) => {
                return Err(format!(
                    "GitHub PR checks are not green for {repo_slug}#{pr_num}: {state}"
                ))
            }
            PrCheckRollupState::Pending(state) => {
                if Instant::now() >= deadline {
                    return Err(format!(
                        "GitHub PR checks did not reach SUCCESS for {repo_slug}#{pr_num} \
                         within {timeout_secs}s; last state: {state}"
                    ));
                }
                sleep(poll_interval).await;
            }
        }
    }
}

pub(super) async fn post_review_bot_comment(
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
