use async_trait::async_trait;
use harness_core::{
    agent::AgentAdapter, agent::AgentEvent, agent::ApprovalDecision, agent::TurnRequest,
};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::ChildStdout;
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

/// Persistent stdout reader kept alive across turns.
type StdoutLines = Lines<BufReader<ChildStdout>>;

struct AdapterState {
    child: Option<tokio::process::Child>,
    stdin: Option<tokio::process::ChildStdin>,
    /// Stdout reader persisted between turns so subsequent turns can read events.
    stdout_lines: Option<StdoutLines>,
    next_id: u64,
}

impl AdapterState {
    fn new() -> Self {
        Self {
            child: None,
            stdin: None,
            stdout_lines: None,
            next_id: 1,
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

            // Initialize handshake
            Self::send_request(&mut state, "initialize", json!({})).await?;

            // Persist the stdout reader across turns so subsequent turns can read events.
            let mut lines = BufReader::new(stdout).lines();

            // Read init response
            if let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(line = %line, "codex initialize response");
            }

            // Send initialized notification
            Self::send_notification(&mut state, "initialized").await?;

            state.stdout_lines = Some(lines);
        }

        // Send turn/start for both first and subsequent turns.
        Self::send_request(&mut state, "turn/start", json!({ "text": req.prompt })).await?;

        // Take the persistent stdout reader out of state so we can read from it
        // without holding the lock (which would block concurrent steer/interrupt calls).
        let mut lines = state.stdout_lines.take().ok_or_else(|| {
            harness_core::error::HarnessError::AgentExecution(
                "codex stdout reader not available".into(),
            )
        })?;

        // Release state lock before blocking on stdout.
        drop(state);

        if tx.send(AgentEvent::TurnStarted).await.is_err() {
            // Put reader back so future turns can still use it.
            self.state.lock().await.stdout_lines = Some(lines);
            return Ok(());
        }

        // Read event stream until the turn completes or the channel closes.
        while let Ok(Some(line)) = lines.next_line().await {
            if line.trim().is_empty() {
                continue;
            }

            if let Some(event) = parse_codex_message(&line) {
                let is_turn_completed = matches!(event, AgentEvent::TurnCompleted { .. });
                let is_error = matches!(event, AgentEvent::Error { .. });

                if tx.send(event).await.is_err() {
                    break;
                }

                if is_turn_completed || is_error {
                    break;
                }
            }
        }

        // Return the reader to state for the next turn.
        self.state.lock().await.stdout_lines = Some(lines);

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
            let text = params
                .get("text")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string();
            Some(AgentEvent::MessageDelta { text })
        }
        "item/completed" | "item_completed" => Some(AgentEvent::ItemCompleted),
        "turn/completed" | "turn_completed" => {
            let output = params
                .get("output")
                .and_then(|o| o.as_str())
                .unwrap_or("")
                .to_string();
            Some(AgentEvent::TurnCompleted { output })
        }
        "approval/request" | "approval_request" => {
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
            Some(AgentEvent::ApprovalRequest { id, command })
        }
        _ => None,
    }
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

    #[test]
    fn parse_message_delta_notification() {
        let line =
            r#"{"jsonrpc":"2.0","method":"message/delta","params":{"text":"Let me check..."}}"#;
        let event = parse_codex_message(line).unwrap();
        match event {
            AgentEvent::MessageDelta { text } => assert_eq!(text, "Let me check..."),
            other => panic!("expected MessageDelta, got {other:?}"),
        }
    }

    #[test]
    fn parse_item_completed_notification() {
        let line = r#"{"jsonrpc":"2.0","method":"item/completed","params":{}}"#;
        let event = parse_codex_message(line).unwrap();
        assert!(matches!(event, AgentEvent::ItemCompleted));
    }

    #[test]
    fn parse_turn_completed_notification() {
        let line =
            r#"{"jsonrpc":"2.0","method":"turn/completed","params":{"output":"Bug fixed."}}"#;
        let event = parse_codex_message(line).unwrap();
        match event {
            AgentEvent::TurnCompleted { output } => assert_eq!(output, "Bug fixed."),
            other => panic!("expected TurnCompleted, got {other:?}"),
        }
    }

    #[test]
    fn parse_approval_request_notification() {
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
}
