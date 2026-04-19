use crate::task_executor::pr_detection::{
    build_fix_ci_prompt, build_pr_approved_prompt, build_pr_rework_prompt,
    parse_harness_mention_command, HarnessMentionCommand,
};
use crate::task_runner::CreateTaskRequest;
use harness_core::prompts::has_gemini_body_feedback;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct GitHubRepositoryRef {
    full_name: String,
}

#[derive(Debug, Deserialize)]
struct GitHubIssueRef {
    number: u64,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    html_url: Option<String>,
    #[serde(default)]
    pull_request: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct GitHubCommentRef {
    body: String,
    #[serde(default)]
    html_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GitHubIssueCommentEvent {
    action: String,
    issue: GitHubIssueRef,
    comment: GitHubCommentRef,
    repository: GitHubRepositoryRef,
}

#[derive(Debug, Deserialize)]
struct GitHubIssuesEvent {
    action: String,
    issue: GitHubIssueRef,
}

#[derive(Debug, Deserialize)]
struct GitHubPullRequestRef {
    number: u64,
    #[serde(default)]
    html_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GitHubUserRef {
    login: String,
}

#[derive(Debug, Deserialize)]
struct GitHubReviewRef {
    state: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    html_url: Option<String>,
    #[serde(default)]
    user: Option<GitHubUserRef>,
}

#[derive(Debug, Deserialize)]
struct GitHubPullRequestReviewEvent {
    action: String,
    review: GitHubReviewRef,
    pull_request: GitHubPullRequestRef,
    repository: GitHubRepositoryRef,
}

fn issue_task_request(issue_number: u64) -> CreateTaskRequest {
    let mut req = CreateTaskRequest::default();
    req.issue = Some(issue_number);
    req
}

fn review_task_request(pr_number: u64) -> CreateTaskRequest {
    let mut req = CreateTaskRequest::default();
    req.pr = Some(pr_number);
    req
}

fn pr_rework_task_request(event: &GitHubPullRequestReviewEvent) -> CreateTaskRequest {
    let mut req = CreateTaskRequest::default();
    req.prompt = Some(build_pr_rework_prompt(
        &event.repository.full_name,
        event.pull_request.number,
        &event.review.state,
        event.review.body.as_deref().unwrap_or(""),
        event.review.html_url.as_deref(),
        event.pull_request.html_url.as_deref(),
    ));
    req
}

fn pr_approved_task_request(event: &GitHubPullRequestReviewEvent) -> CreateTaskRequest {
    let mut req = CreateTaskRequest::default();
    req.prompt = Some(build_pr_approved_prompt(
        &event.repository.full_name,
        event.pull_request.number,
        event.review.html_url.as_deref(),
    ));
    req
}

fn fix_ci_task_request(payload: &GitHubIssueCommentEvent) -> CreateTaskRequest {
    let mut req = CreateTaskRequest::default();
    req.prompt = Some(build_fix_ci_prompt(
        &payload.repository.full_name,
        payload.issue.number,
        &payload.comment.body,
        payload.comment.html_url.as_deref(),
        payload.issue.html_url.as_deref(),
    ));
    req
}

pub(crate) fn is_valid_github_event_name(event: &str) -> bool {
    !event.is_empty()
        && event
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

pub(crate) fn parse_github_webhook_task_request(
    event: &str,
    payload: &[u8],
) -> Result<(Option<CreateTaskRequest>, String), String> {
    match event {
        "ping" => Ok((None, "ping".to_string())),
        "issues" => {
            let parsed: GitHubIssuesEvent = serde_json::from_slice(payload)
                .map_err(|err| format!("invalid issues payload: {err}"))?;
            if !matches!(parsed.action.as_str(), "opened" | "reopened") {
                return Ok((None, "ignored issues action".to_string()));
            }
            if parsed.issue.pull_request.is_some() {
                return Ok((None, "issues event references pull request".to_string()));
            }
            match parse_harness_mention_command(parsed.issue.body.as_deref().unwrap_or("")) {
                Some(HarnessMentionCommand::Mention) => Ok((
                    Some(issue_task_request(parsed.issue.number)),
                    "issue mention".to_string(),
                )),
                Some(HarnessMentionCommand::Review) => {
                    Ok((None, "review command ignored on issue body".to_string()))
                }
                Some(HarnessMentionCommand::FixCi) => {
                    Ok((None, "fix ci command ignored on issue body".to_string()))
                }
                None => Ok((None, "no @harness command in issue body".to_string())),
            }
        }
        "issue_comment" => {
            let parsed: GitHubIssueCommentEvent = serde_json::from_slice(payload)
                .map_err(|err| format!("invalid issue_comment payload: {err}"))?;
            if parsed.action != "created" {
                return Ok((None, "ignored issue_comment action".to_string()));
            }

            let command = match parse_harness_mention_command(&parsed.comment.body) {
                Some(command) => command,
                None => return Ok((None, "no @harness command in comment".to_string())),
            };

            if parsed.issue.pull_request.is_some() {
                return match command {
                    HarnessMentionCommand::Mention | HarnessMentionCommand::Review => Ok((
                        Some(review_task_request(parsed.issue.number)),
                        "pr review command".to_string(),
                    )),
                    HarnessMentionCommand::FixCi => Ok((
                        Some(fix_ci_task_request(&parsed)),
                        "pr fix ci command".to_string(),
                    )),
                };
            }

            match command {
                HarnessMentionCommand::Mention => Ok((
                    Some(issue_task_request(parsed.issue.number)),
                    "issue mention command".to_string(),
                )),
                HarnessMentionCommand::Review => {
                    Ok((None, "review command ignored on issue comment".to_string()))
                }
                HarnessMentionCommand::FixCi => {
                    Ok((None, "fix ci command ignored on issue comment".to_string()))
                }
            }
        }
        "pull_request_review" => {
            let parsed: GitHubPullRequestReviewEvent = serde_json::from_slice(payload)
                .map_err(|err| format!("invalid pull_request_review payload: {err}"))?;
            if parsed.action != "submitted" {
                return Ok((None, "ignored pull_request_review action".to_string()));
            }
            match parsed.review.state.to_ascii_lowercase().as_str() {
                "changes_requested" => Ok((
                    Some(pr_rework_task_request(&parsed)),
                    "pr review changes_requested".to_string(),
                )),
                "approved" => Ok((
                    Some(pr_approved_task_request(&parsed)),
                    "pr review approved".to_string(),
                )),
                "commented" => {
                    let body = parsed.review.body.as_deref().unwrap_or("").trim();
                    if body.is_empty() {
                        return Ok((None, "pr review comment: empty body ignored".to_string()));
                    }
                    let is_gemini = parsed
                        .review
                        .user
                        .as_ref()
                        .map(|u| u.login.to_ascii_lowercase().contains("gemini"))
                        .unwrap_or(false);
                    if is_gemini && !has_gemini_body_feedback(body) {
                        return Ok((
                            None,
                            "pr review comment: Gemini body lacks trigger phrases, ignored"
                                .to_string(),
                        ));
                    }
                    Ok((
                        Some(pr_rework_task_request(&parsed)),
                        "pr review comment: actionable feedback".to_string(),
                    ))
                }
                _ => Ok((None, "unsupported review state".to_string())),
            }
        }
        _ => Ok((None, "unsupported event".to_string())),
    }
}

pub(crate) fn decode_hex(value: &str) -> Option<Vec<u8>> {
    if value.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(value.len() / 2);
    let bytes = value.as_bytes();
    for pair in bytes.chunks_exact(2) {
        let high = (pair[0] as char).to_digit(16)? as u8;
        let low = (pair[1] as char).to_digit(16)? as u8;
        out.push((high << 4) | low);
    }
    Some(out)
}

pub(crate) fn verify_github_signature(
    secret: &str,
    signature_header: &str,
    payload: &[u8],
) -> bool {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    let Some(signature_hex) = signature_header.strip_prefix("sha256=") else {
        return false;
    };
    let Some(signature_bytes) = decode_hex(signature_hex) else {
        return false;
    };

    let mut mac = match Hmac::<Sha256>::new_from_slice(secret.as_bytes()) {
        Ok(mac) => mac,
        Err(_) => return false,
    };
    mac.update(payload);
    mac.verify_slice(&signature_bytes).is_ok()
}

#[cfg(test)]
#[path = "webhook_tests.rs"]
mod tests;
