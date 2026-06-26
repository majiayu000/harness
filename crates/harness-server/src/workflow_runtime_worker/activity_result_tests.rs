use super::*;
use harness_workflow::runtime::{ActivityStatus, RuntimeKind};

#[test]
fn activity_result_from_turn_fails_when_no_fenced_block_present() {
    // P0-1: a completed agent turn that emits no `harness-activity-result`
    // fenced block must NOT be silently treated as success. Returning
    // succeeded here historically caused state-machine no-progress loops:
    // an agent could return prose, the reducer would observe no structured
    // state transition, and the next tick would re-dispatch.
    let job = RuntimeJob::pending(
        "command-1",
        RuntimeKind::ClaudeCode,
        "claude-default",
        json!({
            "activity": "implement_issue"
        }),
    );
    let items = vec![Item::AgentReasoning {
        content: "I scanned the repo and saw no new issues. Done.".to_string(),
    }];

    let result = activity_result_from_turn(
        &job,
        &TurnStatus::Completed,
        &items,
        &ThreadId::from_str("thread-1"),
        &TurnId::from_str("turn-1"),
        "claude",
        Path::new("/project"),
        "digest-1",
    );

    assert_eq!(result.activity, "implement_issue");
    assert_eq!(
        result.status,
        ActivityStatus::Failed,
        "missing structured result MUST surface as failed"
    );
    assert_eq!(
        result.error_kind,
        Some(ActivityErrorKind::Configuration),
        "missing structured result is a configuration-class failure (prompt or agent contract issue)"
    );
    assert!(
        result
            .error
            .as_deref()
            .is_some_and(|e| e.contains("harness-activity-result")),
        "error message MUST mention the missing block so operators can diagnose"
    );
    let envelope = envelope_artifact(&result);
    assert_eq!(envelope["outcome"], "missing_structured_output");
    assert_eq!(envelope["extraction_strategy"], "fenced_activity_result");
    assert_eq!(envelope["raw_status"], "completed");
}

#[test]
fn activity_result_from_turn_parses_structured_activity_result_block() {
    let job = RuntimeJob::pending(
        "command-1",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({
            "activity": "implement_issue"
        }),
    );
    let items = vec![Item::AgentReasoning {
        content: r#"Work completed.

```harness-activity-result
{"activity":"implement_issue","status":"succeeded","summary":"stale summary","artifacts":[{"artifact_type":"pull_request","artifact":{"pr_number":66,"pr_url":"https://github.com/owner/repo/pull/66"}}]}
```

Final result:

```harness-activity-result
{"activity":"implement_issue","status":"succeeded","summary":"Implementation completed.","artifacts":[{"artifact_type":"pull_request","artifact":{"pr_number":77,"pr_url":"https://github.com/owner/repo/pull/77"}}]}
```"#
            .to_string(),
    }];

    let result = activity_result_from_turn(
        &job,
        &TurnStatus::Completed,
        &items,
        &ThreadId::from_str("thread-1"),
        &TurnId::from_str("turn-1"),
        "codex",
        Path::new("/project"),
        "digest-1",
    );

    assert_eq!(result.activity, "implement_issue");
    assert_eq!(result.status, ActivityStatus::Succeeded);
    assert_eq!(result.summary, "Implementation completed.");
    let pr_artifact = artifact_by_type(&result, "pull_request");
    assert_eq!(pr_artifact.artifact["pr_number"], 77);
    assert_eq!(
        pr_artifact.artifact["pr_url"],
        "https://github.com/owner/repo/pull/77"
    );
    let prompt_artifact = artifact_by_type(&result, "runtime_prompt_packet");
    assert_eq!(prompt_artifact.artifact["digest"], "digest-1");
    let turn_artifact = artifact_by_type(&result, "runtime_turn");
    assert_eq!(turn_artifact.artifact["thread_id"], "thread-1");
    assert_eq!(turn_artifact.artifact["turn_id"], "turn-1");
    let envelope = envelope_artifact(&result);
    assert_eq!(envelope["outcome"], "accepted");
    assert_eq!(envelope["extracted_activity"], "implement_issue");
    assert_eq!(envelope["final_result"]["status"], "succeeded");
    assert!(result
        .signals
        .iter()
        .any(|signal| signal.signal_type == "RuntimeTurnCompleted"));
}

#[test]
fn activity_result_from_turn_rejects_generic_json_activity_result_block() {
    let job = RuntimeJob::pending(
        "command-1",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({
            "activity": "implement_issue"
        }),
    );
    let items = vec![Item::AgentReasoning {
        content: r#"Work completed.

```json
{"activity":"implement_issue","status":"succeeded","summary":"Implementation completed from generic JSON.","artifacts":[{"artifact_type":"pull_request","artifact":{"pr_number":88,"pr_url":"https://github.com/owner/repo/pull/88"}}]}
```"#
            .to_string(),
    }];

    let result = activity_result_from_turn(
        &job,
        &TurnStatus::Completed,
        &items,
        &ThreadId::from_str("thread-1"),
        &TurnId::from_str("turn-1"),
        "codex",
        Path::new("/project"),
        "digest-1",
    );

    assert_eq!(result.activity, "implement_issue");
    assert_eq!(result.status, ActivityStatus::Failed);
    assert_eq!(
        result.error_kind,
        Some(ActivityErrorKind::Configuration),
        "generic JSON fences must not satisfy the final output contract"
    );
    assert!(!result
        .artifacts
        .iter()
        .any(|artifact| artifact.artifact_type == "pull_request"));
    let envelope = envelope_artifact(&result);
    assert_eq!(envelope["outcome"], "missing_structured_output");
    assert_eq!(envelope["extraction_strategy"], "fenced_activity_result");
    assert!(envelope["extracted_activity"].is_null());
    assert!(envelope["extraction_error"]
        .as_str()
        .is_some_and(|error| error.contains("harness-activity-result")));
}

#[test]
fn activity_result_from_turn_does_not_repair_ambiguous_json_block() {
    let job = RuntimeJob::pending(
        "command-1",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({
            "activity": "implement_issue"
        }),
    );
    let items = vec![Item::AgentReasoning {
        content: r#"Work completed.

```json
{"summary":"Done, but this is not an ActivityResult."}
```"#
            .to_string(),
    }];

    let result = activity_result_from_turn(
        &job,
        &TurnStatus::Completed,
        &items,
        &ThreadId::from_str("thread-1"),
        &TurnId::from_str("turn-1"),
        "codex",
        Path::new("/project"),
        "digest-1",
    );

    assert_eq!(result.activity, "implement_issue");
    assert_eq!(result.status, ActivityStatus::Failed);
    assert_eq!(result.error_kind, Some(ActivityErrorKind::Configuration));
    let envelope = envelope_artifact(&result);
    assert_eq!(envelope["outcome"], "missing_structured_output");
    assert_eq!(envelope["extraction_strategy"], "fenced_activity_result");
}

#[test]
fn activity_result_from_turn_fails_mismatched_structured_activity() {
    let job = RuntimeJob::pending(
        "command-1",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({
            "activity": "implement_issue"
        }),
    );
    let content = r#"Wrong activity result.

```harness-activity-result
{"activity":"replan_issue","status":"succeeded","summary":"Wrong activity.","artifacts":[{"artifact_type":"pull_request","artifact":{"pr_number":77,"pr_url":"https://github.com/owner/repo/pull/77"}}]}
```"#;
    let items = vec![Item::AgentReasoning {
        content: content.to_string(),
    }];

    let result = activity_result_from_turn(
        &job,
        &TurnStatus::Completed,
        &items,
        &ThreadId::from_str("thread-1"),
        &TurnId::from_str("turn-1"),
        "codex",
        Path::new("/project"),
        "digest-1",
    );

    assert_eq!(result.activity, "implement_issue");
    assert_eq!(result.status, ActivityStatus::Failed);
    assert_eq!(result.summary, "Structured activity result was invalid.");
    assert_eq!(
        result.error.as_deref(),
        Some("activity result block reported activity `replan_issue`, expected `implement_issue`")
    );
    assert_eq!(result.error_kind, Some(ActivityErrorKind::Configuration));
    assert!(!result
        .artifacts
        .iter()
        .any(|artifact| artifact.artifact_type == "pull_request"));
    let envelope = envelope_artifact(&result);
    assert_eq!(envelope["outcome"], "invalid_structured_output");
    assert_eq!(envelope["extracted_activity"], "replan_issue");
    assert!(result
        .artifacts
        .iter()
        .any(|artifact| artifact.artifact_type == "runtime_prompt_packet"));
    assert!(result
        .artifacts
        .iter()
        .any(|artifact| artifact.artifact_type == "runtime_turn"));
}

#[test]
fn activity_result_from_turn_fails_latest_malformed_structured_activity() {
    let job = RuntimeJob::pending(
        "command-1",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({
            "activity": "implement_issue"
        }),
    );
    let items = vec![Item::AgentReasoning {
        content: r#"Work completed.

```harness-activity-result
{"activity":"implement_issue","status":"succeeded","summary":"stale summary","artifacts":[{"artifact_type":"pull_request","artifact":{"pr_number":66,"pr_url":"https://github.com/owner/repo/pull/66"}}]}
```

Final result:

```harness-activity-result
{"activity":"implement_issue","status":"succeeded","summary":
```"#
            .to_string(),
    }];

    let result = activity_result_from_turn(
        &job,
        &TurnStatus::Completed,
        &items,
        &ThreadId::from_str("thread-1"),
        &TurnId::from_str("turn-1"),
        "codex",
        Path::new("/project"),
        "digest-1",
    );

    assert_eq!(result.activity, "implement_issue");
    assert_eq!(result.status, ActivityStatus::Failed);
    assert_eq!(result.summary, "Structured activity result was invalid.");
    assert!(result
        .error
        .as_deref()
        .is_some_and(|error| error.starts_with("activity result block is invalid JSON:")));
    assert_eq!(result.error_kind, Some(ActivityErrorKind::Configuration));
    assert!(!result
        .artifacts
        .iter()
        .any(|artifact| artifact.artifact_type == "pull_request"));
    let envelope = envelope_artifact(&result);
    assert_eq!(envelope["outcome"], "invalid_structured_output");
    assert!(envelope["extracted_activity"].is_null());
    assert!(result
        .artifacts
        .iter()
        .any(|artifact| artifact.artifact_type == "runtime_prompt_packet"));
    assert!(result
        .artifacts
        .iter()
        .any(|artifact| artifact.artifact_type == "runtime_turn"));
}

#[test]
fn activity_result_from_turn_classifies_timeout_error_kind() {
    let job = RuntimeJob::pending(
        "command-1",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({
            "activity": "implement_issue"
        }),
    );
    let items = vec![Item::Error {
        code: 1,
        message: "Agent turn timed out after 30s".to_string(),
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

    assert_eq!(result.activity, "implement_issue");
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

fn envelope_artifact(result: &ActivityResult) -> &serde_json::Value {
    &artifact_by_type(result, "activity_result_envelope").artifact
}

fn artifact_by_type<'a>(result: &'a ActivityResult, artifact_type: &str) -> &'a ActivityArtifact {
    match result
        .artifacts
        .iter()
        .find(|artifact| artifact.artifact_type == artifact_type)
    {
        Some(artifact) => artifact,
        None => panic!("{artifact_type} artifact should be appended"),
    }
}
