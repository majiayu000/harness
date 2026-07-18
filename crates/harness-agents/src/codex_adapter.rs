use crate::codex::{parse_codex_error_item_message, parse_codex_item, parse_codex_token_usage};
use crate::streaming::capture_agent_stderr_diagnostics;
use async_trait::async_trait;
use harness_core::agent::{AgentAdapter, AgentEvent, ApprovalDecision, TurnRequest};
use harness_core::config::agents::{CodexAgentConfig, CodexCloudConfig, SandboxMode};
use harness_sandbox::SandboxSpec;
use serde_json::{json, Value};
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::ChildStdout;
use tokio::sync::{mpsc, Mutex};

type StdoutLines = Lines<BufReader<ChildStdout>>;
const MAX_PROTOCOL_LINE_PREVIEW: usize = 240;

fn prepare_app_server_spawn(
    cli_path: &std::path::Path,
    req: &TurnRequest,
) -> harness_core::error::Result<crate::spawn_contract::PreparedAgentSpawn> {
    let args = [
        OsString::from("app-server"),
        OsString::from("--listen"),
        OsString::from("stdio://"),
    ];
    let sandbox_mode = req.sandbox_mode.unwrap_or(SandboxMode::DangerFullAccess);
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
    })
}

pub struct CodexAdapter {
    cli_path: PathBuf,
    default_model: String,
    reasoning_effort: String,
    cloud: CodexCloudConfig,
    sandbox_mode: SandboxMode,
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

    fn child_ready(&self) -> bool {
        self.child.is_some() && self.stdin.is_some() && self.stdout_lines.is_some()
    }

    async fn reset_child(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        self.stdin = None;
        self.stdout_lines = None;
        self.thread_id = None;
        self.active_turn_id = None;
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
        let config = CodexAgentConfig {
            cli_path,
            ..CodexAgentConfig::default()
        };
        Self::from_config(config, SandboxMode::DangerFullAccess)
    }

    pub fn from_config(config: CodexAgentConfig, sandbox_mode: SandboxMode) -> Self {
        Self {
            cli_path: config.cli_path,
            default_model: config.default_model,
            reasoning_effort: config.reasoning_effort,
            cloud: config.cloud,
            sandbox_mode,
            state: Arc::new(Mutex::new(AdapterState::new())),
        }
    }

    fn effective_turn_request(&self, mut req: TurnRequest) -> TurnRequest {
        if req.model.is_none() {
            req.model = Some(self.default_model.clone());
        }
        if req.reasoning_effort.is_none() {
            req.reasoning_effort = Some(self.reasoning_effort.clone());
        }
        if req.sandbox_mode.is_none() {
            req.sandbox_mode = Some(self.sandbox_mode);
        }
        let run_identity = crate::resolve_agent_run_identity(&req.env_vars);
        run_identity.write_env_vars(&mut req.env_vars);
        if self.cloud.enabled {
            for key in &self.cloud.setup_secret_env {
                req.env_vars.remove(key);
            }
        }
        req
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
        parse_codex_message(&line).map(Some).ok_or_else(|| {
            harness_core::error::HarnessError::AgentExecution(format!(
                "codex app-server emitted invalid JSON-RPC stdout: {}",
                protocol_line_preview(&line)
            ))
        })
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
                "codex app-server state is incomplete; restarting before starting a new turn"
            );
            state.reset_child().await;
        }

        let run_identity = crate::resolve_agent_run_identity(&req.env_vars);
        let prepared_spawn = prepare_app_server_spawn(&self.cli_path, req)?;
        let mut cmd = tokio::process::Command::new(&prepared_spawn.program);
        cmd.args(&prepared_spawn.args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .current_dir(&prepared_spawn.current_dir)
            .kill_on_drop(true);
        #[cfg(unix)]
        crate::set_process_group(&mut cmd);
        crate::spawn_contract::apply_process_env(&mut cmd, &prepared_spawn);
        crate::strip_claude_env(&mut cmd);
        crate::apply_agent_run_identity_env(&mut cmd, &run_identity);
        if self.cloud.enabled {
            for key in &self.cloud.setup_secret_env {
                cmd.env_remove(key);
            }
        }

        let mut child = cmd.spawn().map_err(|error| {
            let message = crate::classify_missing_workspace_spawn_failure(
                &error,
                &req.project_root,
                format!("failed to spawn codex app-server: {error}"),
            );
            harness_core::error::HarnessError::AgentExecution(message)
        })?;
        if let Some(pid) = child.id() {
            crate::write_provisional_agent_run_binding(
                &run_identity,
                "codex",
                pid,
                &req.project_root,
            );
        }

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

        let thread_id_request =
            Self::send_request(state, "thread/start", thread_start_params(req)).await?;

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
        let req = self.effective_turn_request(req);
        crate::cloud_setup::run_setup_phase(&self.cloud, &req.project_root).await?;
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
            turn_start_params(&req, &thread_id),
        )
        .await?;

        let mut lines = state.stdout_lines.take().ok_or_else(|| {
            harness_core::error::HarnessError::AgentExecution(
                "codex stdout reader not available".into(),
            )
        })?;
        drop(state);

        let mut turn_completed = false;
        let mut receiver_closed = false;
        let mut stdout_closed = false;
        while let Some(message) = Self::read_next_message(&mut lines).await? {
            match message {
                ParsedCodexMessage::TurnStarted { turn_id } => {
                    let mut guard = self.state.lock().await;
                    guard.active_turn_id = Some(turn_id);
                    drop(guard);
                    if tx.send(AgentEvent::TurnStarted).await.is_err() {
                        receiver_closed = true;
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
        if !turn_completed && !receiver_closed {
            stdout_closed = true;
        }

        if stdout_closed {
            drop(lines);
            let mut state = self.state.lock().await;
            state.reset_child().await;
            return Err(harness_core::error::HarnessError::AgentExecution(
                "codex app-server stdout closed before turn/completed".into(),
            ));
        }

        self.state.lock().await.stdout_lines = Some(lines);
        if receiver_closed {
            return Err(harness_core::error::HarnessError::AgentExecution(
                "codex event receiver closed before turn/completed".into(),
            ));
        }
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

fn protocol_line_preview(line: &str) -> String {
    let mut chars = line.chars();
    let mut preview: String = chars.by_ref().take(MAX_PROTOCOL_LINE_PREVIEW).collect();
    if chars.next().is_some() {
        preview.push_str("...");
    }
    preview
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

fn sandbox_mode_value(mode: Option<SandboxMode>) -> Option<String> {
    mode.map(|value| {
        match value {
            SandboxMode::ReadOnly | SandboxMode::ReadOnlyWithNetwork => "read-only",
            SandboxMode::WorkspaceWrite => "workspace-write",
            SandboxMode::DangerFullAccess => "danger-full-access",
        }
        .to_string()
    })
}

fn sandbox_policy_value(mode: Option<SandboxMode>, project_root: &PathBuf) -> Option<Value> {
    mode.map(|value| match value {
        SandboxMode::ReadOnly => json!({ "type": "readOnly" }),
        SandboxMode::ReadOnlyWithNetwork => json!({
            "type": "readOnly",
            "networkAccess": true,
        }),
        SandboxMode::WorkspaceWrite => json!({
            "type": "workspaceWrite",
            "writableRoots": [project_root],
        }),
        SandboxMode::DangerFullAccess => json!({ "type": "dangerFullAccess" }),
    })
}

fn thread_start_params(req: &TurnRequest) -> Value {
    json!({
        "cwd": req.project_root,
        "model": req.model,
        "sandbox": sandbox_mode_value(req.sandbox_mode),
        "approvalPolicy": req.approval_policy,
        "ephemeral": true,
    })
}

fn turn_start_params(req: &TurnRequest, thread_id: &str) -> Value {
    json!({
        "threadId": thread_id,
        "cwd": req.project_root,
        "model": req.model,
        "effort": req.reasoning_effort,
        "sandboxPolicy": sandbox_policy_value(req.sandbox_mode, &req.project_root),
        "approvalPolicy": req.approval_policy,
        "input": [
            {
                "type": "text",
                "text": req.prompt,
            }
        ],
    })
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
            .map(|item| {
                if let Some(message) = parse_codex_error_item_message(item) {
                    ParsedCodexMessage::Event(AgentEvent::Error { message })
                } else {
                    parse_codex_item(item)
                        .map(|item| {
                            ParsedCodexMessage::Event(AgentEvent::ItemCompletedPayload { item })
                        })
                        .unwrap_or(ParsedCodexMessage::Ignore)
                }
            })
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
#[path = "codex_adapter_tests.rs"]
mod tests;
