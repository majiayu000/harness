use async_trait::async_trait;
use harness_core::{
    agent::AgentAdapter,
    agent::AgentEvent,
    agent::ApprovalDecision,
    agent::TurnRequest,
    config::agents::{CodexCloudConfig, SandboxMode},
};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, Mutex};

/// Codex App Server adapter (L3-L4).
///
/// Spawns `codex app-server` as a stdio subprocess speaking JSON-RPC 2.0.
/// Supports approval gates, interrupt, and steer for a single live turn.
pub struct CodexAdapter {
    cli_path: PathBuf,
    sandbox_mode: SandboxMode,
    cloud: CodexCloudConfig,
    state: Arc<Mutex<AdapterState>>,
}

struct AdapterState {
    child: Option<tokio::process::Child>,
    stdin: Option<tokio::process::ChildStdin>,
    next_id: u64,
}

impl AdapterState {
    fn new() -> Self {
        Self {
            child: None,
            stdin: None,
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
    pub fn new(cli_path: PathBuf, sandbox_mode: SandboxMode, cloud: CodexCloudConfig) -> Self {
        Self {
            cli_path,
            sandbox_mode,
            cloud,
            state: Arc::new(Mutex::new(AdapterState::new())),
        }
    }

    fn sandbox_policy(&self) -> &'static str {
        match self.sandbox_mode {
            SandboxMode::ReadOnly => "readOnly",
            SandboxMode::WorkspaceWrite => "workspaceWrite",
            SandboxMode::DangerFullAccess => "dangerFullAccess",
        }
    }

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
        if state.child.is_some() {
            return Err(harness_core::error::HarnessError::AgentExecution(
                "codex adapter cannot reuse an existing app-server process across turns"
                    .to_string(),
            ));
        }

        if self.cloud.enabled {
            crate::cloud_setup::run_setup_phase(&self.cloud, &req.project_root).await?;
        }

        let mut cmd = tokio::process::Command::new(&self.cli_path);
        cmd.arg("app-server")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .current_dir(&req.project_root)
            .kill_on_drop(true);
        if self.cloud.enabled {
            cmd.arg("--ephemeral");
        }
        for key in &self.cloud.setup_secret_env {
            cmd.env_remove(key);
        }
        #[cfg(unix)]
        crate::set_process_group(&mut cmd);
        crate::strip_claude_env(&mut cmd);

        let mut child = cmd.spawn().map_err(|e| {
            harness_core::error::HarnessError::AgentExecution(format!("failed to spawn codex: {e}"))
        })?;

        state.stdin = child.stdin.take();
        let stdout = child.stdout.take().ok_or_else(|| {
            harness_core::error::HarnessError::AgentExecution("no stdout from codex".into())
        })?;
        state.child = Some(child);

        Self::send_request(
            &mut state,
            "initialize",
            json!({
                "clientInfo": {
                    "name": "harness",
                    "title": "Harness",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }),
        )
        .await?;
        Self::send_request(
            &mut state,
            "thread/start",
            json!({
                "cwd": req.project_root,
                "sandboxPolicy": { "type": self.sandbox_policy() }
            }),
        )
        .await?;

        let reader = tokio::io::BufReader::new(stdout);
        let mut lines = reader.lines();
        let mut thread_id: Option<String> = None;

        // Loop until we find the thread/start response containing the thread id.
        // codex app-server may emit intermediate notifications (e.g. thread/started)
        // before the RPC response, so a fixed read count can miss the id.
        let mut init_lines_read = 0u32;
        const MAX_INIT_LINES: u32 = 20;
        while thread_id.is_none() && init_lines_read < MAX_INIT_LINES {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    init_lines_read += 1;
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) {
                        if let Some(id) = value
                            .get("result")
                            .and_then(|result| result.get("thread"))
                            .and_then(|thread| thread.get("id"))
                            .and_then(|id| id.as_str())
                        {
                            thread_id = Some(id.to_string());
                        }
                    }
                }
                Ok(None) => break,
                Err(_) => break,
            }
        }

        Self::send_notification(&mut state, "initialized").await?;

        let thread_id = thread_id.ok_or_else(|| {
            harness_core::error::HarnessError::AgentExecution(
                "codex thread/start did not return thread id".into(),
            )
        })?;
        let turn_start_id = Self::send_request(
            &mut state,
            "turn/start",
            json!({
                "threadId": thread_id,
                "input": [{ "type": "text", "text": req.prompt }],
                "sandboxPolicy": { "type": self.sandbox_policy() }
            }),
        )
        .await?;

        // Consume lines until we see the RPC response for turn/start (matched by id).
        // An unconditional single-line drop can lose early notifications that arrive
        // before the turn/start ACK when JSON-RPC traffic is interleaved.
        let mut turn_start_acked = false;
        let mut ack_lines_read = 0u32;
        const MAX_ACK_LINES: u32 = 20;
        while !turn_start_acked && ack_lines_read < MAX_ACK_LINES {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    ack_lines_read += 1;
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) {
                        if value.get("id").and_then(|v| v.as_u64()) == Some(turn_start_id) {
                            turn_start_acked = true;
                        }
                        // Notifications arriving before the ack are intentionally skipped;
                        // the main event loop below handles all subsequent notifications.
                    }
                }
                Ok(None) => break,
                Err(_) => break,
            }
        }

        drop(state);

        let mut turn_started_sent = false;
        while let Ok(Some(line)) = lines.next_line().await {
            if line.trim().is_empty() {
                continue;
            }

            if let Some(event) = parse_codex_message(&line) {
                if !turn_started_sent {
                    if tx.send(AgentEvent::TurnStarted).await.is_err() {
                        break;
                    }
                    turn_started_sent = true;
                }

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

        let mut state = self.state.lock().await;
        state.stdin = None;
        state.child = None;
        Ok(())
    }

    async fn interrupt(&self) -> harness_core::error::Result<()> {
        let mut state = self.state.lock().await;
        if state.stdin.is_some() {
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
pub fn parse_codex_message(line: &str) -> Option<AgentEvent> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;

    if let Some(method) = v.get("method").and_then(|m| m.as_str()) {
        let params = v.get("params").cloned().unwrap_or(serde_json::Value::Null);
        return parse_notification(method, &params);
    }

    if v.get("id").is_some() {
        if let Some(error) = v.get("error") {
            let message = error
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error")
                .to_string();
            return Some(AgentEvent::Error { message });
        }
        return None;
    }

    None
}

fn parse_notification(method: &str, params: &serde_json::Value) -> Option<AgentEvent> {
    match method {
        "turn/started" | "turn_started" | "thread/started" | "thread_started" => {
            Some(AgentEvent::TurnStarted)
        }
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

    #[test]
    fn sandbox_policy_maps_config_values() {
        let read_only = CodexAdapter::new(
            PathBuf::from("codex"),
            SandboxMode::ReadOnly,
            CodexCloudConfig::default(),
        );
        assert_eq!(read_only.sandbox_policy(), "readOnly");

        let workspace_write = CodexAdapter::new(
            PathBuf::from("codex"),
            SandboxMode::WorkspaceWrite,
            CodexCloudConfig::default(),
        );
        assert_eq!(workspace_write.sandbox_policy(), "workspaceWrite");

        let danger = CodexAdapter::new(
            PathBuf::from("codex"),
            SandboxMode::DangerFullAccess,
            CodexCloudConfig::default(),
        );
        assert_eq!(danger.sandbox_policy(), "dangerFullAccess");
    }

    #[tokio::test]
    async fn interrupt_noop_when_no_child() {
        let adapter = CodexAdapter::new(
            PathBuf::from("codex"),
            SandboxMode::DangerFullAccess,
            CodexCloudConfig::default(),
        );
        adapter.interrupt().await.unwrap();
    }
}
