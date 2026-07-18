use super::*;
use harness_workflow::runtime::{ActivityStatus, RuntimeKind};

const SKILL_BUDGET_AGENT_ERROR: &str = "agent execution failed: Skill descriptions were shortened to fit the 2% skills context budget. Codex can still see every skill, but some descriptions are shorter.";
const SKILL_BUDGET_AGENT_ERROR_WITH_STDERR: &str = "agent execution failed: Skill descriptions were shortened to fit the 2% skills context budget. Codex can still see every skill, but some descriptions are shorter.; stderr=[adapter failed]";
const SKILL_BUDGET_CODEX_STRUCTURED_AGENT_ERROR: &str = "agent execution failed: codex structured error: Skill descriptions were shortened to fit the 2% skills context budget. Codex can still see every skill, but some descriptions are shorter.";
const SKILL_BUDGET_EXIT_STRUCTURED_AGENT_ERROR: &str = "agent execution failed: codex structured error: exit exit status: 1: Skill descriptions were shortened to fit the 2% skills context budget. Codex can still see every skill, but some descriptions are shorter.";
const SKILL_BUDGET_EXIT_STRUCTURED_AGENT_ERROR_WITH_STDERR: &str = "agent execution failed: codex structured error: exit exit status: 1: Skill descriptions were shortened to fit the 2% skills context budget. Codex can still see every skill, but some descriptions are shorter.; stderr=[adapter failed]";
const SKILL_BUDGET_CODEX_STRUCTURED_AGENT_ERROR_WITH_ADVICE: &str = "agent execution failed: codex structured error: Skill descriptions were shortened to fit the 2% skills context budget. Codex can still see every skill, but some descriptions are shorter. Disable unused skills or plugins to leave more room for the rest.";

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
fn failed_skill_budget_warning_accepts_structured_activity_result() {
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
            content: r#"The agent completed the PR before the wrapper warning.

```harness-activity-result
{"activity":"implement_issue","status":"succeeded","summary":"Implementation completed.","artifacts":[{"artifact_type":"pull_request","artifact":{"pr_number":170,"pr_url":"https://github.com/owner/repo/pull/170"}}]}
```"#
                .to_string(),
        },
        Item::Error {
            code: 1,
            message: SKILL_BUDGET_AGENT_ERROR.to_string(),
        },
        Item::Error {
            code: 1,
            message: SKILL_BUDGET_CODEX_STRUCTURED_AGENT_ERROR.to_string(),
        },
        Item::Error {
            code: 1,
            message: SKILL_BUDGET_EXIT_STRUCTURED_AGENT_ERROR.to_string(),
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
    assert_eq!(result.status, ActivityStatus::Succeeded);
    assert_eq!(result.summary, "Implementation completed.");
    assert!(result.error.is_none());
    assert!(result
        .artifacts
        .iter()
        .any(|artifact| artifact.artifact_type == "pull_request"));
    let envelope = envelope_artifact(&result);
    assert_eq!(envelope["outcome"], "accepted_with_turn_warning");
    assert_eq!(envelope["raw_status"], "failed");
    assert_eq!(envelope["extraction_strategy"], "fenced_activity_result");
    assert!(envelope["extraction_error"]
        .as_str()
        .is_some_and(|error| error.contains("Skill descriptions were shortened")));
    assert_eq!(envelope["final_result"]["status"], "succeeded");
}

#[test]
fn failed_skill_budget_warning_with_advice_accepts_structured_activity_result() {
    let job = RuntimeJob::pending(
        "command-1",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({
            "activity": "plan_issue"
        }),
    );
    let items = vec![
        Item::AgentReasoning {
            content: r#"```harness-activity-result
{"activity":"plan_issue","status":"succeeded","summary":"Planning completed.","artifacts":[{"artifact_type":"issue_plan","artifact":{"summary":"Verify the existing PR.","task_class":"bugfix","target_files":[],"validation_plan":["cargo test"],"blockers":[]}}]}
```"#
                .to_string(),
        },
        Item::Error {
            code: 1,
            message: SKILL_BUDGET_CODEX_STRUCTURED_AGENT_ERROR_WITH_ADVICE.to_string(),
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

    assert_eq!(result.activity, "plan_issue");
    assert_eq!(result.status, ActivityStatus::Succeeded);
    assert_eq!(result.summary, "Planning completed.");
    assert!(result.error.is_none());
    assert!(result
        .artifacts
        .iter()
        .any(|artifact| artifact.artifact_type == "issue_plan"));
    assert_eq!(
        envelope_artifact(&result)["outcome"],
        "accepted_with_turn_warning"
    );
}

#[test]
fn failed_wrapped_skill_budget_warning_accepts_structured_activity_result() {
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
            content: r#"The agent completed the PR before the structured wrapper warning.

```harness-activity-result
{"activity":"implement_issue","status":"succeeded","summary":"Implementation completed.","artifacts":[{"artifact_type":"pull_request","artifact":{"pr_number":170,"pr_url":"https://github.com/owner/repo/pull/170"}}]}
```"#
                .to_string(),
        },
        Item::Error {
            code: 1,
            message: SKILL_BUDGET_EXIT_STRUCTURED_AGENT_ERROR.to_string(),
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
    assert_eq!(result.status, ActivityStatus::Succeeded);
    assert_eq!(result.summary, "Implementation completed.");
    assert!(result.error.is_none());
    assert!(result
        .artifacts
        .iter()
        .any(|artifact| artifact.artifact_type == "pull_request"));
    let envelope = envelope_artifact(&result);
    assert_eq!(envelope["outcome"], "accepted_with_turn_warning");
    assert_eq!(envelope["raw_status"], "failed");
    assert_eq!(envelope["extraction_strategy"], "fenced_activity_result");
    assert_eq!(
        envelope["extraction_error"],
        SKILL_BUDGET_EXIT_STRUCTURED_AGENT_ERROR
    );
    assert_eq!(envelope["final_result"]["status"], "succeeded");
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
{"activity":"implement_issue","status":"succeeded","summary":"Implementation completed.","artifacts":[{"artifact_type":"pull_request","artifact":{"pr_number":170,"pr_url":"https://github.com/owner/repo/pull/170"}}]}
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

    assert_eq!(result.activity, "implement_issue");
    assert_eq!(result.status, ActivityStatus::Failed);
    assert_eq!(result.error_kind, Some(ActivityErrorKind::Timeout));
    assert_eq!(
        result.error.as_deref(),
        Some("Agent turn timed out after 30s")
    );
    assert!(!result
        .artifacts
        .iter()
        .any(|artifact| artifact.artifact_type == "pull_request"));
    let envelope = envelope_artifact(&result);
    assert_eq!(envelope["outcome"], "turn_failed");
    assert_eq!(envelope["extraction_strategy"], "not_attempted");
}

#[test]
fn failed_real_error_before_skill_budget_warning_ignores_structured_activity_result() {
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
            content: r#"A stale success must not hide a real failure.

```harness-activity-result
{"activity":"implement_issue","status":"succeeded","summary":"Implementation completed.","artifacts":[{"artifact_type":"pull_request","artifact":{"pr_number":170,"pr_url":"https://github.com/owner/repo/pull/170"}}]}
```"#
                .to_string(),
        },
        Item::Error {
            code: 1,
            message: "Agent turn timed out after 30s".to_string(),
        },
        Item::Error {
            code: 1,
            message: SKILL_BUDGET_AGENT_ERROR.to_string(),
        },
        Item::Error {
            code: 1,
            message: SKILL_BUDGET_CODEX_STRUCTURED_AGENT_ERROR.to_string(),
        },
        Item::Error {
            code: 1,
            message: SKILL_BUDGET_EXIT_STRUCTURED_AGENT_ERROR.to_string(),
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
    assert_eq!(result.error_kind, Some(ActivityErrorKind::Timeout));
    assert_eq!(
        result.error.as_deref(),
        Some("Agent turn timed out after 30s")
    );
    assert!(!result
        .artifacts
        .iter()
        .any(|artifact| artifact.artifact_type == "pull_request"));
    let envelope = envelope_artifact(&result);
    assert_eq!(envelope["outcome"], "turn_failed");
    assert_eq!(envelope["extraction_strategy"], "not_attempted");
}

#[test]
fn failed_skill_budget_warning_with_stderr_ignores_structured_activity_result() {
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
            content: r#"A warning with stderr diagnostics is still a failed turn.

```harness-activity-result
{"activity":"implement_issue","status":"succeeded","summary":"Implementation completed.","artifacts":[{"artifact_type":"pull_request","artifact":{"pr_number":170,"pr_url":"https://github.com/owner/repo/pull/170"}}]}
```"#
                .to_string(),
        },
        Item::Error {
            code: 1,
            message: SKILL_BUDGET_AGENT_ERROR_WITH_STDERR.to_string(),
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
    assert_eq!(
        result.error.as_deref(),
        Some(SKILL_BUDGET_AGENT_ERROR_WITH_STDERR)
    );
    assert!(!result
        .artifacts
        .iter()
        .any(|artifact| artifact.artifact_type == "pull_request"));
    let envelope = envelope_artifact(&result);
    assert_eq!(envelope["outcome"], "turn_failed");
    assert_eq!(envelope["extraction_strategy"], "not_attempted");
}

#[test]
fn failed_wrapped_skill_budget_warning_with_stderr_ignores_structured_activity_result() {
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
            content: r#"A structured wrapper warning with stderr diagnostics is still a failed turn.

```harness-activity-result
{"activity":"implement_issue","status":"succeeded","summary":"Implementation completed.","artifacts":[{"artifact_type":"pull_request","artifact":{"pr_number":170,"pr_url":"https://github.com/owner/repo/pull/170"}}]}
```"#
                .to_string(),
        },
        Item::Error {
            code: 1,
            message: SKILL_BUDGET_EXIT_STRUCTURED_AGENT_ERROR_WITH_STDERR.to_string(),
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
    assert_eq!(
        result.error.as_deref(),
        Some(SKILL_BUDGET_EXIT_STRUCTURED_AGENT_ERROR_WITH_STDERR)
    );
    assert!(!result
        .artifacts
        .iter()
        .any(|artifact| artifact.artifact_type == "pull_request"));
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
