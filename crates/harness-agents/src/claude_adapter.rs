use crate::provider_backpressure::{
    ProviderBackpressureGate, ProviderBackpressurePermit, ProviderPhase,
    PROVIDER_WAIT_HEARTBEAT_INTERVAL, PROVIDER_WAIT_INITIAL_HEARTBEAT_DELAY,
};
use async_trait::async_trait;
use harness_core::{agent::AgentAdapter, agent::AgentEvent, agent::TurnRequest, types::TokenUsage};
use harness_observe::usage::parse_result_usage_metrics;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::AsyncBufReadExt;
use tokio::process::Command;
use tokio::sync::{mpsc, Mutex};
use tracing;

/// Streaming Claude Code adapter (L1-L2).
///
/// Spawns `claude --output-format stream-json -p <prompt>` and parses JSONL
/// events in realtime, mapping them to `AgentEvent`s.
pub struct ClaudeAdapter {
    cli_path: PathBuf,
    default_model: String,
    child: Arc<Mutex<Option<tokio::process::Child>>>,
    provider_gate: ProviderBackpressureGate,
}

impl ClaudeAdapter {
    pub fn new(cli_path: PathBuf, default_model: String) -> Self {
        Self {
            cli_path,
            default_model,
            child: Arc::new(Mutex::new(None)),
            provider_gate: ProviderBackpressureGate::disabled(),
        }
    }

    pub fn with_provider_backpressure_gate(mut self, gate: ProviderBackpressureGate) -> Self {
        self.provider_gate = gate;
        self
    }
}

#[async_trait]
impl AgentAdapter for ClaudeAdapter {
    fn name(&self) -> &str {
        "claude"
    }

    async fn start_turn(
        &self,
        req: TurnRequest,
        tx: mpsc::Sender<AgentEvent>,
    ) -> harness_core::error::Result<()> {
        // Check token expiry before spawning.
        // See also: claude.rs — both files must stay in sync on this check.
        if let Some(ref token) = req.capability_token {
            if token.is_expired() {
                return Err(harness_core::error::HarnessError::AgentExecution(format!(
                    "capability token for subtask {} has expired",
                    token.subtask_index
                )));
            }
        }

        let model = req.model.as_deref().unwrap_or(&self.default_model);
        let mut cmd = Command::new(&self.cli_path);
        // Prompt MUST follow -p immediately: Claude CLI parses `-p <VALUE>`.
        cmd.arg("-p")
            .arg(&req.prompt)
            .arg("--dangerously-skip-permissions")
            .arg("--output-format")
            .arg("stream-json")
            .arg("--model")
            .arg(model)
            .arg("--verbose")
            .current_dir(&req.project_root)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        crate::set_process_group(&mut cmd);
        crate::strip_claude_env(&mut cmd);

        if !req.allowed_tools.is_empty() {
            cmd.arg("--allowedTools").arg(req.allowed_tools.join(","));
        }

        let provider_permit =
            acquire_provider_permit_with_event_heartbeat(&self.provider_gate, &req, &tx).await?;
        tracing::debug!(
            phase = provider_permit.phase().label(),
            waited_ms = provider_permit.waited_ms(),
            "claude adapter admitted by provider gate"
        );

        let mut child = cmd.spawn().map_err(|e| {
            harness_core::error::HarnessError::AgentExecution(format!(
                "failed to spawn claude: {e}"
            ))
        })?;

        let stdout = child.stdout.take().ok_or_else(|| {
            harness_core::error::HarnessError::AgentExecution(
                "no stdout from claude process".into(),
            )
        })?;

        // Drain stderr concurrently to avoid pipe-buffer deadlock and capture
        // the error message when the process exits non-zero.
        let stderr_handle = child.stderr.take();
        let stderr_task = tokio::spawn(async move {
            let Some(stderr) = stderr_handle else {
                return String::new();
            };
            let mut buf = String::new();
            if let Err(e) = tokio::io::AsyncReadExt::read_to_string(
                &mut tokio::io::BufReader::new(stderr),
                &mut buf,
            )
            .await
            {
                tracing::warn!("claude: failed to read stderr: {e}");
            }
            buf
        });

        // Store child handle for interrupt()
        {
            let mut guard = self.child.lock().await;
            *guard = Some(child);
        }

        if tx.send(AgentEvent::TurnStarted).await.is_err() {
            return Ok(());
        }

        let reader = tokio::io::BufReader::new(stdout);
        let mut lines = reader.lines();
        let mut output_buf = String::new();

        while let Ok(Some(line)) = lines.next_line().await {
            if line.trim().is_empty() {
                continue;
            }

            let event = match parse_stream_json_line(&line) {
                Some(ev) => ev,
                None => continue,
            };

            if let Some(usage) = parse_stream_json_usage(&line) {
                if tx.send(AgentEvent::TokenUsage { usage }).await.is_err() {
                    break;
                }
            }

            // Accumulate output text for TurnCompleted
            if let AgentEvent::MessageDelta { ref text } = event {
                output_buf.push_str(text);
            }

            if tx.send(event).await.is_err() {
                break;
            }
        }

        // Wait for process to finish and get exit status
        let exit_status = {
            let mut guard = self.child.lock().await;
            if let Some(ref mut child) = *guard {
                child
                    .wait()
                    .await
                    .map_err(|e| {
                        tracing::warn!("claude: failed to wait for child process: {e}");
                    })
                    .ok()
            } else {
                None
            }
        };

        let stderr_text = stderr_task.await.unwrap_or_default();
        if !stderr_text.is_empty() {
            tracing::warn!("claude stderr: {}", stderr_text.trim());
        }

        if let Some(status) = exit_status {
            if !status.success() {
                let stderr_suffix = if stderr_text.is_empty() {
                    String::new()
                } else {
                    // Keep last 500 chars of stderr for the error message.
                    let trimmed: String = stderr_text
                        .chars()
                        .rev()
                        .take(500)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect();
                    format!(": {}", trimmed.trim())
                };
                if let Err(e) = tx
                    .send(AgentEvent::Error {
                        message: format!("claude exited with {status}{stderr_suffix}"),
                    })
                    .await
                {
                    tracing::debug!("claude: event channel closed before error could be sent: {e}");
                }
            }
        }

        if let Err(e) = tx
            .send(AgentEvent::TurnCompleted { output: output_buf })
            .await
        {
            tracing::debug!("claude: event channel closed before turn completed: {e}");
        }

        // Clean up child handle
        let mut guard = self.child.lock().await;
        *guard = None;

        Ok(())
    }

    async fn interrupt(&self) -> harness_core::error::Result<()> {
        let mut guard = self.child.lock().await;
        if let Some(ref mut child) = *guard {
            child.kill().await.map_err(|e| {
                harness_core::error::HarnessError::AgentExecution(format!(
                    "failed to kill claude process: {e}"
                ))
            })?;
        }
        Ok(())
    }

    async fn steer(&self, _text: String) -> harness_core::error::Result<()> {
        // Claude CLI is a one-shot process launched with `-p`.  It has no open
        // stdin channel for mid-turn injection, so live steering is not possible
        // without process restart.  Future interactive-mode support would require
        // a different spawning strategy.
        Err(harness_core::error::HarnessError::Unsupported(
            "Claude CLI does not support live steering: it is a one-shot process \
             launched with -p and has no stdin channel for mid-turn injection"
                .into(),
        ))
    }

    async fn respond_approval(
        &self,
        _id: String,
        _decision: harness_core::agent::ApprovalDecision,
    ) -> harness_core::error::Result<()> {
        // Claude CLI runs with --dangerously-skip-permissions and auto-approves
        // all tool calls.  There is no approval gate protocol to respond to.
        Err(harness_core::error::HarnessError::Unsupported(
            "Claude CLI does not support approval responses: it runs with \
             --dangerously-skip-permissions and cannot receive mid-turn input"
                .into(),
        ))
    }
}

async fn acquire_provider_permit_with_event_heartbeat(
    gate: &ProviderBackpressureGate,
    req: &TurnRequest,
    tx: &mpsc::Sender<AgentEvent>,
) -> harness_core::error::Result<ProviderBackpressurePermit> {
    let prompt_chars = req.prompt.chars().count();
    let prompt_bytes = req.prompt.len();
    if !gate.is_enabled() {
        return gate
            .acquire(req.execution_phase, prompt_chars, prompt_bytes)
            .await;
    }

    let phase = ProviderPhase::from_execution_phase(req.execution_phase);
    let mut acquire = Box::pin(gate.acquire(req.execution_phase, prompt_chars, prompt_bytes));
    let mut heartbeat = Box::pin(tokio::time::sleep(PROVIDER_WAIT_INITIAL_HEARTBEAT_DELAY));
    loop {
        tokio::select! {
            permit = &mut acquire => return permit,
            _ = &mut heartbeat => {
                if tx.send(AgentEvent::Warning {
                    message: provider_wait_message(phase),
                }).await.is_err() {
                    return Err(harness_core::error::HarnessError::AgentExecution(
                        "agent event channel closed while waiting for Claude provider capacity".into(),
                    ));
                }
                heartbeat.as_mut().reset(tokio::time::Instant::now() + PROVIDER_WAIT_HEARTBEAT_INTERVAL);
            }
        }
    }
}

fn provider_wait_message(phase: ProviderPhase) -> String {
    format!(
        "Waiting for Claude provider capacity for {} phase",
        phase.label()
    )
}

/// Parse a single line of Claude Code `--output-format stream-json` output.
///
/// Returns `None` for unrecognized event types (forward compatibility).
pub fn parse_stream_json_line(line: &str) -> Option<AgentEvent> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let event_type = v.get("type")?.as_str()?;

    match event_type {
        "assistant" => {
            let text = parse_assistant_text(v.get("message")?)?;
            Some(AgentEvent::MessageDelta { text })
        }
        "tool_use" => {
            let name = v.get("name")?.as_str()?.to_string();
            let input = v.get("input").cloned().unwrap_or(serde_json::Value::Null);
            Some(AgentEvent::ToolCall { name, input })
        }
        "tool_result" => Some(AgentEvent::ItemCompleted),
        "result" => {
            let output = v
                .get("result")
                .and_then(|r| r.as_str())
                .unwrap_or("")
                .to_string();
            Some(AgentEvent::TurnCompleted { output })
        }
        "error" => {
            let message = v
                .get("error")
                .and_then(|e| e.as_str())
                .unwrap_or("unknown error")
                .to_string();
            Some(AgentEvent::Error { message })
        }
        _ => None,
    }
}

fn parse_assistant_text(message: &Value) -> Option<String> {
    if let Some(text) = message.as_str() {
        return Some(text.to_string());
    }

    let content = message.get("content")?.as_array()?;
    let text = content
        .iter()
        .filter_map(|block| {
            if block.get("type").and_then(Value::as_str) == Some("text") {
                block.get("text").and_then(Value::as_str)
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("");

    (!text.is_empty()).then_some(text)
}

pub fn parse_stream_json_usage(line: &str) -> Option<TokenUsage> {
    let usage = parse_result_usage_metrics(line)?;

    Some(TokenUsage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        total_tokens: usage.total_tokens(),
        cost_usd: 0.0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::agent::ApprovalDecision;
    use harness_core::config::agents::ClaudeProviderBackpressureConfig;
    use harness_core::types::ExecutionPhase;
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
        let line =
            r#"{"type":"result","result":"Done","usage":{"input_tokens":10,"output_tokens":3}}"#;
        let usage = parse_stream_json_usage(line).expect("usage should parse");
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.output_tokens, 3);
        assert_eq!(usage.total_tokens, 13);
    }

    #[test]
    fn parse_result_usage_allows_zero_tokens() {
        let line =
            r#"{"type":"result","result":"Done","usage":{"input_tokens":0,"output_tokens":0}}"#;
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

        let gate =
            ProviderBackpressureGate::from_claude_config(&ClaudeProviderBackpressureConfig {
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
            project_root,
            model: None,
            reasoning_effort: None,
            execution_phase: Some(ExecutionPhase::Execution),
            sandbox_mode: None,
            approval_policy: None,
            allowed_tools: vec![],
            context: vec![],
            timeout_secs: None,
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
}
