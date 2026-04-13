use async_trait::async_trait;
use harness_core::{
    agent::AgentAdapter, agent::AgentEvent, agent::ApprovalDecision, agent::TurnRequest,
};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, Mutex};
use tracing;

/// Codex App Server adapter (L3-L4).
///
/// Spawns `codex` as a long-lived stdio subprocess speaking JSON-RPC 2.0.
/// Bidirectional: sends requests via stdin, reads responses/notifications from stdout.
/// Supports approval gates, interrupt, and steer.
pub struct CodexAdapter {
    cli_path: PathBuf,
    state: Arc<Mutex<AdapterState>>,
}

struct AdapterState {
    child: Option<tokio::process::Child>,
    stdin: Option<tokio::process::ChildStdin>,
    next_id: u64,
    /// Thread ID returned by `thread/start`; required for subsequent `turn/start` calls.
    thread_id: Option<String>,
}

impl AdapterState {
    fn new() -> Self {
        Self {
            child: None,
            stdin: None,
            next_id: 1,
            thread_id: None,
        }
    }

    fn next_request_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }
}

impl CodexAdapter {
    pub fn new(cli_path: PathBuf) -> Self {
        Self {
            cli_path,
            state: Arc::new(Mutex::new(AdapterState::new())),
        }
    }

    /// Send a JSON-RPC request via stdin and return the request id.
    async fn send_request(
        state: &mut AdapterState,
        method: &str,
        params: serde_json::Value,
    ) -> harness_core::error::Result<u64> {
        let id = state.next_request_id();
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        let stdin = state.stdin.as_mut().ok_or_else(|| {
            harness_core::error::HarnessError::AgentExecution("codex stdin not available".into())
        })?;

        let mut line = serde_json::to_string(&request).map_err(|e| {
            harness_core::error::HarnessError::AgentExecution(format!(
                "failed to serialize request: {e}"
            ))
        })?;
        line.push('\n');

        stdin.write_all(line.as_bytes()).await.map_err(|e| {
            harness_core::error::HarnessError::AgentExecution(format!(
                "failed to write to codex: {e}"
            ))
        })?;
        stdin.flush().await.map_err(|e| {
            harness_core::error::HarnessError::AgentExecution(format!(
                "failed to flush codex stdin: {e}"
            ))
        })?;

        Ok(id)
    }

    /// Send a JSON-RPC notification (no id, no response expected).
    async fn send_notification(
        state: &mut AdapterState,
        method: &str,
    ) -> harness_core::error::Result<()> {
        let notification = json!({
            "jsonrpc": "2.0",
            "method": method,
        });

        let stdin = state.stdin.as_mut().ok_or_else(|| {
            harness_core::error::HarnessError::AgentExecution("codex stdin not available".into())
        })?;

        let mut line = serde_json::to_string(&notification).map_err(|e| {
            harness_core::error::HarnessError::AgentExecution(format!("failed to serialize: {e}"))
        })?;
        line.push('\n');

        stdin.write_all(line.as_bytes()).await.map_err(|e| {
            harness_core::error::HarnessError::AgentExecution(format!(
                "failed to write to codex: {e}"
            ))
        })?;
        stdin.flush().await.map_err(|e| {
            harness_core::error::HarnessError::AgentExecution(format!(
                "failed to flush codex stdin: {e}"
            ))
        })?;
        Ok(())
    }
}

#[async_trait]
impl AgentAdapter for CodexAdapter {
    fn name(&self) -> &str {
        "codex"
    }

    async fn start_turn(
        &self,
        req: TurnRequest,
        tx: mpsc::Sender<AgentEvent>,
    ) -> harness_core::error::Result<()> {
        let mut state = self.state.lock().await;

        // Spawn codex if not already running
        if state.child.is_none() {
            let mut cmd = tokio::process::Command::new(&self.cli_path);
            cmd.stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .current_dir(&req.project_root)
                .kill_on_drop(true);
            #[cfg(unix)]
            crate::set_process_group(&mut cmd);
            crate::strip_claude_env(&mut cmd);

            let mut child = cmd.spawn().map_err(|e| {
                harness_core::error::HarnessError::AgentExecution(format!(
                    "failed to spawn codex: {e}"
                ))
            })?;

            state.stdin = child.stdin.take();
            let stdout = child.stdout.take().ok_or_else(|| {
                harness_core::error::HarnessError::AgentExecution("no stdout from codex".into())
            })?;
            state.child = Some(child);

            // Step 1: initialize
            Self::send_request(&mut state, "initialize", json!({})).await?;

            let reader = tokio::io::BufReader::new(stdout);
            let mut lines = reader.lines();

            // Read initialize response
            if let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(line = %line, "codex initialize response");
            }

            // Step 2: initialized notification
            Self::send_notification(&mut state, "initialized").await?;

            // Step 3: thread/start — must complete before any turn/start
            Self::send_request(
                &mut state,
                "thread/start",
                json!({ "workdir": req.project_root.to_string_lossy() }),
            )
            .await?;

            let thread_id = if let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(line = %line, "codex thread/start response");
                serde_json::from_str::<serde_json::Value>(&line)
                    .ok()
                    .and_then(|v| {
                        v.get("result")
                            .and_then(|r| r.get("id"))
                            .and_then(|id| id.as_str())
                            .map(|s| s.to_string())
                    })
                    .unwrap_or_default()
            } else {
                String::new()
            };
            state.thread_id = Some(thread_id.clone());

            // Step 4: turn/start with thread_id
            let turn_params = if thread_id.is_empty() {
                json!({ "text": req.prompt })
            } else {
                json!({ "thread_id": thread_id, "text": req.prompt })
            };
            Self::send_request(&mut state, "turn/start", turn_params).await?;

            // Release lock before reading event stream
            drop(state);

            if tx.send(AgentEvent::TurnStarted).await.is_err() {
                return Ok(());
            }

            // Read event stream until turn completes or error.
            // Track whether a terminal turn/completed event was received.
            // If the process disconnects (EOF) without sending it, that is an
            // execution failure — not a success.
            let mut output_buf = String::new();
            let mut turn_completed_received = false;
            while let Ok(Some(line)) = lines.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }

                if let Some(event) = parse_codex_message(&line) {
                    if let AgentEvent::MessageDelta { ref text } = event {
                        output_buf.push_str(text);
                    }

                    let is_turn_completed = matches!(event, AgentEvent::TurnCompleted { .. });
                    let is_error = matches!(event, AgentEvent::Error { .. });

                    if is_turn_completed {
                        turn_completed_received = true;
                    }

                    if tx.send(event).await.is_err() {
                        break;
                    }

                    if is_turn_completed || is_error {
                        break;
                    }
                }
            }

            // EOF without a terminal turn/completed means the codex process
            // crashed or disconnected mid-turn — surface this as an error
            // instead of silently recording a successful (possibly empty) turn.
            if !turn_completed_received {
                tracing::error!(
                    output_so_far = %output_buf,
                    "codex disconnected before turn/completed — treating as execution failure"
                );
                let _ = tx
                    .send(AgentEvent::Error {
                        message: "codex process disconnected before turn/completed".into(),
                    })
                    .await;
                return Err(harness_core::error::HarnessError::AgentExecution(
                    "codex process disconnected before turn/completed".into(),
                ));
            }
        } else {
            // Already running — send a new turn using the stored thread_id
            let thread_id = state.thread_id.clone().unwrap_or_default();
            let turn_params = if thread_id.is_empty() {
                json!({ "text": req.prompt })
            } else {
                json!({ "thread_id": thread_id, "text": req.prompt })
            };
            Self::send_request(&mut state, "turn/start", turn_params).await?;
            drop(state);

            if let Err(e) = tx.send(AgentEvent::TurnStarted).await {
                tracing::debug!("stream channel closed: {e}");
            }
        }

        Ok(())
    }

    async fn interrupt(&self) -> harness_core::error::Result<()> {
        let mut state = self.state.lock().await;
        if state.stdin.is_some() {
            // Try graceful interrupt first
            if let Err(e) = Self::send_request(&mut state, "turn/interrupt", json!({})).await {
                tracing::warn!("failed to send turn/interrupt: {e}, killing process");
                if let Some(ref mut child) = state.child {
                    if let Err(e) = child.kill().await {
                        tracing::warn!(pid = ?child.id(), "kill failed: {e}");
                    }
                }
            }
        }
        Ok(())
    }

    async fn steer(&self, text: String) -> harness_core::error::Result<()> {
        let mut state = self.state.lock().await;
        Self::send_request(&mut state, "turn/steer", json!({ "text": text })).await?;
        Ok(())
    }

    async fn respond_approval(
        &self,
        id: String,
        decision: ApprovalDecision,
    ) -> harness_core::error::Result<()> {
        let mut state = self.state.lock().await;
        let decision_value = match decision {
            ApprovalDecision::Accept => json!({ "decision": "accept" }),
            ApprovalDecision::Reject { reason } => {
                json!({ "decision": "reject", "reason": reason })
            }
        };

        let stdin = state.stdin.as_mut().ok_or_else(|| {
            harness_core::error::HarnessError::AgentExecution("codex stdin not available".into())
        })?;

        // Approval response uses the original request id
        let response = json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": decision_value,
        });

        let mut line = serde_json::to_string(&response).map_err(|e| {
            harness_core::error::HarnessError::AgentExecution(format!("failed to serialize: {e}"))
        })?;
        line.push('\n');

        stdin.write_all(line.as_bytes()).await.map_err(|e| {
            harness_core::error::HarnessError::AgentExecution(format!(
                "failed to write to codex: {e}"
            ))
        })?;
        stdin.flush().await.map_err(|e| {
            harness_core::error::HarnessError::AgentExecution(format!(
                "failed to flush codex stdin: {e}"
            ))
        })?;
        Ok(())
    }
}

/// Parse a Codex App Server JSON-RPC message into an AgentEvent.
///
/// Handles both notifications (method field, no id) and responses (id, result/error).
pub fn parse_codex_message(line: &str) -> Option<AgentEvent> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;

    // Notification: has "method" field
    if let Some(method) = v.get("method").and_then(|m| m.as_str()) {
        let params = v.get("params").cloned().unwrap_or(serde_json::Value::Null);
        return parse_notification(method, &params);
    }

    // Response: has "id" field
    if v.get("id").is_some() {
        if let Some(error) = v.get("error") {
            let message = error
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error")
                .to_string();
            return Some(AgentEvent::Error { message });
        }
        // Successful response — typically handled by request correlation, skip here
        return None;
    }

    None
}

fn parse_notification(method: &str, params: &serde_json::Value) -> Option<AgentEvent> {
    match method {
        "turn/started" | "turn_started" => Some(AgentEvent::TurnStarted),
        "item/started" | "item_started" => {
            let item_type = params
                .get("type")
                .and_then(|t| t.as_str())
                .unwrap_or("unknown")
                .to_string();
            Some(AgentEvent::ItemStarted { item_type })
        }
        "message/delta" | "message_delta" => {
            // Current protocol: params.delta.text
            // Legacy protocol: params.text
            let text = params
                .get("delta")
                .and_then(|d| d.get("text"))
                .and_then(|t| t.as_str())
                .or_else(|| params.get("text").and_then(|t| t.as_str()))
                .unwrap_or("")
                .to_string();
            Some(AgentEvent::MessageDelta { text })
        }
        "item/completed" | "item_completed" => Some(AgentEvent::ItemCompleted),
        "turn/completed" | "turn_completed" => {
            let output = assemble_turn_output(params);
            Some(AgentEvent::TurnCompleted { output })
        }
        "approval/request" | "approval_request" => {
            let (id, command) = extract_approval_fields(params);
            Some(AgentEvent::ApprovalRequest { id, command })
        }
        // Rich item events: tool calls emitted as named tool invocations
        "tool/call" | "tool_call" => {
            let name = params
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("unknown")
                .to_string();
            let input = params
                .get("input")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            Some(AgentEvent::ToolCall { name, input })
        }
        _ => None,
    }
}

/// Assemble a turn's text output from a `turn/completed` params object.
///
/// Current protocol: `params.items` is an array of output items; message items
/// carry content blocks with `text` fields.
/// Legacy protocol: `params.output` is a plain string.
fn assemble_turn_output(params: &serde_json::Value) -> String {
    if let Some(items) = params.get("items").and_then(|i| i.as_array()) {
        let parts: Vec<String> = items
            .iter()
            .filter_map(|item| {
                let item_type = item.get("type").and_then(|t| t.as_str())?;
                if item_type == "message" {
                    item.get("content")
                        .and_then(|c| c.as_array())
                        .and_then(|arr| arr.first())
                        .and_then(|block| block.get("text"))
                        .and_then(|t| t.as_str())
                        .map(|s| s.to_string())
                } else {
                    None
                }
            })
            .collect();
        if !parts.is_empty() {
            return parts.join("\n");
        }
    }
    // Legacy fallback
    params
        .get("output")
        .and_then(|o| o.as_str())
        .unwrap_or("")
        .to_string()
}

/// Extract `(id, command)` from an `approval/request` params object.
///
/// Current protocol: fields nested under `params.request`.
/// Command may be an array of strings (argv) or a plain string.
/// Legacy protocol: `params.id` and `params.command` at the top level.
fn extract_approval_fields(params: &serde_json::Value) -> (String, String) {
    if let Some(req_obj) = params.get("request") {
        let id = req_obj
            .get("id")
            .and_then(|i| i.as_str())
            .unwrap_or("")
            .to_string();
        let command = req_obj
            .get("command")
            .map(|c| {
                if let Some(arr) = c.as_array() {
                    arr.iter()
                        .filter_map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join(" ")
                } else {
                    c.as_str().unwrap_or("").to_string()
                }
            })
            .unwrap_or_default();
        return (id, command);
    }
    // Legacy fallback
    let id = params
        .get("id")
        .and_then(|i| i.as_str())
        .unwrap_or("")
        .to_string();
    let command = params
        .get("command")
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_string();
    (id, command)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_turn_started_notification() {
        let line = r#"{"jsonrpc":"2.0","method":"turn/started","params":{}}"#;
        let event = parse_codex_message(line).unwrap();
        assert!(matches!(event, AgentEvent::TurnStarted));
    }

    #[test]
    fn parse_item_started_notification() {
        let line = r#"{"jsonrpc":"2.0","method":"item/started","params":{"type":"tool_call"}}"#;
        let event = parse_codex_message(line).unwrap();
        match event {
            AgentEvent::ItemStarted { item_type } => assert_eq!(item_type, "tool_call"),
            other => panic!("expected ItemStarted, got {other:?}"),
        }
    }

    // Legacy shape: params.text
    #[test]
    fn parse_message_delta_legacy_shape() {
        let line =
            r#"{"jsonrpc":"2.0","method":"message/delta","params":{"text":"Let me check..."}}"#;
        let event = parse_codex_message(line).unwrap();
        match event {
            AgentEvent::MessageDelta { text } => assert_eq!(text, "Let me check..."),
            other => panic!("expected MessageDelta, got {other:?}"),
        }
    }

    // Current shape: params.delta.text
    #[test]
    fn parse_message_delta_current_shape() {
        let line =
            r#"{"jsonrpc":"2.0","method":"message/delta","params":{"delta":{"text":"Hello"}}}"#;
        let event = parse_codex_message(line).unwrap();
        match event {
            AgentEvent::MessageDelta { text } => assert_eq!(text, "Hello"),
            other => panic!("expected MessageDelta, got {other:?}"),
        }
    }

    #[test]
    fn parse_item_completed_notification() {
        let line = r#"{"jsonrpc":"2.0","method":"item/completed","params":{}}"#;
        let event = parse_codex_message(line).unwrap();
        assert!(matches!(event, AgentEvent::ItemCompleted));
    }

    // Legacy shape: params.output string
    #[test]
    fn parse_turn_completed_legacy_shape() {
        let line =
            r#"{"jsonrpc":"2.0","method":"turn/completed","params":{"output":"Bug fixed."}}"#;
        let event = parse_codex_message(line).unwrap();
        match event {
            AgentEvent::TurnCompleted { output } => assert_eq!(output, "Bug fixed."),
            other => panic!("expected TurnCompleted, got {other:?}"),
        }
    }

    // Current shape: params.items array of message items
    #[test]
    fn parse_turn_completed_items_shape() {
        let line = r#"{"jsonrpc":"2.0","method":"turn/completed","params":{"items":[{"type":"message","content":[{"type":"text","text":"All done."}]}]}}"#;
        let event = parse_codex_message(line).unwrap();
        match event {
            AgentEvent::TurnCompleted { output } => assert_eq!(output, "All done."),
            other => panic!("expected TurnCompleted, got {other:?}"),
        }
    }

    // Legacy shape: params.id and params.command at top level
    #[test]
    fn parse_approval_request_legacy_shape() {
        let line = r#"{"jsonrpc":"2.0","method":"approval/request","params":{"id":"req-42","command":"rm -rf /tmp/test"}}"#;
        let event = parse_codex_message(line).unwrap();
        match event {
            AgentEvent::ApprovalRequest { id, command } => {
                assert_eq!(id, "req-42");
                assert_eq!(command, "rm -rf /tmp/test");
            }
            other => panic!("expected ApprovalRequest, got {other:?}"),
        }
    }

    // Current shape: params.request.{id, command} with command as argv array
    #[test]
    fn parse_approval_request_current_shape() {
        let line = r#"{"jsonrpc":"2.0","method":"approval/request","params":{"request":{"id":"req-7","command":["git","push","origin","main"]}}}"#;
        let event = parse_codex_message(line).unwrap();
        match event {
            AgentEvent::ApprovalRequest { id, command } => {
                assert_eq!(id, "req-7");
                assert_eq!(command, "git push origin main");
            }
            other => panic!("expected ApprovalRequest, got {other:?}"),
        }
    }

    // Current shape: params.request.command as plain string
    #[test]
    fn parse_approval_request_current_shape_string_command() {
        let line = r#"{"jsonrpc":"2.0","method":"approval/request","params":{"request":{"id":"req-8","command":"cargo test"}}}"#;
        let event = parse_codex_message(line).unwrap();
        match event {
            AgentEvent::ApprovalRequest { id, command } => {
                assert_eq!(id, "req-8");
                assert_eq!(command, "cargo test");
            }
            other => panic!("expected ApprovalRequest, got {other:?}"),
        }
    }

    #[test]
    fn parse_tool_call_notification() {
        let line = r#"{"jsonrpc":"2.0","method":"tool/call","params":{"name":"bash","input":{"cmd":"ls -la"}}}"#;
        let event = parse_codex_message(line).unwrap();
        match event {
            AgentEvent::ToolCall { name, input } => {
                assert_eq!(name, "bash");
                assert_eq!(input["cmd"], "ls -la");
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn parse_error_response() {
        let line =
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32600,"message":"invalid request"}}"#;
        let event = parse_codex_message(line).unwrap();
        match event {
            AgentEvent::Error { message } => assert_eq!(message, "invalid request"),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn parse_success_response_returns_none() {
        let line = r#"{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}"#;
        assert!(parse_codex_message(line).is_none());
    }

    #[test]
    fn parse_unknown_notification_returns_none() {
        let line = r#"{"jsonrpc":"2.0","method":"custom/unknown","params":{}}"#;
        assert!(parse_codex_message(line).is_none());
    }

    #[test]
    fn parse_invalid_json_returns_none() {
        assert!(parse_codex_message("not json").is_none());
        assert!(parse_codex_message("").is_none());
    }

    #[test]
    fn parse_snake_case_notifications() {
        let line = r#"{"jsonrpc":"2.0","method":"turn_completed","params":{"output":"done"}}"#;
        let event = parse_codex_message(line).unwrap();
        assert!(matches!(event, AgentEvent::TurnCompleted { .. }));
    }

    #[tokio::test]
    async fn interrupt_noop_when_no_child() {
        let adapter = CodexAdapter::new(PathBuf::from("codex"));
        adapter.interrupt().await.unwrap();
    }

    #[test]
    fn assemble_turn_output_prefers_items_over_output() {
        let params = serde_json::json!({
            "output": "legacy",
            "items": [{"type": "message", "content": [{"type": "text", "text": "current"}]}]
        });
        assert_eq!(assemble_turn_output(&params), "current");
    }

    #[test]
    fn assemble_turn_output_falls_back_to_output_field() {
        let params = serde_json::json!({ "output": "fallback" });
        assert_eq!(assemble_turn_output(&params), "fallback");
    }

    #[test]
    fn assemble_turn_output_skips_non_message_items() {
        let params = serde_json::json!({
            "items": [
                {"type": "tool_call", "name": "bash"},
                {"type": "message", "content": [{"type": "text", "text": "done"}]}
            ]
        });
        assert_eq!(assemble_turn_output(&params), "done");
    }
}
