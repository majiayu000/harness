use super::*;
use harness_workflow::runtime::{ActivityStatus, RuntimeKind};

#[test]
fn activity_result_from_turn_marks_quota_failure_non_retryable() {
    let job = RuntimeJob::pending(
        "command-1",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({
            "activity": "implement_prompt"
        }),
    );
    let items = vec![Item::Error {
        code: -1,
        message: "agent quota exhausted: codex structured error: usage limit reached".to_string(),
    }];

    let result = activity_result_from_turn(
        &job,
        &TurnStatus::Failed,
        &items,
        &ThreadId::from_str("thread-1"),
        &TurnId::from_str("turn-1"),
        "codex",
        Path::new("/project"),
        "digest-1",
    );

    assert_eq!(result.activity, "implement_prompt");
    assert_eq!(result.status, ActivityStatus::Failed);
    assert_eq!(result.error_kind, Some(ActivityErrorKind::Configuration));
    assert_eq!(
        result.error.as_deref(),
        Some("agent quota exhausted: codex structured error: usage limit reached")
    );
}

#[test]
fn failed_turn_never_accepts_stale_structured_activity_result() {
    let job = RuntimeJob::pending(
        "command-1",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({
            "activity": "implement_issue"
        }),
    );
    let items = vec![
        Item::AgentReasoning {
            content: r#"A stale result must not override an explicit terminal failure.

```harness-activity-result
{"activity":"implement_issue","status":"succeeded","summary":"Implementation completed.","artifacts":[{"artifact_type":"pull_request","artifact":{"pr_number":170,"pr_url":"https://github.com/owner/repo/pull/170"}}]}
```"#
                .to_string(),
        },
        Item::Error {
            code: 1,
            message: "Codex reported turn/failed".to_string(),
        },
    ];

    let result = activity_result_from_turn(
        &job,
        &TurnStatus::Failed,
        &items,
        &ThreadId::from_str("thread-1"),
        &TurnId::from_str("turn-1"),
        "codex",
        Path::new("/project"),
        "digest-1",
    );

    assert_eq!(result.activity, "implement_issue");
    assert_eq!(result.status, ActivityStatus::Failed);
    assert_eq!(result.error.as_deref(), Some("Codex reported turn/failed"));
    assert!(!result
        .artifacts
        .iter()
        .any(|artifact| artifact.artifact_type == "pull_request"));
    let envelope = envelope_artifact(&result);
    assert_eq!(envelope["outcome"], "turn_failed");
    assert_eq!(envelope["extraction_strategy"], "not_attempted");
}

#[test]
fn failed_timeout_ignores_stale_structured_activity_result() {
    let job = RuntimeJob::pending(
        "command-1",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({
            "activity": "implement_issue"
        }),
    );
    let items = vec![
        Item::AgentReasoning {
            content: r#"An old result should not override a real timeout.

```harness-activity-result
{"activity":"implement_issue","status":"succeeded","summary":"Implementation completed.","artifacts":[]}
```"#
                .to_string(),
        },
        Item::Error {
            code: 1,
            message: "Agent turn timed out after 30s".to_string(),
        },
    ];

    let result = activity_result_from_turn(
        &job,
        &TurnStatus::Failed,
        &items,
        &ThreadId::from_str("thread-1"),
        &TurnId::from_str("turn-1"),
        "codex",
        Path::new("/project"),
        "digest-1",
    );

    assert_eq!(result.status, ActivityStatus::Failed);
    assert_eq!(result.error_kind, Some(ActivityErrorKind::Timeout));
    assert_eq!(
        result.error.as_deref(),
        Some("Agent turn timed out after 30s")
    );
    let envelope = envelope_artifact(&result);
    assert_eq!(envelope["outcome"], "turn_failed");
    assert_eq!(envelope["extraction_strategy"], "not_attempted");
}

#[test]
fn failed_real_error_is_truncated_before_envelope_storage() {
    let job = RuntimeJob::pending(
        "command-1",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({
            "activity": "implement_issue"
        }),
    );
    let long_error = format!("adapter failure: {}", "x".repeat(1400));
    let items = vec![Item::Error {
        code: 1,
        message: long_error,
    }];

    let result = activity_result_from_turn(
        &job,
        &TurnStatus::Failed,
        &items,
        &ThreadId::from_str("thread-1"),
        &TurnId::from_str("turn-1"),
        "codex",
        Path::new("/project"),
        "digest-1",
    );

    let error = match result.error.as_deref() {
        Some(error) => error,
        None => panic!("failed activity result should include an error"),
    };
    assert!(error.starts_with("adapter failure: "));
    assert!(error.ends_with("..."));
    assert_eq!(error.len(), 1203);
    let envelope = envelope_artifact(&result);
    assert_eq!(envelope["outcome"], "turn_failed");
    assert_eq!(envelope["extraction_error"], error);
}

fn envelope_artifact(result: &ActivityResult) -> &serde_json::Value {
    match result
        .artifacts
        .iter()
        .find(|artifact| artifact.artifact_type == "activity_result_envelope")
    {
        Some(artifact) => &artifact.artifact,
        None => panic!("activity result envelope artifact should be appended"),
    }
}
