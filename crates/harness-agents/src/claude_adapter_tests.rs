use super::*;
use harness_core::agent::{AgentPromptLayers, ApprovalDecision};
use harness_core::config::agents::ClaudeProviderBackpressureConfig;
use harness_core::types::ExecutionPhase;
use std::collections::HashMap;
use std::fs;
use std::num::NonZeroUsize;
use std::time::Duration;
use tokio::time::timeout;

#[test]
fn parse_assistant_message() {
    let line = r#"{"type": "assistant", "message": "Let me read the file..."}"#;
    let event = parse_stream_json_line(line).unwrap();
    match event {
        AgentEvent::MessageDelta { text } => {
            assert_eq!(text, "Let me read the file...");
        }
        other => panic!("expected MessageDelta, got {other:?}"),
    }
}

#[test]
fn parse_assistant_message_content_blocks() {
    let line = r#"{"type":"assistant","message":{"content":[{"type":"thinking","text":"hidden"},{"type":"text","text":"Hello "},{"type":"text","text":"world"}]}}"#;
    let Some(event) = parse_stream_json_line(line) else {
        panic!("assistant content blocks should parse");
    };
    match event {
        AgentEvent::MessageDelta { text } => {
            assert_eq!(text, "Hello world");
        }
        other => panic!("expected MessageDelta, got {other:?}"),
    }
}

#[test]
fn parse_tool_use() {
    let line = r#"{"type": "tool_use", "name": "Read", "input": {"path": "src/main.rs"}}"#;
    let event = parse_stream_json_line(line).unwrap();
    match event {
        AgentEvent::ToolCall { name, input } => {
            assert_eq!(name, "Read");
            assert_eq!(input["path"], "src/main.rs");
        }
        other => panic!("expected ToolCall, got {other:?}"),
    }
}

#[test]
fn parse_tool_result() {
    let line = r#"{"type": "tool_result", "output": "file contents here"}"#;
    let event = parse_stream_json_line(line).unwrap();
    assert!(matches!(event, AgentEvent::ItemCompleted));
}

#[test]
fn parse_result_event() {
    let line = r#"{"type": "result", "result": "Done, bug fixed."}"#;
    let event = parse_stream_json_line(line).unwrap();
    match event {
        AgentEvent::TurnCompleted { output } => {
            assert_eq!(output, "Done, bug fixed.");
        }
        other => panic!("expected TurnCompleted, got {other:?}"),
    }
}

#[test]
fn parse_result_usage_with_cache_fields() {
    let line = r#"{"type":"result","result":"Done","usage":{"input_tokens":10,"output_tokens":3,"cache_read_input_tokens":4,"cache_creation_input_tokens":2}}"#;
    let usage = parse_stream_json_usage(line).expect("usage should parse");
    assert_eq!(usage.input_tokens, 10);
    assert_eq!(usage.output_tokens, 3);
    assert_eq!(usage.total_tokens, 19);
}

#[test]
fn parse_result_usage_allows_missing_cache_fields() {
    let line = r#"{"type":"result","result":"Done","usage":{"input_tokens":10,"output_tokens":3}}"#;
    let usage = parse_stream_json_usage(line).expect("usage should parse");
    assert_eq!(usage.input_tokens, 10);
    assert_eq!(usage.output_tokens, 3);
    assert_eq!(usage.total_tokens, 13);
}

#[test]
fn parse_result_usage_allows_zero_tokens() {
    let line = r#"{"type":"result","result":"Done","usage":{"input_tokens":0,"output_tokens":0}}"#;
    let usage = parse_stream_json_usage(line).expect("usage should parse");
    assert_eq!(usage.total_tokens, 0);
}

#[test]
fn parse_result_usage_ignores_malformed_json() {
    assert!(parse_stream_json_usage("{not-json").is_none());
}

#[test]
fn parse_error_event() {
    let line = r#"{"type": "error", "error": "rate limit exceeded"}"#;
    let event = parse_stream_json_line(line).unwrap();
    match event {
        AgentEvent::Error { message } => {
            assert_eq!(message, "rate limit exceeded");
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[test]
fn parse_unknown_type_returns_none() {
    let line = r#"{"type": "system_prompt", "text": "you are helpful"}"#;
    assert!(parse_stream_json_line(line).is_none());
}

#[test]
fn parse_invalid_json_returns_none() {
    assert!(parse_stream_json_line("not json").is_none());
    assert!(parse_stream_json_line("").is_none());
}

#[test]
fn parse_missing_type_returns_none() {
    let line = r#"{"message": "no type field"}"#;
    assert!(parse_stream_json_line(line).is_none());
}

#[tokio::test]
async fn interrupt_noop_when_no_child() {
    let adapter = ClaudeAdapter::new(PathBuf::from("claude"), "test-model".into());
    // Should not error when no child process exists
    adapter.interrupt().await.unwrap();
}

#[tokio::test]
async fn start_turn_missing_workspace_reports_workspace_missing() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let missing = dir.path().join("missing-workspace");
    let adapter = ClaudeAdapter::new(std::env::current_exe()?, "test-model".into());
    let request = turn_request("ignored", missing.clone());
    let (tx, _rx) = mpsc::channel(4);

    let error = match adapter.start_turn(request, tx).await {
        Ok(()) => panic!("missing project root should fail before claude starts"),
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
    assert!(message.contains("failed to spawn claude"));
    Ok(())
}

#[tokio::test]
async fn start_turn_sends_layered_static_prompt_through_system_prompt_args() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let args_path = dir.path().join("args.txt");
    let script = dir.path().join("mock-claude-layered.sh");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\nset -eu\n: > '{}'\nfor arg in \"$@\"; do printf '%s\\n' \"$arg\" >> '{}'; done\nprintf '%s\\n' '{{\"type\":\"assistant\",\"message\":\"done\"}}'\nprintf '%s\\n' '{{\"type\":\"result\",\"result\":\"done\"}}'\n",
            args_path.display(),
            args_path.display()
        ),
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&script)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script, perms)?;
    }

    let adapter = ClaudeAdapter::new(script, "test-model".into());
    let mut request = turn_request("static-context-dynamic", dir.path().to_path_buf());
    request.prompt_layers = Some(AgentPromptLayers::new("static", "context-", "dynamic"));
    let (tx, mut rx) = mpsc::channel(8);

    adapter.start_turn(request, tx).await?;
    while rx.recv().await.is_some() {}

    let args: Vec<String> = fs::read_to_string(args_path)?
        .lines()
        .map(str::to_string)
        .collect();
    assert_eq!(args[0], "-p");
    assert_eq!(args[1], "context-dynamic");
    assert!(args
        .windows(2)
        .any(|window| window == ["--append-system-prompt", "static"]));
    assert!(args.contains(&"--exclude-dynamic-system-prompt-sections".to_string()));
    Ok(())
}

#[tokio::test]
async fn start_turn_full_profile_uses_dangerously_skip_permissions() -> anyhow::Result<()> {
    let args = captured_args_for_allowed_tools(vec![]).await?;
    assert!(args.contains(&"--dangerously-skip-permissions".to_string()));
    assert!(!args.contains(&"--allowedTools".to_string()));
    assert!(!args.contains(&"--permission-mode".to_string()));
    Ok(())
}

#[tokio::test]
async fn start_turn_restricted_profile_splits_permission_flags() -> anyhow::Result<()> {
    let args =
        captured_args_for_allowed_tools(vec!["Read".to_string(), "Bash".to_string()]).await?;
    assert!(
        !args.contains(&"--dangerously-skip-permissions".to_string()),
        "--allowedTools and --dangerously-skip-permissions are mutually exclusive"
    );
    assert!(args
        .windows(2)
        .any(|w| w == ["--permission-mode", "bypassPermissions"]));
    assert!(args
        .windows(2)
        .any(|w| w == ["--allowedTools", "Read,Bash"]));
    Ok(())
}

async fn captured_args_for_allowed_tools(
    allowed_tools: Vec<String>,
) -> anyhow::Result<Vec<String>> {
    let dir = tempfile::tempdir()?;
    let args_path = dir.path().join("args.txt");
    let script = dir.path().join("mock-claude-permissions.sh");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\nset -eu\n: > '{}'\nfor arg in \"$@\"; do printf '%s\\n' \"$arg\" >> '{}'; done\nprintf '%s\\n' '{{\"type\":\"assistant\",\"message\":\"done\"}}'\nprintf '%s\\n' '{{\"type\":\"result\",\"result\":\"done\"}}'\n",
            args_path.display(),
            args_path.display()
        ),
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&script)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script, perms)?;
    }

    let adapter = ClaudeAdapter::new(script, "test-model".into());
    let mut request = turn_request("prompt", dir.path().to_path_buf());
    request.allowed_tools = allowed_tools;
    let (tx, mut rx) = mpsc::channel(8);
    adapter.start_turn(request, tx).await?;
    while rx.recv().await.is_some() {}

    Ok(fs::read_to_string(args_path)?
        .lines()
        .map(str::to_string)
        .collect())
}

#[tokio::test]
async fn start_turn_propagates_request_env_vars_to_process() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let env_path = dir.path().join("env.txt");
    let script = dir.path().join("mock-claude-env.sh");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\nset -eu\nprintf '%s\\n' \"${{HARNESS_ADAPTER_ENV_PROBE:-missing}}\" > '{}'\nprintf '%s\\n' '{{\"type\":\"result\",\"result\":\"done\"}}'\n",
            env_path.display()
        ),
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&script)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script, perms)?;
    }

    let adapter = ClaudeAdapter::new(script, "test-model".into());
    let mut request = turn_request("prompt", dir.path().to_path_buf());
    request.env_vars.insert(
        "HARNESS_ADAPTER_ENV_PROBE".to_string(),
        "probe-value".to_string(),
    );
    let (tx, mut rx) = mpsc::channel(8);
    adapter.start_turn(request, tx).await?;
    while rx.recv().await.is_some() {}

    assert_eq!(fs::read_to_string(env_path)?.trim(), "probe-value");
    Ok(())
}

#[tokio::test]
async fn start_turn_emits_provider_wait_warning_before_spawn() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let started = dir.path().join("started.txt");
    let release = dir.path().join("release.txt");
    let script = dir.path().join("mock-claude-adapter-provider-gate.sh");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\nset -eu\nprompt=\"$2\"\necho \"$prompt\" >> \"{}\"\nif [ \"$prompt\" = first ]; then while [ ! -f \"{}\" ]; do sleep 0.02; done; fi\nprintf '%s\\n' '{{\"type\":\"assistant\",\"message\":\"done\"}}'\nprintf '%s\\n' '{{\"type\":\"result\",\"result\":\"done\"}}'\n",
            started.display(),
            release.display()
        ),
    )
    .expect("write script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&script)
            .expect("script metadata")
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script, perms).expect("set executable permissions");
    }

    let gate = ProviderBackpressureGate::from_claude_config(&ClaudeProviderBackpressureConfig {
        max_concurrent_sessions: Some(NonZeroUsize::new(1).expect("non-zero limit")),
        ..ClaudeProviderBackpressureConfig::default()
    });
    let adapter = Arc::new(
        ClaudeAdapter::new(script, "test-model".into()).with_provider_backpressure_gate(gate),
    );

    let first_adapter = adapter.clone();
    let first_req = turn_request("first", dir.path().to_path_buf());
    let first = tokio::spawn(async move {
        let (tx, _rx) = mpsc::channel(8);
        first_adapter.start_turn(first_req, tx).await
    });
    timeout(Duration::from_secs(10), async {
        loop {
            let started_text = fs::read_to_string(&started).unwrap_or_default();
            if started_text.contains("first") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("first adapter process should start");

    let second_adapter = adapter.clone();
    let second_req = turn_request("second", dir.path().to_path_buf());
    let (second_tx, mut second_rx) = mpsc::channel(8);
    let second =
        tokio::spawn(async move { second_adapter.start_turn(second_req, second_tx).await });

    let wait_event = timeout(Duration::from_secs(2), second_rx.recv())
        .await
        .expect("queued Claude adapter should emit provider wait activity")
        .expect("second adapter closed before provider wait activity");
    match wait_event {
        AgentEvent::Warning { message } => {
            assert!(
                message.contains("Waiting for Claude provider capacity"),
                "unexpected provider wait message: {message}"
            );
        }
        other => panic!("expected provider wait warning before spawn, got {other:?}"),
    }

    let started_before_release = fs::read_to_string(&started).unwrap_or_default();
    assert!(
        !started_before_release.contains("second"),
        "second process must not spawn while provider capacity is saturated"
    );

    fs::write(&release, "release").expect("release first process");
    timeout(Duration::from_secs(2), first)
        .await
        .expect("first should finish")
        .expect("first task should join")
        .expect("first turn should succeed");
    timeout(Duration::from_secs(2), second)
        .await
        .expect("second should finish after first releases provider capacity")
        .expect("second task should join")
        .expect("second turn should succeed");
}

fn turn_request(prompt: &str, project_root: PathBuf) -> TurnRequest {
    TurnRequest {
        prompt: prompt.to_string(),
        prompt_layers: None,
        project_root,
        model: None,
        reasoning_effort: None,
        execution_phase: Some(ExecutionPhase::Execution),
        sandbox_mode: None,
        approval_policy: None,
        allowed_tools: vec![],
        context: vec![],
        timeout_secs: None,
        env_vars: HashMap::new(),
        capability_token: None,
    }
}

#[tokio::test]
async fn steer_returns_unsupported_with_claude_cli_message() {
    let adapter = ClaudeAdapter::new(PathBuf::from("claude"), "test-model".into());
    let err = adapter
        .steer("redirect".into())
        .await
        .expect_err("steer should return Unsupported");
    assert!(
        err.to_string().contains("Claude CLI does not support"),
        "error must name the Claude CLI limitation, got: {err}"
    );
}

#[tokio::test]
async fn respond_approval_returns_unsupported_with_claude_cli_message() {
    let adapter = ClaudeAdapter::new(PathBuf::from("claude"), "test-model".into());
    let err = adapter
        .respond_approval("req-1".into(), ApprovalDecision::Accept)
        .await
        .expect_err("respond_approval should return Unsupported");
    assert!(
        err.to_string().contains("Claude CLI does not support"),
        "error must name the Claude CLI limitation, got: {err}"
    );
}
