use crate::task_executor::pr_detection::{
    build_fix_ci_prompt, build_pr_approved_prompt, parse_harness_mention_command,
    HarnessMentionCommand,
};
use crate::task_runner::CreateTaskRequest;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct GitHubRepositoryRef {
    full_name: String,
}

#[derive(Debug, Deserialize)]
struct GitHubLabelRef {
    #[serde(default)]
    name: String,
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
    #[serde(default)]
    labels: Vec<GitHubLabelRef>,
}

impl GitHubIssueRef {
    fn has_label(&self, label: &str) -> bool {
        self.labels.iter().any(|l| l.name == label)
    }
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
    /// The label that was just added — present on the `labeled` action.
    #[serde(default)]
    label: Option<GitHubLabelRef>,
}

#[derive(Debug, Deserialize)]
struct GitHubPullRequestRef {
    number: u64,
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
    req.source = Some("github".to_string());
    req.external_id = Some(format!("issue:{issue_number}"));
    req
}

fn review_task_request(pr_number: u64, repo: &str) -> CreateTaskRequest {
    let mut req = CreateTaskRequest::default();
    req.pr = Some(pr_number);
    req.repo = Some(repo.to_string());
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
    autonomous_issues: bool,
    autonomous_label: Option<&str>,
) -> Result<(Option<CreateTaskRequest>, String), String> {
    match event {
        "ping" => Ok((None, "ping".to_string())),
        "issues" => {
            let parsed: GitHubIssuesEvent = serde_json::from_slice(payload)
                .map_err(|err| format!("invalid issues payload: {err}"))?;
            if parsed.issue.pull_request.is_some() {
                return Ok((None, "issues event references pull request".to_string()));
            }
            let enqueue = || issue_task_request(parsed.issue.number, &parsed.repository.full_name);
            match parsed.action.as_str() {
                "opened" | "reopened" => {
                    match parse_harness_mention_command(parsed.issue.body.as_deref().unwrap_or(""))
                    {
                        Some(HarnessMentionCommand::Mention) => {
                            Ok((Some(enqueue()), "issue mention".to_string()))
                        }
                        Some(HarnessMentionCommand::Review) => {
                            Ok((None, "review command ignored on issue body".to_string()))
                        }
                        Some(HarnessMentionCommand::FixCi) => {
                            Ok((None, "fix ci command ignored on issue body".to_string()))
                        }
                        // In webhook/hybrid intake mode, an opened/reopened issue is
                        // enqueued directly — no `@harness` mention required — so the
                        // webhook drives autonomous intake the way the poller does. The
                        // configured label filter is honored: with a non-empty label,
                        // only issues carrying it are enqueued.
                        None if autonomous_issues => match autonomous_label {
                            Some(label) if !label.is_empty() && !parsed.issue.has_label(label) => {
                                Ok((
                                    None,
                                    format!(
                                        "autonomous intake skipped: issue lacks label '{label}'"
                                    ),
                                ))
                            }
                            _ => Ok((Some(enqueue()), "autonomous issue intake".to_string())),
                        },
                        None => Ok((None, "no @harness command in issue body".to_string())),
                    }
                }
                // In webhook-only mode the poller is off, so an issue that gains the
                // configured intake label *after* it was opened must be picked up via
                // the `labeled` event (the poller would have caught it on its next
                // scan). Only enqueue when the just-added label is the configured one.
                "labeled" if autonomous_issues => match autonomous_label {
                    Some(want) if !want.is_empty() => {
                        let added = parsed.label.as_ref().map(|l| l.name.as_str());
                        if added == Some(want) {
                            Ok((
                                Some(enqueue()),
                                "autonomous issue intake (labeled)".to_string(),
                            ))
                        } else {
                            Ok((
                                None,
                                "labeled action: not the configured intake label".to_string(),
                            ))
                        }
                    }
                    // No label filter configured → `labeled` events are noise.
                    _ => Ok((
                        None,
                        "labeled action ignored: no intake label configured".to_string(),
                    )),
                },
                _ => Ok((None, "ignored issues action".to_string())),
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
            // Actionable review feedback (changes requested, or a non-empty
            // commented review) is routed into the structured per-PR feedback
            // workflow via `review_task_request` (which sets `pr`). That path is
            // keyed by the PR and deduped (ActiveCommandExists), so multiple
            // reviews on the same PR coalesce into one inspect_pr_feedback turn
            // instead of spawning concurrent freeform reworks that race on the
            // branch. The activity re-reads all PR feedback itself, so the review
            // body does not need to be threaded through here.
            match parsed.review.state.to_ascii_lowercase().as_str() {
                "changes_requested" => Ok((
                    Some(review_task_request(
                        parsed.pull_request.number,
                        &parsed.repository.full_name,
                    )),
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
                        Some(review_task_request(
                            parsed.pull_request.number,
                            &parsed.repository.full_name,
                        )),
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
mod tests;
