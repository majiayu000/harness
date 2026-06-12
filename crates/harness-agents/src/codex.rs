use crate::cloud_setup;
use crate::streaming::{
    capture_agent_stderr_diagnostics, captured_stderr_tail, enrich_stream_exit_error,
    log_captured_stderr_diagnostics, send_stream_item,
};
use async_trait::async_trait;
use harness_core::agent::{AgentRequest, AgentResponse, CodeAgent, StreamItem};
use harness_core::config::agents::SandboxMode;
use harness_core::config::agents::{CodexAgentConfig, CodexCloudConfig};
use harness_core::types::Capability;
use harness_sandbox::{wrap_command, SandboxSpec};
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

#[path = "codex_exec_parser.rs"]
mod codex_exec_parser;
pub(crate) use self::codex_exec_parser::{
    parse_codex_error_item_message, parse_codex_item, parse_codex_token_usage,
};
use self::codex_exec_parser::{parse_codex_exec_output, stream_codex_exec_output};

const READ_ONLY_WITH_NETWORK_PROFILE: &str = "harness_read_only_with_network";

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

#[derive(Debug, Clone)]
pub struct CodexReviewRequest {
    pub project_root: PathBuf,
    pub instructions: Option<String>,
    pub base_ref: Option<String>,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub sandbox_mode: SandboxMode,
    pub approval_policy: Option<String>,
    pub env_vars: HashMap<String, String>,
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
        ];
        push_codex_sandbox_args(&mut args, sandbox_mode);
        if let Some(approval_policy) = req.approval_policy.as_deref() {
            push_codex_approval_policy_args(&mut args, approval_policy);
        }

        if self.cloud.enabled {
            args.push(OsString::from("--ephemeral"));
        }

        args.push(OsString::from("-C"));
        args.push(req.project_root.as_os_str().to_os_string());
        args.push(OsString::from(req.prompt.clone()));
        args
    }

    fn review_args(&self, req: &CodexReviewRequest) -> Vec<OsString> {
        let model = req.model.as_deref().unwrap_or(&self.default_model);
        let reasoning_effort = req
            .reasoning_effort
            .as_deref()
            .unwrap_or(&self.reasoning_effort);
        let base_ref = req
            .base_ref
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let mut args = vec![
            OsString::from("-C"),
            req.project_root.as_os_str().to_os_string(),
            OsString::from("-m"),
            OsString::from(model),
            OsString::from("-c"),
            OsString::from(format!("model_reasoning_effort=\"{}\"", reasoning_effort)),
        ];
        push_codex_sandbox_args(&mut args, req.sandbox_mode);
        if let Some(approval_policy) = req.approval_policy.as_deref() {
            push_codex_approval_policy_args(&mut args, approval_policy);
        }
        if let Some(instructions) = req
            .instructions
            .as_deref()
            .filter(|_| review_uses_config_instructions(req))
        {
            push_codex_developer_instructions_args(&mut args, instructions);
        }
        if self.cloud.enabled {
            args.push(OsString::from("--ephemeral"));
        }

        args.push(OsString::from("review"));
        if let Some(base_ref) = base_ref {
            args.push(OsString::from("--base"));
            args.push(OsString::from(base_ref));
        }
        if review_uses_stdin_prompt(req) {
            args.push(OsString::from("-"));
        }
        args
    }

    pub async fn execute_review(
        &self,
        req: CodexReviewRequest,
    ) -> harness_core::error::Result<AgentResponse> {
        self.run_setup_phase(&req.project_root).await?;

        let review_args = self.review_args(&req);
        let use_stdin_prompt = review_uses_stdin_prompt(&req);
        let sandbox_spec = SandboxSpec::new(req.sandbox_mode, &req.project_root);
        let wrapped_command =
            wrap_command(&self.cli_path, &review_args, &sandbox_spec).map_err(|error| {
                harness_core::error::HarnessError::AgentExecution(format!(
                    "sandbox setup failed for codex review: {error}"
                ))
            })?;

        let mut cmd = Command::new(&wrapped_command.program);
        cmd.args(&wrapped_command.args)
            .current_dir(&req.project_root)
            .stdin(if use_stdin_prompt {
                Stdio::piped()
            } else {
                Stdio::null()
            })
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

        tracing::debug!(
            agent = "codex",
            mode = "review",
            program = %wrapped_command.program.display(),
            current_dir = %req.project_root.display(),
            sandbox_engine = ?wrapped_command.engine,
            arg_count = wrapped_command.args.len(),
            has_stdin_instructions = use_stdin_prompt,
            "codex review spawn prepared"
        );
        let mut child = cmd.spawn().map_err(|error| {
            let message = format!(
                "failed to run codex review: {error}; mode=review; program={}; current_dir={}; sandbox_engine={:?}; arg_count={}",
                wrapped_command.program.display(),
                req.project_root.display(),
                wrapped_command.engine,
                wrapped_command.args.len()
            );
            tracing::error!(agent = "codex", mode = "review", error_kind = ?error.kind(), "{message}");
            harness_core::error::HarnessError::AgentExecution(message)
        })?;

        if use_stdin_prompt {
            let Some(instructions) = req.instructions.as_deref() else {
                unreachable!("review stdin prompt requires instructions");
            };
            let Some(mut stdin) = child.stdin.take() else {
                return Err(harness_core::error::HarnessError::AgentExecution(
                    "failed to open stdin for codex review instructions".to_string(),
                ));
            };
            stdin
                .write_all(instructions.as_bytes())
                .await
                .map_err(|error| {
                    harness_core::error::HarnessError::AgentExecution(format!(
                        "failed to write codex review instructions: {error}"
                    ))
                })?;
        }

        let output = child.wait_with_output().await.map_err(|error| {
            harness_core::error::HarnessError::AgentExecution(format!(
                "failed to wait for codex review: {error}"
            ))
        })?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        log_captured_stderr_diagnostics(&stderr, self.name());

        if !output.status.success() {
            let error_output = if stderr.trim().is_empty() {
                stdout.as_str()
            } else {
                stderr.as_str()
            };
            return Err(codex_nonzero_exit_error(output.status, error_output, None));
        }

        Ok(AgentResponse {
            output: stdout,
            stderr,
            items: Vec::new(),
            token_usage: Default::default(),
            model: "codex".to_string(),
            exit_code: output.status.code(),
        })
    }
}

fn review_uses_stdin_prompt(req: &CodexReviewRequest) -> bool {
    req.instructions.is_some()
        && req
            .base_ref
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .is_none()
}

fn review_uses_config_instructions(req: &CodexReviewRequest) -> bool {
    req.instructions.is_some() && !review_uses_stdin_prompt(req)
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

fn codex_structured_error_from_stdout(stdout: &str) -> Option<String> {
    parse_codex_exec_output(stdout).ok()?.structured_error
}

fn codex_structured_error(message: impl Into<String>) -> harness_core::error::HarnessError {
    let message = format!("codex structured error: {}", message.into());
    if harness_core::error::is_billing_failure_message(&message) {
        return harness_core::error::HarnessError::BillingFailed(message);
    }
    if harness_core::error::is_quota_failure_message(&message) {
        return harness_core::error::HarnessError::QuotaExhausted(message);
    }
    harness_core::error::HarnessError::AgentExecution(message)
}

fn codex_nonzero_exit_error(
    status: std::process::ExitStatus,
    stderr: &str,
    structured_error: Option<&str>,
) -> harness_core::error::HarnessError {
    if let Some(message) = structured_error {
        let mut error = codex_structured_error(format!("exit {status}: {message}"));
        if matches!(error, harness_core::error::HarnessError::AgentExecution(_))
            && !stderr.trim().is_empty()
        {
            error = harness_core::error::HarnessError::AgentExecution(format!(
                "{error}; stderr=[{stderr}]"
            ));
        }
        return error;
    }

    if harness_core::error::is_billing_failure_message(stderr) {
        return harness_core::error::HarnessError::BillingFailed(format!(
            "codex billing failure (exit {status}): {stderr}"
        ));
    }
    if harness_core::error::is_quota_failure_message(stderr) {
        return harness_core::error::HarnessError::QuotaExhausted(format!(
            "codex quota exhausted (exit {status}): {stderr}"
        ));
    }
    harness_core::error::HarnessError::AgentExecution(format!(
        "codex exited with {status}: {stderr}"
    ))
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
            let structured_error = codex_structured_error_from_stdout(&stdout);
            return Err(codex_nonzero_exit_error(
                output.status,
                &stderr,
                structured_error.as_deref(),
            ));
        }

        let parsed = parse_codex_exec_output(&stdout)?;
        if let Some(message) = parsed.structured_error {
            return Err(codex_structured_error(message));
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
            return Err(codex_nonzero_exit_error(
                status,
                &stderr,
                parsed.structured_error.as_deref(),
            ));
        }
        if let Some(message) = parsed.structured_error {
            return Err(codex_structured_error(message));
        }
        send_stream_item(&tx, StreamItem::Done, self.name(), "done").await?;
        Ok(())
    }
}

fn codex_sandbox_mode(mode: SandboxMode) -> &'static str {
    match mode {
        SandboxMode::ReadOnly | SandboxMode::ReadOnlyWithNetwork => "read-only",
        SandboxMode::WorkspaceWrite => "workspace-write",
        SandboxMode::DangerFullAccess => "danger-full-access",
    }
}

fn push_codex_sandbox_args(args: &mut Vec<OsString>, mode: SandboxMode) {
    if mode == SandboxMode::ReadOnlyWithNetwork {
        args.push(OsString::from("-c"));
        args.push(OsString::from(format!(
            "default_permissions=\"{READ_ONLY_WITH_NETWORK_PROFILE}\""
        )));
        args.push(OsString::from("-c"));
        args.push(OsString::from(format!(
            "permissions.{READ_ONLY_WITH_NETWORK_PROFILE}.filesystem={{\":minimal\"=\"read\",\":project_roots\"={{\".\"=\"read\"}}}}"
        )));
        args.push(OsString::from("-c"));
        args.push(OsString::from(format!(
            "permissions.{READ_ONLY_WITH_NETWORK_PROFILE}.network.enabled=true"
        )));
        return;
    }

    args.push(OsString::from("-s"));
    args.push(OsString::from(codex_sandbox_mode(mode)));
}

fn push_codex_approval_policy_args(args: &mut Vec<OsString>, approval_policy: &str) {
    let approval_policy = escape_codex_config_string(approval_policy);
    args.push(OsString::from("-c"));
    args.push(OsString::from(format!(
        "approval_policy=\"{approval_policy}\""
    )));
}

fn push_codex_developer_instructions_args(args: &mut Vec<OsString>, instructions: &str) {
    let instructions = escape_codex_config_string(instructions);
    args.push(OsString::from("-c"));
    args.push(OsString::from(format!(
        "developer_instructions=\"{instructions}\""
    )));
}

fn escape_codex_config_string(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            '\u{08}' => escaped.push_str("\\b"),
            '\u{0C}' => escaped.push_str("\\f"),
            ch => escaped.push(ch),
        }
    }
    escaped
}

#[cfg(test)]
mod approval_policy_arg_tests {
    use super::push_codex_approval_policy_args;

    #[test]
    fn approval_policy_args_escape_config_string_delimiters() {
        let mut args = Vec::new();

        push_codex_approval_policy_args(&mut args, "ask\"me\\first\nnext");

        let args: Vec<String> = args
            .into_iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            vec![
                "-c".to_string(),
                "approval_policy=\"ask\\\"me\\\\first\\nnext\"".to_string()
            ]
        );
    }
}

#[cfg(test)]
#[path = "codex_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "codex_failure_tests.rs"]
mod failure_tests;

#[cfg(test)]
#[path = "codex_review_tests.rs"]
mod review_tests;
