use super::*;
use harness_core::types::Item;

#[test]
fn parse_no_jsonrpc_thread_started_notification() {
    let line = r#"{"method":"thread/started","params":{"thread":{"id":"thread-1"}}}"#;
    let message = parse_codex_message(line).unwrap();
    assert_eq!(
        message,
        ParsedCodexMessage::ThreadStarted {
            thread_id: "thread-1".into()
        }
    );
}

#[test]
fn parse_no_jsonrpc_turn_started_notification() {
    let line = r#"{"method":"turn/started","params":{"threadId":"thread-1","turn":{"id":"turn-1","items":[],"status":"inProgress"}}}"#;
    let message = parse_codex_message(line).unwrap();
    assert_eq!(
        message,
        ParsedCodexMessage::TurnStarted {
            turn_id: "turn-1".into()
        }
    );
}

#[test]
fn parse_agent_message_delta_notification() {
    let line = r#"{"method":"item/agentMessage/delta","params":{"itemId":"item-1","threadId":"thread-1","turnId":"turn-1","delta":"hello"}}"#;
    let message = parse_codex_message(line).unwrap();
    assert_eq!(
        message,
        ParsedCodexMessage::Event(AgentEvent::MessageDelta {
            text: "hello".into()
        })
    );
}

#[test]
fn parse_command_output_delta_notification() {
    let line = r#"{"method":"item/commandExecution/outputDelta","params":{"itemId":"item-1","threadId":"thread-1","turnId":"turn-1","delta":"cargo check\n"}}"#;
    let message = parse_codex_message(line).unwrap();
    assert_eq!(
        message,
        ParsedCodexMessage::Event(AgentEvent::ToolOutputDelta {
            item_id: "item-1".into(),
            text: "cargo check\n".into()
        })
    );
}

#[test]
fn parse_item_started_payload_notification() {
    let line = r#"{"method":"item/started","params":{"threadId":"thread-1","turnId":"turn-1","item":{"id":"item-1","type":"commandExecution","command":"pwd","commandActions":[],"cwd":"/tmp","status":"inProgress","aggregatedOutput":null,"exitCode":null}}}"#;
    let message = parse_codex_message(line).unwrap();
    assert_eq!(
        message,
        ParsedCodexMessage::Event(AgentEvent::ItemStartedPayload {
            item: Item::ShellCommand {
                command: "pwd".into(),
                exit_code: None,
                stdout: String::new(),
                stderr: String::new(),
            }
        })
    );
}

#[test]
fn parse_item_completed_payload_notification() {
    let line = r#"{"method":"item/completed","params":{"threadId":"thread-1","turnId":"turn-1","item":{"id":"item-2","type":"agentMessage","text":"done"}}}"#;
    let message = parse_codex_message(line).unwrap();
    assert_eq!(
        message,
        ParsedCodexMessage::Event(AgentEvent::ItemCompletedPayload {
            item: Item::AgentReasoning {
                content: "done".into()
            }
        })
    );
}

#[test]
fn parse_item_completed_error_notification() {
    let line = r#"{"method":"item/completed","params":{"threadId":"thread-1","turnId":"turn-1","item":{"id":"item-2","type":"error","message":"bad config"}}}"#;
    let message = parse_codex_message(line).unwrap();
    assert_eq!(
        message,
        ParsedCodexMessage::Event(AgentEvent::Error {
            message: "bad config".into()
        })
    );
}

#[test]
fn parse_warning_notification() {
    let line = r#"{"method":"warning","params":{"message":"be careful"}}"#;
    let message = parse_codex_message(line).unwrap();
    assert_eq!(
        message,
        ParsedCodexMessage::Event(AgentEvent::Warning {
            message: "be careful".into()
        })
    );
}

#[test]
fn parse_error_notification() {
    let line = r#"{"method":"error","params":{"threadId":"thread-1","turnId":"turn-1","willRetry":false,"error":{"message":"boom"}}}"#;
    let message = parse_codex_message(line).unwrap();
    assert_eq!(
        message,
        ParsedCodexMessage::Event(AgentEvent::Error {
            message: "boom".into()
        })
    );
}

#[test]
fn otel_turn_spans_parse_token_usage_notification() {
    let line = r#"{"method":"thread/tokenUsage/updated","params":{"threadId":"thread-1","turnId":"turn-1","tokenUsage":{"last":{"inputTokens":10,"cachedInputTokens":4,"outputTokens":3,"reasoningOutputTokens":2,"totalTokens":15},"total":{"inputTokens":10,"cachedInputTokens":4,"outputTokens":3,"reasoningOutputTokens":2,"totalTokens":15}}}}"#;
    let message = parse_codex_message(line).unwrap();
    assert_eq!(
        message,
        ParsedCodexMessage::Event(AgentEvent::TokenUsage {
            usage: harness_core::types::TokenUsage {
                input_tokens: 10,
                output_tokens: 3,
                total_tokens: 15,
                cost_usd: 0.0,
            }
        })
    );
}

#[test]
fn parse_turn_completed_with_empty_output() {
    let line = r#"{"method":"turn/completed","params":{"threadId":"thread-1","turn":{"id":"turn-1","items":[],"status":"completed"}}}"#;
    let message = parse_codex_message(line).unwrap();
    assert_eq!(
        message,
        ParsedCodexMessage::Event(AgentEvent::TurnCompleted {
            output: String::new()
        })
    );
}

#[test]
fn parse_turn_completed_with_embedded_output() {
    let line = r#"{"method":"turn/completed","params":{"threadId":"thread-1","turn":{"id":"turn-1","status":"completed","items":[{"id":"item-9","type":"agentMessage","text":"final answer"}]}}}"#;
    let message = parse_codex_message(line).unwrap();
    assert_eq!(
        message,
        ParsedCodexMessage::Event(AgentEvent::TurnCompleted {
            output: "final answer".into()
        })
    );
}

#[test]
fn parse_approval_request_with_numeric_id() {
    let line = r#"{"id":42,"method":"item/commandExecution/requestApproval","params":{"threadId":"thread-1","turnId":"turn-1","itemId":"item-1","command":"rm -rf /tmp/test"}}"#;
    let message = parse_codex_message(line).unwrap();
    assert_eq!(
        message,
        ParsedCodexMessage::Event(AgentEvent::ApprovalRequest {
            id: "42".into(),
            command: "rm -rf /tmp/test".into()
        })
    );
}

#[test]
fn parse_success_response_without_jsonrpc() {
    let line = r#"{"id":1,"result":{"thread":{"id":"thread-1"}}}"#;
    let message = parse_codex_message(line).unwrap();
    assert_eq!(
        message,
        ParsedCodexMessage::Response {
            id: Value::from(1),
            result: json!({"thread":{"id":"thread-1"}}),
        }
    );
}

#[test]
fn parse_error_response_without_jsonrpc() {
    let line = r#"{"id":1,"error":{"message":"invalid request"}}"#;
    let message = parse_codex_message(line).unwrap();
    assert_eq!(
        message,
        ParsedCodexMessage::Event(AgentEvent::Error {
            message: "invalid request".into()
        })
    );
}

#[test]
fn parse_unknown_notification_returns_ignore() {
    let line = r#"{"method":"custom/unknown","params":{}}"#;
    let message = parse_codex_message(line).unwrap();
    assert_eq!(message, ParsedCodexMessage::Ignore);
}

#[test]
fn parse_invalid_json_returns_none() {
    assert!(parse_codex_message("not json").is_none());
    assert!(parse_codex_message("").is_none());
}

#[test]
fn initialized_notification_payload_has_no_request_id() {
    assert_eq!(
        notification_payload("initialized", Value::Null),
        json!({
            "method": "initialized",
            "params": null,
        })
    );
}

#[test]
fn approval_decision_result_uses_app_server_shape() {
    assert_eq!(
        approval_decision_result(ApprovalDecision::Accept),
        json!({ "decision": "accept" })
    );
    assert_eq!(
        approval_decision_result(ApprovalDecision::Reject {
            reason: "nope".into()
        }),
        json!({
            "decision": "decline",
            "reason": "nope",
        })
    );
}

#[test]
fn start_params_include_runtime_profile_overrides() {
    let req = TurnRequest {
        prompt: "ping".to_string(),
        project_root: PathBuf::from("/tmp/project"),
        model: Some("gpt-runtime".to_string()),
        reasoning_effort: Some("medium".to_string()),
        execution_phase: None,
        sandbox_mode: Some(SandboxMode::WorkspaceWrite),
        approval_policy: Some("on-request".to_string()),
        allowed_tools: vec![],
        context: vec![],
        timeout_secs: Some(60),
        capability_token: None,
    };

    assert_eq!(
        thread_start_params(&req),
        json!({
            "cwd": "/tmp/project",
            "model": "gpt-runtime",
            "sandbox": "workspace-write",
            "approvalPolicy": "on-request",
            "ephemeral": true,
        })
    );
    assert_eq!(
        turn_start_params(&req, "thread-1"),
        json!({
            "threadId": "thread-1",
            "cwd": "/tmp/project",
            "model": "gpt-runtime",
            "effort": "medium",
            "sandboxPolicy": {
                "type": "workspaceWrite",
                "writableRoots": ["/tmp/project"],
            },
            "approvalPolicy": "on-request",
            "input": [
                {
                    "type": "text",
                    "text": "ping",
                }
            ],
        })
    );
}

#[test]
fn sandbox_mode_value_uses_app_server_enum_shape() {
    assert_eq!(
        sandbox_mode_value(Some(SandboxMode::ReadOnly)).as_deref(),
        Some("read-only")
    );
    assert_eq!(
        sandbox_mode_value(Some(SandboxMode::ReadOnlyWithNetwork)).as_deref(),
        Some("read-only")
    );
    assert_eq!(
        sandbox_mode_value(Some(SandboxMode::WorkspaceWrite)).as_deref(),
        Some("workspace-write")
    );
    assert_eq!(
        sandbox_mode_value(Some(SandboxMode::DangerFullAccess)).as_deref(),
        Some("danger-full-access")
    );
    assert_eq!(sandbox_mode_value(None), None);
}

#[test]
fn sandbox_policy_value_preserves_network_for_read_only_with_network() {
    assert_eq!(
        sandbox_policy_value(
            Some(SandboxMode::ReadOnlyWithNetwork),
            &PathBuf::from("/tmp/project")
        ),
        Some(json!({
            "type": "readOnly",
            "networkAccess": true,
        }))
    );
}

#[test]
fn protocol_line_preview_truncates_without_full_count_scan() {
    assert_eq!(protocol_line_preview("short"), "short");
    assert_eq!(
        protocol_line_preview(&"x".repeat(MAX_PROTOCOL_LINE_PREVIEW)),
        "x".repeat(MAX_PROTOCOL_LINE_PREVIEW)
    );
    assert_eq!(
        protocol_line_preview(&format!("{}y", "x".repeat(MAX_PROTOCOL_LINE_PREVIEW))),
        format!("{}...", "x".repeat(MAX_PROTOCOL_LINE_PREVIEW))
    );
}

#[tokio::test]
async fn interrupt_noop_when_no_child() {
    let adapter = CodexAdapter::new(PathBuf::from("codex"));
    adapter.interrupt().await.unwrap();
}

#[tokio::test]
async fn start_turn_missing_workspace_reports_workspace_missing() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let missing = dir.path().join("missing-workspace");
    let adapter = CodexAdapter::new(std::env::current_exe()?);
    let request = TurnRequest {
        prompt: "ping".to_string(),
        project_root: missing.clone(),
        model: None,
        reasoning_effort: None,
        execution_phase: None,
        sandbox_mode: None,
        approval_policy: None,
        allowed_tools: vec![],
        context: vec![],
        timeout_secs: None,
        capability_token: None,
    };
    let (tx, _rx) = mpsc::channel(4);

    let error = match adapter.start_turn(request, tx).await {
        Ok(()) => panic!("missing project root should fail before codex app-server starts"),
        Err(error) => error,
    };
    let message = error.to_string();

    assert!(
        message.starts_with(&format!(
            "agent execution failed: workspace missing: {}",
            missing.display()
        )),
        "missing workspace must be primary, got: {message}"
    );
    assert!(message.contains("failed to spawn codex app-server"));
    Ok(())
}

#[tokio::test]
async fn clear_active_turn_id_drops_stale_turn_state() {
    let adapter = CodexAdapter::new(PathBuf::from("codex"));
    adapter.state.lock().await.active_turn_id = Some("turn-1".into());

    adapter.clear_active_turn_id().await;

    assert_eq!(adapter.state.lock().await.active_turn_id, None);
}

#[tokio::test]
async fn start_turn_fails_when_stdout_eofs_before_terminal_event() {
    let adapter = CodexAdapter::new(PathBuf::from("codex"));
    let mut child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(
            r#"printf '%s\n' '{"method":"turn/started","params":{"threadId":"thread-1","turn":{"id":"turn-1","status":"inProgress","items":[]}}}'; read _ || true"#,
        )
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("stub app-server should spawn");
    let stdout = child.stdout.take().expect("stdout should be piped");
    let stdin = child.stdin.take().expect("stdin should be piped");
    {
        let mut state = adapter.state.lock().await;
        state.child = Some(child);
        state.stdin = Some(stdin);
        state.stdout_lines = Some(BufReader::new(stdout).lines());
        state.thread_id = Some("thread-1".into());
    }

    let req = TurnRequest {
        prompt: "ping".to_string(),
        project_root: PathBuf::from("/tmp/project"),
        model: None,
        reasoning_effort: None,
        execution_phase: None,
        sandbox_mode: None,
        approval_policy: None,
        allowed_tools: vec![],
        context: vec![],
        timeout_secs: None,
        capability_token: None,
    };
    let (tx, mut rx) = mpsc::channel(4);

    let error = adapter
        .start_turn(req, tx)
        .await
        .expect_err("stdout EOF before a terminal event should fail");

    assert!(matches!(rx.try_recv(), Ok(AgentEvent::TurnStarted)));
    assert!(format!("{error}").contains("stdout closed before turn/completed"));
    let state = adapter.state.lock().await;
    assert!(state.child.is_none());
    assert!(state.stdin.is_none());
    assert!(state.stdout_lines.is_none());
    assert!(state.thread_id.is_none());
    assert!(state.active_turn_id.is_none());
}

#[tokio::test]
async fn adapter_state_reports_incomplete_child_when_stdout_reader_is_missing() {
    let mut child = tokio::process::Command::new("sleep")
        .arg("60")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("sleep process should spawn");
    let stdout = child.stdout.take().expect("stdout should be piped");
    let stdin = child.stdin.take().expect("stdin should be piped");
    let mut state = AdapterState::new();
    state.child = Some(child);
    state.stdin = Some(stdin);
    state.stdout_lines = Some(BufReader::new(stdout).lines());

    assert!(state.child_ready());
    state.stdout_lines = None;
    assert!(!state.child_ready());

    state.reset_child().await;
}
