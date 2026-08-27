use crate::github_pr_snapshot::GitHubPrSnapshotTarget;
use harness_core::config::intake::GitHubMergeMethod;
use harness_workflow::runtime::ActivityErrorKind;
use reqwest::header::{ACCEPT, USER_AGENT};
use reqwest::StatusCode;
use serde_json::{json, Value};
use std::fmt;
use std::time::Duration;

const GITHUB_MERGE_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GitHubPrMergeOptions {
    pub method: GitHubMergeMethod,
    pub expected_head_sha: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct GitHubPrMergeOutcome {
    pub merged: bool,
    pub message: Option<String>,
    pub sha: Option<String>,
    pub raw: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GitHubPrMergeError {
    pub error_kind: ActivityErrorKind,
    pub message: String,
    pub status_code: Option<u16>,
    pub response_body: Option<String>,
}

impl GitHubPrMergeError {
    fn configuration(message: impl Into<String>) -> Self {
        Self {
            error_kind: ActivityErrorKind::Configuration,
            message: message.into(),
            status_code: None,
            response_body: None,
        }
    }

    fn external(message: impl Into<String>) -> Self {
        Self {
            error_kind: ActivityErrorKind::ExternalDependency,
            message: message.into(),
            status_code: None,
            response_body: None,
        }
    }

    fn fatal(message: impl Into<String>) -> Self {
        Self {
            error_kind: ActivityErrorKind::Fatal,
            message: message.into(),
            status_code: None,
            response_body: None,
        }
    }

    fn status(status: StatusCode, body: String) -> Self {
        let message = github_error_message(&body)
            .unwrap_or_else(|| format!("GitHub pull request merge failed with status {status}"));
        Self {
            error_kind: merge_error_kind_for_status(status),
            message,
            status_code: Some(status.as_u16()),
            response_body: Some(body),
        }
    }
}

impl fmt::Display for GitHubPrMergeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.status_code {
            Some(status) => write!(
                f,
                "GitHub merge failed with status {status}: {}",
                self.message
            ),
            None => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for GitHubPrMergeError {}

pub(crate) async fn merge_pull_request(
    target: &GitHubPrSnapshotTarget,
    github_token: Option<&str>,
    options: &GitHubPrMergeOptions,
) -> Result<GitHubPrMergeOutcome, GitHubPrMergeError> {
    let client = reqwest::Client::new();
    merge_pull_request_with_client(
        &client,
        &crate::reconciliation::github_api_base_url(),
        target,
        github_token,
        options,
    )
    .await
}

pub(crate) async fn merge_pull_request_with_client(
    client: &reqwest::Client,
    api_base_url: &str,
    target: &GitHubPrSnapshotTarget,
    github_token: Option<&str>,
    options: &GitHubPrMergeOptions,
) -> Result<GitHubPrMergeOutcome, GitHubPrMergeError> {
    let token = crate::github_auth::resolve_github_token(github_token).ok_or_else(|| {
        GitHubPrMergeError::configuration(
            "server-executed merge requires a GitHub token with pull request merge permission",
        )
    })?;
    let Some((owner, repo)) = target.repo_slug.split_once('/') else {
        return Err(GitHubPrMergeError::configuration(format!(
            "invalid GitHub repo slug `{}`",
            target.repo_slug
        )));
    };
    let mut body = json!({
        "merge_method": options.method.to_string(),
    });
    if let Some(expected_head_sha) = options
        .expected_head_sha
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        body["sha"] = json!(expected_head_sha);
    }
    let url = format!(
        "{}/repos/{owner}/{repo}/pulls/{}/merge",
        api_base_url.trim_end_matches('/'),
        target.pr_number
    );
    let request = client
        .put(url)
        .header(ACCEPT, "application/vnd.github+json")
        .header(USER_AGENT, "harness-server")
        .bearer_auth(token)
        .json(&body);
    let response = tokio::time::timeout(GITHUB_MERGE_TIMEOUT, request.send())
        .await
        .map_err(|_| GitHubPrMergeError::external("GitHub pull request merge timed out"))?
        .map_err(|error| {
            GitHubPrMergeError::external(format!(
                "GitHub pull request merge request failed: {error}"
            ))
        })?;
    let status = response.status();
    let body = response.text().await.map_err(|error| {
        GitHubPrMergeError::external(format!(
            "GitHub pull request merge response could not be read: {error}"
        ))
    })?;
    if !status.is_success() {
        return Err(GitHubPrMergeError::status(status, body));
    }
    Ok(merge_outcome_from_body(&body))
}

pub(crate) async fn delete_pull_request_head_branch(
    target: &GitHubPrSnapshotTarget,
    github_token: Option<&str>,
    head_ref: &str,
    expected_head_sha: &str,
) -> Result<Value, GitHubPrMergeError> {
    let client = reqwest::Client::new();
    delete_pull_request_head_branch_with_client(
        &client,
        &crate::reconciliation::github_api_base_url(),
        target,
        github_token,
        head_ref,
        expected_head_sha,
    )
    .await
}

async fn delete_pull_request_head_branch_with_client(
    client: &reqwest::Client,
    api_base_url: &str,
    target: &GitHubPrSnapshotTarget,
    github_token: Option<&str>,
    head_ref: &str,
    expected_head_sha: &str,
) -> Result<Value, GitHubPrMergeError> {
    let token = crate::github_auth::resolve_github_token(github_token).ok_or_else(|| {
        GitHubPrMergeError::configuration(
            "server-executed branch cleanup requires a GitHub token with ref deletion permission",
        )
    })?;
    let Some((owner, repo)) = target.repo_slug.split_once('/') else {
        return Err(GitHubPrMergeError::configuration(format!(
            "invalid GitHub repo slug `{}`",
            target.repo_slug
        )));
    };
    let head_ref = head_ref.trim();
    if head_ref.is_empty() {
        return Err(GitHubPrMergeError::configuration(
            "server-executed branch cleanup requires a non-empty PR head ref",
        ));
    }
    let mut url = reqwest::Url::parse(api_base_url).map_err(|error| {
        GitHubPrMergeError::configuration(format!("invalid GitHub API base URL: {error}"))
    })?;
    url.path_segments_mut()
        .map_err(|_| GitHubPrMergeError::configuration("GitHub API base URL cannot be a base"))?
        .pop_if_empty()
        .extend(["repos", owner, repo, "git", "refs", "heads", head_ref]);
    let observed = tokio::time::timeout(
        GITHUB_MERGE_TIMEOUT,
        client
            .get(url.clone())
            .header(ACCEPT, "application/vnd.github+json")
            .header(USER_AGENT, "harness-server")
            .bearer_auth(&token)
            .send(),
    )
    .await
    .map_err(|_| GitHubPrMergeError::external("GitHub branch verification timed out"))?
    .map_err(|error| {
        GitHubPrMergeError::external(format!(
            "GitHub branch verification request failed: {error}"
        ))
    })?;
    let observed_status = observed.status();
    let observed_body = observed.text().await.map_err(|error| {
        GitHubPrMergeError::external(format!(
            "GitHub branch verification response could not be read: {error}"
        ))
    })?;
    if !observed_status.is_success() {
        return Err(GitHubPrMergeError::status(observed_status, observed_body));
    }
    let observed_sha = serde_json::from_str::<Value>(&observed_body)
        .ok()
        .and_then(|value| {
            value
                .pointer("/object/sha")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
        .ok_or_else(|| GitHubPrMergeError::external("GitHub ref response is missing object.sha"))?;
    if observed_sha != expected_head_sha {
        return Err(GitHubPrMergeError::fatal(format!(
            "ref heads/{head_ref} advanced from assessed head {expected_head_sha} to {observed_sha}; refusing branch deletion"
        )));
    }
    let response = tokio::time::timeout(
        GITHUB_MERGE_TIMEOUT,
        client
            .delete(url)
            .header(ACCEPT, "application/vnd.github+json")
            .header(USER_AGENT, "harness-server")
            .bearer_auth(token)
            .send(),
    )
    .await
    .map_err(|_| GitHubPrMergeError::external("GitHub branch deletion timed out"))?
    .map_err(|error| {
        GitHubPrMergeError::external(format!("GitHub branch deletion request failed: {error}"))
    })?;
    let status = response.status();
    let body = response.text().await.map_err(|error| {
        GitHubPrMergeError::external(format!(
            "GitHub branch deletion response could not be read: {error}"
        ))
    })?;
    if status == StatusCode::NOT_FOUND {
        return Ok(json!({"status": "already_absent", "head_ref": head_ref}));
    }
    if !status.is_success() {
        return Err(GitHubPrMergeError::status(status, body));
    }
    Ok(json!({"status": "deleted", "head_ref": head_ref}))
}

fn merge_outcome_from_body(body: &str) -> GitHubPrMergeOutcome {
    let raw = serde_json::from_str::<Value>(body).unwrap_or_else(|_| json!({ "raw": body }));
    GitHubPrMergeOutcome {
        merged: raw.get("merged").and_then(Value::as_bool).unwrap_or(false),
        message: raw
            .get("message")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .map(ToOwned::to_owned),
        sha: raw
            .get("sha")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .map(ToOwned::to_owned),
        raw,
    }
}

fn github_error_message(body: &str) -> Option<String> {
    serde_json::from_str::<Value>(body).ok().and_then(|value| {
        value
            .get("message")
            .and_then(Value::as_str)
            .filter(|message| !message.trim().is_empty())
            .map(ToOwned::to_owned)
    })
}

fn merge_error_kind_for_status(status: StatusCode) -> ActivityErrorKind {
    match status.as_u16() {
        401 | 403 => ActivityErrorKind::Configuration,
        405 | 409 | 422 => ActivityErrorKind::Fatal,
        408 | 429 | 500..=599 => ActivityErrorKind::ExternalDependency,
        _ if status.is_client_error() => ActivityErrorKind::Fatal,
        _ => ActivityErrorKind::ExternalDependency,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::{Path, State};
    use axum::http::{HeaderMap, StatusCode as AxumStatusCode};
    use axum::routing::{get, put};
    use axum::{Json, Router};
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    #[tokio::test]
    async fn merge_pull_request_sends_rest_merge_request() -> anyhow::Result<()> {
        #[derive(Clone, Default)]
        struct Captured {
            payload: Arc<Mutex<Option<Value>>>,
            authorization: Arc<Mutex<Option<String>>>,
        }

        async fn handler(
            State(captured): State<Captured>,
            Path((owner, repo, pr)): Path<(String, String, u64)>,
            headers: HeaderMap,
            Json(payload): Json<Value>,
        ) -> Json<Value> {
            assert_eq!(owner, "owner");
            assert_eq!(repo, "repo");
            assert_eq!(pr, 77);
            *captured.payload.lock().expect("payload lock") = Some(payload);
            *captured.authorization.lock().expect("authorization lock") = headers
                .get(axum::http::header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .map(ToOwned::to_owned);
            Json(json!({
                "merged": true,
                "message": "Pull Request successfully merged",
                "sha": "merge-sha"
            }))
        }

        let captured = Captured::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let app = Router::new()
            .route("/repos/{owner}/{repo}/pulls/{pr}/merge", put(handler))
            .with_state(captured.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await });

        let target = GitHubPrSnapshotTarget::new("owner/repo", 77)?;
        let outcome = merge_pull_request_with_client(
            &reqwest::Client::new(),
            &format!("http://{addr}"),
            &target,
            Some("cfg-token"),
            &GitHubPrMergeOptions {
                method: GitHubMergeMethod::Squash,
                expected_head_sha: Some("head-sha".to_string()),
            },
        )
        .await?;

        assert!(outcome.merged);
        assert_eq!(outcome.sha.as_deref(), Some("merge-sha"));
        assert_eq!(
            captured.payload.lock().expect("payload lock").as_ref(),
            Some(&json!({
                "merge_method": "squash",
                "sha": "head-sha",
            }))
        );
        assert_eq!(
            captured
                .authorization
                .lock()
                .expect("authorization lock")
                .as_deref(),
            Some("Bearer cfg-token")
        );
        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn branch_cleanup_deletes_the_exact_head_ref() -> anyhow::Result<()> {
        #[derive(Clone, Default)]
        struct Captured(Arc<Mutex<Option<String>>>);

        async fn handler(
            State(captured): State<Captured>,
            Path(branch): Path<String>,
        ) -> AxumStatusCode {
            *captured.0.lock().expect("branch lock") = Some(branch);
            AxumStatusCode::NO_CONTENT
        }

        async fn ref_handler() -> Json<Value> {
            Json(json!({"object": {"sha": "head-sha"}}))
        }

        let captured = Captured::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let app = Router::new()
            .route(
                "/repos/owner/repo/git/refs/heads/{*branch}",
                get(ref_handler).delete(handler),
            )
            .with_state(captured.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        let target = GitHubPrSnapshotTarget::new("owner/repo", 77)?;

        let outcome = delete_pull_request_head_branch_with_client(
            &reqwest::Client::new(),
            &format!("http://{addr}"),
            &target,
            Some("cfg-token"),
            "feature/nested",
            "head-sha",
        )
        .await?;

        assert_eq!(outcome["status"], "deleted");
        assert_eq!(
            captured.0.lock().expect("branch lock").as_deref(),
            Some("feature/nested")
        );
        *captured.0.lock().expect("branch lock") = None;
        let error = delete_pull_request_head_branch_with_client(
            &reqwest::Client::new(),
            &format!("http://{addr}"),
            &target,
            Some("cfg-token"),
            "feature/nested",
            "stale-head",
        )
        .await
        .expect_err("advanced branch must not be deleted");
        assert_eq!(error.error_kind, ActivityErrorKind::Fatal);
        assert_eq!(*captured.0.lock().expect("branch lock"), None);
        server.abort();
        Ok(())
    }

    #[test]
    fn merge_statuses_map_to_existing_error_kinds() {
        assert_eq!(
            merge_error_kind_for_status(StatusCode::UNAUTHORIZED),
            ActivityErrorKind::Configuration
        );
        assert_eq!(
            merge_error_kind_for_status(StatusCode::FORBIDDEN),
            ActivityErrorKind::Configuration
        );
        assert_eq!(
            merge_error_kind_for_status(StatusCode::METHOD_NOT_ALLOWED),
            ActivityErrorKind::Fatal
        );
        assert_eq!(
            merge_error_kind_for_status(StatusCode::CONFLICT),
            ActivityErrorKind::Fatal
        );
        assert_eq!(
            merge_error_kind_for_status(StatusCode::UNPROCESSABLE_ENTITY),
            ActivityErrorKind::Fatal
        );
        assert_eq!(
            merge_error_kind_for_status(StatusCode::INTERNAL_SERVER_ERROR),
            ActivityErrorKind::ExternalDependency
        );
        assert_eq!(
            merge_error_kind_for_status(AxumStatusCode::TOO_MANY_REQUESTS),
            ActivityErrorKind::ExternalDependency
        );
    }
}
