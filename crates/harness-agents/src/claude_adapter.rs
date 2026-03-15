use async_trait::async_trait;
use harness_core::{AgentAdapter, AgentEvent, TurnRequest};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::AsyncBufReadExt;
use tokio::process::Command;
use tokio::sync::{mpsc, Mutex};

/// Streaming Claude Code adapter (L1-L2).
///
/// Spawns `claude --output-format stream-json -p <prompt>` and parses JSONL
/// events in realtime, mapping them to `AgentEvent`s.
pub struct ClaudeAdapter {
    cli_path: PathBuf,
    default_model: String,
    child: Arc<Mutex<Option<tokio::process::Child>>>,
}

impl ClaudeAdapter {
    pub fn new(cli_path: PathBuf, default_model: String) -> Self {
        Self {
            cli_path,
            default_model,
            child: Arc::new(Mutex::new(None)),
        }
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
    ) -> harness_core::Result<()> {
        let model = req.model.as_deref().unwrap_or(&self.default_model);
        let mut cmd = Command::new(&self.cli_path);
        cmd.arg("-p")
            .arg("--dangerously-skip-permissions")
            .arg("--output-format")
            .arg("stream-json")
            .arg("--model")
            .arg(model)
            .arg("--verbose")
            .current_dir(&req.project_root)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        crate::strip_claude_env(&mut cmd);
        crate::process_group::apply_process_group_management(&mut cmd);

        if !req.allowed_tools.is_empty() {
            cmd.arg("--allowedTools").arg(req.allowed_tools.join(","));
        }

        cmd.arg(&req.prompt);

        let mut child = cmd.spawn().map_err(|e| {
            harness_core::HarnessError::AgentExecution(format!("failed to spawn claude: {e}"))
        })?;

        let child_pid = child.id();
        // ProcessGroupGuard kills the entire process group on drop (including
        // grandchildren that kill_on_drop(true) misses). Fires even if this
        // future is cancelled mid-execution due to an external timeout.
        let _pg_guard = crate::process_group::ProcessGroupGuard::new(child_pid);

        let stdout = child.stdout.take().ok_or_else(|| {
            harness_core::HarnessError::AgentExecution("no stdout from claude process".into())
        })?;

        // Drain stderr in a background task so the pipe buffer never fills up.
        // A full stderr pipe causes the child to block, triggering stall timeout
        // and leaving orphan processes.
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                crate::streaming::filter_agent_stderr(stderr, "claude").await;
            });
        }

        // Store child handle for interrupt()
        {
            let mut guard = self.child.lock().await;
            *guard = Some(child);
        }

        if tx.send(AgentEvent::TurnStarted).await.is_err() {
            let mut guard = self.child.lock().await;
            if let Some(ref mut child) = *guard {
                if let Err(e) = child.kill().await {
                    tracing::debug!("claude kill on early send failure: {e}");
                }
            }
            *guard = None;
            return Ok(());
        }

        let reader = tokio::io::BufReader::new(stdout);
        let mut lines = reader.lines();
        let mut output_buf = String::new();
        let mut send_failed = false;

        while let Ok(Some(line)) = lines.next_line().await {
            if line.trim().is_empty() {
                continue;
            }

            let event = match parse_stream_json_line(&line) {
                Some(ev) => ev,
                None => continue,
            };

            // Accumulate output text for TurnCompleted
            if let AgentEvent::MessageDelta { ref text } = event {
                output_buf.push_str(text);
            }

            if tx.send(event).await.is_err() {
                send_failed = true;
                break;
            }
        }

        // Kill immediately on send failure (receiver dropped); wait otherwise.
        {
            let mut guard = self.child.lock().await;
            if let Some(ref mut child) = *guard {
                if send_failed {
                    if let Err(e) = child.kill().await {
                        tracing::debug!("claude kill on send failure: {e}");
                    }
                } else {
                    let exit_status = child.wait().await.ok();
                    if let Some(status) = exit_status {
                        if !status.success() {
                            if tx
                                .send(AgentEvent::Error {
                                    message: format!("claude exited with {status}"),
                                })
                                .await
                                .is_err()
                            {
                                tracing::debug!("claude: receiver dropped before error event");
                            }
                        }
                    }
                }
            }
            *guard = None;
        }

        if !send_failed {
            if tx
                .send(AgentEvent::TurnCompleted { output: output_buf })
                .await
                .is_err()
            {
                tracing::debug!("claude: receiver dropped before TurnCompleted");
            }
        }

        Ok(())
    }

    async fn interrupt(&self) -> harness_core::Result<()> {
        let mut guard = self.child.lock().await;
        if let Some(ref mut child) = *guard {
            child.kill().await.map_err(|e| {
                harness_core::HarnessError::AgentExecution(format!(
                    "failed to kill claude process: {e}"
                ))
            })?;
        }
        Ok(())
    }
}

/// Parse a single line of Claude Code `--output-format stream-json` output.
///
/// Returns `None` for unrecognized event types (forward compatibility).
pub fn parse_stream_json_line(line: &str) -> Option<AgentEvent> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let event_type = v.get("type")?.as_str()?;

    match event_type {
        "assistant" => {
            let text = v.get("message")?.as_str()?.to_string();
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

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::ApprovalDecision;

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
    async fn steer_returns_unsupported() {
        let adapter = ClaudeAdapter::new(PathBuf::from("claude"), "test-model".into());
        let err = adapter
            .steer("redirect".into())
            .await
            .expect_err("steer should return Unsupported");
        assert!(err.to_string().contains("unsupported"));
    }

    #[tokio::test]
    async fn respond_approval_returns_unsupported() {
        let adapter = ClaudeAdapter::new(PathBuf::from("claude"), "test-model".into());
        let err = adapter
            .respond_approval("req-1".into(), ApprovalDecision::Accept)
            .await
            .expect_err("respond_approval should return Unsupported");
        assert!(err.to_string().contains("unsupported"));
    }
}
