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
async fn execute_preserves_quota_evidence_when_completion_precedes_nonzero_exit() {
    let (dir, script) = write_executable_script(
        r#"
printf '%s\n' '{"type":"error","message":"usage limit reached; try again later"}'
printf '%s\n' '{"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":1}}'
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
        .expect_err("nonzero execution should fail");

    assert!(
        matches!(err, harness_core::error::HarnessError::QuotaExhausted(_)),
        "expected preceding diagnostic evidence to preserve quota classification, got: {err}"
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

#[tokio::test]
async fn execute_stream_preserves_non_quota_structured_error_as_upstream() {
    let (dir, script) = write_executable_script(
        r#"
printf '%s\n' '{"type":"thread.started","thread_id":"thread-1"}'
printf '%s\n' '{"type":"turn.started"}'
printf '%s\n' '{"type":"error","message":"provider cannot currently serve this request"}'
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

    assert!(matches!(
        err,
        harness_core::error::HarnessError::Upstream(_)
    ));
    assert_eq!(
        err.turn_failure().expect("turn failure").kind,
        harness_core::types::TurnFailureKind::Upstream
    );
}

#[test]
fn classifies_authentication_failure_as_configuration() {
    let err = codex_errors::codex_structured_error(
        "authentication failed",
        codex_exec_parser::CodexStructuredErrorKind::Provider,
    );

    assert!(
        matches!(err, harness_core::error::HarnessError::Config(_)),
        "expected a configuration failure, got: {err:?}"
    );
    assert!(
        err.turn_failure().is_none(),
        "configuration failures must remain non-retryable"
    );
}

#[test]
fn keeps_permanent_structured_cli_errors_non_retryable() {
    let err = codex_errors::codex_structured_error(
        "bad config",
        codex_exec_parser::CodexStructuredErrorKind::Permanent,
    );

    assert!(matches!(
        err,
        harness_core::error::HarnessError::AgentExecution(_)
    ));
    assert_ne!(
        err.turn_failure().expect("turn failure").kind,
        harness_core::types::TurnFailureKind::Upstream
    );
}

#[test]
fn preserves_stderr_for_permanent_structured_failure() {
    use std::os::unix::process::ExitStatusExt;

    let parsed = codex_exec_parser::ParsedCodexExecOutput {
        structured_error: Some("bad config".to_string()),
        structured_error_kind: Some(codex_exec_parser::CodexStructuredErrorKind::Permanent),
        ..Default::default()
    };
    let err = codex_errors::codex_nonzero_exit_error_from_parsed(
        std::process::ExitStatus::from_raw(1 << 8),
        "configuration parse failed at line 4",
        &parsed,
    );

    assert!(matches!(
        err,
        harness_core::error::HarnessError::AgentExecution(_)
    ));
    assert!(
        err.to_string()
            .contains("configuration parse failed at line 4"),
        "unexpected error: {err:?}"
    );
}
