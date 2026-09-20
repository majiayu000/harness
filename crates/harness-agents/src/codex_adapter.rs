use crate::streaming::capture_agent_stderr_diagnostics;
use async_trait::async_trait;
use harness_core::agent::{
    AgentAdapter, AgentDiagnosticSeverity, AgentEvent, AgentRequest, ApprovalDecision,
};
use harness_core::config::agents::{CodexAgentConfig, CodexCloudConfig, SandboxMode};
use harness_sandbox::SandboxSpec;
use serde_json::{json, Value};
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{ChildStdin, ChildStdout};
use tokio::sync::{mpsc, Mutex, Notify};
use tokio::time::Instant;
type StdoutLines = Lines<BufReader<ChildStdout>>;
const MAX_PROTOCOL_LINE_PREVIEW: usize = 240;
mod attempt;
mod protocol;
use self::attempt::{
    absolute_init_deadline_error, cancelled_error, frame_write_deadline_error,
    overlapping_start_error, stale_generation_error, stop_cleanup_deadline_error,
    wait_until_cancelled, ActiveAttempt, AttemptDeadlines, TurnAttemptGuard,
};
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
async fn prepare_app_server_spawn(
    cli_path: &std::path::Path,
    cloud: &CodexCloudConfig,
    req: &AgentRequest,
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
    let mut spawn_env_vars = req.env_vars.clone();
    let container_bind_mounts =
        crate::cloud_setup::apply_container_state(cloud, &req.project_root, &mut spawn_env_vars)?;
    crate::spawn_contract::prepare_agent_spawn(crate::spawn_contract::AgentSpawnInput {
        program: cli_path,
        args: &args,
        project_root: &req.project_root,
        sandbox_spec: &sandbox_spec,
        env_vars: &spawn_env_vars,
        secret_env_keys: &[],
        container_bind_mounts: &container_bind_mounts,
        permission_mode: req.permission_mode,
        // The app-server protocol is driven over the child's stdin.
        forward_stdin: true,
    })
    .await
}
pub struct CodexAdapter {
    cli_path: PathBuf,
    default_model: String,
    reasoning_effort: String,
    cloud: CodexCloudConfig,
    sandbox_mode: SandboxMode,
    state: Arc<Mutex<AdapterState>>,
    /// Wakes waiters when the current attempt's cancel flag is set. Signalling
    /// does not require holding the lifecycle mutex across protocol I/O.
    cancel_notify: Arc<Notify>,
    deadlines: AttemptDeadlines,
}
struct AdapterState {
    child: Option<crate::ManagedChild>,
    stdin: Option<ChildStdin>,
    stdout_lines: Option<StdoutLines>,
    next_id: u64,
    thread_id: Option<String>,
    /// Remains mirrored from `active_attempt.remote_turn_id` for interrupt/steer.
    active_turn_id: Option<String>,
    child_workspace: Option<PathBuf>,
    spawn_policy_fingerprint: Option<crate::spawn_contract::AdapterSpawnPolicyFingerprint>,
    egress_verified_at_dispatch: bool,
    /// Last failed cleanup outcome. Retained so a later `terminate_and_drain`
    /// cannot report success merely because `child` is now `None`, and so
    /// `ensure_child` cannot spawn a replacement until cleanup is confirmed.
    failed_cleanup: Option<String>,
    next_generation: u64,
    active_attempt: Option<ActiveAttempt>,
    /// Set after a cancelled/timed-out write that may have sent partial bytes.
    /// The session must be terminated; never append another JSON frame.
    protocol_poisoned: bool,
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
            spawn_policy_fingerprint: None,
            egress_verified_at_dispatch: false,
            failed_cleanup: None,
            next_generation: 1,
            active_attempt: None,
            protocol_poisoned: false,
        }
    }
    fn next_request_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }
    fn child_ready(&self) -> bool {
        self.failed_cleanup.is_none()
            && !self.protocol_poisoned
            && self.child.is_some()
            && self.stdin.is_some()
            && self.stdout_lines.is_some()
    }
    /// Whether this adapter session may start or reuse a child process.
    #[cfg(test)]
    fn permits_child_reuse(&self) -> bool {
        self.failed_cleanup.is_none() && !self.protocol_poisoned
    }
    fn clear_protocol_handles(&mut self) {
        self.stdin = None;
        self.stdout_lines = None;
        self.thread_id = None;
        self.active_turn_id = None;
        self.child_workspace = None;
        self.spawn_policy_fingerprint = None;
        self.egress_verified_at_dispatch = false;
        self.protocol_poisoned = false;
    }
    fn begin_attempt(&mut self) -> harness_core::error::Result<u64> {
        if self.active_attempt.is_some() {
            return Err(overlapping_start_error());
        }
        let generation = self.next_generation;
        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        self.active_attempt = Some(ActiveAttempt {
            generation,
            cancel_requested: false,
            remote_turn_id: None,
        });
        self.active_turn_id = None;
        Ok(generation)
    }
    fn clear_attempt_if_current(&mut self, generation: u64) {
        if self
            .active_attempt
            .as_ref()
            .is_some_and(|attempt| attempt.generation == generation)
        {
            self.active_attempt = None;
            self.active_turn_id = None;
        }
    }
    fn generation_is_current(&self, generation: u64) -> bool {
        self.active_attempt
            .as_ref()
            .is_some_and(|attempt| attempt.generation == generation)
    }
    fn cancel_requested_for(&self, generation: u64) -> bool {
        self.active_attempt
            .as_ref()
            .is_some_and(|attempt| attempt.generation == generation && attempt.cancel_requested)
    }
    fn set_remote_turn_id(&mut self, generation: u64, turn_id: String) -> bool {
        let Some(attempt) = self.active_attempt.as_mut() else {
            return false;
        };
        if attempt.generation != generation {
            return false;
        }
        attempt.remote_turn_id = Some(turn_id.clone());
        self.active_turn_id = Some(turn_id);
        true
    }
    /// Terminate the managed child and confirm descendant cleanup.
    ///
    /// `Ok(())` means required process/descendant cleanup succeeded. Protocol
    /// handles are always cleared because they are unusable after terminate.
    /// On cleanup failure the `ManagedChild` owner is retained and the failure
    /// outcome is remembered so retries cannot become false successes.
    async fn reset_child(&mut self) -> harness_core::error::Result<()> {
        self.reset_child_with_deadline(DEFAULT_STOP_CLEANUP_FROM_STATE)
            .await
    }

    async fn reset_child_with_deadline(
        &mut self,
        stop_cleanup: Duration,
    ) -> harness_core::error::Result<()> {
        self.clear_protocol_handles();

        let Some(mut child) = self.child.take() else {
            if let Some(message) = self.failed_cleanup.clone() {
                return Err(harness_core::error::HarnessError::AgentExecution(message));
            }
            return Ok(());
        };

        child.terminate_now();
        match tokio::time::timeout(stop_cleanup, child.wait_and_cleanup_descendants()).await {
            Ok(Ok(_)) => {
                self.failed_cleanup = None;
                Ok(())
            }
            Ok(Err(error)) => {
                let message = format!("failed to clean up codex app-server child: {error}");
                tracing::warn!("{message}");
                self.failed_cleanup = Some(message.clone());
                self.child = Some(child);
                Err(harness_core::error::HarnessError::AgentExecution(message))
            }
            Err(_) => {
                let message = stop_cleanup_deadline_error(stop_cleanup).to_string();
                tracing::warn!("{message}");
                self.failed_cleanup = Some(message.clone());
                self.child = Some(child);
                Err(harness_core::error::HarnessError::AgentExecution(message))
            }
        }
    }
}

/// Fallback when `reset_child` is called without adapter deadlines (tests / Drop).
const DEFAULT_STOP_CLEANUP_FROM_STATE: Duration = attempt::DEFAULT_STOP_CLEANUP_DEADLINE;

fn attach_cleanup_failure(
    primary: harness_core::error::HarnessError,
    cleanup: harness_core::error::HarnessError,
) -> harness_core::error::HarnessError {
    harness_core::error::HarnessError::AgentExecution(format!(
        "{primary}; cleanup failed: {cleanup}"
    ))
}

async fn reset_after_error(
    state: &mut AdapterState,
    primary: harness_core::error::HarnessError,
    stop_cleanup: Duration,
) -> harness_core::error::HarnessError {
    match state.reset_child_with_deadline(stop_cleanup).await {
        Ok(()) => primary,
        Err(cleanup) => attach_cleanup_failure(primary, cleanup),
    }
}

fn cloud_setup_env_removals(cloud: &CodexCloudConfig) -> Vec<String> {
    if cloud.enabled {
        cloud.setup_secret_env.clone()
    } else {
        Vec::new()
    }
}

fn app_server_stall_timeout(req: &AgentRequest) -> Option<Duration> {
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

fn log_codex_diagnostic(severity: AgentDiagnosticSeverity, message: &str) {
    match severity {
        AgentDiagnosticSeverity::Warning => tracing::warn!(agent = "codex", "{message}"),
        AgentDiagnosticSeverity::Error => {
            tracing::error!(agent = "codex", "non-terminal Codex diagnostic: {message}")
        }
    }
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
            cancel_notify: Arc::new(Notify::new()),
            deadlines: AttemptDeadlines::default(),
        }
    }

    /// Test-only private deadline injection (#2095 §3.3).
    #[cfg(test)]
    fn with_deadlines(mut self, deadlines: AttemptDeadlines) -> Self {
        self.deadlines = deadlines;
        self
    }

    fn effective_turn_request(&self, mut req: AgentRequest) -> AgentRequest {
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

    async fn ensure_attempt_not_cancelled(
        &self,
        generation: u64,
    ) -> harness_core::error::Result<()> {
        let state = self.state.lock().await;
        if !state.generation_is_current(generation) {
            return Err(stale_generation_error());
        }
        if state.cancel_requested_for(generation) {
            return Err(cancelled_error());
        }
        Ok(())
    }

    async fn poison_and_reset(
        &self,
        generation: u64,
        primary: harness_core::error::HarnessError,
    ) -> harness_core::error::HarnessError {
        let mut state = self.state.lock().await;
        if state.generation_is_current(generation) {
            state.protocol_poisoned = true;
        }
        reset_after_error(&mut state, primary, self.deadlines.stop_cleanup).await
    }

    async fn send_json_line_for_attempt(
        &self,
        generation: u64,
        payload: &Value,
    ) -> harness_core::error::Result<()> {
        self.ensure_attempt_not_cancelled(generation).await?;

        let mut line = serde_json::to_string(payload).map_err(|error| {
            harness_core::error::HarnessError::AgentExecution(format!(
                "failed to serialize codex payload: {error}"
            ))
        })?;
        line.push('\n');
        let bytes = line.into_bytes();

        let mut stdin = {
            let mut state = self.state.lock().await;
            if !state.generation_is_current(generation) {
                return Err(stale_generation_error());
            }
            if state.protocol_poisoned {
                return Err(harness_core::error::HarnessError::AgentExecution(
                    "codex protocol session is poisoned after a partial write".into(),
                ));
            }
            if state.cancel_requested_for(generation) {
                return Err(cancelled_error());
            }
            state.stdin.take().ok_or_else(|| {
                harness_core::error::HarnessError::AgentExecution(
                    "codex stdin not available".into(),
                )
            })?
        };

        // Lifecycle lock is released during write so interrupt can publish cancel.
        let write = async {
            stdin.write_all(&bytes).await.map_err(|error| {
                harness_core::error::HarnessError::AgentExecution(format!(
                    "failed to write to codex: {error}"
                ))
            })?;
            stdin.flush().await.map_err(|error| {
                harness_core::error::HarnessError::AgentExecution(format!(
                    "failed to flush codex stdin: {error}"
                ))
            })?;
            Ok::<(), harness_core::error::HarnessError>(())
        };

        let write_deadline = self.deadlines.frame_write;
        let outcome = tokio::select! {
            biased;
            _ = wait_until_cancelled(&self.state, &self.cancel_notify, generation) => {
                Err(cancelled_error())
            }
            result = tokio::time::timeout(write_deadline, write) => {
                match result {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(error)) => Err(error),
                    Err(_) => Err(frame_write_deadline_error(write_deadline)),
                }
            }
        };

        match outcome {
            Ok(()) => {
                let mut state = self.state.lock().await;
                if !state.generation_is_current(generation) {
                    drop(stdin);
                    return Err(stale_generation_error());
                }
                state.stdin = Some(stdin);
                Ok(())
            }
            Err(error) => {
                drop(stdin);
                Err(self.poison_and_reset(generation, error).await)
            }
        }
    }

    async fn send_request_for_attempt(
        &self,
        generation: u64,
        method: &str,
        params: Value,
    ) -> harness_core::error::Result<u64> {
        let id = {
            let mut state = self.state.lock().await;
            if !state.generation_is_current(generation) {
                return Err(stale_generation_error());
            }
            state.next_request_id()
        };
        let payload = json!({
            "id": id,
            "method": method,
            "params": params,
        });
        self.send_json_line_for_attempt(generation, &payload)
            .await?;
        Ok(id)
    }

    async fn send_notification_for_attempt(
        &self,
        generation: u64,
        method: &str,
        params: Value,
    ) -> harness_core::error::Result<()> {
        self.send_json_line_for_attempt(generation, &notification_payload(method, params))
            .await
    }

    async fn send_response_unlocked(
        state: &mut AdapterState,
        id: Value,
        result: Value,
    ) -> harness_core::error::Result<()> {
        // Approval responses are rare control replies; keep single-writer ordering
        // by writing while the caller holds the lifecycle lock briefly.
        let payload = json!({
            "id": id,
            "result": result,
        });
        let stdin = state.stdin.as_mut().ok_or_else(|| {
            harness_core::error::HarnessError::AgentExecution("codex stdin not available".into())
        })?;
        let mut line = serde_json::to_string(&payload).map_err(|error| {
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
        if line == crate::spawn_contract::egress::CONTAINER_EGRESS_CANARY_VERIFIED {
            return Ok(Some(ParsedCodexMessage::Event(
                AgentEvent::EgressVerifiedAtDispatch,
            )));
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

    async fn read_next_message_cancellable(
        &self,
        lines: &mut StdoutLines,
        generation: u64,
        stall_timeout: Option<Duration>,
        absolute_deadline: Option<Instant>,
        phase: &str,
    ) -> harness_core::error::Result<Option<ParsedCodexMessage>> {
        self.ensure_attempt_not_cancelled(generation).await?;
        if let Some(deadline) = absolute_deadline {
            if Instant::now() >= deadline {
                return Err(absolute_init_deadline_error(self.deadlines.absolute_init));
            }
        }

        let read = Self::read_next_message_with_timeout(lines, stall_timeout, phase);
        let absolute_wait = async {
            match absolute_deadline {
                Some(deadline) => {
                    tokio::time::sleep_until(deadline).await;
                    Err(absolute_init_deadline_error(self.deadlines.absolute_init))
                }
                None => {
                    std::future::pending::<harness_core::error::Result<Option<ParsedCodexMessage>>>(
                    )
                    .await
                }
            }
        };

        tokio::select! {
            biased;
            _ = wait_until_cancelled(&self.state, &self.cancel_notify, generation) => {
                Err(cancelled_error())
            }
            result = absolute_wait => result,
            result = read => result,
        }
    }

    async fn send_event_cancellable(
        &self,
        tx: &mpsc::Sender<AgentEvent>,
        generation: u64,
        event: AgentEvent,
    ) -> harness_core::error::Result<()> {
        self.ensure_attempt_not_cancelled(generation).await?;
        tokio::select! {
            biased;
            _ = wait_until_cancelled(&self.state, &self.cancel_notify, generation) => {
                Err(cancelled_error())
            }
            result = tx.send(event) => {
                result.map_err(|error| {
                    harness_core::error::HarnessError::AgentExecution(format!(
                        "codex app-server event receiver closed: {error}"
                    ))
                })
            }
        }
    }

    async fn maybe_deliver_pending_interrupt(
        &self,
        generation: u64,
    ) -> harness_core::error::Result<()> {
        let (thread_id, turn_id) = {
            let state = self.state.lock().await;
            if !state.cancel_requested_for(generation) {
                return Ok(());
            }
            let thread_id = state.thread_id.clone();
            let turn_id = state
                .active_attempt
                .as_ref()
                .and_then(|attempt| attempt.remote_turn_id.clone());
            match (thread_id, turn_id) {
                (Some(thread_id), Some(turn_id)) => (thread_id, turn_id),
                _ => return Ok(()),
            }
        };
        let _ = self
            .send_request_for_attempt(
                generation,
                "turn/interrupt",
                json!({
                    "threadId": thread_id,
                    "turnId": turn_id,
                }),
            )
            .await?;
        Ok(())
    }
    async fn ensure_child(
        &self,
        req: &AgentRequest,
        generation: u64,
    ) -> harness_core::error::Result<()> {
        self.ensure_attempt_not_cancelled(generation).await?;
        let requested_fingerprint =
            crate::spawn_contract::adapter_spawn_policy_fingerprint(req, self.sandbox_mode);

        {
            let mut state = self.state.lock().await;
            if !state.generation_is_current(generation) {
                return Err(stale_generation_error());
            }
            if state.child_ready()
                && state.spawn_policy_fingerprint.as_ref() == Some(&requested_fingerprint)
            {
                if let Some(child) = state.child.as_ref() {
                    match child.validate_egress_proxy().await {
                        Ok(()) => return Ok(()),
                        Err(error) => tracing::warn!(
                            "codex app-server egress proxy is unavailable; restarting before starting a new turn: {error}"
                        ),
                    }
                }
            }
            if state.child.is_some() || state.failed_cleanup.is_some() || state.protocol_poisoned {
                tracing::warn!("codex app-server spawn policy changed, cleanup failed, poisoned, or state is incomplete; restarting before starting a new turn");
                state
                    .reset_child_with_deadline(self.deadlines.stop_cleanup)
                    .await?;
            }
        }

        self.ensure_attempt_not_cancelled(generation).await?;
        let run_identity = crate::resolve_agent_run_identity(&req.env_vars);
        let prepared_spawn = {
            let spawn = prepare_app_server_spawn(&self.cli_path, &self.cloud, req);
            tokio::select! {
                biased;
                _ = wait_until_cancelled(&self.state, &self.cancel_notify, generation) => {
                    return Err(cancelled_error());
                }
                result = spawn => result?,
            }
        };
        let spawn_project_root = req.project_root.clone();
        let supervised = {
            let spawn = crate::spawn_supervisor::spawn_agent(
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
            );
            tokio::select! {
                biased;
                _ = wait_until_cancelled(&self.state, &self.cancel_notify, generation) => {
                    return Err(cancelled_error());
                }
                result = spawn => result?,
            }
        };
        let child_workspace = supervised.prepared_spawn.child_workspace.clone();
        let mut child = supervised.child;
        let await_container_egress_canary = child.awaits_container_egress_canary();
        let mut egress_verified_at_dispatch = child.egress_verified_before_spawn();

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
        {
            let mut state = self.state.lock().await;
            if !state.generation_is_current(generation) {
                drop(child);
                drop(stdout);
                return Err(stale_generation_error());
            }
            state.stdin = child.inner_mut().stdin.take();
            state.stdout_lines = Some(BufReader::new(stdout).lines());
            state.child = Some(child);
            state.child_workspace = Some(child_workspace.clone());
        }

        let stall_timeout = app_server_stall_timeout(req);
        let absolute_deadline = Instant::now() + self.deadlines.absolute_init;

        let init_id = match self
            .send_request_for_attempt(
                generation,
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
            Err(error) => return Err(error),
        };

        let mut lines = {
            let mut state = self.state.lock().await;
            if !state.generation_is_current(generation) {
                return Err(stale_generation_error());
            }
            state.stdout_lines.take().ok_or_else(|| {
                harness_core::error::HarnessError::AgentExecution(
                    "codex stdout reader not available".into(),
                )
            })?
        };

        let protocol_result = async {
            loop {
                match self
                    .read_next_message_cancellable(
                        &mut lines,
                        generation,
                        stall_timeout,
                        Some(absolute_deadline),
                        "initialize",
                    )
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
                    Some(ParsedCodexMessage::Event(AgentEvent::Diagnostic {
                        severity,
                        message,
                    })) => {
                        log_codex_diagnostic(severity, &message);
                    }
                    Some(ParsedCodexMessage::Event(AgentEvent::Error { message })) => {
                        return Err(harness_core::error::HarnessError::AgentExecution(message));
                    }
                    Some(ParsedCodexMessage::Event(AgentEvent::EgressVerifiedAtDispatch))
                        if await_container_egress_canary =>
                    {
                        egress_verified_at_dispatch = true;
                    }
                    Some(_) => {}
                    None => {
                        return Err(harness_core::error::HarnessError::AgentExecution(
                            "codex app-server exited during initialize".into(),
                        ));
                    }
                }
            }

            self.send_notification_for_attempt(generation, "initialized", Value::Null)
                .await?;

            let thread_id_request = self
                .send_request_for_attempt(
                    generation,
                    "thread/start",
                    thread_start_params(req, &child_workspace),
                )
                .await?;

            loop {
                match self
                    .read_next_message_cancellable(
                        &mut lines,
                        generation,
                        stall_timeout,
                        Some(absolute_deadline),
                        "thread/start",
                    )
                    .await?
                {
                    Some(ParsedCodexMessage::ThreadStarted { thread_id }) => {
                        let mut state = self.state.lock().await;
                        if !state.generation_is_current(generation) {
                            return Err(stale_generation_error());
                        }
                        state.thread_id = Some(thread_id);
                        break;
                    }
                    Some(ParsedCodexMessage::Response { id, result })
                        if response_id_matches(&id, thread_id_request) =>
                    {
                        if let Some(thread_id) = thread_id_from_result(&result) {
                            let mut state = self.state.lock().await;
                            if !state.generation_is_current(generation) {
                                return Err(stale_generation_error());
                            }
                            state.thread_id = Some(thread_id);
                            break;
                        }
                    }
                    Some(ParsedCodexMessage::Event(AgentEvent::Warning { message })) => {
                        tracing::warn!(agent = "codex", "{message}");
                    }
                    Some(ParsedCodexMessage::Event(AgentEvent::Diagnostic {
                        severity,
                        message,
                    })) => {
                        log_codex_diagnostic(severity, &message);
                    }
                    Some(ParsedCodexMessage::Event(AgentEvent::Error { message })) => {
                        return Err(harness_core::error::HarnessError::AgentExecution(message));
                    }
                    Some(ParsedCodexMessage::Event(AgentEvent::EgressVerifiedAtDispatch))
                        if await_container_egress_canary =>
                    {
                        egress_verified_at_dispatch = true;
                    }
                    Some(_) => {}
                    None => {
                        return Err(harness_core::error::HarnessError::AgentExecution(
                            "codex app-server exited before thread/start completed".into(),
                        ));
                    }
                }
            }
            if await_container_egress_canary && !egress_verified_at_dispatch {
                return Err(harness_core::error::HarnessError::AgentExecution(
                    "codex app-server started before the container egress canary reported success"
                        .into(),
                ));
            }
            Ok(())
        }
        .await;

        match protocol_result {
            Ok(()) => {
                let mut state = self.state.lock().await;
                if !state.generation_is_current(generation) {
                    drop(lines);
                    return Err(stale_generation_error());
                }
                state.stdout_lines = Some(lines);
                state.spawn_policy_fingerprint = Some(requested_fingerprint);
                state.egress_verified_at_dispatch = egress_verified_at_dispatch;
                Ok(())
            }
            Err(error) => {
                drop(lines);
                Err(self.poison_and_reset(generation, error).await)
            }
        }
    }

    async fn clear_active_turn_id(&self) {
        let mut state = self.state.lock().await;
        state.active_turn_id = None;
        if let Some(attempt) = state.active_attempt.as_mut() {
            attempt.remote_turn_id = None;
        }
    }

    async fn finish_attempt_success(
        &self,
        generation: u64,
        lines: StdoutLines,
    ) -> harness_core::error::Result<()> {
        let mut state = self.state.lock().await;
        if !state.generation_is_current(generation) {
            drop(lines);
            return Err(stale_generation_error());
        }
        state.stdout_lines = Some(lines);
        state.clear_attempt_if_current(generation);
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
        req: AgentRequest,
        tx: mpsc::Sender<AgentEvent>,
    ) -> harness_core::error::Result<()> {
        let req = self.effective_turn_request(req);
        crate::spawn_supervisor::validate_capability_token(req.capability_token.as_ref())?;

        let generation = {
            let mut state = self.state.lock().await;
            state.begin_attempt()?
        };
        let attempt_guard =
            TurnAttemptGuard::new(self.state.clone(), self.cancel_notify.clone(), generation);

        let setup = crate::cloud_setup::run_setup_phase(
            &self.cloud,
            crate::cloud_setup::CloudSetupContext {
                project_root: &req.project_root,
                sandbox_mode: req.sandbox_mode.unwrap_or(self.sandbox_mode),
                permission_mode: req.permission_mode,
                env_vars: &req.env_vars,
                capability_token: req.capability_token.as_ref(),
            },
        );
        tokio::select! {
            biased;
            _ = wait_until_cancelled(&self.state, &self.cancel_notify, generation) => {
                let error = self.poison_and_reset(generation, cancelled_error()).await;
                self.state.lock().await.clear_attempt_if_current(generation);
                attempt_guard.disarm();
                return Err(error);
            }
            result = setup => {
                if let Err(error) = result {
                    self.state.lock().await.clear_attempt_if_current(generation);
                    attempt_guard.disarm();
                    return Err(error);
                }
            }
        }

        if let Err(error) = self.ensure_child(&req, generation).await {
            self.state.lock().await.clear_attempt_if_current(generation);
            attempt_guard.disarm();
            return Err(error);
        }

        let (egress_verified, thread_id, child_workspace) = {
            let state = self.state.lock().await;
            if !state.generation_is_current(generation) {
                attempt_guard.disarm();
                return Err(stale_generation_error());
            }
            if state.cancel_requested_for(generation) {
                let error = cancelled_error();
                drop(state);
                let error = self.poison_and_reset(generation, error).await;
                self.state.lock().await.clear_attempt_if_current(generation);
                attempt_guard.disarm();
                return Err(error);
            }
            (
                state.egress_verified_at_dispatch,
                state.thread_id.clone(),
                state.child_workspace.clone(),
            )
        };

        if egress_verified {
            if let Err(error) = self
                .send_event_cancellable(&tx, generation, AgentEvent::EgressVerifiedAtDispatch)
                .await
            {
                let error = self.poison_and_reset(generation, error).await;
                self.state.lock().await.clear_attempt_if_current(generation);
                attempt_guard.disarm();
                return Err(error);
            }
        }

        let thread_id = match thread_id {
            Some(thread_id) => thread_id,
            None => {
                let error = self
                    .poison_and_reset(
                        generation,
                        harness_core::error::HarnessError::AgentExecution(
                            "codex thread/start did not yield a thread id".into(),
                        ),
                    )
                    .await;
                self.state.lock().await.clear_attempt_if_current(generation);
                attempt_guard.disarm();
                return Err(error);
            }
        };
        let child_workspace = match child_workspace {
            Some(child_workspace) => child_workspace,
            None => {
                let error = self
                    .poison_and_reset(
                        generation,
                        harness_core::error::HarnessError::AgentExecution(
                            "codex child workspace unavailable".into(),
                        ),
                    )
                    .await;
                self.state.lock().await.clear_attempt_if_current(generation);
                attempt_guard.disarm();
                return Err(error);
            }
        };

        if let Err(error) = self
            .send_request_for_attempt(
                generation,
                "turn/start",
                turn_start_params(&req, &thread_id, &child_workspace),
            )
            .await
        {
            self.state.lock().await.clear_attempt_if_current(generation);
            attempt_guard.disarm();
            return Err(error);
        }

        let mut lines = {
            let mut state = self.state.lock().await;
            if !state.generation_is_current(generation) {
                attempt_guard.disarm();
                return Err(stale_generation_error());
            }
            state.stdout_lines.take().ok_or_else(|| {
                harness_core::error::HarnessError::AgentExecution(
                    "codex stdout reader not available".into(),
                )
            })?
        };

        let mut turn_completed = false;
        let mut receiver_closed = false;
        let mut stdout_closed = false;
        let stall_timeout = app_server_stall_timeout(&req);
        let read_result = async {
            while let Some(message) = self
                .read_next_message_cancellable(&mut lines, generation, stall_timeout, None, "turn")
                .await?
            {
                match message {
                    ParsedCodexMessage::TurnStarted { turn_id } => {
                        {
                            let mut guard = self.state.lock().await;
                            if !guard.set_remote_turn_id(generation, turn_id) {
                                return Err(stale_generation_error());
                            }
                        }
                        self.maybe_deliver_pending_interrupt(generation).await?;
                        if let Err(error) = self
                            .send_event_cancellable(&tx, generation, AgentEvent::TurnStarted)
                            .await
                        {
                            if matches!(
                                &error,
                                harness_core::error::HarnessError::AgentExecution(message)
                                    if message.contains("event receiver closed")
                            ) {
                                receiver_closed = true;
                                break;
                            }
                            return Err(error);
                        }
                    }
                    ParsedCodexMessage::ThreadStarted { thread_id } => {
                        let mut guard = self.state.lock().await;
                        if !guard.generation_is_current(generation) {
                            return Err(stale_generation_error());
                        }
                        guard.thread_id = Some(thread_id);
                    }
                    ParsedCodexMessage::Response { .. } | ParsedCodexMessage::Ignore => {}
                    ParsedCodexMessage::Event(event) => {
                        let is_terminal = matches!(
                            event,
                            AgentEvent::TurnCompleted { .. }
                                | AgentEvent::TurnCancelled { .. }
                                | AgentEvent::Error { .. }
                        );
                        if is_terminal {
                            self.clear_active_turn_id().await;
                        }
                        if let Err(error) =
                            self.send_event_cancellable(&tx, generation, event).await
                        {
                            if matches!(
                                &error,
                                harness_core::error::HarnessError::AgentExecution(message)
                                    if message.contains("event receiver closed")
                            ) {
                                receiver_closed = true;
                                break;
                            }
                            return Err(error);
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
            let error = if matches!(
                &error,
                harness_core::error::HarnessError::AgentExecution(message)
                    if message.contains("cancelled before completion")
            ) {
                // Cancelled attempts stop via supervisor when interrupt cannot finish
                // within the stop budget; terminate retains cleanup ownership.
                let stop = async {
                    let mut state = self.state.lock().await;
                    state
                        .reset_child_with_deadline(self.deadlines.stop_cleanup)
                        .await
                };
                match tokio::time::timeout(self.deadlines.stop_cleanup, stop).await {
                    Ok(Ok(())) => error,
                    Ok(Err(cleanup)) => attach_cleanup_failure(error, cleanup),
                    Err(_) => attach_cleanup_failure(
                        error,
                        stop_cleanup_deadline_error(self.deadlines.stop_cleanup),
                    ),
                }
            } else {
                self.poison_and_reset(generation, error).await
            };
            self.state.lock().await.clear_attempt_if_current(generation);
            attempt_guard.disarm();
            return Err(error);
        }
        if !turn_completed && !receiver_closed {
            stdout_closed = true;
        }

        if stdout_closed {
            drop(lines);
            let error = self
                .poison_and_reset(
                    generation,
                    harness_core::error::HarnessError::AgentExecution(
                        "codex app-server stdout closed before turn/completed".into(),
                    ),
                )
                .await;
            self.state.lock().await.clear_attempt_if_current(generation);
            attempt_guard.disarm();
            return Err(error);
        }

        if receiver_closed {
            drop(lines);
            let error = self
                .poison_and_reset(
                    generation,
                    harness_core::error::HarnessError::AgentExecution(
                        "codex event receiver closed before turn/completed".into(),
                    ),
                )
                .await;
            self.state.lock().await.clear_attempt_if_current(generation);
            attempt_guard.disarm();
            return Err(error);
        }

        let result = self.finish_attempt_success(generation, lines).await;
        attempt_guard.disarm();
        result
    }

    async fn interrupt(&self) -> harness_core::error::Result<()> {
        let (generation, thread_id, turn_id) = {
            let mut state = self.state.lock().await;
            let (generation, turn_id) = match state.active_attempt.as_mut() {
                Some(attempt) => {
                    attempt.cancel_requested = true;
                    (attempt.generation, attempt.remote_turn_id.clone())
                }
                None => {
                    // Idle interrupt must not poison a future attempt (#2095 invariant 7).
                    return Ok(());
                }
            };
            let thread_id = state.thread_id.clone();
            self.cancel_notify.notify_waiters();
            (generation, thread_id, turn_id)
        };

        if let (Some(thread_id), Some(turn_id)) = (thread_id, turn_id) {
            // Best-effort interrupt delivery without holding the lifecycle lock.
            let _ = self
                .send_request_for_attempt(
                    generation,
                    "turn/interrupt",
                    json!({
                        "threadId": thread_id,
                        "turnId": turn_id,
                    }),
                )
                .await;
        }
        Ok(())
    }

    async fn terminate_and_drain(&self) -> harness_core::error::Result<()> {
        let mut state = self.state.lock().await;
        if let Some(attempt) = state.active_attempt.as_mut() {
            attempt.cancel_requested = true;
            self.cancel_notify.notify_waiters();
        }
        let result = state
            .reset_child_with_deadline(self.deadlines.stop_cleanup)
            .await;
        state.active_attempt = None;
        result
    }

    async fn steer(&self, text: String) -> harness_core::error::Result<()> {
        let generation = {
            let state = self.state.lock().await;
            state
                .active_attempt
                .as_ref()
                .map(|attempt| attempt.generation)
                .ok_or_else(|| {
                    harness_core::error::HarnessError::AgentExecution(
                        "codex active turn unavailable".into(),
                    )
                })?
        };
        let (thread_id, turn_id) = {
            let state = self.state.lock().await;
            let thread_id = state.thread_id.clone().ok_or_else(|| {
                harness_core::error::HarnessError::AgentExecution(
                    "codex thread id unavailable".into(),
                )
            })?;
            let turn_id = state.active_turn_id.clone().ok_or_else(|| {
                harness_core::error::HarnessError::AgentExecution(
                    "codex active turn unavailable".into(),
                )
            })?;
            (thread_id, turn_id)
        };
        self.send_request_for_attempt(
            generation,
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
        Self::send_response_unlocked(&mut state, request_id, result).await
    }
}

#[cfg(test)]
#[path = "codex_adapter_tests.rs"]
mod tests;

#[cfg(all(test, unix))]
mod spawn_policy_tests {
    use super::*;
    use std::collections::HashMap;

    #[tokio::test]
    async fn ready_child_restarts_when_spawn_policy_changes() -> anyhow::Result<()> {
        let project = tempfile::tempdir()?;
        let adapter = CodexAdapter::new(project.path().join("missing-codex"));
        let request = AgentRequest {
            prompt: "ping".to_string(),
            prompt_layers: None,
            project_root: project.path().to_path_buf(),
            permission_mode: harness_core::config::agents::AgentPermissionMode::Full,
            model: None,
            reasoning_effort: None,
            execution_phase: None,
            sandbox_mode: Some(SandboxMode::DangerFullAccess),
            approval_policy: None,
            allowed_tools: None,
            max_budget_usd: None,
            context: Vec::new(),
            timeout_secs: None,
            env_vars: HashMap::new(),
            capability_token: None,
        };
        let mut command = tokio::process::Command::new("sleep");
        command
            .arg("60")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true);
        crate::set_process_group(&mut command);
        let mut child = command.spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("missing stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("missing stdout"))?;
        let generation = {
            let mut state = adapter.state.lock().await;
            let generation = state.begin_attempt()?;
            state.stdin = Some(stdin);
            state.stdout_lines = Some(BufReader::new(stdout).lines());
            state.child = Some(crate::ManagedChild::new(child, "codex policy test"));
            state.spawn_policy_fingerprint =
                Some(crate::spawn_contract::adapter_spawn_policy_fingerprint(
                    &request,
                    adapter.sandbox_mode,
                ));
            generation
        };

        adapter.ensure_child(&request, generation).await?;
        let mut scoped = request;
        scoped.permission_mode = harness_core::config::agents::AgentPermissionMode::Scoped;
        adapter
            .ensure_child(&scoped, generation)
            .await
            .expect_err("changed policy must attempt a fresh spawn");

        let state = adapter.state.lock().await;
        assert!(state.child.is_none());
        assert!(state.spawn_policy_fingerprint.is_none());
        Ok(())
    }
}

#[cfg(test)]
#[path = "codex_adapter_receiver_tests.rs"]
mod receiver_tests;
