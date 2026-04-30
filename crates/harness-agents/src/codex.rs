use crate::cloud_setup;
use crate::streaming::{
    capture_agent_stderr_diagnostics, captured_stderr_tail, enrich_stream_exit_error,
    log_captured_stderr_diagnostics, send_stream_item,
};
use async_trait::async_trait;
use harness_core::agent::{AgentRequest, AgentResponse, CodeAgent, StreamItem};
use harness_core::config::agents::SandboxMode;
use harness_core::config::agents::{CodexAgentConfig, CodexCloudConfig};
use harness_core::types::{Capability, Item, TokenUsage};
use harness_sandbox::{wrap_command, SandboxSpec};
use serde_json::Value;
use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

pub struct CodexAgent {
    pub cli_path: PathBuf,
    pub default_model: String,
    pub reasoning_effort: String,
    pub cloud: CodexCloudConfig,
    pub sandbox_mode: SandboxMode,
    /// Maximum seconds of idle silence on the output stream before the
    /// subprocess is declared a zombie and terminated. `None` = no timeout.
    pub stream_timeout_secs: Option<u64>,
}

impl CodexAgent {
    pub fn new(cli_path: PathBuf, sandbox_mode: SandboxMode) -> Self {
        Self::with_cloud(cli_path, CodexCloudConfig::default(), sandbox_mode)
    }

    pub fn with_cloud(
        cli_path: PathBuf,
        cloud: CodexCloudConfig,
        sandbox_mode: SandboxMode,
    ) -> Self {
        Self {
            cli_path,
            default_model: "gpt-5.4".to_string(),
            reasoning_effort: "high".to_string(),
            cloud,
            sandbox_mode,
            stream_timeout_secs: Some(3600),
        }
    }

    pub fn from_config(config: CodexAgentConfig, sandbox_mode: SandboxMode) -> Self {
        let mut agent = Self::with_cloud(config.cli_path, config.cloud, sandbox_mode);
        agent.default_model = config.default_model;
        agent.reasoning_effort = config.reasoning_effort;
        agent
    }

    /// Set the per-line idle timeout for stream zombie detection.
    pub fn with_stream_timeout(mut self, secs: Option<u64>) -> Self {
        self.stream_timeout_secs = secs;
        self
    }

    async fn run_setup_phase(&self, project_root: &Path) -> harness_core::error::Result<()> {
        cloud_setup::run_setup_phase(&self.cloud, project_root).await
    }

    fn effective_reasoning_effort<'a>(&'a self, req: &'a AgentRequest) -> &'a str {
        req.reasoning_effort
            .as_deref()
            .unwrap_or(&self.reasoning_effort)
    }

    fn effective_sandbox_mode(&self, req: &AgentRequest) -> SandboxMode {
        req.sandbox_mode.unwrap_or(self.sandbox_mode)
    }

    fn base_args(&self, req: &AgentRequest) -> Vec<OsString> {
        let model = req.model.as_deref().unwrap_or(&self.default_model);
        let reasoning_effort = self.effective_reasoning_effort(req);
        let sandbox_mode = self.effective_sandbox_mode(req);
        let mut args = vec![
            OsString::from("exec"),
            OsString::from("--skip-git-repo-check"),
            OsString::from("--json"),
            OsString::from("--color"),
            OsString::from("never"),
            OsString::from("-m"),
            OsString::from(model),
            OsString::from("-c"),
            OsString::from(format!("model_reasoning_effort=\"{}\"", reasoning_effort)),
            OsString::from("-s"),
            OsString::from(codex_sandbox_mode(sandbox_mode)),
        ];

        if self.cloud.enabled {
            args.push(OsString::from("--ephemeral"));
        }

        args.push(OsString::from("-C"));
        args.push(req.project_root.as_os_str().to_os_string());
        args.push(OsString::from(req.prompt.clone()));
        args
    }
}

#[derive(Debug)]
enum ParsedCodexExecEvent {
    MessageDelta { item_id: String, text: String },
    ToolOutputDelta { item_id: String, text: String },
    ItemStarted { item: Item },
    ItemCompleted { item_id: String, item: Item },
    TokenUsage { usage: TokenUsage },
    Warning { message: String },
    Error { message: String },
    Ignore,
}

#[derive(Debug, Default)]
struct ParsedCodexExecOutput {
    output: String,
    items: Vec<Item>,
    token_usage: TokenUsage,
    warnings: Vec<String>,
    structured_error: Option<String>,
}

fn json_str_field<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(|field| field.as_str()))
}

pub(crate) fn parse_codex_item(item: &Value) -> Option<Item> {
    match json_str_field(item, &["type"])? {
        "agent_message" | "agentMessage" => Some(Item::AgentReasoning {
            content: json_str_field(item, &["text"])?.to_string(),
        }),
        "command_execution" | "commandExecution" => Some(Item::ShellCommand {
            command: json_str_field(item, &["command"])?.to_string(),
            exit_code: item
                .get("exit_code")
                .or_else(|| item.get("exitCode"))
                .and_then(|field| field.as_i64())
                .and_then(|code| i32::try_from(code).ok()),
            stdout: json_str_field(item, &["aggregated_output", "aggregatedOutput"])
                .unwrap_or_default()
                .to_string(),
            stderr: String::new(),
        }),
        _ => None,
    }
}

pub(crate) fn parse_codex_token_usage(usage: &Value) -> Option<TokenUsage> {
    let input_tokens = usage
        .get("input_tokens")
        .or_else(|| usage.get("inputTokens"))
        .and_then(|field| field.as_u64())?;
    let output_tokens = usage
        .get("output_tokens")
        .or_else(|| usage.get("outputTokens"))
        .and_then(|field| field.as_u64())?;
    let total_tokens = usage
        .get("total_tokens")
        .or_else(|| usage.get("totalTokens"))
        .and_then(|field| field.as_u64())
        .unwrap_or(input_tokens.saturating_add(output_tokens));

    Some(TokenUsage {
        input_tokens,
        output_tokens,
        total_tokens,
        cost_usd: 0.0,
    })
}

fn parse_codex_exec_event_line(line: &str) -> Option<ParsedCodexExecEvent> {
    let value: Value = serde_json::from_str(line).ok()?;
    let event_type = json_str_field(&value, &["type"])?;

    match event_type {
        "thread.started" | "turn.started" => Some(ParsedCodexExecEvent::Ignore),
        "warning" => Some(ParsedCodexExecEvent::Warning {
            message: json_str_field(&value, &["message"])
                .or_else(|| value.get("warning").and_then(Value::as_str))
                .unwrap_or("unknown warning")
                .to_string(),
        }),
        "error" => Some(ParsedCodexExecEvent::Error {
            message: json_str_field(&value, &["message"])
                .or_else(|| {
                    value
                        .get("error")
                        .and_then(|error| json_str_field(error, &["message"]))
                })
                .unwrap_or("unknown error")
                .to_string(),
        }),
        "turn.completed" => value
            .get("usage")
            .and_then(parse_codex_token_usage)
            .map(|usage| ParsedCodexExecEvent::TokenUsage { usage })
            .or(Some(ParsedCodexExecEvent::Ignore)),
        "item.started" | "item.completed" => {
            let item_value = value.get("item")?;
            let item_id = json_str_field(item_value, &["id"])?.to_string();
            let item = parse_codex_item(item_value)?;
            if event_type == "item.started" {
                let _ = item_id;
                Some(ParsedCodexExecEvent::ItemStarted { item })
            } else {
                Some(ParsedCodexExecEvent::ItemCompleted { item_id, item })
            }
        }
        "item.delta" | "item/agentMessage/delta" | "item.agent_message.delta" => {
            Some(ParsedCodexExecEvent::MessageDelta {
                item_id: json_str_field(&value, &["item_id", "itemId"])?.to_string(),
                text: json_str_field(&value, &["delta", "text"])?.to_string(),
            })
        }
        "item/commandExecution/outputDelta"
        | "item.command_execution.output_delta"
        | "item.command_output_delta" => Some(ParsedCodexExecEvent::ToolOutputDelta {
            item_id: json_str_field(&value, &["item_id", "itemId"])?.to_string(),
            text: json_str_field(&value, &["delta", "text"])?.to_string(),
        }),
        _ => Some(ParsedCodexExecEvent::Ignore),
    }
}

fn apply_codex_exec_event(
    parsed: &mut ParsedCodexExecOutput,
    seen_message_deltas: &mut HashSet<String>,
    event: ParsedCodexExecEvent,
    emitted_items: &mut Vec<StreamItem>,
) {
    match event {
        ParsedCodexExecEvent::MessageDelta { item_id, text } => {
            seen_message_deltas.insert(item_id);
            parsed.output.push_str(&text);
            emitted_items.push(StreamItem::MessageDelta { text });
        }
        ParsedCodexExecEvent::ToolOutputDelta { item_id, text } => {
            emitted_items.push(StreamItem::ToolOutputDelta { item_id, text });
        }
        ParsedCodexExecEvent::ItemStarted { item } => {
            emitted_items.push(StreamItem::ItemStarted { item });
        }
        ParsedCodexExecEvent::ItemCompleted { item_id, item } => {
            if let Item::AgentReasoning { content } = &item {
                if !seen_message_deltas.contains(&item_id) {
                    parsed.output.push_str(content);
                    emitted_items.push(StreamItem::MessageDelta {
                        text: content.clone(),
                    });
                }
            } else {
                parsed.items.push(item.clone());
            }
            emitted_items.push(StreamItem::ItemCompleted { item });
        }
        ParsedCodexExecEvent::TokenUsage { usage } => {
            parsed.token_usage = usage.clone();
            emitted_items.push(StreamItem::TokenUsage { usage });
        }
        ParsedCodexExecEvent::Warning { message } => {
            parsed.warnings.push(message.clone());
            emitted_items.push(StreamItem::Warning { message });
        }
        ParsedCodexExecEvent::Error { message } => {
            parsed.structured_error = Some(message.clone());
            emitted_items.push(StreamItem::Error { message });
        }
        ParsedCodexExecEvent::Ignore => {}
    }
}

fn parse_codex_exec_output(stdout: &str) -> harness_core::error::Result<ParsedCodexExecOutput> {
    let mut parsed = ParsedCodexExecOutput::default();
    let mut seen_message_deltas = HashSet::new();

    for line in stdout.lines() {
        let event = parse_codex_exec_event_line(line).ok_or_else(|| {
            harness_core::error::HarnessError::AgentExecution(format!(
                "failed to parse codex json line: {line}"
            ))
        })?;
        let mut ignored = Vec::new();
        apply_codex_exec_event(&mut parsed, &mut seen_message_deltas, event, &mut ignored);
    }

    Ok(parsed)
}

async fn stream_codex_exec_output(
    child: &mut tokio::process::Child,
    tx: &tokio::sync::mpsc::Sender<StreamItem>,
    idle_timeout: Option<Duration>,
) -> harness_core::error::Result<ParsedCodexExecOutput> {
    let stdout = child.stdout.take().ok_or_else(|| {
        harness_core::error::HarnessError::AgentExecution("codex stdout unavailable".into())
    })?;
    let mut lines = BufReader::new(stdout).lines();
    let mut parsed = ParsedCodexExecOutput::default();
    let mut seen_message_deltas = HashSet::new();

    loop {
        let maybe_line = if let Some(duration) = idle_timeout {
            tokio::time::timeout(duration, lines.next_line())
                .await
                .map_err(|_| {
                    #[cfg(unix)]
                    crate::kill_process_group(child);
                    harness_core::error::HarnessError::AgentExecution(format!(
                        "codex stream idle timeout after {}s: zombie connection terminated",
                        duration.as_secs()
                    ))
                })?
                .map_err(|error| {
                    harness_core::error::HarnessError::AgentExecution(format!(
                        "failed reading codex stdout: {error}"
                    ))
                })?
        } else {
            lines.next_line().await.map_err(|error| {
                harness_core::error::HarnessError::AgentExecution(format!(
                    "failed reading codex stdout: {error}"
                ))
            })?
        };
        let Some(line) = maybe_line else {
            break;
        };
        let event = parse_codex_exec_event_line(&line).ok_or_else(|| {
            harness_core::error::HarnessError::AgentExecution(format!(
                "failed to parse codex json line: {line}"
            ))
        })?;
        let mut emitted_items = Vec::new();
        apply_codex_exec_event(
            &mut parsed,
            &mut seen_message_deltas,
            event,
            &mut emitted_items,
        );
        for item in emitted_items {
            let item_label = match &item {
                StreamItem::ItemStarted { .. } => "item_started",
                StreamItem::MessageDelta { .. } => "message_delta",
                StreamItem::ToolOutputDelta { .. } => "tool_output_delta",
                StreamItem::ItemCompleted { .. } => "item_completed",
                StreamItem::TokenUsage { .. } => "token_usage",
                StreamItem::Warning { .. } => "warning",
                StreamItem::Error { .. } => "error",
                StreamItem::ApprovalRequest { .. } => "approval_request",
                StreamItem::Done => "done",
            };
            send_stream_item(tx, item, "codex", item_label).await?;
        }
    }

    Ok(parsed)
}

#[derive(Debug)]
struct ProgramSpawnDiagnostics {
    resolved_path: Option<PathBuf>,
    exists: bool,
    executable: Option<bool>,
}

fn resolve_program_for_spawn(program: &Path, current_dir: &Path) -> Option<PathBuf> {
    if program.components().count() == 1 {
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path)
            .map(|dir| dir.join(program))
            .find(|candidate| candidate.is_file())
    } else if program.is_absolute() && program.exists() {
        Some(program.to_path_buf())
    } else {
        let candidate = current_dir.join(program);
        candidate.exists().then_some(candidate)
    }
}

#[cfg(unix)]
fn is_executable(path: &Path) -> Option<bool> {
    use std::os::unix::fs::PermissionsExt;

    let metadata = std::fs::metadata(path).ok()?;
    Some(metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> Option<bool> {
    let metadata = std::fs::metadata(path).ok()?;
    Some(metadata.is_file())
}

fn program_spawn_diagnostics(program: &Path, current_dir: &Path) -> ProgramSpawnDiagnostics {
    let resolved_path = resolve_program_for_spawn(program, current_dir);
    let executable = resolved_path.as_deref().and_then(is_executable);
    ProgramSpawnDiagnostics {
        exists: resolved_path.is_some(),
        resolved_path,
        executable,
    }
}

fn log_codex_spawn_attempt(
    program: &Path,
    arg_count: usize,
    req: &AgentRequest,
    engine: harness_sandbox::SandboxEngine,
    mode: &'static str,
) {
    let program_diag = program_spawn_diagnostics(program, &req.project_root);
    let current_dir_exists = req.project_root.exists();
    let current_dir_is_dir = req.project_root.is_dir();
    tracing::debug!(
        agent = "codex",
        mode,
        program = %program.display(),
        program_resolved = program_diag
            .resolved_path
            .as_ref()
            .map(|p| p.display().to_string())
            .as_deref()
            .unwrap_or("<unresolved>"),
        program_exists = program_diag.exists,
        program_executable = ?program_diag.executable,
        current_dir = %req.project_root.display(),
        current_dir_exists,
        current_dir_is_dir,
        phase = ?req.execution_phase,
        sandbox_engine = ?engine,
        arg_count,
        prompt_len = req.prompt.len(),
        env_var_count = req.env_vars.len(),
        "codex spawn prepared"
    );
}

fn codex_spawn_failure_message(
    error: &std::io::Error,
    program: &Path,
    req: &AgentRequest,
    engine: harness_sandbox::SandboxEngine,
    mode: &'static str,
) -> String {
    let program_diag = program_spawn_diagnostics(program, &req.project_root);
    let current_dir_exists = req.project_root.exists();
    let current_dir_is_dir = req.project_root.is_dir();
    let resolved_program = program_diag
        .resolved_path
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "<unresolved>".to_string());

    format!(
        "failed to run codex: {error}; mode={mode}; phase={:?}; program={}; \
         program_resolved={resolved_program}; program_exists={}; program_executable={:?}; \
         current_dir={}; current_dir_exists={current_dir_exists}; \
         current_dir_is_dir={current_dir_is_dir}; sandbox_engine={engine:?}; \
         prompt_len={}; env_var_count={}",
        req.execution_phase,
        program.display(),
        program_diag.exists,
        program_diag.executable,
        req.project_root.display(),
        req.prompt.len(),
        req.env_vars.len(),
    )
}

#[async_trait]
impl CodeAgent for CodexAgent {
    fn name(&self) -> &str {
        "codex"
    }

    fn capabilities(&self) -> Vec<Capability> {
        vec![Capability::Read, Capability::Write, Capability::Execute]
    }

    async fn execute(&self, req: AgentRequest) -> harness_core::error::Result<AgentResponse> {
        if let Some(ref token) = req.capability_token {
            if token.is_expired() {
                return Err(harness_core::error::HarnessError::AgentExecution(format!(
                    "capability token for subtask {} has expired",
                    token.subtask_index
                )));
            }
        }

        self.run_setup_phase(&req.project_root).await?;

        let base_args = self.base_args(&req);
        let sandbox_mode = self.effective_sandbox_mode(&req);
        let sandbox_spec = if let Some(ref token) = req.capability_token {
            SandboxSpec::new(sandbox_mode, &req.project_root)
                .with_allowed_write_paths(token.allowed_write_paths.clone())
        } else {
            SandboxSpec::new(sandbox_mode, &req.project_root)
        };
        let wrapped_command =
            wrap_command(&self.cli_path, &base_args, &sandbox_spec).map_err(|error| {
                harness_core::error::HarnessError::AgentExecution(format!(
                    "sandbox setup failed for codex: {error}"
                ))
            })?;

        let mut cmd = Command::new(&wrapped_command.program);
        cmd.args(&wrapped_command.args)
            .current_dir(&req.project_root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        crate::set_process_group(&mut cmd);
        crate::strip_claude_env(&mut cmd);
        cmd.envs(&req.env_vars);

        if self.cloud.enabled {
            for key in &self.cloud.setup_secret_env {
                cmd.env_remove(key);
            }
        }

        log_codex_spawn_attempt(
            &wrapped_command.program,
            wrapped_command.args.len(),
            &req,
            wrapped_command.engine,
            "execute",
        );
        let child = cmd.spawn().map_err(|err| {
            let message = codex_spawn_failure_message(
                &err,
                &wrapped_command.program,
                &req,
                wrapped_command.engine,
                "execute",
            );
            tracing::error!(
                agent = "codex",
                mode = "execute",
                error_kind = ?err.kind(),
                "{message}"
            );
            harness_core::error::HarnessError::AgentExecution(message)
        })?;
        let output = child.wait_with_output().await.map_err(|err| {
            harness_core::error::HarnessError::AgentExecution(format!(
                "failed to wait for codex: {err}"
            ))
        })?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        log_captured_stderr_diagnostics(&stderr, self.name());

        if !output.status.success() {
            if harness_core::error::is_billing_failure_message(&stderr) {
                return Err(harness_core::error::HarnessError::BillingFailed(format!(
                    "codex billing failure (exit {}): {stderr}",
                    output.status
                )));
            }
            if harness_core::error::is_quota_failure_message(&stderr) {
                return Err(harness_core::error::HarnessError::QuotaExhausted(format!(
                    "codex quota exhausted (exit {}): {stderr}",
                    output.status
                )));
            }
            return Err(harness_core::error::HarnessError::AgentExecution(format!(
                "codex exited with {}: {stderr}",
                output.status
            )));
        }

        let parsed = parse_codex_exec_output(&stdout)?;
        if let Some(message) = parsed.structured_error {
            return Err(harness_core::error::HarnessError::AgentExecution(message));
        }
        for warning in &parsed.warnings {
            tracing::warn!(agent = self.name(), "{warning}");
        }

        Ok(AgentResponse {
            output: parsed.output,
            stderr,
            items: parsed.items,
            token_usage: parsed.token_usage,
            model: "codex".to_string(),
            exit_code: output.status.code(),
        })
    }

    async fn execute_stream(
        &self,
        req: AgentRequest,
        tx: tokio::sync::mpsc::Sender<StreamItem>,
    ) -> harness_core::error::Result<()> {
        if let Some(ref token) = req.capability_token {
            if token.is_expired() {
                return Err(harness_core::error::HarnessError::AgentExecution(format!(
                    "capability token for subtask {} has expired",
                    token.subtask_index
                )));
            }
        }

        self.run_setup_phase(&req.project_root).await?;

        let base_args = self.base_args(&req);
        let sandbox_mode = self.effective_sandbox_mode(&req);
        let sandbox_spec = if let Some(ref token) = req.capability_token {
            SandboxSpec::new(sandbox_mode, &req.project_root)
                .with_allowed_write_paths(token.allowed_write_paths.clone())
        } else {
            SandboxSpec::new(sandbox_mode, &req.project_root)
        };
        let wrapped_command =
            wrap_command(&self.cli_path, &base_args, &sandbox_spec).map_err(|error| {
                harness_core::error::HarnessError::AgentExecution(format!(
                    "sandbox setup failed for codex: {error}"
                ))
            })?;

        let mut cmd = Command::new(&wrapped_command.program);
        cmd.args(&wrapped_command.args)
            .current_dir(&req.project_root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        crate::set_process_group(&mut cmd);
        crate::strip_claude_env(&mut cmd);
        cmd.envs(&req.env_vars);

        if self.cloud.enabled {
            for key in &self.cloud.setup_secret_env {
                cmd.env_remove(key);
            }
        }

        log_codex_spawn_attempt(
            &wrapped_command.program,
            wrapped_command.args.len(),
            &req,
            wrapped_command.engine,
            "execute_stream",
        );
        let mut child = cmd.spawn().map_err(|error| {
            let message = codex_spawn_failure_message(
                &error,
                &wrapped_command.program,
                &req,
                wrapped_command.engine,
                "execute_stream",
            );
            tracing::error!(
                agent = "codex",
                mode = "execute_stream",
                error_kind = ?error.kind(),
                "{message}"
            );
            harness_core::error::HarnessError::AgentExecution(message)
        })?;

        let stderr_capture = Arc::new(Mutex::new(String::new()));
        let mut stderr_task = None;
        if let Some(stderr) = child.stderr.take() {
            let agent = self.name().to_string();
            let captured = Arc::clone(&stderr_capture);
            stderr_task = Some(tokio::spawn(async move {
                capture_agent_stderr_diagnostics(stderr, &agent, Some(captured)).await;
            }));
        }

        let idle_timeout = self
            .stream_timeout_secs
            .filter(|&s| s > 0)
            .map(std::time::Duration::from_secs);
        let stream_result = stream_codex_exec_output(&mut child, &tx, idle_timeout).await;
        let stream_send_failed = matches!(
            &stream_result,
            Err(harness_core::error::HarnessError::AgentExecution(message))
                if message.contains("stream send failed")
        );
        if stream_result.is_err() {
            #[cfg(unix)]
            crate::kill_process_group(&child);
            let _ = child.start_kill();
        }
        if stream_send_failed {
            return Err(stream_result.expect_err("stream send failures return an error"));
        }
        if let Some(stderr_task) = stderr_task {
            let _ = stderr_task.await;
        }
        let parsed = match stream_result {
            Ok(parsed) => parsed,
            Err(error) => {
                let stderr = captured_stderr_tail(&stderr_capture);
                if !stderr.is_empty() {
                    if harness_core::error::is_billing_failure_message(&stderr) {
                        return Err(harness_core::error::HarnessError::BillingFailed(format!(
                            "codex billing failure (streamed exit): {stderr}"
                        )));
                    }
                    if harness_core::error::is_quota_failure_message(&stderr) {
                        return Err(harness_core::error::HarnessError::QuotaExhausted(format!(
                            "codex quota exhausted (streamed exit): {stderr}"
                        )));
                    }
                }
                return Err(enrich_stream_exit_error(error, &stderr));
            }
        };
        let status = child.wait().await.map_err(|error| {
            harness_core::error::HarnessError::AgentExecution(format!(
                "failed waiting for codex process: {error}"
            ))
        })?;
        if !status.success() {
            let stderr = captured_stderr_tail(&stderr_capture);
            if !stderr.is_empty() {
                if harness_core::error::is_billing_failure_message(&stderr) {
                    return Err(harness_core::error::HarnessError::BillingFailed(format!(
                        "codex billing failure (streamed exit): {stderr}"
                    )));
                }
                if harness_core::error::is_quota_failure_message(&stderr) {
                    return Err(harness_core::error::HarnessError::QuotaExhausted(format!(
                        "codex quota exhausted (streamed exit): {stderr}"
                    )));
                }
            }
            return Err(enrich_stream_exit_error(
                harness_core::error::HarnessError::AgentExecution(format!(
                    "codex exited with {status}"
                )),
                &stderr,
            ));
        }
        if let Some(message) = parsed.structured_error {
            return Err(harness_core::error::HarnessError::AgentExecution(message));
        }
        send_stream_item(&tx, StreamItem::Done, self.name(), "done").await?;
        Ok(())
    }
}

fn codex_sandbox_mode(mode: SandboxMode) -> &'static str {
    match mode {
        SandboxMode::ReadOnly => "read-only",
        SandboxMode::WorkspaceWrite => "workspace-write",
        SandboxMode::DangerFullAccess => "danger-full-access",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::types::Item;
    use std::fs;
    use std::sync::{Mutex, MutexGuard, OnceLock};
    use std::time::Duration;
    use tempfile::tempdir;
    use tokio::time::timeout;

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    /// RAII guard that sets one or more env vars under the shared env lock and
    /// restores them on drop (including on panic).  All keys are set atomically
    /// while holding the lock, eliminating re-entrant lock attempts.
    struct ScopedEnvVar {
        entries: Vec<(String, Option<String>)>,
        _guard: MutexGuard<'static, ()>,
    }

    impl ScopedEnvVar {
        fn set(key: &str, value: &str) -> Self {
            Self::set_pairs(&[(key, value)])
        }

        fn set_pairs(pairs: &[(&str, &str)]) -> Self {
            let guard = env_lock().lock().expect("env lock should not be poisoned");
            let entries = pairs
                .iter()
                .map(|(key, value)| {
                    let original = std::env::var(key).ok();
                    unsafe { std::env::set_var(key, value) };
                    (key.to_string(), original)
                })
                .collect();
            Self {
                entries,
                _guard: guard,
            }
        }
    }

    impl Drop for ScopedEnvVar {
        fn drop(&mut self) {
            for (key, original) in &self.entries {
                if let Some(value) = original {
                    unsafe { std::env::set_var(key, value) };
                } else {
                    unsafe { std::env::remove_var(key) };
                }
            }
        }
    }

    fn write_executable_script(script_body: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("create tempdir");
        let path = dir.path().join("mock-codex.sh");
        let script = format!("#!/bin/sh\nset -eu\n{script_body}\n");
        fs::write(&path, script).expect("write script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&path).expect("script metadata").permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&path, perms).expect("set executable permissions");
        }
        (dir, path)
    }

    #[test]
    fn base_args_enable_structured_json_stdout() {
        let agent = CodexAgent::new(PathBuf::from("codex"), SandboxMode::WorkspaceWrite);
        let request = AgentRequest {
            prompt: "ping".to_string(),
            project_root: PathBuf::from("/tmp/project"),
            ..Default::default()
        };

        let args: Vec<String> = agent
            .base_args(&request)
            .iter()
            .map(|value| value.to_string_lossy().to_string())
            .collect();

        assert!(args.iter().any(|arg| arg == "--json"));
        assert!(args.windows(2).any(|window| window == ["--color", "never"]));
    }

    #[test]
    fn parse_exec_agent_message_completion() {
        let line = r#"{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"hi"}}"#;
        let event = parse_codex_exec_event_line(line).expect("event should parse");
        match event {
            ParsedCodexExecEvent::ItemCompleted { item_id, item } => {
                assert_eq!(item_id, "item_0");
                assert_eq!(
                    item,
                    Item::AgentReasoning {
                        content: "hi".into()
                    }
                );
            }
            other => panic!("expected item completion, got {other:?}"),
        }
    }

    #[test]
    fn parse_exec_command_item_started() {
        let line = r#"{"type":"item.started","item":{"id":"item_0","type":"command_execution","command":"pwd","aggregated_output":"","exit_code":null,"status":"in_progress"}}"#;
        let event = parse_codex_exec_event_line(line).expect("event should parse");
        match event {
            ParsedCodexExecEvent::ItemStarted { item } => {
                assert_eq!(
                    item,
                    Item::ShellCommand {
                        command: "pwd".into(),
                        exit_code: None,
                        stdout: String::new(),
                        stderr: String::new(),
                    }
                );
            }
            other => panic!("expected item start, got {other:?}"),
        }
    }

    #[test]
    fn parse_exec_command_output_delta() {
        let line = r#"{"type":"item.command_execution.output_delta","item_id":"item_0","delta":"cargo check\n"}"#;
        let event = parse_codex_exec_event_line(line).expect("event should parse");
        match event {
            ParsedCodexExecEvent::ToolOutputDelta { item_id, text } => {
                assert_eq!(item_id, "item_0");
                assert_eq!(text, "cargo check\n");
            }
            other => panic!("expected tool output delta, got {other:?}"),
        }
    }

    #[test]
    fn parse_exec_warning_and_error() {
        let warning = parse_codex_exec_event_line(r#"{"type":"warning","message":"careful"}"#)
            .expect("warning should parse");
        let error = parse_codex_exec_event_line(
            r#"{"type":"error","error":{"message":"something failed"}}"#,
        )
        .expect("error should parse");

        assert!(matches!(
            warning,
            ParsedCodexExecEvent::Warning { ref message } if message == "careful"
        ));
        assert!(matches!(
            error,
            ParsedCodexExecEvent::Error { ref message } if message == "something failed"
        ));
    }

    #[test]
    fn parse_exec_turn_completed_usage() {
        let line = r#"{"type":"turn.completed","usage":{"input_tokens":10,"cached_input_tokens":4,"output_tokens":3,"reasoning_output_tokens":2}}"#;
        let event = parse_codex_exec_event_line(line).expect("event should parse");
        match event {
            ParsedCodexExecEvent::TokenUsage { usage } => {
                assert_eq!(
                    usage,
                    TokenUsage {
                        input_tokens: 10,
                        output_tokens: 3,
                        total_tokens: 13,
                        cost_usd: 0.0,
                    }
                );
            }
            other => panic!("expected token usage, got {other:?}"),
        }
    }

    #[test]
    fn parse_exec_output_deduplicates_completed_agent_message_after_delta() {
        let stdout = concat!(
            r#"{"type":"item.delta","item_id":"item_0","delta":"he"}"#,
            "\n",
            r#"{"type":"item.delta","item_id":"item_0","delta":"llo"}"#,
            "\n",
            r#"{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"hello"}}"#,
            "\n",
            r#"{"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":2}}"#,
        );

        let parsed = parse_codex_exec_output(stdout).expect("stdout should parse");
        assert_eq!(parsed.output, "hello");
        assert_eq!(parsed.token_usage.total_tokens, 3);
    }

    enum StreamObservation {
        Item(Option<StreamItem>),
        TaskFinished(Result<harness_core::error::Result<()>, tokio::task::JoinError>),
    }

    fn describe_stream_task_outcome(
        outcome: Result<harness_core::error::Result<()>, tokio::task::JoinError>,
    ) -> String {
        match outcome {
            Ok(Ok(())) => "task returned Ok(())".to_string(),
            Ok(Err(err)) => format!("task returned Err({err})"),
            Err(join_err) => format!("task join failed: {join_err}"),
        }
    }

    async fn wait_for_stream_item_or_task_exit(
        rx: &mut tokio::sync::mpsc::Receiver<StreamItem>,
        handle: &mut tokio::task::JoinHandle<harness_core::error::Result<()>>,
        timeout_duration: Duration,
        description: &str,
    ) -> Option<StreamItem> {
        match timeout(timeout_duration, async {
            tokio::select! {
                item = rx.recv() => StreamObservation::Item(item),
                result = &mut *handle => StreamObservation::TaskFinished(result),
            }
        })
        .await
        {
            Ok(StreamObservation::Item(item)) => item,
            Ok(StreamObservation::TaskFinished(outcome)) => {
                panic!(
                    "execute_stream finished before {description}: {}",
                    describe_stream_task_outcome(outcome)
                );
            }
            Err(_) => {
                handle.abort();
                let abort_outcome = timeout(Duration::from_secs(2), handle).await;
                panic!(
                    "timed out waiting for {description} after {timeout_duration:?}; abort outcome: {abort_outcome:?}"
                );
            }
        }
    }

    async fn assert_path_observed_before_task_exit(
        path: &std::path::Path,
        handle: &mut tokio::task::JoinHandle<harness_core::error::Result<()>>,
        timeout_duration: Duration,
        description: &str,
    ) {
        let deadline = tokio::time::Instant::now() + timeout_duration;
        loop {
            if path.exists() {
                return;
            }
            if handle.is_finished() {
                let outcome = handle.await;
                panic!(
                    "execute_stream finished before {description} at `{}`: {}",
                    path.display(),
                    describe_stream_task_outcome(outcome)
                );
            }
            if tokio::time::Instant::now() >= deadline {
                handle.abort();
                let abort_outcome = timeout(Duration::from_secs(2), handle).await;
                panic!(
                    "timed out waiting for {description} at `{}` after {timeout_duration:?}; abort outcome: {abort_outcome:?}",
                    path.display()
                );
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn execute_stream_returns_error_when_channel_closed() {
        let agent = CodexAgent::new(
            PathBuf::from("/usr/bin/true"),
            SandboxMode::DangerFullAccess,
        );
        let request = AgentRequest::default();
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        drop(rx);

        let err = agent
            .execute_stream(request, tx)
            .await
            .expect_err("execute_stream should fail when receiver is dropped");

        let message = err.to_string();
        assert!(
            message.contains("stream send failed"),
            "expected send failure in error message, got: {message}"
        );
    }

    #[tokio::test]
    async fn execute_stream_emits_delta_before_completion_and_done() {
        let (dir, script) = write_executable_script(
            r#"
printf '%s\n' '{"type":"item.delta","item_id":"item_0","delta":"hello "}'
sleep 0.2
printf '%s\n' '{"type":"item.delta","item_id":"item_0","delta":"world"}'
printf '%s\n' '{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"hello world"}}'
printf '%s\n' '{"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":2}}'
"#,
        );
        let agent = CodexAgent::new(script, SandboxMode::DangerFullAccess);
        let request = AgentRequest {
            prompt: "ignored".to_string(),
            project_root: dir.path().to_path_buf(),
            ..AgentRequest::default()
        };

        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        agent
            .execute_stream(request, tx)
            .await
            .expect("stream execution should succeed");

        let mut events = Vec::new();
        loop {
            let item = timeout(Duration::from_secs(2), rx.recv())
                .await
                .expect("timed out waiting for stream item")
                .expect("stream should not close before done");
            let is_done = matches!(item, StreamItem::Done);
            events.push(item);
            if is_done {
                break;
            }
        }

        let first_delta = events
            .iter()
            .position(|item| matches!(item, StreamItem::MessageDelta { .. }))
            .expect("expected at least one message delta");
        let completed = events
            .iter()
            .position(|item| matches!(item, StreamItem::ItemCompleted { .. }))
            .expect("expected item completed event");
        let done = events
            .iter()
            .position(|item| matches!(item, StreamItem::Done))
            .expect("expected done event");
        assert!(
            first_delta < completed,
            "delta must precede item completed: {events:?}"
        );
        assert!(
            completed < done,
            "item completed must precede done: {events:?}"
        );

        match &events[completed] {
            StreamItem::ItemCompleted {
                item: Item::AgentReasoning { content },
            } => {
                assert!(
                    content.contains("hello") && content.contains("world"),
                    "completed content should include streamed output, got: {content:?}"
                );
            }
            other => panic!("unexpected completed event payload: {other:?}"),
        }
    }

    #[tokio::test]
    async fn execute_stream_classifies_quota_failure_from_stderr() {
        let (dir, script) = write_executable_script(
            r#"
echo 'quota exhausted: retry later' >&2
exit 1
"#,
        );
        let agent = CodexAgent::new(script, SandboxMode::DangerFullAccess);
        let request = AgentRequest {
            prompt: "ignored".to_string(),
            project_root: dir.path().to_path_buf(),
            ..AgentRequest::default()
        };

        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let err = agent
            .execute_stream(request, tx)
            .await
            .expect_err("stream execution should fail");

        assert!(
            matches!(err, harness_core::error::HarnessError::QuotaExhausted(_)),
            "expected streamed codex stderr to preserve quota classification, got: {err}"
        );
        assert_eq!(
            err.turn_failure().expect("turn failure").kind,
            harness_core::types::TurnFailureKind::Quota
        );
    }

    #[tokio::test]
    async fn execute_stream_waits_for_late_stderr_before_classifying_exit() {
        let (dir, script) = write_executable_script(
            r#"
(sleep 0.2; echo 'quota exhausted: retry later' >&2) >/dev/null &
exit 1
"#,
        );
        let agent = CodexAgent::new(script, SandboxMode::DangerFullAccess);
        let request = AgentRequest {
            prompt: "ignored".to_string(),
            project_root: dir.path().to_path_buf(),
            ..AgentRequest::default()
        };

        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let err = agent
            .execute_stream(request, tx)
            .await
            .expect_err("stream execution should fail");

        assert!(
            matches!(err, harness_core::error::HarnessError::QuotaExhausted(_)),
            "expected late stderr to influence streamed exit classification, got: {err}"
        );
    }

    #[tokio::test]
    async fn execute_stream_cancel_path_converges_when_receiver_dropped_mid_stream() {
        let (dir, script) = write_executable_script(
            r#"
printf '%s\n' '{"type":"item.delta","item_id":"item_0","delta":"first"}'
sleep 0.1
printf '%s\n' '{"type":"item.delta","item_id":"item_0","delta":"second"}'
sleep 30
"#,
        );
        let agent = CodexAgent::new(script, SandboxMode::DangerFullAccess);
        let request = AgentRequest {
            prompt: "ignored".to_string(),
            project_root: dir.path().to_path_buf(),
            ..AgentRequest::default()
        };

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let mut handle = tokio::spawn(async move { agent.execute_stream(request, tx).await });

        if let Some(item) = wait_for_stream_item_or_task_exit(
            &mut rx,
            &mut handle,
            Duration::from_secs(20),
            "first stream item",
        )
        .await
        {
            assert!(
                matches!(item, StreamItem::MessageDelta { .. }),
                "expected first event to be delta, got {item:?}"
            );
        }

        drop(rx);

        let result = timeout(Duration::from_secs(10), handle)
            .await
            .expect("execute_stream task should converge after cancellation")
            .expect("join should succeed");
        // Either a send failure (receiver dropped mid-stream) or Ok (process
        // already finished before we dropped the receiver) is acceptable.
        if let Err(err) = &result {
            assert!(
                err.to_string().contains("stream send failed"),
                "expected stream send failure after cancellation, got: {err}"
            );
        }
    }

    #[tokio::test]
    async fn execute_stream_timeout_drop_does_not_leave_hanging_process() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let started_marker = dir.path().join("timeout-started.txt");
        let marker = dir.path().join("timeout-marker.txt");
        let script = dir.path().join("mock-codex-timeout.sh");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\nset -eu\necho started > \"{}\"\nsleep 5\necho reached > \"{}\"\n",
                started_marker.display(),
                marker.display()
            ),
        )
        .expect("write timeout script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&script)
                .expect("script metadata")
                .permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&script, perms).expect("set executable permissions");
        }

        let agent = CodexAgent::new(script, SandboxMode::DangerFullAccess);
        let request = AgentRequest {
            prompt: "ignored".to_string(),
            project_root: dir.path().to_path_buf(),
            ..AgentRequest::default()
        };
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let mut handle = tokio::spawn(async move { agent.execute_stream(request, tx).await });

        assert_path_observed_before_task_exit(
            &started_marker,
            &mut handle,
            Duration::from_secs(20),
            "startup marker",
        )
        .await;

        handle.abort();
        let join_err = timeout(Duration::from_secs(2), handle)
            .await
            .expect("aborted execute_stream task should resolve")
            .expect_err("aborted execute_stream task should not return successfully");
        assert!(
            join_err.is_cancelled(),
            "expected cancelled join error after abort, got: {join_err}"
        );

        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !marker.exists(),
            "process should be killed when stream future is dropped"
        );
    }

    #[test]
    fn local_mode_uses_read_only_approval_without_ephemeral() {
        let agent = CodexAgent::new(PathBuf::from("codex"), SandboxMode::ReadOnly);
        let request = AgentRequest {
            prompt: "ping".to_string(),
            project_root: PathBuf::from("/tmp/project"),
            ..Default::default()
        };

        let args: Vec<String> = agent
            .base_args(&request)
            .iter()
            .map(|value| value.to_string_lossy().to_string())
            .collect();

        assert!(args.windows(2).any(|window| window == ["-s", "read-only"]));
        assert!(!args.iter().any(|arg| arg == "--ephemeral"));
    }

    #[test]
    fn cloud_mode_uses_workspace_write_approval_and_ephemeral() {
        let cloud = CodexCloudConfig {
            enabled: true,
            cache_ttl_hours: 12,
            setup_commands: Vec::new(),
            setup_secret_env: Vec::new(),
        };
        let agent =
            CodexAgent::with_cloud(PathBuf::from("codex"), cloud, SandboxMode::WorkspaceWrite);
        let request = AgentRequest {
            prompt: "ping".to_string(),
            project_root: PathBuf::from("/tmp/project"),
            ..Default::default()
        };

        let args: Vec<String> = agent
            .base_args(&request)
            .iter()
            .map(|value| value.to_string_lossy().to_string())
            .collect();

        assert!(args
            .windows(2)
            .any(|window| window == ["-s", "workspace-write"]));
        assert!(args.iter().any(|arg| arg == "--ephemeral"));
    }

    #[tokio::test]
    async fn cloud_setup_phase_uses_cache_within_ttl() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let marker = dir.path().join("setup-runs.log");
        let setup = format!("echo run >> \"{}\"", marker.display());
        let cloud = CodexCloudConfig {
            enabled: true,
            cache_ttl_hours: 12,
            setup_commands: vec![setup],
            setup_secret_env: Vec::new(),
        };

        let agent = CodexAgent::with_cloud(
            PathBuf::from("/usr/bin/true"),
            cloud,
            SandboxMode::DangerFullAccess,
        );
        let request = AgentRequest {
            prompt: "ping".to_string(),
            project_root: dir.path().to_path_buf(),
            ..Default::default()
        };

        agent.execute(request.clone()).await?;
        agent.execute(request).await?;

        let log = fs::read_to_string(marker)?;
        assert_eq!(log.lines().count(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn setup_secret_is_available_in_setup_but_removed_for_agent_phase() -> anyhow::Result<()>
    {
        let secret_name = "HARNESS_TEST_SETUP_SECRET";
        let secret_value = format!("secret-value-{}", std::process::id());
        let _secret_guard = ScopedEnvVar::set(secret_name, &secret_value);

        let dir = tempdir()?;
        let setup_capture = dir.path().join("setup-secret.txt");
        let agent_capture = dir.path().join("agent-env.txt");
        let cli_script = dir.path().join("capture-agent-env.sh");

        fs::write(
            &cli_script,
            format!("#!/bin/sh\nenv > \"{}\"\nexit 0\n", agent_capture.display()),
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mut perms = fs::metadata(&cli_script)?.permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&cli_script, perms)?;
        }

        let setup = format!("printenv '{secret_name}' > \"{}\"", setup_capture.display());

        let cloud = CodexCloudConfig {
            enabled: true,
            cache_ttl_hours: 12,
            setup_commands: vec![setup],
            setup_secret_env: vec![secret_name.to_string()],
        };

        let agent = CodexAgent::with_cloud(cli_script, cloud, SandboxMode::DangerFullAccess);
        let request = AgentRequest {
            prompt: "ping".to_string(),
            project_root: dir.path().to_path_buf(),
            ..Default::default()
        };

        agent.execute(request).await?;

        let setup_secret = fs::read_to_string(setup_capture)?;
        assert_eq!(setup_secret.trim_end_matches('\n'), secret_value);

        let agent_env = fs::read_to_string(agent_capture)?;
        assert!(
            !agent_env
                .lines()
                .any(|line| line.starts_with(&format!("{secret_name}="))),
            "setup secret leaked into agent phase environment"
        );
        Ok(())
    }

    #[tokio::test]
    async fn execute_removes_claude_code_env_vars() -> anyhow::Result<()> {
        let _guard = ScopedEnvVar::set_pairs(&[
            ("CLAUDECODE", "1"),
            ("CLAUDE_CODE_ENTRYPOINT", "claude-code"),
        ]);

        let dir = tempdir()?;
        let agent_capture = dir.path().join("agent-env.txt");
        let cli_script = dir.path().join("capture-env.sh");

        fs::write(
            &cli_script,
            format!("#!/bin/sh\nenv > \"{}\"\nexit 0\n", agent_capture.display()),
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&cli_script)?.permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&cli_script, perms)?;
        }

        let agent = CodexAgent::new(cli_script, SandboxMode::DangerFullAccess);
        let request = AgentRequest {
            prompt: "ping".to_string(),
            project_root: dir.path().to_path_buf(),
            ..Default::default()
        };

        agent.execute(request).await?;

        let agent_env = fs::read_to_string(agent_capture)?;
        assert!(
            !agent_env
                .lines()
                .any(|line| line.starts_with("CLAUDECODE=")),
            "CLAUDECODE must not be passed to codex agent"
        );
        assert!(
            !agent_env
                .lines()
                .any(|line| line.starts_with("CLAUDE_CODE_ENTRYPOINT=")),
            "CLAUDE_CODE_ENTRYPOINT must not be passed to codex agent"
        );
        Ok(())
    }

    #[tokio::test]
    async fn execute_stream_removes_claude_code_env_vars() -> anyhow::Result<()> {
        let _guard = ScopedEnvVar::set_pairs(&[
            ("CLAUDECODE", "1"),
            ("CLAUDE_CODE_ENTRYPOINT", "claude-code"),
        ]);

        let dir = tempdir()?;
        let agent_capture = dir.path().join("agent-env.txt");
        let cli_script = dir.path().join("capture-stream-env.sh");

        fs::write(
            &cli_script,
            format!("#!/bin/sh\nenv > \"{}\"\nexit 0\n", agent_capture.display()),
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&cli_script)?.permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&cli_script, perms)?;
        }

        let agent = CodexAgent::new(cli_script, SandboxMode::DangerFullAccess);
        let request = AgentRequest {
            prompt: "ping".to_string(),
            project_root: dir.path().to_path_buf(),
            ..AgentRequest::default()
        };

        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        agent.execute_stream(request, tx).await?;
        // Drain the channel so no items are left pending.
        while rx.try_recv().is_ok() {}

        let agent_env = fs::read_to_string(agent_capture)?;
        assert!(
            !agent_env
                .lines()
                .any(|line| line.starts_with("CLAUDECODE=")),
            "CLAUDECODE must not be passed to codex agent in streaming mode"
        );
        assert!(
            !agent_env
                .lines()
                .any(|line| line.starts_with("CLAUDE_CODE_ENTRYPOINT=")),
            "CLAUDE_CODE_ENTRYPOINT must not be passed to codex agent in streaming mode"
        );
        Ok(())
    }

    #[test]
    fn codex_sandbox_mode_maps_to_codex_cli_values() {
        assert_eq!(codex_sandbox_mode(SandboxMode::ReadOnly), "read-only");
        assert_eq!(
            codex_sandbox_mode(SandboxMode::WorkspaceWrite),
            "workspace-write"
        );
        assert_eq!(
            codex_sandbox_mode(SandboxMode::DangerFullAccess),
            "danger-full-access"
        );
    }

    #[test]
    fn base_args_uses_request_reasoning_effort_and_sandbox_override() {
        let agent = CodexAgent::new(PathBuf::from("codex"), SandboxMode::DangerFullAccess);
        let request = AgentRequest {
            prompt: "ping".to_string(),
            project_root: PathBuf::from("/tmp/project"),
            reasoning_effort: Some("medium".to_string()),
            sandbox_mode: Some(SandboxMode::ReadOnly),
            ..Default::default()
        };

        let args: Vec<String> = agent
            .base_args(&request)
            .iter()
            .map(|value| value.to_string_lossy().to_string())
            .collect();

        assert!(args
            .windows(2)
            .any(|window| window == ["-c", "model_reasoning_effort=\"medium\""]));
        assert!(args.windows(2).any(|window| window == ["-s", "read-only"]));
    }

    #[test]
    fn spawn_diagnostics_resolve_relative_program_from_spawn_current_dir() {
        let project = tempdir().expect("create project dir");
        let bin_dir = project.path().join("bin");
        fs::create_dir(&bin_dir).expect("create bin dir");
        let program_path = bin_dir.join("codex");
        fs::write(&program_path, "#!/bin/sh\n").expect("write program");

        let resolved = resolve_program_for_spawn(Path::new("bin/codex"), project.path())
            .expect("relative program should resolve from spawn current dir");

        assert_eq!(resolved, program_path);
    }

    #[test]
    fn allowed_tools_does_not_override_configured_workspace_write_sandbox() {
        let agent = CodexAgent::new(PathBuf::from("codex"), SandboxMode::WorkspaceWrite);
        let request = AgentRequest {
            prompt: "ping".to_string(),
            project_root: PathBuf::from("/tmp/project"),
            allowed_tools: Some(vec!["Read".to_string(), "Grep".to_string()]),
            ..Default::default()
        };

        let args: Vec<String> = agent
            .base_args(&request)
            .iter()
            .map(|value| value.to_string_lossy().to_string())
            .collect();
        assert!(args
            .windows(2)
            .any(|window| window == ["-s", "workspace-write"]));
    }

    #[test]
    fn deny_all_allowed_tools_keeps_configured_sandbox_mode() {
        let agent = CodexAgent::new(PathBuf::from("codex"), SandboxMode::DangerFullAccess);
        let request = AgentRequest {
            prompt: "ping".to_string(),
            project_root: PathBuf::from("/tmp/project"),
            allowed_tools: Some(vec![]),
            ..Default::default()
        };

        let args: Vec<String> = agent
            .base_args(&request)
            .iter()
            .map(|value| value.to_string_lossy().to_string())
            .collect();
        assert!(args
            .windows(2)
            .any(|window| window == ["-s", "danger-full-access"]));
    }
}
