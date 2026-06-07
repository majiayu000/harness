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
