use crate::cloud_setup;
use crate::streaming::{
    capture_agent_stderr_diagnostics, captured_stderr_tail, enrich_stream_exit_error,
    log_captured_stderr_diagnostics, send_stream_item,
};
use async_trait::async_trait;
use harness_core::agent::{
    AgentRequest, AgentResponse, CodeAgent, StreamItem, AGENT_OUTPUT_SCHEMA_PATH_ENV,
};
use harness_core::config::agents::{AgentPermissionMode, SandboxMode};
use harness_core::config::agents::{CodexAgentConfig, CodexCloudConfig};
use harness_core::types::Capability;
use harness_sandbox::SandboxSpec;
use std::collections::HashMap;
use std::ffi::OsString;
#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::io::AsyncWriteExt;

#[path = "codex_exec_parser.rs"]
mod codex_exec_parser;
pub(crate) use self::codex_exec_parser::{
    parse_codex_error_item_message, parse_codex_item, parse_codex_token_usage,
};
use self::codex_exec_parser::{parse_codex_exec_output, stream_codex_exec_output};

#[path = "codex_args.rs"]
mod codex_args;
#[cfg(test)]
use self::codex_args::codex_sandbox_mode;
use self::codex_args::{
    push_codex_approval_policy_args, push_codex_developer_instructions_args,
    push_codex_sandbox_args,
};

#[path = "codex_spawn.rs"]
mod codex_spawn;
#[cfg(test)]
use self::codex_spawn::resolve_program_for_spawn;
use self::codex_spawn::{codex_spawn_failure_message, log_codex_spawn_attempt};

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
    pub permission_mode: AgentPermissionMode,
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

    async fn run_setup_phase(&self, req: &AgentRequest) -> harness_core::error::Result<()> {
        cloud_setup::run_setup_phase(
            &self.cloud,
            cloud_setup::CloudSetupContext {
                project_root: &req.project_root,
                sandbox_mode: self.effective_sandbox_mode(req),
                permission_mode: req.permission_mode,
                env_vars: &req.env_vars,
                capability_token: req.capability_token.as_ref(),
            },
        )
        .await
    }

    async fn run_review_setup_phase(
        &self,
        req: &CodexReviewRequest,
    ) -> harness_core::error::Result<()> {
        cloud_setup::run_setup_phase(
            &self.cloud,
            cloud_setup::CloudSetupContext {
                project_root: &req.project_root,
                sandbox_mode: req.sandbox_mode,
                permission_mode: req.permission_mode,
                env_vars: &req.env_vars,
                capability_token: None,
            },
        )
        .await
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
        if let Some([]) = req.allowed_tools.as_deref() {
            args.push(OsString::from("--ignore-user-config"));
        }
        push_codex_sandbox_args(&mut args, sandbox_mode);
        if let Some(approval_policy) = req.approval_policy.as_deref() {
            push_codex_approval_policy_args(&mut args, approval_policy);
        }
        if let Some(schema_path) = req
            .env_vars
            .get(AGENT_OUTPUT_SCHEMA_PATH_ENV)
            .map(String::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            args.push(OsString::from("--output-schema"));
            args.push(OsString::from(schema_path));
        }

        if self.cloud.enabled {
            args.push(OsString::from("--ephemeral"));
        }

        args.push(OsString::from("-C"));
        args.push(OsString::from("."));
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

    /// Plan the review spawn without launching it.
    ///
    /// Review must go through the same spawn contract as the execute paths.
    /// Calling `wrap_command` directly (as this path used to) skipped container
    /// isolation and the operator-secret env filtering that
    /// `prepare_agent_spawn` applies.
    async fn prepare_review_spawn(
        &self,
        req: &CodexReviewRequest,
    ) -> harness_core::error::Result<(
        crate::spawn_contract::PreparedAgentSpawn,
        harness_core::run_id::RunIdentity,
    )> {
        let review_args = self.review_args(req);
        let sandbox_spec = SandboxSpec::new(req.sandbox_mode, &req.project_root);

        let mut spawn_env_vars = req.env_vars.clone();
        spawn_env_vars.insert(
            crate::spawn_contract::REVIEW_GIT_SAFE_WORKSPACE_ENV.to_string(),
            "1".to_string(),
        );
        let run_identity = crate::resolve_agent_run_identity(&spawn_env_vars);
        run_identity.write_env_vars(&mut spawn_env_vars);
        if self.cloud.enabled {
            for key in &self.cloud.setup_secret_env {
                spawn_env_vars.remove(key);
            }
        }
        let container_bind_mounts = cloud_setup::apply_container_state(
            &self.cloud,
            &req.project_root,
            &mut spawn_env_vars,
        )?;

        let prepared_spawn =
            crate::spawn_contract::prepare_agent_spawn(crate::spawn_contract::AgentSpawnInput {
                program: &self.cli_path,
                args: &review_args,
                project_root: &req.project_root,
                sandbox_spec: &sandbox_spec,
                env_vars: &spawn_env_vars,
                secret_env_keys: &[],
                container_bind_mounts: &container_bind_mounts,
                permission_mode: req.permission_mode,
                forward_stdin: review_uses_stdin_prompt(req),
            })
            .await?;
        Ok((prepared_spawn, run_identity))
    }

    pub async fn execute_review(
        &self,
        req: CodexReviewRequest,
    ) -> harness_core::error::Result<AgentResponse> {
        self.run_review_setup_phase(&req).await?;

        let use_stdin_prompt = review_uses_stdin_prompt(&req);
        let (prepared_spawn, run_identity) = self.prepare_review_spawn(&req).await?;

        tracing::debug!(
            agent = "codex",
            mode = "review",
            program = %prepared_spawn.program.display(),
            current_dir = %prepared_spawn.current_dir.display(),
            sandbox_engine = ?prepared_spawn.sandbox_engine,
            arg_count = prepared_spawn.args.len(),
            has_stdin_instructions = use_stdin_prompt,
            "codex review spawn prepared"
        );
        let spawn_project_root = req.project_root.clone();
        let supervised = crate::spawn_supervisor::spawn_agent(
            crate::spawn_supervisor::AgentSpawnPlan {
                prepared_spawn,
                run_identity,
                native_kind: "codex",
                process_label: "codex review",
                stdio: crate::spawn_supervisor::AgentStdio::piped_output(
                    if use_stdin_prompt {
                        Stdio::piped()
                    } else {
                        Stdio::null()
                    },
                ),
                extra_env_removals: cloud_setup_env_removals(&self.cloud),
                map_spawn_error: Box::new(move |error, spawn| {
                    let message = format!(
                        "failed to run codex review: {error}; mode=review; program={}; current_dir={}; sandbox_engine={:?}; arg_count={}",
                        spawn.program.display(),
                        spawn.current_dir.display(),
                        spawn.sandbox_engine,
                        spawn.args.len()
                    );
                    let message = crate::classify_missing_workspace_spawn_failure(
                        error,
                        &spawn_project_root,
                        message,
                    );
                    harness_core::error::HarnessError::AgentExecution(message)
                }),
            },
            None,
        )
        .await?;
        let mut child = supervised.child;

        if use_stdin_prompt {
            let Some(instructions) = req.instructions.as_deref() else {
                unreachable!("review stdin prompt requires instructions");
            };
            let Some(mut stdin) = child.inner_mut().stdin.take() else {
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

        let limits = crate::OutputLimits::from_stream_timeout_secs(self.stream_timeout_secs);
        let output = child.wait_with_output(&limits).await.map_err(|error| {
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

fn cloud_setup_env_removals(cloud: &CodexCloudConfig) -> Vec<String> {
    if cloud.enabled {
        cloud.setup_secret_env.clone()
    } else {
        Vec::new()
    }
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

        self.run_setup_phase(&req).await?;

        let base_args = self.base_args(&req);
        let sandbox_mode = self.effective_sandbox_mode(&req);
        let sandbox_spec = if let Some(ref token) = req.capability_token {
            SandboxSpec::new(sandbox_mode, &req.project_root)
                .with_allowed_write_paths(token.allowed_write_paths.clone())
        } else {
            SandboxSpec::new(sandbox_mode, &req.project_root)
        };
        let mut spawn_env_vars = req.env_vars.clone();
        spawn_env_vars.remove(AGENT_OUTPUT_SCHEMA_PATH_ENV);
        let run_identity = crate::resolve_agent_run_identity(&spawn_env_vars);
        run_identity.write_env_vars(&mut spawn_env_vars);
        if self.cloud.enabled {
            for key in &self.cloud.setup_secret_env {
                spawn_env_vars.remove(key);
            }
        }
        let container_bind_mounts = cloud_setup::apply_container_state(
            &self.cloud,
            &req.project_root,
            &mut spawn_env_vars,
        )?;
        let prepared_spawn =
            crate::spawn_contract::prepare_agent_spawn(crate::spawn_contract::AgentSpawnInput {
                program: &self.cli_path,
                args: &base_args,
                project_root: &req.project_root,
                sandbox_spec: &sandbox_spec,
                env_vars: &spawn_env_vars,
                secret_env_keys: &[],
                container_bind_mounts: &container_bind_mounts,
                permission_mode: req.permission_mode,
                forward_stdin: false,
            })
            .await?;

        log_codex_spawn_attempt(
            &prepared_spawn.program,
            prepared_spawn.args.len(),
            &req,
            prepared_spawn.sandbox_engine,
            "execute",
        );
        let spawn_error_req = req.clone();
        let supervised = crate::spawn_supervisor::spawn_agent(
            crate::spawn_supervisor::AgentSpawnPlan {
                prepared_spawn,
                run_identity,
                native_kind: "codex",
                process_label: "codex execute",
                stdio: crate::spawn_supervisor::AgentStdio::piped_output(Stdio::null()),
                extra_env_removals: cloud_setup_env_removals(&self.cloud),
                map_spawn_error: Box::new(move |error, spawn| {
                    harness_core::error::HarnessError::AgentExecution(codex_spawn_failure_message(
                        error,
                        &spawn.program,
                        &spawn_error_req,
                        spawn.sandbox_engine,
                        "execute",
                    ))
                }),
            },
            req.capability_token.as_ref(),
        )
        .await?;
        let mut child = supervised.child;
        let limits = crate::OutputLimits::from_stream_timeout_secs(self.stream_timeout_secs);
        let output = child.wait_with_output(&limits).await.map_err(|err| {
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

        self.run_setup_phase(&req).await?;

        let base_args = self.base_args(&req);
        let sandbox_mode = self.effective_sandbox_mode(&req);
        let sandbox_spec = if let Some(ref token) = req.capability_token {
            SandboxSpec::new(sandbox_mode, &req.project_root)
                .with_allowed_write_paths(token.allowed_write_paths.clone())
        } else {
            SandboxSpec::new(sandbox_mode, &req.project_root)
        };
        let mut spawn_env_vars = req.env_vars.clone();
        spawn_env_vars.remove(AGENT_OUTPUT_SCHEMA_PATH_ENV);
        let run_identity = crate::resolve_agent_run_identity(&spawn_env_vars);
        run_identity.write_env_vars(&mut spawn_env_vars);
        if self.cloud.enabled {
            for key in &self.cloud.setup_secret_env {
                spawn_env_vars.remove(key);
            }
        }
        let container_bind_mounts = cloud_setup::apply_container_state(
            &self.cloud,
            &req.project_root,
            &mut spawn_env_vars,
        )?;
        let prepared_spawn =
            crate::spawn_contract::prepare_agent_spawn(crate::spawn_contract::AgentSpawnInput {
                program: &self.cli_path,
                args: &base_args,
                project_root: &req.project_root,
                sandbox_spec: &sandbox_spec,
                env_vars: &spawn_env_vars,
                secret_env_keys: &[],
                container_bind_mounts: &container_bind_mounts,
                permission_mode: req.permission_mode,
                forward_stdin: false,
            })
            .await?;

        log_codex_spawn_attempt(
            &prepared_spawn.program,
            prepared_spawn.args.len(),
            &req,
            prepared_spawn.sandbox_engine,
            "execute_stream",
        );
        let spawn_error_req = req.clone();
        let supervised = crate::spawn_supervisor::spawn_agent(
            crate::spawn_supervisor::AgentSpawnPlan {
                prepared_spawn,
                run_identity,
                native_kind: "codex",
                process_label: "codex execute_stream",
                stdio: crate::spawn_supervisor::AgentStdio::piped_output(Stdio::null()),
                extra_env_removals: cloud_setup_env_removals(&self.cloud),
                map_spawn_error: Box::new(move |error, spawn| {
                    harness_core::error::HarnessError::AgentExecution(codex_spawn_failure_message(
                        error,
                        &spawn.program,
                        &spawn_error_req,
                        spawn.sandbox_engine,
                        "execute_stream",
                    ))
                }),
            },
            req.capability_token.as_ref(),
        )
        .await?;
        let mut child = supervised.child;

        let stderr_capture = Arc::new(Mutex::new(String::new()));
        let mut stderr_task = None;
        if let Some(stderr) = child.inner_mut().stderr.take() {
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
        let stream_result = stream_codex_exec_output(child.inner_mut(), &tx, idle_timeout).await;
        let stream_send_failed = matches!(
            &stream_result,
            Err(harness_core::error::HarnessError::AgentExecution(message))
                if message.contains("stream send failed")
        );
        let stream_process_exited = matches!(
            &stream_result,
            Err(harness_core::error::HarnessError::AgentExecution(message))
                if message.contains("codex exited with")
        );
        if stream_result.is_err() && !stream_process_exited {
            child.terminate_now();
        }
        let status = child
            .wait_and_cleanup_descendants()
            .await
            .map_err(|error| {
                harness_core::error::HarnessError::AgentExecution(format!(
                    "failed waiting for codex process: {error}"
                ))
            })?;
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

#[cfg(test)]
#[path = "codex_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "codex_failure_tests.rs"]
mod failure_tests;

#[cfg(test)]
#[path = "codex_review_tests.rs"]
mod review_tests;
