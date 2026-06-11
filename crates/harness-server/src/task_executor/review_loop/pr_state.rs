use serde::Deserialize;
use std::path::Path;
use tokio::time::Duration;

/// External state of a PR as observed via GitHub. Used to short-circuit the
/// review loop when a PR has been merged or closed outside of this task so the
/// loop does not keep invoking the reviewer agent against stale work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PrExternalState {
    Open,
    Merged,
    Closed,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PrOpenOrMergedState {
    Open,
    Merged,
}

#[derive(Debug, Deserialize)]
struct GitHubPullState {
    state: String,
    merged_at: Option<String>,
    head: Option<GitHubPullHead>,
}

#[derive(Debug, Deserialize)]
struct GitHubPullHead {
    sha: String,
}

/// Query the GitHub REST API with a 10 s timeout and classify the result.
/// Returns [`PrExternalState::Unknown`] on any transient
/// failure so callers do not abort a healthy review loop because of a flaky
/// network call.
pub(super) async fn fetch_pr_external_state(
    pr_num: u64,
    project: &Path,
    github_token: Option<&str>,
) -> PrExternalState {
    let Some(repo_slug) = crate::task_executor::pr_detection::detect_repo_slug(project).await
    else {
        tracing::debug!(
            pr = pr_num,
            "PR state check skipped because repository slug is unavailable"
        );
        return PrExternalState::Unknown;
    };
    let resolved_token = crate::github_auth::resolve_github_token(github_token);
    fetch_pr_external_state_for_repo(&repo_slug, pr_num, resolved_token.as_deref()).await
}

async fn fetch_pr_external_state_for_repo(
    repo_slug: &str,
    pr_num: u64,
    github_token: Option<&str>,
) -> PrExternalState {
    let client = reqwest::Client::new();
    let mut request = client
        .get(format!(
            "https://api.github.com/repos/{repo_slug}/pulls/{pr_num}"
        ))
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .header(reqwest::header::USER_AGENT, "harness-server");
    if let Some(token) = github_token {
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

pub(crate) async fn verify_pr_open_or_merged(
    repo_slug: &str,
    pr_num: u64,
    github_token: Option<&str>,
) -> Result<PrOpenOrMergedState, String> {
    let resolved_token =
        crate::github_auth::resolve_github_token(github_token).ok_or_else(|| {
            "GitHub token is required to verify PR open-state when hosted review is disabled; \
         set server.github_token, GITHUB_TOKEN, or GH_TOKEN"
                .to_string()
        })?;
    match fetch_pr_external_state_for_repo(repo_slug, pr_num, Some(&resolved_token)).await {
        PrExternalState::Open => Ok(PrOpenOrMergedState::Open),
        PrExternalState::Merged => Ok(PrOpenOrMergedState::Merged),
        PrExternalState::Closed => Err(format!(
            "GitHub PR {repo_slug}#{pr_num} is closed without merge"
        )),
        PrExternalState::Unknown => Err(format!(
            "GitHub PR open-state lookup could not verify {repo_slug}#{pr_num}"
        )),
    }
}

pub(crate) async fn fetch_pr_head_sha_for_gate(
    repo_slug: &str,
    pr_num: u64,
    github_token: Option<&str>,
) -> Result<String, String> {
    let resolved_token =
        crate::github_auth::resolve_github_token(github_token).ok_or_else(|| {
            "GitHub token is required to verify PR head when hosted review is disabled; \
         set server.github_token, GITHUB_TOKEN, or GH_TOKEN"
                .to_string()
        })?;
    let client = reqwest::Client::new();
    let request = client
        .get(format!(
            "https://api.github.com/repos/{repo_slug}/pulls/{pr_num}"
        ))
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .header(reqwest::header::USER_AGENT, "harness-server")
        .bearer_auth(resolved_token);
    let response = match tokio::time::timeout(Duration::from_secs(10), request.send()).await {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => {
            return Err(format!(
                "GitHub PR head lookup failed for {repo_slug}#{pr_num}: {error}"
            ))
        }
        Err(_) => {
            return Err(format!(
                "GitHub PR head lookup timed out for {repo_slug}#{pr_num}"
            ))
        }
    };
    if !response.status().is_success() {
        return Err(format!(
            "GitHub PR head lookup failed for {repo_slug}#{pr_num} with status {}",
            response.status()
        ));
    }
    let state = response.json::<GitHubPullState>().await.map_err(|error| {
        format!("GitHub PR head response was not parseable for {repo_slug}#{pr_num}: {error}")
    })?;
    state
        .head
        .map(|head| head.sha)
        .filter(|sha| !sha.trim().is_empty())
        .ok_or_else(|| {
            format!("GitHub PR head lookup returned no head SHA for {repo_slug}#{pr_num}")
        })
}

/// Returns `true` when the trailing three entries of `counts` are all `Some`
/// and the issue count is not decreasing (`c >= a && c >= b`).
///
/// Used to detect convergence failure in the review loop so the caller can
/// switch to impasse (critical-only) mode.
pub(super) fn issue_count_not_decreasing(counts: &[Option<u32>]) -> bool {
    if counts.len() < 3 {
        return false;
    }
    let tail = &counts[counts.len() - 3..];
    matches!(tail, [Some(a), Some(b), Some(c)] if c >= a && c >= b)
}

pub(super) const MAX_CONSECUTIVE_WAITS: u32 = 10;
