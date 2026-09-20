use super::bounded_frame::{BoundedStdoutReader, DEFAULT_MAX_PROTOCOL_FRAME_BYTES};
use super::*;
use harness_core::agent::AgentAdapter;
use harness_core::{agent::AgentDiagnosticSeverity, types::Item};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::BufReader;
use tokio::time::Instant;

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
        ParsedCodexMessage::Event(AgentEvent::Diagnostic {
            severity: AgentDiagnosticSeverity::Error,
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
        ParsedCodexMessage::Event(AgentEvent::Diagnostic {
            severity: AgentDiagnosticSeverity::Warning,
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
        ParsedCodexMessage::Event(AgentEvent::Diagnostic {
            severity: AgentDiagnosticSeverity::Error,
            message: "boom".into()
        })
    );
}

#[test]
fn parse_failed_turn_completed_notification_as_terminal_error() {
    let line = r#"{"method":"turn/completed","params":{"threadId":"thread-1","turn":{"id":"turn-1","status":"failed","items":[],"error":{"message":"model failed"}}}}"#;
    let message = parse_codex_message(line).unwrap();
    assert_eq!(
        message,
        ParsedCodexMessage::Event(AgentEvent::Error {
            message: "model failed".into()
        })
    );
}

#[test]
fn parse_interrupted_turn_completed_notification_as_cancellation() {
    let line = r#"{"method":"turn/completed","params":{"threadId":"thread-1","turn":{"id":"turn-1","status":"interrupted","items":[],"error":null}}}"#;
    let message = parse_codex_message(line).unwrap();
    assert_eq!(
        message,
        ParsedCodexMessage::Event(AgentEvent::TurnCancelled {
            message: "codex turn interrupted".into()
        })
    );
}

#[test]
fn parse_turn_completed_without_status_fails_closed() {
    let line = r#"{"method":"turn/completed","params":{"threadId":"thread-1","turn":{"id":"turn-1","items":[]}}}"#;
    let message = parse_codex_message(line).unwrap();
    assert_eq!(
        message,
        ParsedCodexMessage::Event(AgentEvent::Error {
            message: "codex turn/completed omitted required turn.status".into()
        })
    );
}

#[test]
fn otel_turn_spans_parse_token_usage_notification() {
    let line = r#"{"method":"thread/tokenUsage/updated","params":{"threadId":"thread-1","turnId":"turn-1","tokenUsage":{"last":{"inputTokens":10,"cachedInputTokens":4,"outputTokens":3,"reasoningOutputTokens":2,"totalTokens":13},"total":{"inputTokens":25,"cachedInputTokens":9,"outputTokens":8,"reasoningOutputTokens":5,"totalTokens":33}}}}"#;
    let message = parse_codex_message(line).unwrap();
    let ParsedCodexMessage::Event(AgentEvent::TokenUsage { ref usage, .. }) = message else {
        panic!("expected usage notification");
    };
    let metrics = harness_observe::usage::UsageMetrics::from_token_usage(usage);
    assert_eq!(metrics.cache_read_input_tokens, 9);
    assert_eq!(metrics.total_tokens(), 33);
    assert_eq!(
        message,
        ParsedCodexMessage::Event(AgentEvent::TokenUsage {
            usage: harness_core::types::TokenUsage {
                input_tokens: 16,
                output_tokens: 8,
                total_tokens: 33,
                cost_usd: 0.0,
            },
            cost_usd_observed: false,
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
    let mut lines =
        BoundedStdoutReader::new(BufReader::new(stdout), DEFAULT_MAX_PROTOCOL_FRAME_BYTES);

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
            r#"printf '%s\n' '{"id":1,"error":{"message":"init failed"}}'"#,
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
                r#"printf '%s\n' '{"id":2,"error":{"message":"thread failed"}}'"#,
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
        state.stdout_lines = Some(adapter.wrap_stdout(stdout));
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
async fn start_turn_continues_after_error_notification_until_completed() -> anyhow::Result<()> {
    let project_root = tempfile::tempdir()?;
    let adapter = CodexAdapter::new(PathBuf::from("codex"));
    let mut child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(
            r#"printf '%s\n' '{"method":"turn/started","params":{"threadId":"thread-1","turn":{"id":"turn-1","status":"inProgress","items":[]}}}'; printf '%s\n' '{"method":"error","params":{"threadId":"thread-1","turnId":"turn-1","willRetry":false,"error":{"message":"Skill descriptions were shortened to fit the skills context budget."}}}'; printf '%s\n' '{"method":"turn/completed","params":{"threadId":"thread-1","turn":{"id":"turn-1","status":"completed","items":[]}}}'; read _ || true"#,
        )
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("stdout should be piped"))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("stdin should be piped"))?;
    {
        let mut state = adapter.state.lock().await;
        state.child = Some(crate::ManagedChild::new(child, "codex app-server test"));
        state.stdin = Some(stdin);
        state.stdout_lines = Some(adapter.wrap_stdout(stdout));
        state.thread_id = Some("thread-1".into());
        state.child_workspace = Some(project_root.path().to_path_buf());
    }

    let request = test_turn_request(project_root.path().to_path_buf());
    adapter.state.lock().await.spawn_policy_fingerprint = Some(
        crate::spawn_contract::adapter_spawn_policy_fingerprint(&request, adapter.sandbox_mode),
    );
    let (tx, mut rx) = mpsc::channel(8);

    adapter.start_turn(request, tx).await?;

    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    assert!(events.iter().any(|event| {
        matches!(
            event,
            AgentEvent::Diagnostic {
                severity: AgentDiagnosticSeverity::Error,
                ..
            }
        )
    }));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::TurnCompleted { .. })),
        "diagnostic notifications must not hide the explicit turn terminal event: {events:?}"
    );
    Ok(())
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
    state.stdout_lines = Some(BoundedStdoutReader::new(
        BufReader::new(stdout),
        DEFAULT_MAX_PROTOCOL_FRAME_BYTES,
    ));

    assert!(state.child_ready());
    state.stdout_lines = None;
    assert!(!state.child_ready());

    state
        .reset_child()
        .await
        .expect("sleep child cleanup should succeed");
}

#[tokio::test]
#[cfg(unix)]
async fn terminate_propagates_injected_cleanup_failure_and_blocks_reuse() {
    let mut child = tokio::process::Command::new("sleep")
        .arg("60")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("sleep process should spawn");
    let stdout = child.stdout.take().expect("stdout should be piped");
    let stdin = child.stdin.take().expect("stdin should be piped");
    let adapter = CodexAdapter::new(PathBuf::from("unused-codex"));
    {
        let mut state = adapter.state.lock().await;
        state.child = Some(
            crate::ManagedChild::new(child, "codex app-server test").with_injected_cleanup_error(
                std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "injected descendant cleanup failure",
                ),
            ),
        );
        state.stdin = Some(stdin);
        state.stdout_lines = Some(adapter.wrap_stdout(stdout));
        state.thread_id = Some("thread-1".into());
        state.active_turn_id = Some("turn-1".into());
        state.child_workspace = Some(PathBuf::from("/tmp/workspace"));
    }

    let first = adapter
        .terminate_and_drain()
        .await
        .expect_err("injected cleanup failure must surface");
    let first_message = first.to_string();
    assert!(
        first_message.contains("injected descendant cleanup failure"),
        "terminate must report the cleanup failure: {first_message}"
    );

    {
        let mut state = adapter.state.lock().await;
        assert!(
            state.child.is_some(),
            "failed cleanup must retain ManagedChild ownership"
        );
        assert!(
            state.failed_cleanup.is_some(),
            "failed cleanup outcome must remain visible"
        );
        assert!(
            !state.permits_child_reuse(),
            "reuse gate must reject after cleanup failure"
        );
        assert!(!state.child_ready());
        assert!(state.stdin.is_none());
        assert!(state.stdout_lines.is_none());
        // Simulate losing the child handle while cleanup remains unconfirmed:
        // terminate must not become success merely because `child` is now None.
        state.child = None;
    }

    let second = adapter
        .terminate_and_drain()
        .await
        .expect_err("repeated terminate must not become success while cleanup remains failed");
    assert!(
        second.to_string().contains("failed to clean up"),
        "repeated terminate must keep the failed cleanup outcome: {second}"
    );
    assert!(
        !adapter.state.lock().await.permits_child_reuse(),
        "reuse gate must stay closed until cleanup is confirmed"
    );
}

#[tokio::test]
#[cfg(unix)]
async fn reset_after_error_preserves_primary_and_cleanup_failures() {
    let mut child = tokio::process::Command::new("sleep")
        .arg("60")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("sleep process should spawn");
    let _stdout = child.stdout.take().expect("stdout should be piped");
    let _stdin = child.stdin.take().expect("stdin should be piped");
    let mut state = AdapterState::new();
    state.child = Some(
        crate::ManagedChild::new(child, "codex app-server test").with_injected_cleanup_error(
            std::io::Error::other("injected descendant cleanup failure"),
        ),
    );

    let combined = reset_after_error(
        &mut state,
        harness_core::error::HarnessError::AgentExecution(
            "codex app-server stdout closed before turn/completed".into(),
        ),
        std::time::Duration::from_secs(45),
    )
    .await;
    let message = combined.to_string();
    assert!(
        message.contains("stdout closed before turn/completed"),
        "primary error must be preserved: {message}"
    );
    assert!(
        message.contains("cleanup failed"),
        "cleanup failure must be attached: {message}"
    );
    assert!(
        message.contains("injected descendant cleanup failure"),
        "cleanup detail must remain: {message}"
    );
    assert!(state.child.is_some());
    assert!(!state.permits_child_reuse());
}

#[cfg(unix)]
fn write_python_app_server(dir: &std::path::Path, body: &str) -> anyhow::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;

    let path = dir.join("codex-app-server-stub");
    std::fs::write(
        &path,
        format!("#!/usr/bin/env python3\nimport json, os, sys, time\n{body}\n"),
    )?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    Ok(path)
}

fn short_deadlines() -> AttemptDeadlines {
    AttemptDeadlines {
        absolute_init: Duration::from_millis(250),
        frame_write: Duration::from_millis(200),
        stop_cleanup: Duration::from_millis(500),
    }
}

fn control_test_deadlines() -> AttemptDeadlines {
    AttemptDeadlines {
        absolute_init: Duration::from_secs(5),
        frame_write: Duration::from_millis(200),
        stop_cleanup: Duration::from_secs(2),
    }
}

#[tokio::test]
#[cfg(unix)]
async fn interrupt_before_remote_turn_id_is_honored_after_barrier() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let turn_start_seen = dir.path().join("turn-start-seen");
    let release_turn_started = dir.path().join("release-turn-started");
    let stub = write_python_app_server(
        dir.path(),
        r#"
turn_start_seen = os.environ["TURN_START_SEEN"]
release_turn_started = os.environ["RELEASE_TURN_STARTED"]
for raw in sys.stdin:
    line = raw.strip()
    if not line:
        continue
    msg = json.loads(line)
    method = msg.get("method")
    if method == "initialize":
        print(json.dumps({"id": msg["id"], "result": {}}), flush=True)
    elif method == "thread/start":
        print(json.dumps({"method": "thread/started", "params": {"thread": {"id": "thread-1"}}}), flush=True)
    elif method == "turn/start":
        open(turn_start_seen, "w").close()
        while not os.path.exists(release_turn_started):
            time.sleep(0.01)
        print(json.dumps({"method": "turn/started", "params": {"threadId": "thread-1", "turn": {"id": "turn-1", "status": "inProgress", "items": []}}}), flush=True)
    elif method == "turn/interrupt":
        print(json.dumps({"id": msg["id"], "result": {}}), flush=True)
        print(json.dumps({"method": "turn/completed", "params": {"threadId": "thread-1", "turn": {"id": "turn-1", "status": "interrupted", "items": [], "error": None}}}), flush=True)
"#,
    )?;
    let adapter = Arc::new(CodexAdapter::new(stub).with_deadlines(AttemptDeadlines {
        absolute_init: Duration::from_secs(5),
        frame_write: Duration::from_secs(2),
        stop_cleanup: Duration::from_secs(2),
    }));
    let mut request = test_turn_request(dir.path().to_path_buf());
    request.env_vars.insert(
        "TURN_START_SEEN".into(),
        turn_start_seen.display().to_string(),
    );
    request.env_vars.insert(
        "RELEASE_TURN_STARTED".into(),
        release_turn_started.display().to_string(),
    );
    request.timeout_secs = Some(5);
    let (tx, mut rx) = mpsc::channel(8);
    let turn: tokio::task::JoinHandle<harness_core::error::Result<()>> = {
        let adapter = Arc::clone(&adapter);
        tokio::spawn(async move { adapter.as_ref().start_turn(request, tx).await })
    };

    let started = Instant::now();
    while !turn_start_seen.exists() {
        if started.elapsed() > Duration::from_secs(15) {
            panic!("timed out waiting for turn/start barrier");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    adapter.interrupt().await?;
    assert!(
        adapter
            .state
            .lock()
            .await
            .active_attempt
            .as_ref()
            .is_some_and(|attempt| attempt.cancel_requested),
        "interrupt before remote turn id must record attempt-scoped cancel intent"
    );
    std::fs::write(&release_turn_started, b"go")?;

    let result = turn.await.expect("join");
    // Either cancelled locally or completed as interrupted — never a quiet success
    // after a pre-ID interrupt without delivering cancel intent.
    match result {
        Ok(()) => {
            let mut saw_cancelled = false;
            while let Ok(event) = rx.try_recv() {
                if matches!(event, AgentEvent::TurnCancelled { .. }) {
                    saw_cancelled = true;
                }
            }
            assert!(
                saw_cancelled,
                "successful return after pre-ID interrupt must observe TurnCancelled"
            );
        }
        Err(error) => {
            let message = error.to_string();
            assert!(
                message.contains("cancelled") || message.contains("interrupted"),
                "unexpected failure after pre-ID interrupt: {message}"
            );
        }
    }
    Ok(())
}

#[tokio::test]
#[cfg(unix)]
async fn interrupt_returns_while_initialize_floods_irrelevant_notifications() -> anyhow::Result<()>
{
    let dir = tempfile::tempdir()?;
    let init_seen = dir.path().join("init-seen");
    let stub = write_python_app_server(
        dir.path(),
        r#"
init_seen = os.environ["INIT_SEEN"]
for raw in sys.stdin:
    line = raw.strip()
    if not line:
        continue
    msg = json.loads(line)
    if msg.get("method") == "initialize":
        open(init_seen, "w").close()
        while True:
            print(json.dumps({"method": "warning", "params": {"message": "noise"}}), flush=True)
            time.sleep(0.02)
"#,
    )?;
    let adapter = Arc::new(CodexAdapter::new(stub).with_deadlines(control_test_deadlines()));
    let mut request = test_turn_request(dir.path().to_path_buf());
    request
        .env_vars
        .insert("INIT_SEEN".into(), init_seen.display().to_string());
    request.timeout_secs = None; // stdout-stall disabled; absolute init still applies
    let (tx, _rx) = mpsc::channel(8);
    let turn: tokio::task::JoinHandle<harness_core::error::Result<()>> = {
        let adapter = Arc::clone(&adapter);
        tokio::spawn(async move { adapter.as_ref().start_turn(request, tx).await })
    };

    let started = Instant::now();
    while !init_seen.exists() {
        if started.elapsed() > Duration::from_secs(15) {
            panic!("timed out waiting for initialize");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let interrupt_started = Instant::now();
    adapter.interrupt().await?;
    assert!(
        interrupt_started.elapsed() < Duration::from_millis(500),
        "interrupt must not wait behind protocol I/O lock"
    );

    let error = turn
        .await
        .expect("join")
        .expect_err("flooded initialize must end via cancel or absolute deadline");
    let message = error.to_string();
    assert!(
        message.contains("cancelled") || message.contains("absolute initialize deadline"),
        "unexpected initialize failure: {message}"
    );
    Ok(())
}

#[tokio::test]
#[cfg(unix)]
async fn absolute_initialize_deadline_fires_on_irrelevant_notifications() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let stub = write_python_app_server(
        dir.path(),
        r#"
for raw in sys.stdin:
    line = raw.strip()
    if not line:
        continue
    msg = json.loads(line)
    if msg.get("method") == "initialize":
        while True:
            print(json.dumps({"method": "warning", "params": {"message": "noise"}}), flush=True)
            time.sleep(0.01)
"#,
    )?;
    let adapter = CodexAdapter::new(stub).with_deadlines(short_deadlines());
    let mut request = test_turn_request(dir.path().to_path_buf());
    request.timeout_secs = None;
    let (tx, _rx) = mpsc::channel(4);
    let error = adapter
        .start_turn(request, tx)
        .await
        .expect_err("absolute initialize deadline must fire");
    assert!(
        format!("{error}").contains("absolute initialize deadline"),
        "{error}"
    );
    Ok(())
}

#[tokio::test]
async fn idle_interrupt_does_not_poison_next_attempt() {
    let adapter = CodexAdapter::new(PathBuf::from("codex"));
    adapter.interrupt().await.unwrap();
    assert!(adapter.state.lock().await.active_attempt.is_none());
    let generation = adapter.state.lock().await.begin_attempt().unwrap();
    assert_eq!(generation, 1);
    assert!(!adapter.state.lock().await.cancel_requested_for(generation));
}

#[tokio::test]
#[cfg(unix)]
async fn overlapping_start_turn_is_rejected() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let gate = dir.path().join("hold");
    let stub = write_python_app_server(
        dir.path(),
        r#"
gate = os.environ["HOLD_GATE"]
open(gate, "w").close()
while os.path.exists(gate):
    time.sleep(0.05)
for raw in sys.stdin:
    pass
"#,
    )?;
    let adapter = Arc::new(CodexAdapter::new(stub).with_deadlines(AttemptDeadlines {
        absolute_init: Duration::from_secs(5),
        frame_write: Duration::from_secs(2),
        stop_cleanup: Duration::from_secs(2),
    }));
    let mut request = test_turn_request(dir.path().to_path_buf());
    request
        .env_vars
        .insert("HOLD_GATE".into(), gate.display().to_string());
    request.timeout_secs = Some(5);
    let (tx1, _rx1) = mpsc::channel(4);
    let first = {
        let adapter = Arc::clone(&adapter);
        tokio::spawn(async move { adapter.start_turn(request, tx1).await })
    };

    let started = Instant::now();
    while !gate.exists() {
        if started.elapsed() > Duration::from_secs(15) {
            panic!("first start_turn never reached stub");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // Give the first attempt time to publish its generation.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let (tx2, _rx2) = mpsc::channel(4);
    let overlap = adapter
        .start_turn(test_turn_request(dir.path().to_path_buf()), tx2)
        .await
        .expect_err("overlapping start must be rejected");
    assert!(
        format!("{overlap}").contains("overlapping start_turn"),
        "{overlap}"
    );

    let _ = std::fs::remove_file(&gate);
    let _ = first.await;
    Ok(())
}

#[tokio::test]
#[cfg(unix)]
async fn frame_write_deadline_poisons_session_when_child_stops_reading() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let stub = write_python_app_server(
        dir.path(),
        r#"
# Complete handshake, then stop consuming stdin so the next large frame write blocks.
for raw in sys.stdin:
    line = raw.strip()
    if not line:
        continue
    msg = json.loads(line)
    method = msg.get("method")
    if method == "initialize":
        print(json.dumps({"id": msg["id"], "result": {}}), flush=True)
    elif method == "thread/start":
        print(json.dumps({"method": "thread/started", "params": {"thread": {"id": "thread-1"}}}), flush=True)
        break
time.sleep(60)
"#,
    )?;
    let adapter = CodexAdapter::new(stub).with_deadlines(AttemptDeadlines {
        absolute_init: Duration::from_secs(5),
        frame_write: Duration::from_millis(100),
        stop_cleanup: Duration::from_millis(500),
    });
    let mut request = test_turn_request(dir.path().to_path_buf());
    // Large prompt forces writes beyond the typical pipe buffer once framed.
    request.prompt = "x".repeat(256 * 1024);
    request.timeout_secs = None;
    let (tx, _rx) = mpsc::channel(4);
    let error = adapter
        .start_turn(request, tx)
        .await
        .expect_err("blocked stdin write must hit frame-write deadline");
    let message = error.to_string();
    assert!(
        message.contains("frame write deadline") || message.contains("poisoned"),
        "{message}"
    );
    Ok(())
}

#[tokio::test]
#[cfg(unix)]
async fn dropped_start_turn_retains_cleanup_ownership() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let init_seen = dir.path().join("init-seen");
    let stub = write_python_app_server(
        dir.path(),
        r#"
init_seen = os.environ["INIT_SEEN"]
for raw in sys.stdin:
    line = raw.strip()
    if not line:
        continue
    msg = json.loads(line)
    if msg.get("method") == "initialize":
        open(init_seen, "w").close()
        print(json.dumps({"id": msg["id"], "result": {}}), flush=True)
        time.sleep(60)
"#,
    )?;
    let adapter = Arc::new(CodexAdapter::new(stub).with_deadlines(control_test_deadlines()));
    let mut request = test_turn_request(dir.path().to_path_buf());
    request
        .env_vars
        .insert("INIT_SEEN".into(), init_seen.display().to_string());
    request.timeout_secs = Some(5);
    let (tx, _rx) = mpsc::channel(4);
    let turn: tokio::task::JoinHandle<harness_core::error::Result<()>> = {
        let adapter = Arc::clone(&adapter);
        tokio::spawn(async move { adapter.as_ref().start_turn(request, tx).await })
    };

    let started = Instant::now();
    while !init_seen.exists() {
        if started.elapsed() > Duration::from_secs(15) {
            panic!("initialize never observed");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    turn.abort();
    let _ = turn.await;

    // Dropped attempt schedules cleanup; wait briefly for the spawn task.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let state = adapter.state.lock().await;
    assert!(
        state.active_attempt.is_none() || state.child.is_none() || state.failed_cleanup.is_some(),
        "dropped start_turn must not leave an unowned live attempt without cleanup bookkeeping"
    );
    Ok(())
}

#[tokio::test]
#[cfg(unix)]
async fn stale_reader_is_not_restored_after_generation_changes() -> anyhow::Result<()> {
    let adapter = CodexAdapter::new(PathBuf::from("codex"));
    let mut child = tokio::process::Command::new("sleep")
        .arg("60")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()?;
    let stdout = child.stdout.take().expect("stdout");
    let stdin = child.stdin.take().expect("stdin");
    let lines = adapter.wrap_stdout(stdout);
    {
        let mut state = adapter.state.lock().await;
        let generation = state.begin_attempt()?;
        state.child = Some(crate::ManagedChild::new(child, "stale reader test"));
        state.stdin = Some(stdin);
        // Simulate an older generation finishing while holding a detached reader.
        state.clear_attempt_if_current(generation);
        let next = state.begin_attempt()?;
        assert_ne!(generation, next);
        // Restoring under the wrong generation must be rejected by finish_attempt_success.
        drop(state);
        let error = adapter
            .finish_attempt_success(generation, lines)
            .await
            .expect_err("stale generation must not restore stdout reader");
        assert!(format!("{error}").contains("generation"));
        assert!(adapter.state.lock().await.stdout_lines.is_none());
    }
    let _ = adapter.terminate_and_drain().await;
    Ok(())
}

#[tokio::test]
#[cfg(unix)]
async fn oversized_frame_with_newline_resets_and_reaps_child() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    // Initialize ok, then emit a frame larger than the injected 64-byte limit.
    let stub = write_python_app_server(
        dir.path(),
        r#"
for raw in sys.stdin:
    line = raw.strip()
    if not line:
        continue
    msg = json.loads(line)
    if msg.get("method") == "initialize":
        print(json.dumps({"id": msg["id"], "result": {}}), flush=True)
        sys.stdout.write("x" * 80 + "\n")
        sys.stdout.flush()
        time.sleep(60)
"#,
    )?;
    let adapter = CodexAdapter::new(stub)
        .with_deadlines(control_test_deadlines())
        .with_max_protocol_frame_bytes(64);
    let mut request = test_turn_request(dir.path().to_path_buf());
    request.timeout_secs = Some(5);
    let (tx, _rx) = mpsc::channel(4);
    let error = adapter
        .start_turn(request, tx)
        .await
        .expect_err("oversized framed stdout must fail");
    let message = format!("{error}");
    assert!(
        message.contains("protocol frame exceeds maximum size"),
        "expected frame-size error, got: {message}"
    );
    let state = adapter.state.lock().await;
    assert!(state.child.is_none(), "child must be reaped after oversize");
    assert!(state.stdin.is_none());
    assert!(state.stdout_lines.is_none());
    assert!(
        state.protocol_poisoned || state.failed_cleanup.is_some() || state.child.is_none(),
        "oversize must poison or fully reset the session"
    );
    Ok(())
}

#[tokio::test]
#[cfg(unix)]
async fn oversized_frame_without_newline_resets_and_reaps_child() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let stub = write_python_app_server(
        dir.path(),
        r#"
for raw in sys.stdin:
    line = raw.strip()
    if not line:
        continue
    msg = json.loads(line)
    if msg.get("method") == "initialize":
        print(json.dumps({"id": msg["id"], "result": {}}), flush=True)
        # No trailing newline: stream must still reject once capacity is exceeded.
        sys.stdout.write("y" * 80)
        sys.stdout.flush()
        time.sleep(60)
"#,
    )?;
    let adapter = CodexAdapter::new(stub)
        .with_deadlines(control_test_deadlines())
        .with_max_protocol_frame_bytes(64);
    let mut request = test_turn_request(dir.path().to_path_buf());
    request.timeout_secs = Some(5);
    let (tx, _rx) = mpsc::channel(4);
    let error = adapter
        .start_turn(request, tx)
        .await
        .expect_err("oversized unframed stdout must fail");
    let message = format!("{error}");
    assert!(
        message.contains("protocol frame exceeds maximum size"),
        "expected frame-size error, got: {message}"
    );
    let state = adapter.state.lock().await;
    assert!(state.child.is_none(), "child must be reaped after oversize");
    assert!(state.stdin.is_none());
    assert!(state.stdout_lines.is_none());
    Ok(())
}

#[tokio::test]
#[cfg(unix)]
async fn injected_limit_accepts_exact_boundary_frame_during_initialize() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    // Large enough that normal handshake JSON fits; exact-limit frame is whitespace.
    let limit = 256usize;
    let stub = write_python_app_server(
        dir.path(),
        &format!(
            r#"
limit = {limit}
for raw in sys.stdin:
    line = raw.strip()
    if not line:
        continue
    msg = json.loads(line)
    if msg.get("method") == "initialize":
        frame = (b" " * limit) + b"\n"
        assert len(frame) == limit + 1
        sys.stdout.buffer.write(frame)
        sys.stdout.buffer.write((json.dumps({{"id": msg["id"], "result": {{}}}}) + "\n").encode())
        sys.stdout.buffer.flush()
    elif msg.get("method") == "thread/start":
        sys.stdout.buffer.write((json.dumps({{"method": "thread/started", "params": {{"thread": {{"id": "thread-1"}}}}}})+ "\n").encode())
        sys.stdout.buffer.write((json.dumps({{"id": msg["id"], "result": {{"thread": {{"id": "thread-1"}}}}}})+ "\n").encode())
        sys.stdout.buffer.flush()
    elif msg.get("method") == "turn/start":
        sys.stdout.buffer.write((json.dumps({{"method": "turn/started", "params": {{"threadId": "thread-1", "turn": {{"id": "turn-1", "status": "inProgress", "items": []}}}}}})+ "\n").encode())
        sys.stdout.buffer.write((json.dumps({{"method": "turn/completed", "params": {{"threadId": "thread-1", "turn": {{"id": "turn-1", "status": "completed", "items": [], "error": None}}}}}})+ "\n").encode())
        sys.stdout.buffer.flush()
"#
        ),
    )?;
    let adapter = CodexAdapter::new(stub)
        .with_deadlines(control_test_deadlines())
        .with_max_protocol_frame_bytes(limit);
    let mut request = test_turn_request(dir.path().to_path_buf());
    request.timeout_secs = Some(5);
    let (tx, mut rx) = mpsc::channel(8);
    adapter.start_turn(request, tx).await?;
    let mut completed = false;
    while let Ok(event) = rx.try_recv() {
        if matches!(event, AgentEvent::TurnCompleted { .. }) {
            completed = true;
        }
    }
    assert!(completed, "exact-limit blank frame must not abort the turn");
    Ok(())
}
