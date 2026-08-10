use super::*;
use harness_core::types::Item;
use std::collections::HashMap;

fn test_turn_request(project_root: PathBuf) -> AgentRequest {
    AgentRequest {
        prompt: "ping".to_string(),
        prompt_layers: None,
        project_root,
        permission_mode: harness_core::config::agents::AgentPermissionMode::Full,
        model: None,
        reasoning_effort: None,
        execution_phase: None,
        sandbox_mode: None,
        approval_policy: None,
        allowed_tools: None,
        max_budget_usd: None,
        context: vec![],
        timeout_secs: None,
        env_vars: HashMap::new(),
        capability_token: None,
    }
}

#[cfg(unix)]
fn write_app_server_stub(dir: &std::path::Path, body: &str) -> anyhow::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;

    let path = dir.join("codex-app-server-stub");
    std::fs::write(&path, format!("#!/bin/sh\n{body}\nsleep 60\n"))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    Ok(path)
}

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
        ParsedCodexMessage::Event(AgentEvent::ItemStarted {
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
        ParsedCodexMessage::Event(AgentEvent::ItemCompleted {
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
    let req = AgentRequest {
        prompt: "ping".to_string(),
        prompt_layers: None,
        project_root: PathBuf::from("/tmp/project"),
        permission_mode: harness_core::config::agents::AgentPermissionMode::Full,
        model: Some("gpt-runtime".to_string()),
        reasoning_effort: Some("medium".to_string()),
        execution_phase: None,
        sandbox_mode: Some(SandboxMode::WorkspaceWrite),
        approval_policy: Some("on-request".to_string()),
        allowed_tools: None,
        max_budget_usd: None,
        context: vec![],
        timeout_secs: Some(60),
        env_vars: HashMap::new(),
        capability_token: None,
    };

    assert_eq!(
        thread_start_params(&req, &req.project_root),
        json!({
            "cwd": "/tmp/project",
            "model": "gpt-runtime",
            "sandbox": "workspace-write",
            "approvalPolicy": "on-request",
            "ephemeral": true,
        })
    );
    assert_eq!(
        turn_start_params(&req, "thread-1", &req.project_root),
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
    let request = AgentRequest {
        prompt: "ping".to_string(),
        prompt_layers: None,
        project_root: PathBuf::from("/tmp/project"),
        permission_mode: harness_core::config::agents::AgentPermissionMode::Full,
        model: None,
        reasoning_effort: None,
        execution_phase: None,
        sandbox_mode: None,
        approval_policy: Some("on-request".to_string()),
        allowed_tools: None,
        max_budget_usd: None,
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
        SandboxMode::DangerFullAccess,
    );
    let request = AgentRequest {
        prompt: "ping".to_string(),
        prompt_layers: None,
        project_root: dir.path().to_path_buf(),
        permission_mode: harness_core::config::agents::AgentPermissionMode::Full,
        model: None,
        reasoning_effort: None,
        execution_phase: None,
        sandbox_mode: None,
        approval_policy: Some("on-request".to_string()),
        allowed_tools: None,
        max_budget_usd: None,
        context: vec![],
        timeout_secs: None,
        env_vars: HashMap::new(),
        capability_token: None,
    };
    let (tx, _rx) = mpsc::channel(4);

    let error = adapter
        .start_turn(request, tx)
        .await
        .expect_err("missing codex executable should fail after setup");

    assert!(marker.exists(), "setup marker missing after error: {error}");
    Ok(())
}

#[tokio::test]
async fn app_server_spawn_honors_container_isolation_without_egress() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let mut env_vars = HashMap::new();
    env_vars.insert(
        harness_core::agent::AGENT_ISOLATION_TIER_ENV.to_string(),
        "container".to_string(),
    );
    let request = AgentRequest {
        prompt: "ping".to_string(),
        prompt_layers: None,
        project_root: root.path().to_path_buf(),
        permission_mode: Default::default(),
        model: None,
        reasoning_effort: None,
        execution_phase: None,
        sandbox_mode: Some(SandboxMode::WorkspaceWrite),
        approval_policy: Some("on-request".to_string()),
        allowed_tools: None,
        max_budget_usd: None,
        context: vec![],
        timeout_secs: None,
        env_vars,
        capability_token: None,
    };

    let cloud = harness_core::config::agents::CodexCloudConfig::default();
    let spawn = prepare_app_server_spawn(std::path::Path::new("codex"), &cloud, &request).await?;
    let args = spawn
        .args
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>();

    assert_eq!(spawn.program, PathBuf::from("docker"));
    assert_eq!(spawn.child_workspace, PathBuf::from("/workspace"));
    assert!(spawn.clear_inherited_env);
    assert!(args.contains(&"--network".to_string()));
    assert!(args.contains(&"none".to_string()));
    assert!(args.contains(&"app-server".to_string()));
    assert!(args.contains(&"stdio://".to_string()));
    Ok(())
}

#[tokio::test]
async fn app_server_spawn_keeps_host_workspace_path() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let request = test_turn_request(root.path().to_path_buf());

    let cloud = harness_core::config::agents::CodexCloudConfig::default();
    let spawn = prepare_app_server_spawn(std::path::Path::new("codex"), &cloud, &request).await?;

    assert_eq!(spawn.child_workspace, root.path());
    assert_eq!(
        thread_start_params(&request, &spawn.child_workspace)["cwd"],
        json!(root.path())
    );
    Ok(())
}

#[test]
fn app_server_params_use_container_workspace_path() {
    let mut request = test_turn_request(PathBuf::from("/host/project"));
    request.sandbox_mode = Some(SandboxMode::WorkspaceWrite);
    let child_workspace = PathBuf::from("/workspace");

    assert_eq!(
        thread_start_params(&request, &child_workspace)["cwd"],
        json!("/workspace")
    );
    let turn = turn_start_params(&request, "thread-1", &child_workspace);
    assert_eq!(turn["cwd"], json!("/workspace"));
    assert_eq!(
        turn["sandboxPolicy"]["writableRoots"],
        json!(["/workspace"])
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
            std::path::Path::new("/tmp/project")
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
#[cfg(unix)]
async fn app_server_read_times_out_when_stdout_stalls() -> anyhow::Result<()> {
    let mut child = tokio::process::Command::new("sleep")
        .arg("60")
        .stdout(std::process::Stdio::piped())
        .spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("stdout should be piped"))?;
    let mut lines = BufReader::new(stdout).lines();

    let error = CodexAdapter::read_next_message_with_timeout(
        &mut lines,
        Some(std::time::Duration::from_millis(50)),
        "initialize",
    )
    .await
    .expect_err("silent app-server stdout must hit the stall timeout");

    child.kill().await?;
    child.wait().await?;
    assert!(format!("{error}").contains("initialize stalled for 50ms"));
    Ok(())
}

#[tokio::test]
#[cfg(unix)]
async fn app_server_protocol_failures_reset_and_reap_child() -> anyhow::Result<()> {
    let scenarios = [
        ("", "initialize stalled"),
        (
            r#"printf '%s\n' '{"method":"error","params":{"message":"init failed"}}'"#,
            "init failed",
        ),
        (
            r#"printf '%s\n' '{"id":1,"result":{}}'"#,
            "thread/start stalled",
        ),
        (
            concat!(
                r#"printf '%s\n' '{"id":1,"result":{}}'"#,
                "\n",
                r#"printf '%s\n' '{"method":"error","params":{"message":"thread failed"}}'"#,
            ),
            "thread failed",
        ),
        (
            concat!(
                r#"printf '%s\n' '{"id":1,"result":{}}'"#,
                "\n",
                r#"printf '%s\n' '{"id":2,"result":{"thread":{"id":"thread-1"}}}'"#,
            ),
            "turn stalled",
        ),
        (
            concat!(
                r#"printf '%s\n' '{"id":1,"result":{}}'"#,
                "\n",
                r#"printf '%s\n' '{"id":2,"result":{"thread":{"id":"thread-1"}}}'"#,
                "\n",
                r#"printf '%s\n' 'not-json'"#,
            ),
            "invalid JSON-RPC",
        ),
    ];

    for (body, expected) in scenarios {
        let dir = tempfile::tempdir()?;
        let adapter = CodexAdapter::new(write_app_server_stub(dir.path(), body)?);
        let mut request = test_turn_request(dir.path().to_path_buf());
        request.timeout_secs = Some(if expected.contains("stalled") { 3 } else { 10 });
        let (tx, _rx) = mpsc::channel(4);

        let error = adapter.start_turn(request, tx).await.expect_err(expected);
        assert!(format!("{error}").contains(expected), "{expected}: {error}");
        let state = adapter.state.lock().await;
        assert!(state.child.is_none(), "{expected}: child was not reaped");
        assert!(state.stdin.is_none());
        assert!(state.stdout_lines.is_none());
        assert!(state.thread_id.is_none());
        assert!(state.active_turn_id.is_none());
        assert!(state.child_workspace.is_none());
    }
    Ok(())
}

#[tokio::test]
#[cfg(unix)]
async fn expired_capability_token_never_spawns_app_server() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let marker = dir.path().join("spawned");
    let adapter = CodexAdapter::new(write_app_server_stub(
        dir.path(),
        r#"printf spawned > "$SPAWN_MARKER""#,
    )?);
    let mut request = test_turn_request(dir.path().to_path_buf());
    request
        .env_vars
        .insert("SPAWN_MARKER".into(), marker.display().to_string());
    let mut token = harness_core::capability::CapabilityToken::new(
        7,
        vec![dir.path().to_path_buf()],
        std::time::Duration::from_secs(60),
    );
    token.expires_at = std::time::SystemTime::UNIX_EPOCH;
    request.capability_token = Some(token);
    request.timeout_secs = Some(1);
    let (tx, _rx) = mpsc::channel(4);

    let error = adapter
        .start_turn(request, tx)
        .await
        .expect_err("expired token must fail before spawn");
    assert!(format!("{error}").contains("subtask 7 has expired"));
    assert!(!marker.exists(), "expired token spawned the app-server");
    assert!(adapter.state.lock().await.child.is_none());
    Ok(())
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
    let request = AgentRequest {
        prompt: "ping".to_string(),
        prompt_layers: None,
        project_root: missing.clone(),
        permission_mode: Default::default(),
        model: None,
        reasoning_effort: None,
        execution_phase: None,
        sandbox_mode: None,
        approval_policy: None,
        allowed_tools: None,
        max_budget_usd: None,
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
        state.child = Some(crate::ManagedChild::new(child, "codex app-server test"));
        state.stdin = Some(stdin);
        state.stdout_lines = Some(BufReader::new(stdout).lines());
        state.thread_id = Some("thread-1".into());
        state.child_workspace = Some(PathBuf::from("/tmp/project"));
    }

    let req = AgentRequest {
        prompt: "ping".to_string(),
        prompt_layers: None,
        project_root: PathBuf::from("/tmp/project"),
        permission_mode: Default::default(),
        model: None,
        reasoning_effort: None,
        execution_phase: None,
        sandbox_mode: None,
        approval_policy: None,
        allowed_tools: None,
        max_budget_usd: None,
        context: vec![],
        timeout_secs: None,
        env_vars: HashMap::new(),
        capability_token: None,
    };
    adapter.state.lock().await.spawn_policy_fingerprint = Some(
        crate::spawn_contract::adapter_spawn_policy_fingerprint(&req, adapter.sandbox_mode),
    );
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
#[cfg(unix)]
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
    state.child = Some(crate::ManagedChild::new(child, "codex app-server test"));
    state.stdin = Some(stdin);
    state.stdout_lines = Some(BufReader::new(stdout).lines());

    assert!(state.child_ready());
    state.stdout_lines = None;
    assert!(!state.child_ready());

    state.reset_child().await;
}
