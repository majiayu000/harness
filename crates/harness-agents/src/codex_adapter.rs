use crate::codex::{parse_codex_item, parse_codex_token_usage};
use crate::streaming::capture_agent_stderr_diagnostics;
use async_trait::async_trait;
use harness_core::agent::{AgentAdapter, AgentEvent, ApprovalDecision, TurnRequest};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::ChildStdout;
use tokio::sync::{mpsc, Mutex};

type StdoutLines = Lines<BufReader<ChildStdout>>;

pub struct CodexAdapter {
    cli_path: PathBuf,
    state: Arc<Mutex<AdapterState>>,
}

struct AdapterState {
    child: Option<tokio::process::Child>,
    stdin: Option<tokio::process::ChildStdin>,
    stdout_lines: Option<StdoutLines>,
    next_id: u64,
    thread_id: Option<String>,
    active_turn_id: Option<String>,
}

impl AdapterState {
    fn new() -> Self {
        Self {
            child: None,
            stdin: None,
            stdout_lines: None,
            next_id: 1,
            thread_id: None,
            active_turn_id: None,
        }
    }

    fn next_request_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ParsedCodexMessage {
    Event(AgentEvent),
    ThreadStarted { thread_id: String },
    TurnStarted { turn_id: String },
    Response { id: Value, result: Value },
    Ignore,
}

impl CodexAdapter {
    pub fn new(cli_path: PathBuf) -> Self {
        Self {
            cli_path,
            state: Arc::new(Mutex::new(AdapterState::new())),
        }
    }

    async fn send_json_line(
        state: &mut AdapterState,
        payload: &Value,
    ) -> harness_core::error::Result<()> {
        let stdin = state.stdin.as_mut().ok_or_else(|| {
            harness_core::error::HarnessError::AgentExecution("codex stdin not available".into())
        })?;

        let mut line = serde_json::to_string(payload).map_err(|error| {
            harness_core::error::HarnessError::AgentExecution(format!(
                "failed to serialize codex payload: {error}"
            ))
        })?;
        line.push('\n');

        stdin.write_all(line.as_bytes()).await.map_err(|error| {
            harness_core::error::HarnessError::AgentExecution(format!(
                "failed to write to codex: {error}"
            ))
        })?;
        stdin.flush().await.map_err(|error| {
            harness_core::error::HarnessError::AgentExecution(format!(
                "failed to flush codex stdin: {error}"
            ))
        })
    }

    async fn send_request(
        state: &mut AdapterState,
        method: &str,
        params: Value,
    ) -> harness_core::error::Result<u64> {
        let id = state.next_request_id();
        let payload = json!({
            "id": id,
            "method": method,
            "params": params,
        });
        Self::send_json_line(state, &payload).await?;
        Ok(id)
    }

    async fn send_notification(
        state: &mut AdapterState,
        method: &str,
        params: Value,
    ) -> harness_core::error::Result<()> {
        Self::send_json_line(state, &notification_payload(method, params)).await
    }

    async fn send_response(
        state: &mut AdapterState,
        id: Value,
        result: Value,
    ) -> harness_core::error::Result<()> {
        let payload = json!({
            "id": id,
            "result": result,
        });
        Self::send_json_line(state, &payload).await
    }

    async fn read_next_message(
        lines: &mut StdoutLines,
    ) -> harness_core::error::Result<Option<ParsedCodexMessage>> {
        let Some(line) = lines.next_line().await.map_err(|error| {
            harness_core::error::HarnessError::AgentExecution(format!(
                "failed reading codex app-server stdout: {error}"
            ))
        })?
        else {
            return Ok(None);
        };
        if line.trim().is_empty() {
            return Ok(Some(ParsedCodexMessage::Ignore));
        }
        Ok(parse_codex_message(&line))
    }

    async fn ensure_child(
        &self,
        req: &TurnRequest,
        state: &mut AdapterState,
    ) -> harness_core::error::Result<()> {
        if state.child.is_some() {
            return Ok(());
        }

        let mut cmd = tokio::process::Command::new(&self.cli_path);
        cmd.arg("app-server")
            .arg("--listen")
            .arg("stdio://")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .current_dir(&req.project_root)
            .kill_on_drop(true);
        #[cfg(unix)]
        crate::set_process_group(&mut cmd);
        crate::strip_claude_env(&mut cmd);

        let mut child = cmd.spawn().map_err(|error| {
            harness_core::error::HarnessError::AgentExecution(format!(
                "failed to spawn codex app-server: {error}"
            ))
        })?;

        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                capture_agent_stderr_diagnostics(stderr, "codex", None).await;
            });
        }

        let stdout = child.stdout.take().ok_or_else(|| {
            harness_core::error::HarnessError::AgentExecution(
                "codex app-server stdout unavailable".into(),
            )
        })?;
        state.stdin = child.stdin.take();
        state.stdout_lines = Some(BufReader::new(stdout).lines());
        state.child = Some(child);

        let init_id = Self::send_request(
            state,
            "initialize",
            json!({
                "clientInfo": {
                    "name": "harness",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "capabilities": {
                    "experimentalApi": true,
                }
            }),
        )
        .await?;

        let mut lines = state.stdout_lines.take().ok_or_else(|| {
            harness_core::error::HarnessError::AgentExecution(
                "codex stdout reader not available".into(),
            )
        })?;
        loop {
            match Self::read_next_message(&mut lines).await? {
                Some(ParsedCodexMessage::Response { id, .. })
                    if response_id_matches(&id, init_id) =>
                {
                    break;
                }
                Some(ParsedCodexMessage::Event(AgentEvent::Warning { message })) => {
                    tracing::warn!(agent = "codex", "{message}");
                }
                Some(ParsedCodexMessage::Event(AgentEvent::Error { message })) => {
                    return Err(harness_core::error::HarnessError::AgentExecution(message));
                }
                Some(_) => {}
                None => {
                    return Err(harness_core::error::HarnessError::AgentExecution(
                        "codex app-server exited during initialize".into(),
                    ));
                }
            }
        }

        Self::send_notification(state, "initialized", Value::Null).await?;

        let thread_id_request = Self::send_request(
            state,
            "thread/start",
            json!({
                "cwd": req.project_root,
                "model": req.model,
                "ephemeral": true,
            }),
        )
        .await?;

        loop {
            match Self::read_next_message(&mut lines).await? {
                Some(ParsedCodexMessage::ThreadStarted { thread_id }) => {
                    state.thread_id = Some(thread_id);
                    break;
                }
                Some(ParsedCodexMessage::Response { id, result })
                    if response_id_matches(&id, thread_id_request) =>
                {
                    if let Some(thread_id) = thread_id_from_result(&result) {
                        state.thread_id = Some(thread_id);
                        break;
                    }
                }
                Some(ParsedCodexMessage::Event(AgentEvent::Warning { message })) => {
                    tracing::warn!(agent = "codex", "{message}");
                }
                Some(ParsedCodexMessage::Event(AgentEvent::Error { message })) => {
                    return Err(harness_core::error::HarnessError::AgentExecution(message));
                }
                Some(_) => {}
                None => {
                    return Err(harness_core::error::HarnessError::AgentExecution(
                        "codex app-server exited before thread/start completed".into(),
                    ));
                }
            }
        }

        state.stdout_lines = Some(lines);

        Ok(())
    }

    async fn clear_active_turn_id(&self) {
        self.state.lock().await.active_turn_id = None;
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
        self.ensure_child(&req, &mut state).await?;

        let thread_id = state.thread_id.clone().ok_or_else(|| {
            harness_core::error::HarnessError::AgentExecution(
                "codex thread/start did not yield a thread id".into(),
            )
        })?;

        Self::send_request(
            &mut state,
            "turn/start",
            json!({
                "threadId": thread_id,
                "cwd": req.project_root,
                "model": req.model,
                "input": [
                    {
                        "type": "text",
                        "text": req.prompt,
                    }
                ],
            }),
        )
        .await?;

        let mut lines = state.stdout_lines.take().ok_or_else(|| {
            harness_core::error::HarnessError::AgentExecution(
                "codex stdout reader not available".into(),
            )
        })?;
        drop(state);

        while let Some(message) = Self::read_next_message(&mut lines).await? {
            match message {
                ParsedCodexMessage::TurnStarted { turn_id } => {
                    let mut guard = self.state.lock().await;
                    guard.active_turn_id = Some(turn_id);
                    drop(guard);
                    if tx.send(AgentEvent::TurnStarted).await.is_err() {
                        break;
                    }
                }
                ParsedCodexMessage::ThreadStarted { thread_id } => {
                    self.state.lock().await.thread_id = Some(thread_id);
                }
                ParsedCodexMessage::Response { .. } | ParsedCodexMessage::Ignore => {}
                ParsedCodexMessage::Event(event) => {
                    let is_terminal = matches!(
                        event,
                        AgentEvent::TurnCompleted { .. } | AgentEvent::Error { .. }
                    );
                    if is_terminal {
                        self.clear_active_turn_id().await;
                    }
                    if tx.send(event).await.is_err() {
                        break;
                    }
                    if is_terminal {
                        break;
                    }
                }
            }
        }

        self.state.lock().await.stdout_lines = Some(lines);
        Ok(())
    }

    async fn interrupt(&self) -> harness_core::error::Result<()> {
        let mut state = self.state.lock().await;
        let Some(thread_id) = state.thread_id.clone() else {
            return Ok(());
        };
        let Some(turn_id) = state.active_turn_id.clone() else {
            return Ok(());
        };
        Self::send_request(
            &mut state,
            "turn/interrupt",
            json!({
                "threadId": thread_id,
                "turnId": turn_id,
            }),
        )
        .await?;
        Ok(())
    }

    async fn steer(&self, text: String) -> harness_core::error::Result<()> {
        let mut state = self.state.lock().await;
        let thread_id = state.thread_id.clone().ok_or_else(|| {
            harness_core::error::HarnessError::AgentExecution("codex thread id unavailable".into())
        })?;
        let turn_id = state.active_turn_id.clone().ok_or_else(|| {
            harness_core::error::HarnessError::AgentExecution(
                "codex active turn unavailable".into(),
            )
        })?;
        Self::send_request(
            &mut state,
            "turn/steer",
            json!({
                "threadId": thread_id,
                "expectedTurnId": turn_id,
                "input": [
                    {
                        "type": "text",
                        "text": text,
                    }
                ],
            }),
        )
        .await?;
        Ok(())
    }

    async fn respond_approval(
        &self,
        id: String,
        decision: ApprovalDecision,
    ) -> harness_core::error::Result<()> {
        let mut state = self.state.lock().await;
        let request_id: Value =
            serde_json::from_str(&id).unwrap_or_else(|_| Value::String(id.clone()));
        let result = approval_decision_result(decision);
        Self::send_response(&mut state, request_id, result).await
    }
}

fn notification_payload(method: &str, params: Value) -> Value {
    json!({
        "method": method,
        "params": params,
    })
}

fn approval_decision_result(decision: ApprovalDecision) -> Value {
    match decision {
        ApprovalDecision::Accept => json!({ "decision": "accept" }),
        ApprovalDecision::Reject { reason } => json!({
            "decision": "decline",
            "reason": reason,
        }),
    }
}

fn response_id_matches(actual: &Value, expected: u64) -> bool {
    actual.as_u64() == Some(expected) || actual.as_str() == Some(&expected.to_string())
}

fn request_id_string(id: &Value) -> String {
    match id {
        Value::String(value) => value.clone(),
        other => other.to_string(),
    }
}

fn thread_id_from_result(result: &Value) -> Option<String> {
    result
        .get("thread")
        .and_then(|thread| thread.get("id"))
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .or_else(|| {
            result
                .get("id")
                .and_then(Value::as_str)
                .map(ToString::to_string)
        })
}

fn parse_app_server_agent_event(
    method: &str,
    params: &Value,
    id: Option<&Value>,
) -> ParsedCodexMessage {
    match method {
        "thread/started" => {
            let thread_id = params
                .get("thread")
                .and_then(|thread| thread.get("id"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            ParsedCodexMessage::ThreadStarted { thread_id }
        }
        "turn/started" => {
            let turn_id = params
                .get("turn")
                .and_then(|turn| turn.get("id"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            ParsedCodexMessage::TurnStarted { turn_id }
        }
        "item/agentMessage/delta" => ParsedCodexMessage::Event(AgentEvent::MessageDelta {
            text: params
                .get("delta")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        }),
        "item/commandExecution/outputDelta" => {
            ParsedCodexMessage::Event(AgentEvent::ToolOutputDelta {
                item_id: params
                    .get("itemId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                text: params
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            })
        }
        "item/started" => params
            .get("item")
            .and_then(parse_codex_item)
            .map(|item| ParsedCodexMessage::Event(AgentEvent::ItemStartedPayload { item }))
            .unwrap_or(ParsedCodexMessage::Ignore),
        "item/completed" => params
            .get("item")
            .and_then(parse_codex_item)
            .map(|item| ParsedCodexMessage::Event(AgentEvent::ItemCompletedPayload { item }))
            .unwrap_or(ParsedCodexMessage::Ignore),
        "thread/tokenUsage/updated" => params
            .get("tokenUsage")
            .and_then(|usage| usage.get("last"))
            .and_then(parse_codex_token_usage)
            .map(|usage| ParsedCodexMessage::Event(AgentEvent::TokenUsage { usage }))
            .unwrap_or(ParsedCodexMessage::Ignore),
        "warning" => ParsedCodexMessage::Event(AgentEvent::Warning {
            message: params
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown warning")
                .to_string(),
        }),
        "error" => ParsedCodexMessage::Event(AgentEvent::Error {
            message: params
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
                .or_else(|| params.get("message").and_then(Value::as_str))
                .unwrap_or("unknown error")
                .to_string(),
        }),
        "turn/completed" => ParsedCodexMessage::Event(AgentEvent::TurnCompleted {
            output: params
                .get("turn")
                .and_then(|turn| turn.get("items"))
                .and_then(Value::as_array)
                .and_then(|items| items.iter().rev().find_map(parse_codex_item))
                .and_then(|item| match item {
                    harness_core::types::Item::AgentReasoning { content } => Some(content),
                    _ => None,
                })
                .unwrap_or_default(),
        }),
        "item/commandExecution/requestApproval"
        | "item/fileChange/requestApproval"
        | "item/permissions/requestApproval" => {
            ParsedCodexMessage::Event(AgentEvent::ApprovalRequest {
                id: id.map(request_id_string).unwrap_or_default(),
                command: params
                    .get("command")
                    .and_then(Value::as_str)
                    .or_else(|| params.get("reason").and_then(Value::as_str))
                    .unwrap_or(method)
                    .to_string(),
            })
        }
        _ => ParsedCodexMessage::Ignore,
    }
}

pub fn parse_codex_message(line: &str) -> Option<ParsedCodexMessage> {
    let value: Value = serde_json::from_str(line).ok()?;

    if let Some(method) = value.get("method").and_then(Value::as_str) {
        let params = value.get("params").cloned().unwrap_or(Value::Null);
        return Some(parse_app_server_agent_event(
            method,
            &params,
            value.get("id"),
        ));
    }

    if let Some(id) = value.get("id") {
        if let Some(error) = value.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error")
                .to_string();
            return Some(ParsedCodexMessage::Event(AgentEvent::Error { message }));
        }
        if let Some(result) = value.get("result") {
            return Some(ParsedCodexMessage::Response {
                id: id.clone(),
                result: result.clone(),
            });
        }
    }

    Some(ParsedCodexMessage::Ignore)
}

#[cfg(test)]
mod tests {
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
    fn parse_token_usage_notification() {
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

    #[tokio::test]
    async fn interrupt_noop_when_no_child() {
        let adapter = CodexAdapter::new(PathBuf::from("codex"));
        adapter.interrupt().await.unwrap();
    }

    #[tokio::test]
    async fn clear_active_turn_id_drops_stale_turn_state() {
        let adapter = CodexAdapter::new(PathBuf::from("codex"));
        adapter.state.lock().await.active_turn_id = Some("turn-1".into());

        adapter.clear_active_turn_id().await;

        assert_eq!(adapter.state.lock().await.active_turn_id, None);
    }
}
