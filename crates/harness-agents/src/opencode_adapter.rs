use crate::streaming::capture_agent_stderr_diagnostics;
use async_trait::async_trait;
use harness_core::agent::{AgentAdapter, AgentEvent, ApprovalDecision, TurnRequest};
use harness_core::config::agents::{OpenCodeAgentConfig, SandboxMode};
use harness_core::types::TokenUsage;
use harness_sandbox::SandboxSpec;
use serde_json::{json, Value};
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::ChildStdout;
use tokio::sync::{mpsc, Mutex};

type StdoutLines = Lines<BufReader<ChildStdout>>;
const MAX_PROTOCOL_LINE_PREVIEW: usize = 240;

fn protocol_line_preview(line: &str) -> String {
    let mut chars = line.chars();
    let mut preview: String = chars.by_ref().take(MAX_PROTOCOL_LINE_PREVIEW).collect();
    if chars.next().is_some() {
        preview.push_str("...");
    }
    preview
}

#[derive(Debug, Clone, PartialEq)]
pub enum ParsedAcpMessage {
    Event(AgentEvent),
    Response { id: Value, result: Value },
    Ignore,
}

/// Parse one ACP JSON-RPC line from `opencode acp` stdout.
pub fn parse_acp_message(line: &str) -> Option<ParsedAcpMessage> {
    let value: Value = serde_json::from_str(line).ok()?;
    if value.get("method").is_some() {
        return parse_acp_notification(&value);
    }
    if value.get("id").is_some() {
        if value.get("error").is_some() {
            return Some(ParsedAcpMessage::Response {
                id: value.get("id").cloned()?,
                result: value.get("error").cloned()?,
            });
        }
        return Some(ParsedAcpMessage::Response {
            id: value.get("id").cloned()?,
            result: value.get("result").cloned()?,
        });
    }
    None
}

fn parse_acp_notification(value: &Value) -> Option<ParsedAcpMessage> {
    let method = value.get("method")?.as_str()?;
    let params = value.get("params").cloned().unwrap_or(Value::Null);
    match method {
        "session/update" => {
            let update = params.get("update")?;
            let session_update = update.get("sessionUpdate")?.as_str()?;
            match session_update {
                "agent_message_chunk" => {
                    let text = update
                        .pointer("/content/text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    Some(ParsedAcpMessage::Event(AgentEvent::MessageDelta { text }))
                }
                "tool_call" => {
                    let name = update
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or("tool")
                        .to_string();
                    let tool_call_id = update
                        .get("toolCallId")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let input = json!({ "toolCallId": tool_call_id });
                    Some(ParsedAcpMessage::Event(AgentEvent::ToolCall {
                        name,
                        input,
                    }))
                }
                "tool_call_update" => {
                    let status = update
                        .get("status")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    match status {
                        "in_progress" => Some(ParsedAcpMessage::Event(AgentEvent::ItemStarted {
                            item_type: "tool_call".into(),
                        })),
                        "completed" | "error" => {
                            Some(ParsedAcpMessage::Event(AgentEvent::ItemCompleted))
                        }
                        _ => Some(ParsedAcpMessage::Ignore),
                    }
                }
                "usage_update" => {
                    let used = update.get("used").and_then(Value::as_u64).unwrap_or(0);
                    let size = update.get("size").and_then(Value::as_u64).unwrap_or(0);
                    let cost = update
                        .pointer("/cost/amount")
                        .and_then(Value::as_f64)
                        .unwrap_or(0.0);
                    let usage = TokenUsage {
                        input_tokens: used,
                        output_tokens: 0,
                        total_tokens: size,
                        cost_usd: cost,
                    };
                    Some(ParsedAcpMessage::Event(AgentEvent::TokenUsage { usage }))
                }
                _ => Some(ParsedAcpMessage::Ignore),
            }
        }
        "session/request_permission" => {
            let id = value.get("id")?.clone();
            let command = params
                .get("prompt")
                .and_then(Value::as_str)
                .unwrap_or("permission requested")
                .to_string();
            Some(ParsedAcpMessage::Event(AgentEvent::ApprovalRequest {
                id: request_id_string(&id),
                command,
            }))
        }
        _ => Some(ParsedAcpMessage::Ignore),
    }
}

fn request_id_string(id: &Value) -> String {
    match id {
        Value::String(value) => value.clone(),
        other => other.to_string(),
    }
}

fn response_id_matches(actual: &Value, expected: u64) -> bool {
    actual.as_u64() == Some(expected) || actual.as_str() == Some(&expected.to_string())
}

fn request_id_from_string(id: &str) -> Value {
    serde_json::from_str(id).unwrap_or_else(|_| Value::String(id.to_string()))
}

fn stall_timeout_for(req: &TurnRequest) -> Option<Duration> {
    req.timeout_secs
        .filter(|seconds| *seconds > 0)
        .map(Duration::from_secs)
}

async fn prepare_acp_spawn(
    cli_path: &std::path::Path,
    req: &TurnRequest,
) -> harness_core::error::Result<crate::spawn_contract::PreparedAgentSpawn> {
    let args = [OsString::from("acp"), OsString::from("--cwd")];
    let sandbox_mode = req.sandbox_mode.unwrap_or(SandboxMode::DangerFullAccess);
    let mut args = args.to_vec();
    args.push(OsString::from(&req.project_root));
    let sandbox_spec = if let Some(token) = req.capability_token.as_ref() {
        SandboxSpec::new(sandbox_mode, &req.project_root)
            .with_allowed_write_paths(token.allowed_write_paths.clone())
    } else {
        SandboxSpec::new(sandbox_mode, &req.project_root)
    };
    crate::spawn_contract::prepare_agent_spawn(crate::spawn_contract::AgentSpawnInput {
        program: cli_path,
        args: &args,
        project_root: &req.project_root,
        sandbox_spec: &sandbox_spec,
        env_vars: &req.env_vars,
        permission_mode: req.permission_mode,
        forward_stdin: true,
    })
    .await
}

pub struct OpenCodeAcpAdapter {
    cli_path: PathBuf,
    default_model: String,
    sandbox_mode: SandboxMode,
    state: Arc<Mutex<AdapterState>>,
}

struct AdapterState {
    child: Option<crate::ManagedChild>,
    stdin: Option<tokio::process::ChildStdin>,
    stdout_lines: Option<StdoutLines>,
    next_id: u64,
    session_id: Option<String>,
}

impl AdapterState {
    fn new() -> Self {
        Self {
            child: None,
            stdin: None,
            stdout_lines: None,
            next_id: 1,
            session_id: None,
        }
    }

    fn next_request_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn child_ready(&self) -> bool {
        self.child.is_some() && self.stdin.is_some() && self.stdout_lines.is_some()
    }

    async fn reset_child(&mut self) {
        if let Some(mut child) = self.child.take() {
            child.terminate_now();
            if let Err(error) = child.wait_and_cleanup_descendants().await {
                tracing::warn!("failed to clean up opencode acp child: {error}");
            }
        }
        self.stdin = None;
        self.stdout_lines = None;
        self.session_id = None;
    }
}

impl OpenCodeAcpAdapter {
    pub fn new(cli_path: PathBuf) -> Self {
        let config = OpenCodeAgentConfig {
            cli_path,
            ..OpenCodeAgentConfig::default()
        };
        Self::from_config(config, SandboxMode::DangerFullAccess)
    }

    pub fn from_config(config: OpenCodeAgentConfig, sandbox_mode: SandboxMode) -> Self {
        Self {
            cli_path: config.cli_path,
            default_model: config.default_model,
            sandbox_mode,
            state: Arc::new(Mutex::new(AdapterState::new())),
        }
    }

    fn effective_turn_request(&self, mut req: TurnRequest) -> TurnRequest {
        if req.model.is_none() && !self.default_model.is_empty() {
            req.model = Some(self.default_model.clone());
        }
        if req.sandbox_mode.is_none() {
            req.sandbox_mode = Some(self.sandbox_mode);
        }
        let run_identity = crate::resolve_agent_run_identity(&req.env_vars);
        run_identity.write_env_vars(&mut req.env_vars);
        req
    }

    async fn send_json_line(
        state: &mut AdapterState,
        payload: &Value,
    ) -> harness_core::error::Result<()> {
        let stdin = state.stdin.as_mut().ok_or_else(|| {
            harness_core::error::HarnessError::AgentExecution("opencode stdin not available".into())
        })?;
        let mut line = serde_json::to_string(payload).map_err(|error| {
            harness_core::error::HarnessError::AgentExecution(format!(
                "failed to serialize opencode payload: {error}"
            ))
        })?;
        line.push('\n');
        stdin.write_all(line.as_bytes()).await.map_err(|error| {
            harness_core::error::HarnessError::AgentExecution(format!(
                "failed to write to opencode: {error}"
            ))
        })?;
        stdin.flush().await.map_err(|error| {
            harness_core::error::HarnessError::AgentExecution(format!(
                "failed to flush opencode stdin: {error}"
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
            "jsonrpc": "2.0",
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
        let payload = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        Self::send_json_line(state, &payload).await
    }

    async fn read_next_message(
        lines: &mut StdoutLines,
    ) -> harness_core::error::Result<Option<ParsedAcpMessage>> {
        let Some(line) = lines.next_line().await.map_err(|error| {
            harness_core::error::HarnessError::AgentExecution(format!(
                "failed reading opencode acp stdout: {error}"
            ))
        })?
        else {
            return Ok(None);
        };
        if line.trim().is_empty() {
            return Ok(Some(ParsedAcpMessage::Ignore));
        }
        parse_acp_message(&line).map(Some).ok_or_else(|| {
            harness_core::error::HarnessError::AgentExecution(format!(
                "opencode acp emitted invalid JSON-RPC stdout: {}",
                protocol_line_preview(&line)
            ))
        })
    }

    async fn read_next_message_with_timeout(
        lines: &mut StdoutLines,
        stall_timeout: Option<Duration>,
        phase: &str,
    ) -> harness_core::error::Result<Option<ParsedAcpMessage>> {
        let read = Self::read_next_message(lines);
        let Some(stall_timeout) = stall_timeout else {
            return read.await;
        };
        match tokio::time::timeout(stall_timeout, read).await {
            Ok(result) => result,
            Err(_) => Err(harness_core::error::HarnessError::AgentExecution(format!(
                "opencode acp {phase} stalled for {stall_timeout:?} without stdout"
            ))),
        }
    }

    async fn ensure_child(
        &self,
        req: &TurnRequest,
        state: &mut AdapterState,
    ) -> harness_core::error::Result<()> {
        if state.child_ready() {
            return Ok(());
        }
        if state.child.is_some() {
            tracing::warn!(
                "opencode acp state is incomplete; restarting before starting a new turn"
            );
            state.reset_child().await;
        }

        let run_identity = crate::resolve_agent_run_identity(&req.env_vars);
        let prepared_spawn = prepare_acp_spawn(&self.cli_path, req).await?;
        let spawn_project_root = req.project_root.clone();
        let supervised = crate::spawn_supervisor::spawn_agent(
            crate::spawn_supervisor::AgentSpawnPlan {
                prepared_spawn,
                run_identity,
                native_kind: "opencode",
                process_label: "opencode acp",
                stdio: crate::spawn_supervisor::AgentStdio::piped_output(
                    std::process::Stdio::piped(),
                ),
                extra_env_removals: Vec::new(),
                map_spawn_error: Box::new(move |error, _spawn| {
                    let message = crate::classify_missing_workspace_spawn_failure(
                        error,
                        &spawn_project_root,
                        format!("failed to spawn opencode acp: {error}"),
                    );
                    harness_core::error::HarnessError::AgentExecution(message)
                }),
            },
            req.capability_token.as_ref(),
        )
        .await?;
        let mut child = supervised.child;

        if let Some(stderr) = child.inner_mut().stderr.take() {
            tokio::spawn(async move {
                capture_agent_stderr_diagnostics(stderr, "opencode", None).await;
            });
        }

        let stdout = child.inner_mut().stdout.take().ok_or_else(|| {
            harness_core::error::HarnessError::AgentExecution(
                "opencode acp stdout unavailable".into(),
            )
        })?;
        state.stdin = child.inner_mut().stdin.take();
        state.stdout_lines = Some(BufReader::new(stdout).lines());
        state.child = Some(child);
        let stall_timeout = stall_timeout_for(req);

        let init_id = match Self::send_request(
            state,
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientInfo": {
                    "name": "harness",
                    "version": env!("CARGO_PKG_VERSION"),
                },
            }),
        )
        .await
        {
            Ok(id) => id,
            Err(error) => {
                state.reset_child().await;
                return Err(error);
            }
        };

        let mut lines = state.stdout_lines.take().ok_or_else(|| {
            harness_core::error::HarnessError::AgentExecution(
                "opencode stdout reader not available".into(),
            )
        })?;
        let protocol_result = async {
            loop {
                match Self::read_next_message_with_timeout(&mut lines, stall_timeout, "initialize")
                    .await?
                {
                    Some(ParsedAcpMessage::Response { id, .. })
                        if response_id_matches(&id, init_id) =>
                    {
                        break;
                    }
                    Some(ParsedAcpMessage::Event(AgentEvent::Warning { message })) => {
                        tracing::warn!(agent = "opencode", "{message}");
                    }
                    Some(ParsedAcpMessage::Event(AgentEvent::Error { message })) => {
                        return Err(harness_core::error::HarnessError::AgentExecution(message));
                    }
                    Some(_) => {}
                    None => {
                        return Err(harness_core::error::HarnessError::AgentExecution(
                            "opencode acp exited during initialize".into(),
                        ));
                    }
                }
            }

            Self::send_notification(state, "notifications/initialized", Value::Null).await?;

            let session_request = Self::send_request(
                state,
                "session/new",
                json!({
                    "cwd": req.project_root,
                    "mcpServers": [],
                    "configOptions": session_config_options(req),
                }),
            )
            .await?;

            loop {
                match Self::read_next_message_with_timeout(&mut lines, stall_timeout, "session/new")
                    .await?
                {
                    Some(ParsedAcpMessage::Response { id, result })
                        if response_id_matches(&id, session_request) =>
                    {
                        if let Some(session_id) = result.get("sessionId").and_then(Value::as_str) {
                            state.session_id = Some(session_id.to_string());
                            break;
                        }
                    }
                    Some(ParsedAcpMessage::Event(AgentEvent::Warning { message })) => {
                        tracing::warn!(agent = "opencode", "{message}");
                    }
                    Some(ParsedAcpMessage::Event(AgentEvent::Error { message })) => {
                        return Err(harness_core::error::HarnessError::AgentExecution(message));
                    }
                    Some(_) => {}
                    None => {
                        return Err(harness_core::error::HarnessError::AgentExecution(
                            "opencode acp exited before session/new completed".into(),
                        ));
                    }
                }
            }
            Ok(())
        }
        .await;

        match protocol_result {
            Ok(()) => {
                state.stdout_lines = Some(lines);
                Ok(())
            }
            Err(error) => {
                drop(lines);
                state.reset_child().await;
                Err(error)
            }
        }
    }
}

fn session_config_options(req: &TurnRequest) -> Vec<Value> {
    let mut options = Vec::new();
    if let Some(model) = req.model.as_deref().filter(|value| !value.is_empty()) {
        options.push(json!({ "id": "model", "value": model }));
    }
    options
}

#[async_trait]
impl AgentAdapter for OpenCodeAcpAdapter {
    fn name(&self) -> &str {
        "opencode"
    }

    async fn start_turn(
        &self,
        req: TurnRequest,
        tx: mpsc::Sender<AgentEvent>,
    ) -> harness_core::error::Result<()> {
        let req = self.effective_turn_request(req);
        crate::spawn_supervisor::validate_capability_token(req.capability_token.as_ref())?;
        let mut state = self.state.lock().await;
        self.ensure_child(&req, &mut state).await?;

        let session_id = state.session_id.clone().ok_or_else(|| {
            harness_core::error::HarnessError::AgentExecution(
                "opencode session/new did not yield a session id".into(),
            )
        })?;

        if let Err(error) = Self::send_request(
            &mut state,
            "session/prompt",
            json!({
                "sessionId": session_id,
                "prompt": [
                    {
                        "type": "text",
                        "text": req.prompt,
                    }
                ],
            }),
        )
        .await
        {
            state.reset_child().await;
            return Err(error);
        }

        let mut lines = state.stdout_lines.take().ok_or_else(|| {
            harness_core::error::HarnessError::AgentExecution(
                "opencode stdout reader not available".into(),
            )
        })?;
        drop(state);

        let mut turn_completed = false;
        let mut receiver_closed = false;
        let mut stdout_closed = false;
        let stall_timeout = stall_timeout_for(&req);
        let read_result = async {
            while let Some(message) =
                Self::read_next_message_with_timeout(&mut lines, stall_timeout, "turn").await?
            {
                match message {
                    ParsedAcpMessage::Response { result, .. } => {
                        // A JSON-RPC error response (e.g. -32602 invalid
                        // params) must fail the turn, not be treated as a
                        // successful completion.
                        if result.get("error").is_some() || result.get("code").is_some() {
                            let message = result
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or("opencode acp request failed")
                                .to_string();
                            if tx.send(AgentEvent::Error { message }).await.is_err() {
                                receiver_closed = true;
                            }
                            turn_completed = true;
                            break;
                        }
                        let stop_reason = result.get("stopReason").and_then(Value::as_str);
                        if stop_reason == Some("cancelled")
                            && tx
                                .send(AgentEvent::Error {
                                    message: "opencode turn cancelled by agent".into(),
                                })
                                .await
                                .is_err()
                        {
                            receiver_closed = true;
                        }
                        turn_completed = true;
                        break;
                    }
                    ParsedAcpMessage::Ignore => {}
                    ParsedAcpMessage::Event(event) => {
                        let is_terminal = matches!(
                            event,
                            AgentEvent::TurnCompleted { .. } | AgentEvent::Error { .. }
                        );
                        if tx.send(event).await.is_err() {
                            receiver_closed = true;
                            break;
                        }
                        if is_terminal {
                            turn_completed = true;
                            break;
                        }
                    }
                }
            }
            Ok(())
        }
        .await;
        if let Err(error) = read_result {
            drop(lines);
            let mut state = self.state.lock().await;
            state.reset_child().await;
            return Err(error);
        }
        if !turn_completed && !receiver_closed {
            stdout_closed = true;
        }

        if stdout_closed {
            drop(lines);
            let mut state = self.state.lock().await;
            state.reset_child().await;
            return Err(harness_core::error::HarnessError::AgentExecution(
                "opencode acp stdout closed before turn/completed".into(),
            ));
        }

        if receiver_closed {
            drop(lines);
            let mut state = self.state.lock().await;
            state.reset_child().await;
            return Err(harness_core::error::HarnessError::AgentExecution(
                "opencode event receiver closed before turn/completed".into(),
            ));
        }
        self.state.lock().await.stdout_lines = Some(lines);
        Ok(())
    }

    async fn interrupt(&self) -> harness_core::error::Result<()> {
        let mut state = self.state.lock().await;
        let Some(session_id) = state.session_id.clone() else {
            return Ok(());
        };
        Self::send_notification(
            &mut state,
            "session/cancel",
            json!({ "sessionId": session_id }),
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
        let request_id = request_id_from_string(&id);
        let result = match decision {
            ApprovalDecision::Accept => json!({ "outcome": "approved" }),
            ApprovalDecision::Reject { reason } => {
                json!({ "outcome": "rejected", "reason": reason })
            }
        };
        let payload = json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "result": result,
        });
        Self::send_json_line(&mut state, &payload).await
    }
}

#[cfg(test)]
#[path = "opencode_adapter_tests.rs"]
mod tests;
