use anyhow::Context;
use serde::de::{DeserializeOwned, IgnoredAny};
use serde::{Deserialize, Deserializer};
use serde_json::{json, Value};
use std::time::Duration;

/// External GitHub state observed for one candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GitHubState {
    PrMerged,
    PrClosed,
    IssueCompleted,
    IssueClosed,
    Open,
    Unknown,
}

#[derive(Debug, Deserialize)]
pub(super) struct GitHubPullState {
    pub(super) state: String,
    pub(super) merged_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ExactGitHubPullState {
    number: u64,
    state: String,
    merged_at: Option<String>,
    base: ExactGitHubPullBase,
}

#[derive(Debug, Deserialize)]
struct ExactGitHubPullBase {
    repo: ExactGitHubRepository,
}

#[derive(Debug, Deserialize)]
struct ExactGitHubRepository {
    full_name: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct GitHubIssueState {
    pub(super) state: String,
    #[serde(default)]
    pub(super) state_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ExactGitHubIssueState {
    number: u64,
    repository_url: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    html_url: String,
    labels: Option<Vec<ExactGitHubIssueLabel>>,
    updated_at: Option<String>,
    state: String,
    #[serde(default)]
    state_reason: Option<String>,
    #[serde(
        default,
        rename = "pull_request",
        deserialize_with = "deserialize_field_presence"
    )]
    has_pull_request_marker: bool,
}

#[derive(Debug, Deserialize)]
struct ExactGitHubIssueLabel {
    name: String,
}

fn deserialize_field_presence<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: Deserializer<'de>,
{
    IgnoredAny::deserialize(deserializer)?;
    Ok(true)
}

pub(crate) use crate::github_client::github_api_base_url;

const GITHUB_STATE_TIMEOUT: Duration = Duration::from_secs(10);

fn github_state_client() -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?)
}

async fn github_get_json<T: DeserializeOwned>(path: &str, github_token: Option<&str>) -> Option<T> {
    github_get_json_with_timeout(path, github_token, GITHUB_STATE_TIMEOUT).await
}

async fn try_github_get_json<T: DeserializeOwned>(
    path: &str,
    github_token: Option<&str>,
) -> anyhow::Result<T> {
    let client = github_state_client()?;
    try_github_get_json_with_client_timeout(&client, path, github_token, GITHUB_STATE_TIMEOUT).await
}

pub(super) async fn github_get_json_with_timeout<T: DeserializeOwned>(
    path: &str,
    github_token: Option<&str>,
    timeout: Duration,
) -> Option<T> {
    let client = match github_state_client() {
        Ok(client) => client,
        Err(error) => {
            tracing::debug!(%error, path, "failed to build GitHub state client");
            return None;
        }
    };
    github_get_json_with_client_timeout(&client, path, github_token, timeout).await
}

pub(super) async fn github_get_json_with_client_timeout<T: DeserializeOwned>(
    client: &reqwest::Client,
    path: &str,
    github_token: Option<&str>,
    timeout: Duration,
) -> Option<T> {
    match try_github_get_json_with_client_timeout(client, path, github_token, timeout).await {
        Ok(value) => Some(value),
        Err(error) => {
            tracing::debug!(%error, path, "GitHub state check failed");
            None
        }
    }
}

async fn try_github_get_json_with_client_timeout<T: DeserializeOwned>(
    client: &reqwest::Client,
    path: &str,
    github_token: Option<&str>,
    timeout: Duration,
) -> anyhow::Result<T> {
    let mut request = client
        .get(format!("{}{}", github_api_base_url(), path))
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .header(reqwest::header::USER_AGENT, "harness-server");
    if let Some(token) = crate::github_auth::resolve_github_token(github_token) {
        request = request.bearer_auth(token);
    }
    let value = tokio::time::timeout(timeout, async {
        let response = request.send().await?;
        let status = response.status();
        if !status.is_success() {
            anyhow::bail!("GitHub state check returned HTTP {status}");
        }
        Ok::<T, anyhow::Error>(response.json::<T>().await?)
    })
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "GitHub state check timed out after {}ms",
            timeout.as_millis()
        )
    })??;
    Ok(value)
}

pub(super) fn classify_pr_state(state: &GitHubPullState) -> GitHubState {
    let merged_at_empty = state.merged_at.as_deref().unwrap_or("").trim().is_empty();
    // A payload claiming `state=open` while carrying a merge timestamp is
    // self-contradictory; classify it Unknown (fail closed) instead of Open so
    // admission and reconciliation never trust the optimistic half.
    match (state.state.as_str(), merged_at_empty) {
        ("open", true) | ("OPEN", true) => GitHubState::Open,
        ("merged", _) | ("MERGED", _) | ("closed", false) | ("CLOSED", false) => {
            GitHubState::PrMerged
        }
        ("closed", true) | ("CLOSED", true) => GitHubState::PrClosed,
        _ => GitHubState::Unknown,
    }
}

pub(super) fn classify_issue_state(state: &GitHubIssueState) -> GitHubState {
    match (state.state.as_str(), state.state_reason.as_deref()) {
        ("closed" | "CLOSED", Some("completed" | "COMPLETED")) => GitHubState::IssueCompleted,
        ("closed" | "CLOSED", _) => GitHubState::IssueClosed,
        ("open" | "OPEN", _) => GitHubState::Open,
        _ => GitHubState::Unknown,
    }
}

/// Fetch GitHub PR state from a full URL (e.g. `https://github.com/.../pull/42`).
pub(super) async fn fetch_pr_state_by_url(pr_url: &str, github_token: Option<&str>) -> GitHubState {
    let Some((owner, repo, pr_number)) =
        harness_agents::output_parsing::parse_github_pr_url(pr_url)
    else {
        tracing::debug!(pr_url, "GitHub PR state check skipped for unparseable URL");
        return GitHubState::Unknown;
    };
    fetch_pr_state_by_slug_with_token(&format!("{owner}/{repo}"), pr_number, github_token).await
}

pub(crate) async fn fetch_pr_state_by_slug_with_token(
    repo_slug: &str,
    pr_num: u64,
    github_token: Option<&str>,
) -> GitHubState {
    try_fetch_pr_state_by_slug_with_token(repo_slug, pr_num, github_token)
        .await
        .unwrap_or(GitHubState::Unknown)
}

pub(crate) async fn try_fetch_pr_state_by_slug_with_token(
    repo_slug: &str,
    pr_num: u64,
    github_token: Option<&str>,
) -> anyhow::Result<GitHubState> {
    let state = try_github_get_json::<GitHubPullState>(
        &format!("/repos/{repo_slug}/pulls/{pr_num}"),
        github_token,
    )
    .await?;
    let state = classify_pr_state(&state);
    if state == GitHubState::Unknown {
        anyhow::bail!("GitHub PR state response contained an unknown state");
    }
    Ok(state)
}

pub(crate) async fn fetch_issue_state_with_token(
    repo_slug: &str,
    issue_num: u64,
    github_token: Option<&str>,
) -> GitHubState {
    try_fetch_issue_state_with_token(repo_slug, issue_num, github_token)
        .await
        .unwrap_or(GitHubState::Unknown)
}

pub(crate) async fn try_fetch_issue_state_with_token(
    repo_slug: &str,
    issue_num: u64,
    github_token: Option<&str>,
) -> anyhow::Result<GitHubState> {
    let state = try_github_get_json::<GitHubIssueState>(
        &format!("/repos/{repo_slug}/issues/{issue_num}"),
        github_token,
    )
    .await?;
    let state = classify_issue_state(&state);
    if state == GitHubState::Unknown {
        anyhow::bail!("GitHub issue state response contained an unknown state");
    }
    Ok(state)
}

/// Fetch a PR state for admission, rejecting mismatched response identities.
pub(crate) async fn fetch_exact_pr_state_with_token(
    repo_slug: &str,
    pr_num: u64,
    github_token: Option<&str>,
) -> GitHubState {
    let Some(state) = github_get_json::<ExactGitHubPullState>(
        &format!("/repos/{repo_slug}/pulls/{pr_num}"),
        github_token,
    )
    .await
    else {
        return GitHubState::Unknown;
    };
    if state.number != pr_num || !state.base.repo.full_name.eq_ignore_ascii_case(repo_slug) {
        return GitHubState::Unknown;
    }
    classify_pr_state(&GitHubPullState {
        state: state.state,
        merged_at: state.merged_at,
    })
}

/// Fetch an issue state for admission, rejecting PRs returned by the issues API
/// and mismatched response identities.
pub(crate) async fn fetch_exact_issue_state_with_token(
    repo_slug: &str,
    issue_num: u64,
    github_token: Option<&str>,
) -> GitHubState {
    let Some(state) = github_get_json::<ExactGitHubIssueState>(
        &format!("/repos/{repo_slug}/issues/{issue_num}"),
        github_token,
    )
    .await
    else {
        return GitHubState::Unknown;
    };
    if !exact_issue_identity_matches(&state, repo_slug, issue_num) {
        return GitHubState::Unknown;
    }
    classify_issue_state(&GitHubIssueState {
        state: state.state,
        state_reason: state.state_reason,
    })
}

pub(crate) async fn fetch_exact_issue_scope_facts(
    repo_slug: &str,
    issue_num: u64,
    github_token: Option<&str>,
) -> anyhow::Result<Value> {
    let state = try_github_get_json::<ExactGitHubIssueState>(
        &format!("/repos/{repo_slug}/issues/{issue_num}"),
        github_token,
    )
    .await?;
    if !exact_issue_identity_matches(&state, repo_slug, issue_num) {
        anyhow::bail!("GitHub issue response did not match the requested issue identity");
    }
    let labels = state
        .labels
        .context("GitHub issue response omitted its labels")?;
    let updated_at = state
        .updated_at
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .context("GitHub issue response omitted its update timestamp")?;
    if state.title.trim().is_empty() || state.html_url.trim().is_empty() {
        anyhow::bail!("GitHub issue response omitted its title or canonical URL");
    }
    Ok(json!({
        "snapshot_source": "server_github_rest",
        "repo": repo_slug.to_ascii_lowercase(),
        "issue_number": issue_num,
        "title": state.title,
        "body": state.body,
        "url": state.html_url,
        "labels": labels.into_iter().map(|label| label.name).collect::<Vec<_>>(),
        "state": state.state,
        "updated_at": updated_at,
    }))
}

fn exact_issue_identity_matches(
    state: &ExactGitHubIssueState,
    repo_slug: &str,
    issue_num: u64,
) -> bool {
    state.number == issue_num
        && !state.has_pull_request_marker
        && repository_url_matches_slug(&state.repository_url, repo_slug)
        && issue_url_matches_identity(&state.html_url, repo_slug, issue_num)
}

fn issue_url_matches_identity(issue_url: &str, repo_slug: &str, issue_num: u64) -> bool {
    let Ok(url) = reqwest::Url::parse(issue_url) else {
        return false;
    };
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return false;
    }
    let Some((owner, repo)) = repo_slug.split_once('/') else {
        return false;
    };
    let Some(mut segments) = url.path_segments() else {
        return false;
    };
    matches!(
        (
            segments.next(),
            segments.next(),
            segments.next(),
            segments.next(),
            segments.next(),
        ),
        (Some(url_owner), Some(url_repo), Some("issues"), Some(url_issue), None)
            if url_owner.eq_ignore_ascii_case(owner)
                && url_repo.eq_ignore_ascii_case(repo)
                && url_issue.parse::<u64>().ok() == Some(issue_num)
    )
}

fn repository_url_matches_slug(repository_url: &str, repo_slug: &str) -> bool {
    repository_url
        .trim_end_matches('/')
        .rsplit_once("/repos/")
        .is_some_and(|(_, response_slug)| response_slug.eq_ignore_ascii_case(repo_slug))
}
