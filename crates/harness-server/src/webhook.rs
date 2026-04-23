use crate::task_executor::pr_detection::{
    build_fix_ci_prompt, build_pr_approved_prompt, build_pr_rework_prompt,
    parse_harness_mention_command, HarnessMentionCommand,
};
use crate::task_runner::CreateTaskRequest;
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
    repository: GitHubRepositoryRef,
}

#[derive(Debug, Deserialize)]
struct GitHubPullRequestRef {
    number: u64,
    #[serde(default)]
    html_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GitHubReviewRef {
    state: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    html_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GitHubPullRequestReviewEvent {
    action: String,
    review: GitHubReviewRef,
    pull_request: GitHubPullRequestRef,
    repository: GitHubRepositoryRef,
}

fn issue_task_request(issue_number: u64, repo: &str) -> CreateTaskRequest {
    let mut req = CreateTaskRequest::default();
    req.issue = Some(issue_number);
    req.repo = Some(repo.to_string());
    req
}

fn review_task_request(pr_number: u64, repo: &str) -> CreateTaskRequest {
    let mut req = CreateTaskRequest::default();
    req.pr = Some(pr_number);
    req.repo = Some(repo.to_string());
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
    req.repo = Some(event.repository.full_name.clone());
    req
}

fn pr_approved_task_request(event: &GitHubPullRequestReviewEvent) -> CreateTaskRequest {
    let mut req = CreateTaskRequest::default();
    req.prompt = Some(build_pr_approved_prompt(
        &event.repository.full_name,
        event.pull_request.number,
        event.review.html_url.as_deref(),
    ));
    req.repo = Some(event.repository.full_name.clone());
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
    req.repo = Some(payload.repository.full_name.clone());
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
                    Some(issue_task_request(
                        parsed.issue.number,
                        &parsed.repository.full_name,
                    )),
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
                        Some(review_task_request(
                            parsed.issue.number,
                            &parsed.repository.full_name,
                        )),
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
                    Some(issue_task_request(
                        parsed.issue.number,
                        &parsed.repository.full_name,
                    )),
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
mod tests {
    use super::*;

    #[test]
    fn decode_hex_accepts_valid_input() {
        assert_eq!(decode_hex("00ff10"), Some(vec![0x00, 0xff, 0x10]));
    }

    #[test]
    fn decode_hex_rejects_invalid_input() {
        assert!(decode_hex("abc").is_none());
        assert!(decode_hex("zz").is_none());
    }

    #[test]
    fn verify_github_signature_accepts_valid_hmac() {
        use hmac::{Hmac, Mac};
        use sha2::Sha256;

        let secret = "top-secret";
        let payload = br#"{"ok":true}"#;
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(payload);
        let signature = mac.finalize().into_bytes();
        let signature_hex: String = signature.iter().map(|byte| format!("{byte:02x}")).collect();
        let header = format!("sha256={signature_hex}");

        assert!(verify_github_signature(secret, &header, payload));
    }

    #[test]
    fn verify_github_signature_rejects_invalid_hmac() {
        let secret = "top-secret";
        let payload = br#"{"ok":true}"#;
        assert!(!verify_github_signature(
            secret,
            "sha256=0000000000000000000000000000000000000000000000000000000000000000",
            payload,
        ));
    }

    #[test]
    fn parse_issue_comment_issue_mention_maps_to_issue_request() {
        let payload = serde_json::json!({
            "action": "created",
            "issue": { "number": 106 },
            "comment": { "body": "@harness please handle this issue" },
            "repository": { "full_name": "majiayu000/harness" }
        });

        let (request, _) =
            parse_github_webhook_task_request("issue_comment", payload.to_string().as_bytes())
                .unwrap();
        let request = request.expect("request should exist");
        assert_eq!(request.issue, Some(106));
        assert_eq!(request.pr, None);
        assert_eq!(request.prompt, None);
        assert_eq!(request.repo.as_deref(), Some("majiayu000/harness"));
    }

    #[test]
    fn parse_issue_comment_pr_review_maps_to_pr_request() {
        let payload = serde_json::json!({
            "action": "created",
            "issue": { "number": 42, "pull_request": { "url": "https://api.github.com/repos/majiayu000/harness/pulls/42" } },
            "comment": { "body": "@harness review" },
            "repository": { "full_name": "majiayu000/harness" }
        });

        let (request, _) =
            parse_github_webhook_task_request("issue_comment", payload.to_string().as_bytes())
                .unwrap();
        let request = request.expect("request should exist");
        assert_eq!(request.issue, None);
        assert_eq!(request.pr, Some(42));
        assert_eq!(request.prompt, None);
        assert_eq!(request.repo.as_deref(), Some("majiayu000/harness"));
    }

    #[test]
    fn parse_issue_comment_fix_ci_maps_to_prompt_request() {
        let payload = serde_json::json!({
            "action": "created",
            "issue": {
                "number": 42,
                "html_url": "https://github.com/majiayu000/harness/pull/42",
                "pull_request": { "url": "https://api.github.com/repos/majiayu000/harness/pulls/42" }
            },
            "comment": {
                "body": "@harness fix ci",
                "html_url": "https://github.com/majiayu000/harness/issues/42#issuecomment-1"
            },
            "repository": { "full_name": "majiayu000/harness" }
        });

        let (request, _) =
            parse_github_webhook_task_request("issue_comment", payload.to_string().as_bytes())
                .unwrap();
        let request = request.expect("request should exist");
        assert_eq!(request.issue, None);
        assert_eq!(request.pr, None);
        assert_eq!(request.repo.as_deref(), Some("majiayu000/harness"));
        assert!(request
            .prompt
            .as_deref()
            .unwrap_or_default()
            .contains("CI failure repair requested for PR #42"));
    }

    #[test]
    fn parse_issues_opened_with_harness_mention_creates_issue_task() {
        let payload = serde_json::json!({
            "action": "opened",
            "issue": {
                "number": 77,
                "body": "@harness please implement this feature"
            },
            "repository": { "full_name": "org/repo" }
        });

        let (request, reason) =
            parse_github_webhook_task_request("issues", payload.to_string().as_bytes()).unwrap();
        let request = request.expect("request should exist");
        assert_eq!(reason, "issue mention");
        assert_eq!(request.issue, Some(77));
        assert_eq!(request.pr, None);
        assert_eq!(request.prompt, None);
        assert_eq!(request.repo.as_deref(), Some("org/repo"));
    }

    #[test]
    fn parse_issues_reopened_with_harness_mention_creates_issue_task() {
        let payload = serde_json::json!({
            "action": "reopened",
            "issue": {
                "number": 88,
                "body": "This is still broken, @harness fix it"
            },
            "repository": { "full_name": "org/repo" }
        });

        let (request, reason) =
            parse_github_webhook_task_request("issues", payload.to_string().as_bytes()).unwrap();
        let request = request.expect("request should exist");
        assert_eq!(reason, "issue mention");
        assert_eq!(request.issue, Some(88));
        assert_eq!(request.repo.as_deref(), Some("org/repo"));
    }

    #[test]
    fn parse_issues_opened_without_harness_mention_is_ignored() {
        let payload = serde_json::json!({
            "action": "opened",
            "issue": {
                "number": 99,
                "body": "This issue has no harness mention"
            },
            "repository": { "full_name": "org/repo" }
        });

        let (request, reason) =
            parse_github_webhook_task_request("issues", payload.to_string().as_bytes()).unwrap();
        assert!(request.is_none());
        assert_eq!(reason, "no @harness command in issue body");
    }

    #[test]
    fn parse_issues_opened_with_no_body_is_ignored() {
        let payload = serde_json::json!({
            "action": "opened",
            "issue": { "number": 100 },
            "repository": { "full_name": "org/repo" }
        });

        let (request, reason) =
            parse_github_webhook_task_request("issues", payload.to_string().as_bytes()).unwrap();
        assert!(request.is_none());
        assert_eq!(reason, "no @harness command in issue body");
    }

    #[test]
    fn parse_issues_edited_action_is_ignored() {
        let payload = serde_json::json!({
            "action": "edited",
            "issue": {
                "number": 106,
                "body": "@harness please handle this issue"
            },
            "repository": { "full_name": "org/repo" }
        });

        let (request, reason) =
            parse_github_webhook_task_request("issues", payload.to_string().as_bytes()).unwrap();
        assert!(request.is_none());
        assert_eq!(reason, "ignored issues action");
    }

    #[test]
    fn parse_unsupported_event_returns_static_reason() {
        let (request, reason) =
            parse_github_webhook_task_request("unknown_event", br#"{}"#).unwrap();
        assert!(request.is_none());
        assert_eq!(reason, "unsupported event");
    }

    #[test]
    fn github_event_name_validation_is_strict() {
        assert!(is_valid_github_event_name("issue_comment"));
        assert!(is_valid_github_event_name("pull_request"));
        assert!(!is_valid_github_event_name("issue-comment"));
        assert!(!is_valid_github_event_name("IssueComment"));
        assert!(!is_valid_github_event_name("issue comment"));
    }

    #[test]
    fn github_event_name_rejects_empty() {
        assert!(!is_valid_github_event_name(""));
    }

    #[test]
    fn ping_event_returns_none_request() {
        let (request, reason) =
            parse_github_webhook_task_request("ping", br#"{"zen":"anything"}"#).unwrap();
        assert!(request.is_none());
        assert_eq!(reason, "ping");
    }

    #[test]
    fn issue_comment_non_created_action_is_ignored() {
        let payload = serde_json::json!({
            "action": "deleted",
            "issue": { "number": 1 },
            "comment": { "body": "@harness review" },
            "repository": { "full_name": "org/repo" }
        });
        let (request, reason) =
            parse_github_webhook_task_request("issue_comment", payload.to_string().as_bytes())
                .unwrap();
        assert!(request.is_none());
        assert_eq!(reason, "ignored issue_comment action");
    }

    #[test]
    fn issue_comment_without_mention_is_ignored() {
        let payload = serde_json::json!({
            "action": "created",
            "issue": { "number": 1 },
            "comment": { "body": "just a regular comment" },
            "repository": { "full_name": "org/repo" }
        });
        let (request, reason) =
            parse_github_webhook_task_request("issue_comment", payload.to_string().as_bytes())
                .unwrap();
        assert!(request.is_none());
        assert_eq!(reason, "no @harness command in comment");
    }

    #[test]
    fn issues_event_with_pull_request_ref_is_ignored() {
        let payload = serde_json::json!({
            "action": "opened",
            "issue": {
                "number": 5,
                "body": "@harness handle",
                "pull_request": { "url": "https://api.github.com/repos/org/repo/pulls/5" }
            },
            "repository": { "full_name": "org/repo" }
        });
        let (request, reason) =
            parse_github_webhook_task_request("issues", payload.to_string().as_bytes()).unwrap();
        assert!(request.is_none());
        assert_eq!(reason, "issues event references pull request");
    }

    #[test]
    fn invalid_payload_json_returns_error() {
        let result = parse_github_webhook_task_request("issues", b"not valid json");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("invalid issues payload"));
    }

    #[test]
    fn verify_github_signature_rejects_missing_prefix() {
        assert!(!verify_github_signature("secret", "deadbeef", b"payload"));
    }

    #[test]
    fn verify_github_signature_rejects_invalid_hex() {
        assert!(!verify_github_signature(
            "secret",
            "sha256=zzzz",
            b"payload"
        ));
    }

    #[test]
    fn pull_request_review_changes_requested_creates_rework_task() {
        let payload = serde_json::json!({
            "action": "submitted",
            "review": {
                "state": "changes_requested",
                "body": "Please fix the error handling in this function.",
                "html_url": "https://github.com/org/repo/pull/10#pullrequestreview-1"
            },
            "pull_request": {
                "number": 10,
                "html_url": "https://github.com/org/repo/pull/10"
            },
            "repository": { "full_name": "org/repo" }
        });

        let (request, reason) = parse_github_webhook_task_request(
            "pull_request_review",
            payload.to_string().as_bytes(),
        )
        .unwrap();
        let request = request.expect("request should exist");
        assert_eq!(reason, "pr review changes_requested");
        assert_eq!(request.repo.as_deref(), Some("org/repo"));
        let prompt = request.prompt.unwrap();
        assert!(prompt.contains("PR #10"));
        assert!(prompt.contains("org/repo"));
        assert!(prompt.contains("changes_requested"));
        assert!(prompt.contains("Please fix the error handling"));
    }

    #[test]
    fn pull_request_review_approved_creates_approved_task() {
        let payload = serde_json::json!({
            "action": "submitted",
            "review": {
                "state": "approved",
                "body": "LGTM!",
                "html_url": "https://github.com/org/repo/pull/10#pullrequestreview-2"
            },
            "pull_request": {
                "number": 10,
                "html_url": "https://github.com/org/repo/pull/10"
            },
            "repository": { "full_name": "org/repo" }
        });

        let (request, reason) = parse_github_webhook_task_request(
            "pull_request_review",
            payload.to_string().as_bytes(),
        )
        .unwrap();
        let request = request.expect("request should exist");
        assert_eq!(reason, "pr review approved");
        assert_eq!(request.repo.as_deref(), Some("org/repo"));
        let prompt = request.prompt.unwrap();
        assert!(prompt.contains("PR #10"));
        assert!(prompt.contains("org/repo"));
        assert!(prompt.contains("approved"));
        assert!(prompt.contains("ready to merge"));
    }

    #[test]
    fn pull_request_review_commented_with_body_creates_rework_task() {
        let payload = serde_json::json!({
            "action": "submitted",
            "review": {
                "state": "commented",
                "body": "Consider renaming this variable for clarity.",
                "html_url": "https://github.com/org/repo/pull/10#pullrequestreview-3"
            },
            "pull_request": {
                "number": 10,
                "html_url": "https://github.com/org/repo/pull/10"
            },
            "repository": { "full_name": "org/repo" }
        });

        let (request, reason) = parse_github_webhook_task_request(
            "pull_request_review",
            payload.to_string().as_bytes(),
        )
        .unwrap();
        let request = request.expect("request should exist");
        assert_eq!(reason, "pr review comment: actionable feedback");
        assert_eq!(request.repo.as_deref(), Some("org/repo"));
        let prompt = request.prompt.unwrap();
        assert!(prompt.contains("Consider renaming this variable"));
    }

    #[test]
    fn pull_request_review_commented_with_empty_body_is_ignored() {
        let payload = serde_json::json!({
            "action": "submitted",
            "review": {
                "state": "commented",
                "body": "",
                "html_url": "https://github.com/org/repo/pull/10#pullrequestreview-4"
            },
            "pull_request": {
                "number": 10,
                "html_url": "https://github.com/org/repo/pull/10"
            },
            "repository": { "full_name": "org/repo" }
        });

        let (request, reason) = parse_github_webhook_task_request(
            "pull_request_review",
            payload.to_string().as_bytes(),
        )
        .unwrap();
        assert!(request.is_none());
        assert_eq!(reason, "pr review comment: empty body ignored");
    }

    #[test]
    fn pull_request_review_non_submitted_action_is_ignored() {
        let payload = serde_json::json!({
            "action": "dismissed",
            "review": {
                "state": "changes_requested",
                "body": "Some feedback"
            },
            "pull_request": { "number": 10 },
            "repository": { "full_name": "org/repo" }
        });

        let (request, reason) = parse_github_webhook_task_request(
            "pull_request_review",
            payload.to_string().as_bytes(),
        )
        .unwrap();
        assert!(request.is_none());
        assert_eq!(reason, "ignored pull_request_review action");
    }

    #[test]
    fn pull_request_review_invalid_payload_returns_error() {
        let result = parse_github_webhook_task_request("pull_request_review", b"not valid json");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("invalid pull_request_review payload"));
    }

    #[test]
    fn pull_request_review_state_matching_is_case_insensitive() {
        let payload = serde_json::json!({
            "action": "submitted",
            "review": {
                "state": "APPROVED",
                "body": "Looks good",
                "html_url": "https://github.com/org/repo/pull/10#pullrequestreview-5"
            },
            "pull_request": {
                "number": 10,
                "html_url": "https://github.com/org/repo/pull/10"
            },
            "repository": { "full_name": "org/repo" }
        });

        let (request, reason) = parse_github_webhook_task_request(
            "pull_request_review",
            payload.to_string().as_bytes(),
        )
        .unwrap();
        assert!(request.is_some());
        assert_eq!(reason, "pr review approved");
    }

    #[test]
    fn pull_request_review_unsupported_state_is_ignored() {
        let payload = serde_json::json!({
            "action": "submitted",
            "review": {
                "state": "pending",
                "body": "Still reviewing"
            },
            "pull_request": { "number": 10 },
            "repository": { "full_name": "org/repo" }
        });

        let (request, reason) = parse_github_webhook_task_request(
            "pull_request_review",
            payload.to_string().as_bytes(),
        )
        .unwrap();
        assert!(request.is_none());
        assert_eq!(reason, "unsupported review state");
    }
}
