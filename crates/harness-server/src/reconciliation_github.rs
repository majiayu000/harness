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
pub(super) struct GitHubIssueState {
    pub(super) state: String,
    #[serde(default)]
    pub(super) state_reason: Option<String>,
}

pub(crate) fn github_api_base_url() -> String {
    std::env::var("HARNESS_GITHUB_API_BASE_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "https://api.github.com".to_string())
        .trim_end_matches('/')
        .to_string()
}

async fn github_get_json<T: DeserializeOwned>(path: &str, github_token: Option<&str>) -> Option<T> {
    let client = reqwest::Client::new();
    let mut request = client
        .get(format!("{}{}", github_api_base_url(), path))
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .header(reqwest::header::USER_AGENT, "harness-server");
    if let Some(token) = crate::github_auth::resolve_github_token(github_token) {
        request = request.bearer_auth(token);
    }
    let response = match tokio::time::timeout(Duration::from_secs(10), request.send()).await {
        Ok(Ok(response)) if response.status().is_success() => response,
        Ok(Ok(response)) => {
            tracing::debug!(status = %response.status(), path, "GitHub state check failed");
            return None;
        }
        Ok(Err(e)) => {
            tracing::debug!(error = %e, path, "GitHub state check invocation error");
            return None;
        }
        Err(_) => {
            tracing::debug!(path, "GitHub state check timed out after 10s");
            return None;
        }
    };
    response.json::<T>().await.ok()
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
    let Some((owner, repo, pr_number)) = harness_core::prompts::parse_github_pr_url(pr_url) else {
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
