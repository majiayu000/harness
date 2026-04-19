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
        parse_github_webhook_task_request("issue_comment", payload.to_string().as_bytes()).unwrap();
    let request = request.expect("request should exist");
    assert_eq!(request.issue, Some(106));
    assert_eq!(request.pr, None);
    assert_eq!(request.prompt, None);
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
        parse_github_webhook_task_request("issue_comment", payload.to_string().as_bytes()).unwrap();
    let request = request.expect("request should exist");
    assert_eq!(request.issue, None);
    assert_eq!(request.pr, Some(42));
    assert_eq!(request.prompt, None);
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
        parse_github_webhook_task_request("issue_comment", payload.to_string().as_bytes()).unwrap();
    let request = request.expect("request should exist");
    assert_eq!(request.issue, None);
    assert_eq!(request.pr, None);
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
        }
    });

    let (request, reason) =
        parse_github_webhook_task_request("issues", payload.to_string().as_bytes()).unwrap();
    let request = request.expect("request should exist");
    assert_eq!(reason, "issue mention");
    assert_eq!(request.issue, Some(77));
    assert_eq!(request.pr, None);
    assert_eq!(request.prompt, None);
}

#[test]
fn parse_issues_reopened_with_harness_mention_creates_issue_task() {
    let payload = serde_json::json!({
        "action": "reopened",
        "issue": {
            "number": 88,
            "body": "This is still broken, @harness fix it"
        }
    });

    let (request, reason) =
        parse_github_webhook_task_request("issues", payload.to_string().as_bytes()).unwrap();
    let request = request.expect("request should exist");
    assert_eq!(reason, "issue mention");
    assert_eq!(request.issue, Some(88));
}

#[test]
fn parse_issues_opened_without_harness_mention_is_ignored() {
    let payload = serde_json::json!({
        "action": "opened",
        "issue": {
            "number": 99,
            "body": "This issue has no harness mention"
        }
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
        "issue": { "number": 100 }
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
        }
    });

    let (request, reason) =
        parse_github_webhook_task_request("issues", payload.to_string().as_bytes()).unwrap();
    assert!(request.is_none());
    assert_eq!(reason, "ignored issues action");
}

#[test]
fn parse_unsupported_event_returns_static_reason() {
    let (request, reason) = parse_github_webhook_task_request("unknown_event", br#"{}"#).unwrap();
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
        parse_github_webhook_task_request("issue_comment", payload.to_string().as_bytes()).unwrap();
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
        parse_github_webhook_task_request("issue_comment", payload.to_string().as_bytes()).unwrap();
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
        }
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

    let (request, reason) =
        parse_github_webhook_task_request("pull_request_review", payload.to_string().as_bytes())
            .unwrap();
    let request = request.expect("request should exist");
    assert_eq!(reason, "pr review changes_requested");
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

    let (request, reason) =
        parse_github_webhook_task_request("pull_request_review", payload.to_string().as_bytes())
            .unwrap();
    let request = request.expect("request should exist");
    assert_eq!(reason, "pr review approved");
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

    let (request, reason) =
        parse_github_webhook_task_request("pull_request_review", payload.to_string().as_bytes())
            .unwrap();
    let request = request.expect("request should exist");
    assert_eq!(reason, "pr review comment: actionable feedback");
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

    let (request, reason) =
        parse_github_webhook_task_request("pull_request_review", payload.to_string().as_bytes())
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

    let (request, reason) =
        parse_github_webhook_task_request("pull_request_review", payload.to_string().as_bytes())
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

    let (request, reason) =
        parse_github_webhook_task_request("pull_request_review", payload.to_string().as_bytes())
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

    let (request, reason) =
        parse_github_webhook_task_request("pull_request_review", payload.to_string().as_bytes())
            .unwrap();
    assert!(request.is_none());
    assert_eq!(reason, "unsupported review state");
}

#[test]
fn gemini_commented_with_trigger_phrase_creates_rework_task() {
    let payload = serde_json::json!({
        "action": "submitted",
        "review": {
            "state": "commented",
            "body": "Feedback suggests adding a timeout to prevent resource leaks.",
            "html_url": "https://github.com/org/repo/pull/10#pullrequestreview-6",
            "user": { "login": "gemini-code-assist[bot]" }
        },
        "pull_request": {
            "number": 10,
            "html_url": "https://github.com/org/repo/pull/10"
        },
        "repository": { "full_name": "org/repo" }
    });

    let (request, reason) =
        parse_github_webhook_task_request("pull_request_review", payload.to_string().as_bytes())
            .unwrap();
    assert!(request.is_some(), "should create rework task");
    assert_eq!(reason, "pr review comment: actionable feedback");
}

#[test]
fn gemini_commented_without_trigger_phrase_creates_rework_task() {
    // Gemini reviews with inline comments have summary bodies that don't contain
    // body-only trigger phrases. The webhook cannot distinguish inline-comment
    // reviews from body-only reviews, so all non-empty COMMENTED reviews proceed.
    let payload = serde_json::json!({
        "action": "submitted",
        "review": {
            "state": "commented",
            "body": "This PR looks good overall. Nice work on the refactoring.",
            "html_url": "https://github.com/org/repo/pull/10#pullrequestreview-7",
            "user": { "login": "gemini-code-assist[bot]" }
        },
        "pull_request": {
            "number": 10,
            "html_url": "https://github.com/org/repo/pull/10"
        },
        "repository": { "full_name": "org/repo" }
    });

    let (request, reason) =
        parse_github_webhook_task_request("pull_request_review", payload.to_string().as_bytes())
            .unwrap();
    assert!(
        request.is_some(),
        "gemini reviewer with non-empty body should create rework task"
    );
    assert_eq!(reason, "pr review comment: actionable feedback");
}

#[test]
fn non_gemini_commented_with_body_creates_rework_task_regardless_of_trigger_phrases() {
    let payload = serde_json::json!({
        "action": "submitted",
        "review": {
            "state": "commented",
            "body": "Consider renaming this for clarity.",
            "html_url": "https://github.com/org/repo/pull/10#pullrequestreview-8",
            "user": { "login": "human-reviewer" }
        },
        "pull_request": {
            "number": 10,
            "html_url": "https://github.com/org/repo/pull/10"
        },
        "repository": { "full_name": "org/repo" }
    });

    let (request, reason) =
        parse_github_webhook_task_request("pull_request_review", payload.to_string().as_bytes())
            .unwrap();
    assert!(
        request.is_some(),
        "non-gemini reviewer should always create rework task"
    );
    assert_eq!(reason, "pr review comment: actionable feedback");
}
