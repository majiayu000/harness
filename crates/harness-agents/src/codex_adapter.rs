use crate::streaming::capture_agent_stderr_diagnostics;
use async_trait::async_trait;
use harness_core::agent::{AgentAdapter, AgentEvent, ApprovalDecision, TurnRequest};
use harness_core::config::agents::{CodexAgentConfig, CodexCloudConfig, SandboxMode};
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
mod protocol;
/// Parse one Codex app-server JSON-RPC line.
///
/// ```
/// use harness_agents::codex_adapter::parse_codex_message;
///
/// assert!(parse_codex_message(r#"{"method":"custom/unknown","params":{}}"#).is_some());
/// ```
pub use self::protocol::parse_codex_message;
use self::protocol::{
    approval_decision_result, notification_payload, protocol_line_preview, response_id_matches,
    thread_id_from_result, thread_start_params, turn_start_params,
};
#[cfg(test)]
use self::protocol::{sandbox_mode_value, sandbox_policy_value};
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
        // The app-server protocol is driven over the child's stdin.
        forward_stdin: true,
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
    child: Option<crate::ManagedChild>,
    stdin: Option<tokio::process::ChildStdin>,
    stdout_lines: Option<StdoutLines>,
    next_id: u64,
    thread_id: Option<String>,
    active_turn_id: Option<String>,
    child_workspace: Option<PathBuf>,
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
            child_workspace: None,
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
                tracing::warn!("failed to clean up codex app-server child: {error}");
            }
        }
        self.stdin = None;
        self.stdout_lines = None;
        self.thread_id = None;
        self.active_turn_id = None;
        self.child_workspace = None;
    }
}

fn cloud_setup_env_removals(cloud: &CodexCloudConfig) -> Vec<String> {
    if cloud.enabled {
        cloud.setup_secret_env.clone()
    } else {
        Vec::new()
    }
}

fn app_server_stall_timeout(req: &TurnRequest) -> Option<Duration> {
    req.timeout_secs
        .filter(|seconds| *seconds > 0)
        .map(Duration::from_secs)
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

    async fn read_next_message_with_timeout(
        lines: &mut StdoutLines,
        stall_timeout: Option<Duration>,
        phase: &str,
    ) -> harness_core::error::Result<Option<ParsedCodexMessage>> {
        let read = Self::read_next_message(lines);
        let Some(stall_timeout) = stall_timeout else {
            return read.await;
        };
        match tokio::time::timeout(stall_timeout, read).await {
            Ok(result) => result,
            Err(_) => Err(harness_core::error::HarnessError::AgentExecution(format!(
                "codex app-server {phase} stalled for {stall_timeout:?} without stdout"
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
                "codex app-server state is incomplete; restarting before starting a new turn"
            );
            state.reset_child().await;
        }

        let run_identity = crate::resolve_agent_run_identity(&req.env_vars);
        let prepared_spawn = prepare_app_server_spawn(&self.cli_path, req)?;
        let spawn_project_root = req.project_root.clone();
        let supervised = crate::spawn_supervisor::spawn_agent_with_capability(
            crate::spawn_supervisor::AgentSpawnPlan {
                prepared_spawn,
                run_identity,
                native_kind: "codex",
                process_label: "codex app-server",
                stdio: crate::spawn_supervisor::AgentStdio::piped_output(
                    std::process::Stdio::piped(),
                ),
                extra_env_removals: cloud_setup_env_removals(&self.cloud),
                map_spawn_error: Box::new(move |error, _spawn| {
                    let message = crate::classify_missing_workspace_spawn_failure(
                        error,
                        &spawn_project_root,
                        format!("failed to spawn codex app-server: {error}"),
                    );
                    harness_core::error::HarnessError::AgentExecution(message)
                }),
            },
            req.capability_token.as_ref(),
        )
        .await?;
        let child_workspace = supervised.prepared_spawn.child_workspace.clone();
        let mut child = supervised.child;

        if let Some(stderr) = child.inner_mut().stderr.take() {
            tokio::spawn(async move {
                capture_agent_stderr_diagnostics(stderr, "codex", None).await;
            });
        }

        let stdout = child.inner_mut().stdout.take().ok_or_else(|| {
            harness_core::error::HarnessError::AgentExecution(
                "codex app-server stdout unavailable".into(),
            )
        })?;
        state.stdin = child.inner_mut().stdin.take();
        state.stdout_lines = Some(BufReader::new(stdout).lines());
        state.child = Some(child);
        state.child_workspace = Some(child_workspace.clone());
        let stall_timeout = app_server_stall_timeout(req);

        let init_id = match Self::send_request(
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
                "codex stdout reader not available".into(),
            )
        })?;
        let protocol_result = async {
            loop {
                match Self::read_next_message_with_timeout(&mut lines, stall_timeout, "initialize")
                    .await?
                {
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
                thread_start_params(req, &child_workspace),
            )
            .await?;

            loop {
                match Self::read_next_message_with_timeout(
                    &mut lines,
                    stall_timeout,
                    "thread/start",
                )
                .await?
                {
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
        crate::spawn_supervisor::validate_capability_token(req.capability_token.as_ref())?;
        crate::cloud_setup::run_setup_phase(&self.cloud, &req.project_root).await?;
        let mut state = self.state.lock().await;
        self.ensure_child(&req, &mut state).await?;

        let thread_id = state.thread_id.clone().ok_or_else(|| {
            harness_core::error::HarnessError::AgentExecution(
                "codex thread/start did not yield a thread id".into(),
            )
        })?;
        let child_workspace = state.child_workspace.clone().ok_or_else(|| {
            harness_core::error::HarnessError::AgentExecution(
                "codex child workspace unavailable".into(),
            )
        })?;

        if let Err(error) = Self::send_request(
            &mut state,
            "turn/start",
            turn_start_params(&req, &thread_id, &child_workspace),
        )
        .await
        {
            state.reset_child().await;
            return Err(error);
        }

        let mut lines = state.stdout_lines.take().ok_or_else(|| {
            harness_core::error::HarnessError::AgentExecution(
                "codex stdout reader not available".into(),
            )
        })?;
        drop(state);

        let mut turn_completed = false;
        let mut receiver_closed = false;
        let mut stdout_closed = false;
        let stall_timeout = app_server_stall_timeout(&req);
        let read_result = async {
            while let Some(message) =
                Self::read_next_message_with_timeout(&mut lines, stall_timeout, "turn").await?
            {
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

#[cfg(test)]
#[path = "codex_adapter_tests.rs"]
mod tests;
