use serde::de::DeserializeOwned;
use serde::Deserialize;
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
    state: String,
    #[serde(default)]
    state_reason: Option<String>,
    #[serde(default)]
    pull_request: Option<serde_json::Value>,
}

pub(crate) use crate::github_client::github_api_base_url;

const GITHUB_STATE_TIMEOUT: Duration = Duration::from_secs(10);

async fn github_get_json<T: DeserializeOwned>(path: &str, github_token: Option<&str>) -> Option<T> {
    github_get_json_with_timeout(path, github_token, GITHUB_STATE_TIMEOUT).await
}

pub(super) async fn github_get_json_with_timeout<T: DeserializeOwned>(
    path: &str,
    github_token: Option<&str>,
    timeout: Duration,
) -> Option<T> {
    let client = reqwest::Client::new();
    github_get_json_with_client_timeout(&client, path, github_token, timeout).await
}

pub(super) async fn github_get_json_with_client_timeout<T: DeserializeOwned>(
    client: &reqwest::Client,
    path: &str,
    github_token: Option<&str>,
    timeout: Duration,
) -> Option<T> {
    let mut request = client
        .get(format!("{}{}", github_api_base_url(), path))
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .header(reqwest::header::USER_AGENT, "harness-server");
    if let Some(token) = crate::github_auth::resolve_github_token(github_token) {
        request = request.bearer_auth(token);
    }
    let response = tokio::time::timeout(timeout, async {
        let response = request.send().await?;
        let status = response.status();
        if !status.is_success() {
            return Ok((status, None));
        }
        let value = response.json::<T>().await?;
        Ok::<_, reqwest::Error>((status, Some(value)))
    })
    .await;
    match response {
        Ok(Ok((_, Some(value)))) => Some(value),
        Ok(Ok((status, None))) => {
            tracing::debug!(%status, path, "GitHub state check failed");
            None
        }
        Ok(Err(e)) => {
            tracing::debug!(error = %e, path, "GitHub state check invocation error");
            None
        }
        Err(_) => {
            tracing::debug!(
                path,
                timeout_ms = timeout.as_millis(),
                "GitHub state check timed out"
            );
            None
        }
    }
}

pub(super) fn classify_pr_state(state: &GitHubPullState) -> GitHubState {
    let merged_at_empty = state.merged_at.as_deref().unwrap_or("").trim().is_empty();
    match (state.state.as_str(), merged_at_empty) {
        ("open", _) | ("OPEN", _) => GitHubState::Open,
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
    let Some(state) = github_get_json::<GitHubPullState>(
        &format!("/repos/{repo_slug}/pulls/{pr_num}"),
        github_token,
    )
    .await
    else {
        return GitHubState::Unknown;
    };
    classify_pr_state(&state)
}

pub(crate) async fn fetch_issue_state_with_token(
    repo_slug: &str,
    issue_num: u64,
    github_token: Option<&str>,
) -> GitHubState {
    let Some(state) = github_get_json::<GitHubIssueState>(
        &format!("/repos/{repo_slug}/issues/{issue_num}"),
        github_token,
    )
    .await
    else {
        return GitHubState::Unknown;
    };
    classify_issue_state(&state)
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
    if state.number != issue_num
        || state.pull_request.is_some()
        || !repository_url_matches_slug(&state.repository_url, repo_slug)
    {
        return GitHubState::Unknown;
    }
    classify_issue_state(&GitHubIssueState {
        state: state.state,
        state_reason: state.state_reason,
    })
}

fn repository_url_matches_slug(repository_url: &str, repo_slug: &str) -> bool {
    repository_url
        .trim_end_matches('/')
        .rsplit_once("/repos/")
        .is_some_and(|(_, response_slug)| response_slug.eq_ignore_ascii_case(repo_slug))
}
