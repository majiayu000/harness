use super::*;
use harness_core::types::Item;
use std::collections::HashMap;

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
    let line = r#"{"method":"thread/tokenUsage/updated","params":{"threadId":"thread-1","turnId":"turn-1","tokenUsage":{"last":{"inputTokens":10,"cachedInputTokens":4,"outputTokens":3,"reasoningOutputTokens":2,"totalTokens":15},"total":{"inputTokens":25,"cachedInputTokens":9,"outputTokens":8,"reasoningOutputTokens":5,"totalTokens":38}}}}"#;
    let message = parse_codex_message(line).unwrap();
    assert_eq!(
        message,
        ParsedCodexMessage::Event(AgentEvent::TokenUsage {
            usage: harness_core::types::TokenUsage {
                input_tokens: 25,
                output_tokens: 8,
                total_tokens: 38,
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
        prompt_layers: None,
        project_root: PathBuf::from("/tmp/project"),
        model: Some("gpt-runtime".to_string()),
        reasoning_effort: Some("medium".to_string()),
        execution_phase: None,
        sandbox_mode: Some(SandboxMode::WorkspaceWrite),
        approval_policy: Some("on-request".to_string()),
        allowed_tools: vec![],
        context: vec![],
        timeout_secs: Some(60),
        env_vars: HashMap::new(),
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
fn configured_adapter_applies_defaults_identity_and_secret_filtering() {
    let adapter = CodexAdapter::from_config(
        harness_core::config::agents::CodexAgentConfig {
            cli_path: PathBuf::from("codex"),
            default_model: "configured-model".to_string(),
            reasoning_effort: "configured-effort".to_string(),
            cloud: harness_core::config::agents::CodexCloudConfig {
                enabled: true,
                cache_ttl_hours: 0,
                setup_commands: Vec::new(),
                setup_secret_env: vec!["SETUP_SECRET".to_string()],
            },
        },
        SandboxMode::ReadOnly,
    );
    let mut env_vars = HashMap::new();
    env_vars.insert("SETUP_SECRET".to_string(), "secret-value".to_string());
    let request = TurnRequest {
        prompt: "ping".to_string(),
        prompt_layers: None,
        project_root: PathBuf::from("/tmp/project"),
        model: None,
        reasoning_effort: None,
        execution_phase: None,
        sandbox_mode: None,
        approval_policy: Some("on-request".to_string()),
        allowed_tools: vec![],
        context: vec![],
        timeout_secs: None,
        env_vars,
        capability_token: None,
    };

    let request = adapter.effective_turn_request(request);

    assert_eq!(request.model.as_deref(), Some("configured-model"));
    assert_eq!(
        request.reasoning_effort.as_deref(),
        Some("configured-effort")
    );
    assert_eq!(request.sandbox_mode, Some(SandboxMode::ReadOnly));
    assert!(!request.env_vars.contains_key("SETUP_SECRET"));
    assert!(request
        .env_vars
        .get(harness_core::run_id::AGENT_RUN_ID_ENV)
        .is_some_and(|run_id| run_id.starts_with("ar-")));
}

#[tokio::test]
async fn configured_adapter_runs_cloud_setup_before_spawn() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let marker = dir.path().join("adapter-setup-ran");
    let adapter = CodexAdapter::from_config(
        harness_core::config::agents::CodexAgentConfig {
            cli_path: dir.path().join("missing-codex"),
            default_model: "configured-model".to_string(),
            reasoning_effort: "configured-effort".to_string(),
            cloud: harness_core::config::agents::CodexCloudConfig {
                enabled: true,
                cache_ttl_hours: 0,
                setup_commands: vec!["touch adapter-setup-ran".to_string()],
                setup_secret_env: Vec::new(),
            },
        },
        SandboxMode::WorkspaceWrite,
    );
    let request = TurnRequest {
        prompt: "ping".to_string(),
        prompt_layers: None,
        project_root: dir.path().to_path_buf(),
        model: None,
        reasoning_effort: None,
        execution_phase: None,
        sandbox_mode: None,
        approval_policy: Some("on-request".to_string()),
        allowed_tools: vec![],
        context: vec![],
        timeout_secs: None,
        env_vars: HashMap::new(),
        capability_token: None,
    };
    let (tx, _rx) = mpsc::channel(4);

    adapter
        .start_turn(request, tx)
        .await
        .expect_err("missing codex executable should fail after setup");

    assert!(marker.exists());
    Ok(())
}

#[test]
fn app_server_spawn_honors_container_isolation() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let mut env_vars = HashMap::new();
    env_vars.insert(
        harness_core::agent::AGENT_ISOLATION_TIER_ENV.to_string(),
        "container".to_string(),
    );
    env_vars.insert(
        harness_core::agent::AGENT_NETWORK_ALLOWLIST_ENV.to_string(),
        "github.com".to_string(),
    );
    let request = TurnRequest {
        prompt: "ping".to_string(),
        prompt_layers: None,
        project_root: root.path().to_path_buf(),
        model: None,
        reasoning_effort: None,
        execution_phase: None,
        sandbox_mode: Some(SandboxMode::WorkspaceWrite),
        approval_policy: Some("on-request".to_string()),
        allowed_tools: vec![],
        context: vec![],
        timeout_secs: None,
        env_vars,
        capability_token: None,
    };

    let spawn = prepare_app_server_spawn(std::path::Path::new("codex"), &request)?;
    let args = spawn
        .args
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>();

    assert_eq!(spawn.program, PathBuf::from("docker"));
    assert!(spawn.clear_inherited_env);
    assert!(args.contains(&"--network".to_string()));
    assert!(args.contains(&"none".to_string()));
    assert!(args.contains(&"app-server".to_string()));
    assert!(args.contains(&"stdio://".to_string()));
    Ok(())
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
        prompt_layers: None,
        project_root: missing.clone(),
        model: None,
        reasoning_effort: None,
        execution_phase: None,
        sandbox_mode: None,
        approval_policy: None,
        allowed_tools: vec![],
        context: vec![],
        timeout_secs: None,
        env_vars: HashMap::new(),
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
        prompt_layers: None,
        project_root: PathBuf::from("/tmp/project"),
        model: None,
        reasoning_effort: None,
        execution_phase: None,
        sandbox_mode: None,
        approval_policy: None,
        allowed_tools: vec![],
        context: vec![],
        timeout_secs: None,
        env_vars: HashMap::new(),
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
