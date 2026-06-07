use super::tests::write_executable_script;
use super::*;

#[tokio::test]
async fn execute_classifies_quota_failure_from_stdout_json_error() {
    let (dir, script) = write_executable_script(
        r#"
printf '%s\n' '{"type":"thread.started","thread_id":"thread-1"}'
printf '%s\n' '{"type":"error","message":"usage limit reached; try again later"}'
echo 'Reading additional input from stdin...' >&2
exit 1
"#,
    );
    let agent = CodexAgent::new(script, SandboxMode::DangerFullAccess);
    let request = AgentRequest {
        prompt: "ignored".to_string(),
        project_root: dir.path().to_path_buf(),
        ..AgentRequest::default()
    };

    let err = agent
        .execute(request)
        .await
        .expect_err("execution should fail");

    assert!(
        matches!(err, harness_core::error::HarnessError::QuotaExhausted(_)),
        "expected codex stdout JSON error to preserve quota classification, got: {err}"
    );
    assert_eq!(
        err.turn_failure().expect("turn failure").kind,
        harness_core::types::TurnFailureKind::Quota
    );
}

#[tokio::test]
async fn execute_stream_classifies_quota_failure_from_stdout_json_error() {
    let (dir, script) = write_executable_script(
        r#"
printf '%s\n' '{"type":"thread.started","thread_id":"thread-1"}'
printf '%s\n' '{"type":"turn.started"}'
printf '%s\n' '{"type":"error","message":"usage limit reached; try again later"}'
echo 'Reading additional input from stdin...' >&2
exit 1
"#,
    );
    let agent = CodexAgent::new(script, SandboxMode::DangerFullAccess);
    let request = AgentRequest {
        prompt: "ignored".to_string(),
        project_root: dir.path().to_path_buf(),
        ..AgentRequest::default()
    };

    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    let err = agent
        .execute_stream(request, tx)
        .await
        .expect_err("stream execution should fail");

    assert!(
        matches!(err, harness_core::error::HarnessError::QuotaExhausted(_)),
        "expected streamed codex stdout JSON error to preserve quota classification, got: {err}"
    );
    assert_eq!(
        err.turn_failure().expect("turn failure").kind,
        harness_core::types::TurnFailureKind::Quota
    );
}
