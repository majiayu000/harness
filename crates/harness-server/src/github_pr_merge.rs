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
    use axum::routing::put;
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
