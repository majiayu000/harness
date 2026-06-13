use super::*;
use crate::claude_stream::parse_claude_stream_output;
use harness_core::{types::ExecutionPhase, types::Item, types::ReasoningBudget};
use std::fs;
use std::time::Duration;
use tokio::time::timeout;

fn args_to_strings(args: &[OsString]) -> Vec<String> {
    args.iter()
        .map(|a| a.to_string_lossy().to_string())
        .collect()
}

async fn wait_for_path(path: &std::path::Path, timeout_duration: Duration) -> bool {
    timeout(timeout_duration, async {
        loop {
            if path.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok()
}

#[test]
fn base_args_full_profile_uses_dangerously_skip_permissions() {
    let agent = ClaudeCodeAgent::new(
        PathBuf::from("claude"),
        "test-model".to_string(),
        SandboxMode::DangerFullAccess,
    );
    let req = AgentRequest {
        allowed_tools: None, // Full profile — no restriction
        ..AgentRequest::default()
    };
    let args = args_to_strings(&agent.base_args(&req));
    assert!(
        args.contains(&"--dangerously-skip-permissions".to_string()),
        "Full profile must use --dangerously-skip-permissions; got: {args:?}"
    );
    assert!(
        !args.contains(&"--allowedTools".to_string()),
        "Full profile must NOT use --allowedTools; got: {args:?}"
    );
}

#[test]
fn base_args_standard_profile_uses_allowed_tools_flag() {
    let agent = ClaudeCodeAgent::new(
        PathBuf::from("claude"),
        "test-model".to_string(),
        SandboxMode::DangerFullAccess,
    );
    let req = AgentRequest {
        allowed_tools: Some(vec![
            "Read".to_string(),
            "Write".to_string(),
            "Edit".to_string(),
            "Bash".to_string(),
        ]),
        ..AgentRequest::default()
    };
    let args = args_to_strings(&agent.base_args(&req));
    assert!(
        args.contains(&"--allowedTools".to_string()),
        "Standard profile must use --allowedTools; got: {args:?}"
    );
    assert!(
        args.contains(&"--permission-mode".to_string()),
        "Standard profile must use --permission-mode; got: {args:?}"
    );
    let permission_mode_value = args
        .iter()
        .skip_while(|a| *a != "--permission-mode")
        .nth(1)
        .cloned()
        .unwrap_or_default();
    assert_eq!(
            permission_mode_value, "bypassPermissions",
            "--permission-mode value must be bypassPermissions (camelCase); got: {permission_mode_value:?}"
        );
    assert!(
        !args.contains(&"--dangerously-skip-permissions".to_string()),
        "Standard profile must NOT use --dangerously-skip-permissions; got: {args:?}"
    );
    let tools_value = args
        .iter()
        .skip_while(|a| *a != "--allowedTools")
        .nth(1)
        .cloned()
        .unwrap_or_default();
    assert!(
        tools_value.contains("Read")
            && tools_value.contains("Write")
            && tools_value.contains("Edit")
            && tools_value.contains("Bash"),
        "allowedTools value must list all Standard tools; got: {tools_value:?}"
    );
}

#[test]
fn base_args_read_only_profile_uses_allowed_tools_flag() {
    let agent = ClaudeCodeAgent::new(
        PathBuf::from("claude"),
        "test-model".to_string(),
        SandboxMode::DangerFullAccess,
    );
    let req = AgentRequest {
        allowed_tools: Some(vec![
            "Read".to_string(),
            "Grep".to_string(),
            "Glob".to_string(),
        ]),
        ..AgentRequest::default()
    };
    let args = args_to_strings(&agent.base_args(&req));
    assert!(
        args.contains(&"--allowedTools".to_string()),
        "ReadOnly profile must use --allowedTools; got: {args:?}"
    );
    assert!(
        !args.contains(&"--dangerously-skip-permissions".to_string()),
        "ReadOnly profile must NOT use --dangerously-skip-permissions; got: {args:?}"
    );
    let tools_value = args
        .iter()
        .skip_while(|a| *a != "--allowedTools")
        .nth(1)
        .cloned()
        .unwrap_or_default();
    assert!(
        tools_value.contains("Read")
            && tools_value.contains("Grep")
            && tools_value.contains("Glob"),
        "allowedTools value must list all ReadOnly tools; got: {tools_value:?}"
    );
}

#[test]
fn base_args_never_contains_both_permission_flags() {
    let agent = ClaudeCodeAgent::new(
        PathBuf::from("claude"),
        "test-model".to_string(),
        SandboxMode::DangerFullAccess,
    );
    for (label, allowed_tools) in [
        ("full", None),
        (
            "standard",
            Some(vec!["Read".to_string(), "Bash".to_string()]),
        ),
    ] {
        let req = AgentRequest {
            allowed_tools,
            ..AgentRequest::default()
        };
        let args = args_to_strings(&agent.base_args(&req));
        let has_skip = args.contains(&"--dangerously-skip-permissions".to_string());
        let has_allowed = args.contains(&"--allowedTools".to_string());
        assert!(
                !(has_skip && has_allowed),
                "Both --dangerously-skip-permissions and --allowedTools present for {label} profile: {args:?}"
            );
    }
}

#[test]
fn resolve_model_uses_phase_when_budget_configured() {
    let budget = ReasoningBudget::default();
    let agent = ClaudeCodeAgent::new(
        PathBuf::from("claude"),
        "default-model".to_string(),
        SandboxMode::DangerFullAccess,
    )
    .with_reasoning_budget(budget);

    let req_planning = AgentRequest {
        execution_phase: Some(ExecutionPhase::Planning),
        ..AgentRequest::default()
    };
    let req_execution = AgentRequest {
        execution_phase: Some(ExecutionPhase::Execution),
        ..AgentRequest::default()
    };
    let req_validation = AgentRequest {
        execution_phase: Some(ExecutionPhase::Validation),
        ..AgentRequest::default()
    };
    let req_no_phase = AgentRequest::default();

    assert_eq!(agent.resolve_model(&req_planning), "claude-opus-4-6");
    assert_eq!(agent.resolve_model(&req_execution), "claude-sonnet-4-6");
    assert_eq!(agent.resolve_model(&req_validation), "claude-opus-4-6");
    // No phase → falls back to default_model
    assert_eq!(agent.resolve_model(&req_no_phase), "default-model");
}

#[test]
fn resolve_model_falls_back_to_req_model_when_no_budget() {
    let agent = ClaudeCodeAgent::new(
        PathBuf::from("claude"),
        "default-model".to_string(),
        SandboxMode::DangerFullAccess,
    );

    let req_with_model = AgentRequest {
        model: Some("explicit-model".to_string()),
        execution_phase: Some(ExecutionPhase::Planning),
        ..AgentRequest::default()
    };
    let req_no_model = AgentRequest {
        execution_phase: Some(ExecutionPhase::Planning),
        ..AgentRequest::default()
    };

    // No budget → req.model takes precedence over phase
    assert_eq!(agent.resolve_model(&req_with_model), "explicit-model");
    // No budget, no req.model → default_model
    assert_eq!(agent.resolve_model(&req_no_model), "default-model");
}

#[test]
fn base_args_uses_request_reasoning_effort_when_phase_is_absent() {
    let agent = ClaudeCodeAgent::new(
        PathBuf::from("claude"),
        "default-model".to_string(),
        SandboxMode::DangerFullAccess,
    );
    let req = AgentRequest {
        reasoning_effort: Some("medium".to_string()),
        ..AgentRequest::default()
    };

    let args = args_to_strings(&agent.base_args(&req));
    assert!(args
        .windows(2)
        .any(|window| window == ["--effort", "medium"]));
}

#[test]
fn base_args_uses_stream_json_output() {
    let agent = ClaudeCodeAgent::new(
        PathBuf::from("claude"),
        "default-model".to_string(),
        SandboxMode::DangerFullAccess,
    );
    let req = AgentRequest::default();

    let args = args_to_strings(&agent.base_args(&req));

    assert!(args
        .windows(2)
        .any(|window| window == ["--output-format", "stream-json"]));
}

#[test]
fn with_reasoning_budget_sets_field() {
    let budget = ReasoningBudget::default();
    let agent = ClaudeCodeAgent::new(
        PathBuf::from("claude"),
        "default-model".to_string(),
        SandboxMode::DangerFullAccess,
    )
    .with_reasoning_budget(budget);
    assert!(agent.reasoning_budget.is_some());
}

#[test]
fn parse_claude_stream_output_records_result_usage() {
    let stdout = [
            r#"{"type":"assistant","message":"hello"}"#,
            r#"{"type":"result","result":"hello","usage":{"input_tokens":10,"output_tokens":2,"cache_read_input_tokens":3}}"#,
        ]
        .join("\n");

    let parsed = parse_claude_stream_output(&stdout);

    assert_eq!(parsed.output, "hello");
    assert_eq!(parsed.token_usage.input_tokens, 10);
    assert_eq!(parsed.token_usage.output_tokens, 2);
    assert_eq!(parsed.token_usage.total_tokens, 15);
}

#[test]
fn parse_claude_stream_output_keeps_plaintext_fallback() {
    let parsed = parse_claude_stream_output("hello\nworld\n");

    assert_eq!(parsed.output, "hello\nworld\n");
    assert_eq!(parsed.token_usage.total_tokens, 0);
}

fn write_executable_script(script_body: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("create tempdir");
    let path = dir.path().join("mock-claude.sh");
    let script = format!("#!/bin/sh\nset -eu\n{script_body}\n");
    fs::write(&path, &script).expect("write script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&path).expect("script metadata").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).expect("set executable permissions");
    }
    (dir, path)
}

#[tokio::test]
async fn execute_stream_returns_error_when_channel_closed() {
    let agent = ClaudeCodeAgent::new(
        PathBuf::from("/usr/bin/true"),
        "test-model".to_string(),
        SandboxMode::DangerFullAccess,
    );
    let request = AgentRequest::default();
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    drop(rx);

    let err = agent
        .execute_stream(request, tx)
        .await
        .expect_err("execute_stream should fail when receiver is dropped");

    let message = err.to_string();
    assert!(
        message.contains("stream send failed"),
        "expected send failure in error message, got: {message}"
    );
}

#[tokio::test]
async fn execute_stream_emits_delta_before_completion_and_done() {
    let (dir, script) = write_executable_script(
        r#"
printf 'hello\n'
sleep 0.2
printf 'world\n'
"#,
    );
    let agent = ClaudeCodeAgent::new(
        script,
        "test-model".to_string(),
        SandboxMode::DangerFullAccess,
    );
    let request = AgentRequest {
        prompt: "ignored".to_string(),
        project_root: dir.path().to_path_buf(),
        ..AgentRequest::default()
    };

    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    let stream_result = agent.execute_stream(request, tx).await;
    assert!(
        stream_result.is_ok(),
        "stream execution should succeed, got: {stream_result:?}"
    );

    let mut events = Vec::new();
    loop {
        let item = match timeout(Duration::from_secs(2), rx.recv()).await {
            Ok(Some(item)) => item,
            Ok(None) => panic!("stream should not close before done"),
            Err(_) => panic!("timed out waiting for stream item"),
        };
        let is_done = matches!(item, StreamItem::Done);
        events.push(item);
        if is_done {
            break;
        }
    }

    let first_delta = events
        .iter()
        .position(|item| matches!(item, StreamItem::MessageDelta { .. }))
        .expect("expected at least one message delta");
    let completed = events
        .iter()
        .position(|item| matches!(item, StreamItem::ItemCompleted { .. }))
        .expect("expected item completed event");
    let done = events
        .iter()
        .position(|item| matches!(item, StreamItem::Done))
        .expect("expected done event");
    assert!(
        first_delta < completed,
        "delta must precede item completed: {events:?}"
    );
    assert!(
        completed < done,
        "item completed must precede done: {events:?}"
    );

    match &events[completed] {
        StreamItem::ItemCompleted {
            item: Item::AgentReasoning { content },
        } => {
            assert!(
                content.contains("hello") && content.contains("world"),
                "completed content should include streamed output, got: {content:?}"
            );
        }
        other => panic!("unexpected completed event payload: {other:?}"),
    }
}

#[tokio::test]
async fn execute_stream_emits_token_usage_from_stream_json_result() {
    let (dir, script) = write_executable_script(
        r#"
printf '%s\n' '{"type":"assistant","message":"hello"}'
printf '%s\n' '{"type":"result","result":"hello","usage":{"input_tokens":10,"output_tokens":2,"cache_read_input_tokens":3}}'
"#,
    );
    let agent = ClaudeCodeAgent::new(
        script,
        "test-model".to_string(),
        SandboxMode::DangerFullAccess,
    );
    let request = AgentRequest {
        prompt: "ignored".to_string(),
        project_root: dir.path().to_path_buf(),
        ..AgentRequest::default()
    };

    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    let stream_result = agent.execute_stream(request, tx).await;
    assert!(
        stream_result.is_ok(),
        "stream execution should succeed, got: {stream_result:?}"
    );

    let mut events = Vec::new();
    loop {
        let item = match timeout(Duration::from_secs(2), rx.recv()).await {
            Ok(Some(item)) => item,
            Ok(None) => panic!("stream should not close before done"),
            Err(_) => panic!("timed out waiting for stream item"),
        };
        let is_done = matches!(item, StreamItem::Done);
        events.push(item);
        if is_done {
            break;
        }
    }

    let Some(usage) = events.iter().find_map(|item| match item {
        StreamItem::TokenUsage { usage } => Some(usage),
        _ => None,
    }) else {
        panic!("expected token usage event");
    };
    assert_eq!(usage.input_tokens, 10);
    assert_eq!(usage.output_tokens, 2);
    assert_eq!(usage.total_tokens, 15);
}

#[tokio::test]
async fn execute_returns_token_usage_from_stream_json_result() {
    let (dir, script) = write_executable_script(
        r#"
printf '%s\n' '{"type":"assistant","message":"hello"}'
printf '%s\n' '{"type":"result","result":"hello","usage":{"input_tokens":4,"output_tokens":5,"cache_creation_input_tokens":6}}'
"#,
    );
    let agent = ClaudeCodeAgent::new(
        script,
        "test-model".to_string(),
        SandboxMode::DangerFullAccess,
    );
    let request = AgentRequest {
        prompt: "ignored".to_string(),
        project_root: dir.path().to_path_buf(),
        ..AgentRequest::default()
    };

    let response = match agent.execute(request).await {
        Ok(response) => response,
        Err(error) => panic!("execution should succeed, got: {error}"),
    };

    assert_eq!(response.output, "hello");
    assert_eq!(response.token_usage.input_tokens, 4);
    assert_eq!(response.token_usage.output_tokens, 5);
    assert_eq!(response.token_usage.total_tokens, 15);
}

#[tokio::test]
async fn execute_stream_classifies_quota_failure_from_stdout_tail() {
    let (dir, script) = write_executable_script(
        r#"
printf "You've hit your limit · resets 3pm (Asia/Shanghai)\n"
exit 1
"#,
    );
    let agent = ClaudeCodeAgent::new(
        script,
        "test-model".to_string(),
        SandboxMode::DangerFullAccess,
    );
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

    assert_eq!(
        err.turn_failure().expect("turn failure").kind,
        harness_core::types::TurnFailureKind::Quota
    );
    assert!(
        err.to_string()
            .contains("stdout_tail=[You've hit your limit"),
        "streamed claude failures must preserve stdout tail, got: {err}"
    );
}

#[tokio::test]
async fn execute_stream_waits_for_late_stderr_before_classifying_exit() {
    let (dir, script) = write_executable_script(
        r#"
(sleep 0.2; echo 'quota exhausted: retry later' >&2) >/dev/null &
exit 1
"#,
    );
    let agent = ClaudeCodeAgent::new(
        script,
        "test-model".to_string(),
        SandboxMode::DangerFullAccess,
    );
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
        matches!(
            err.turn_failure().expect("turn failure").kind,
            harness_core::types::TurnFailureKind::Quota
        ),
        "expected late stderr to influence streamed exit classification, got: {err}"
    );
}

#[tokio::test]
async fn execute_stream_cancel_path_converges_when_receiver_dropped_mid_stream() {
    let (dir, script) = write_executable_script(
        r#"
printf 'first\n'
sleep 0.3
printf 'second\n'
"#,
    );
    let agent = ClaudeCodeAgent::new(
        script,
        "test-model".to_string(),
        SandboxMode::DangerFullAccess,
    );
    let request = AgentRequest {
        prompt: "ignored".to_string(),
        project_root: dir.path().to_path_buf(),
        ..AgentRequest::default()
    };

    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let handle = tokio::spawn(async move { agent.execute_stream(request, tx).await });

    let first = timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("timed out waiting for first stream item")
        .expect("stream closed before first item");
    assert!(
        matches!(first, StreamItem::MessageDelta { .. }),
        "expected first event to be delta, got {first:?}"
    );

    drop(rx);

    let result = timeout(Duration::from_secs(10), handle)
        .await
        .expect("execute_stream task should converge after cancellation")
        .expect("join should succeed");
    let err = result.expect_err("receiver drop should surface send failure");
    assert!(
        err.to_string().contains("stream send failed"),
        "expected stream send failure after cancellation, got: {err}"
    );
}

#[tokio::test]
async fn execute_stream_timeout_drop_does_not_leave_hanging_process() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let started_marker = dir.path().join("timeout-started.txt");
    let descendant_marker = dir.path().join("timeout-descendant-started.txt");
    let marker = dir.path().join("timeout-marker.txt");
    let script = dir.path().join("mock-claude-timeout.sh");
    // sync_all() ensures the kernel flushes dirty pages before exec;
    // without it, Linux can return ETXTBSY on some CI kernels.
    {
        use std::io::Write;
        let mut f = fs::File::create(&script).expect("create timeout script");
        f.write_all(
            format!(
                "#!/bin/sh\nset -eu\necho started > \"{}\"\n( echo descendant > \"{}\"; sleep 1; echo reached > \"{}\" ) &\nsleep 5\n",
                started_marker.display(),
                descendant_marker.display(),
                marker.display()
            )
            .as_bytes(),
        )
        .expect("write timeout script");
        f.sync_all().expect("sync timeout script");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&script)
            .expect("script metadata")
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script, perms).expect("set executable permissions");
    }

    // On Linux CI kernels (tmpfs), the inode write-access count may not be
    // fully settled immediately after close() + fsync. A short sleep lets
    // the kernel scheduler process the fd-close before execve() is called,
    // preventing ETXTBSY (os error 26). 200 ms is empirically sufficient
    // even on heavily loaded CI runners where 50 ms proved flaky.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let agent = ClaudeCodeAgent::new(
        script,
        "test-model".to_string(),
        SandboxMode::DangerFullAccess,
    );
    let request = AgentRequest {
        prompt: "ignored".to_string(),
        project_root: dir.path().to_path_buf(),
        ..AgentRequest::default()
    };
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    let handle = tokio::spawn(async move { agent.execute_stream(request, tx).await });

    if !wait_for_path(&started_marker, Duration::from_secs(10)).await {
        let outcome = timeout(Duration::from_secs(1), handle).await;
        panic!("stream process did not stay alive long enough to observe startup: {outcome:?}");
    }
    if !wait_for_path(&descendant_marker, Duration::from_secs(10)).await {
        let outcome = timeout(Duration::from_secs(1), handle).await;
        panic!("stream process did not start descendant before cancellation: {outcome:?}");
    }

    handle.abort();
    let join_err = timeout(Duration::from_secs(2), handle)
        .await
        .expect("aborted execute_stream task should resolve")
        .expect_err("aborted execute_stream task should not return successfully");
    assert!(
        join_err.is_cancelled(),
        "expected cancelled join error after abort, got: {join_err}"
    );

    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(
        !marker.exists(),
        "process group descendant should be killed when stream future is dropped"
    );
}
