use crate::claude_stream::{
    claude_stdout_tail, parse_claude_stream_output, stream_claude_code_output,
};
use crate::streaming::{
    captured_stderr_tail, enrich_stream_exit_error, filter_agent_stderr_with_capture,
    log_captured_stderr, send_stream_item,
};
use async_trait::async_trait;
use harness_core::config::agents::SandboxMode;
use harness_core::{
    agent::AgentRequest, agent::AgentResponse, agent::CodeAgent, agent::StreamItem,
    types::Capability, types::ReasoningBudget,
};
use harness_sandbox::{wrap_command, SandboxSpec};
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::process::Command;

pub struct ClaudeCodeAgent {
    pub cli_path: PathBuf,
    pub default_model: String,
    pub sandbox_mode: SandboxMode,
    /// Per-phase model selection. When set, model is chosen based on
    /// `req.execution_phase`. Falls back to `req.model` or `default_model`.
    pub reasoning_budget: Option<ReasoningBudget>,
    /// Maximum seconds of idle silence on the output stream before the
    /// subprocess is declared a zombie and terminated. `None` = no timeout.
    pub stream_timeout_secs: Option<u64>,
}

impl ClaudeCodeAgent {
    pub fn new(cli_path: PathBuf, default_model: String, sandbox_mode: SandboxMode) -> Self {
        Self {
            cli_path,
            default_model,
            sandbox_mode,
            reasoning_budget: None,
            stream_timeout_secs: Some(3600),
        }
    }

    /// Previously probed and enabled `--no-session-persistence`. Now a no-op:
    /// session persistence is required for token-usage tracking and learn
    /// system observability. Retained for API compatibility.
    pub fn with_no_session_persistence_probe(self) -> Self {
        self
    }

    /// Attach a ReasoningBudget for per-phase model selection.
    pub fn with_reasoning_budget(mut self, budget: ReasoningBudget) -> Self {
        self.reasoning_budget = Some(budget);
        self
    }

    /// Set the per-line idle timeout for stream zombie detection.
    pub fn with_stream_timeout(mut self, secs: Option<u64>) -> Self {
        self.stream_timeout_secs = secs;
        self
    }

    fn resolve_model<'a>(&'a self, req: &'a AgentRequest) -> &'a str {
        if let (Some(budget), Some(phase)) = (&self.reasoning_budget, req.execution_phase) {
            return budget.model_for_phase(phase);
        }
        req.model.as_deref().unwrap_or(&self.default_model)
    }

    fn effective_sandbox_mode(&self, req: &AgentRequest) -> SandboxMode {
        req.sandbox_mode.unwrap_or(self.sandbox_mode)
    }

    /// Build CLI arguments for the Claude Code agent.
    ///
    /// **Critical**: the prompt MUST be the token immediately after `-p`.
    /// Claude CLI parses `-p <VALUE>` — the prompt is the value of `-p`,
    /// not a trailing positional argument. Placing it at the end causes
    /// "Input must be provided" errors.
    fn base_args(&self, req: &AgentRequest) -> Vec<OsString> {
        let model = self.resolve_model(req);
        let mut base_args = vec![
            OsString::from("-p"),
            OsString::from(&req.prompt), // prompt MUST follow -p immediately
            OsString::from("--output-format"),
            OsString::from("stream-json"),
            OsString::from("--model"),
            OsString::from(model),
            OsString::from("--verbose"),
        ];

        // Hard tool enforcement at the CLI boundary (issue #514):
        //   Full profile  (allowed_tools = None)    → --dangerously-skip-permissions
        //   Restricted profile (allowed_tools set)  → --permission-mode bypassPermissions
        //                                              --allowedTools <comma-list>
        //
        // --allowedTools and --dangerously-skip-permissions are mutually exclusive
        // in Claude CLI 2.1.70+. Using --allowedTools provides hard enforcement;
        // the agent cannot call tools outside the list regardless of prompt content.
        //
        // --permission-mode bypassPermissions is required alongside --allowedTools so
        // that non-interactive background tasks (preflight, periodic review, reviewer)
        // do not hang waiting for interactive approval prompts on the first Bash/Edit
        // call. The two flags are orthogonal: bypassPermissions auto-approves tool
        // calls within the allowed set; --allowedTools limits what that set is.
        //
        // Post-execution validate_tool_usage() remains as a defense-in-depth layer.
        if req.uses_dangerously_skip_permissions() {
            base_args.push(OsString::from("--dangerously-skip-permissions"));
        } else {
            base_args.push(OsString::from("--permission-mode"));
            base_args.push(OsString::from("bypassPermissions"));
            base_args.push(OsString::from("--allowedTools"));
            let tools = req.allowed_tools.as_deref().unwrap_or(&[]);
            base_args.push(OsString::from(tools.join(",")));
        }

        if let Some(phase) = req.execution_phase {
            base_args.push(OsString::from("--effort"));
            base_args.push(OsString::from(phase.effort_level()));
        } else if let Some(reasoning_effort) = req.reasoning_effort.as_deref() {
            base_args.push(OsString::from("--effort"));
            base_args.push(OsString::from(reasoning_effort));
        }

        if let Some(budget) = req.max_budget_usd {
            base_args.push(OsString::from("--max-budget-usd"));
            base_args.push(OsString::from(budget.to_string()));
        }

        base_args
    }
}

#[async_trait]
impl CodeAgent for ClaudeCodeAgent {
    fn name(&self) -> &str {
        "claude"
    }

    fn capabilities(&self) -> Vec<Capability> {
        vec![Capability::Read, Capability::Write, Capability::Execute]
    }

    async fn execute(&self, req: AgentRequest) -> harness_core::error::Result<AgentResponse> {
        // Check token expiry before spawning.
        // See also: claude_adapter.rs — both files must stay in sync on this check.
        if let Some(ref token) = req.capability_token {
            if token.is_expired() {
                return Err(harness_core::error::HarnessError::AgentExecution(format!(
                    "capability token for subtask {} has expired",
                    token.subtask_index
                )));
            }
        }

        let model = self.resolve_model(&req).to_string();
        let base_args = self.base_args(&req);

        // Narrow sandbox write paths to token scope when present.
        // See also: claude_adapter.rs — both files must stay in sync on this conversion.
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
                    "sandbox setup failed for claude: {error}"
                ))
            })?;

        tracing::debug!(
            cli = %wrapped_command.program.display(),
            project_root = %req.project_root.display(),
            model = %self.resolve_model(&req),
            "spawning claude agent"
        );

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

        let child = cmd.spawn().map_err(|e| {
            harness_core::error::HarnessError::AgentExecution(format!("failed to run claude: {e}"))
        })?;
        let mut child = crate::ManagedChild::new(child, "claude execute");

        let output = child.wait_with_output().await.map_err(|e| {
            harness_core::error::HarnessError::AgentExecution(format!(
                "failed to wait for claude: {e}"
            ))
        })?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let parsed = parse_claude_stream_output(&stdout);
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        log_captured_stderr(&stderr, self.name());

        if !output.status.success() {
            let tail_source = if parsed.output.is_empty() {
                &stdout
            } else {
                &parsed.output
            };
            let stdout_tail = claude_stdout_tail(tail_source);
            return Err(harness_core::error::HarnessError::AgentExecution(format!(
                "claude exited with {}: stderr=[{}] stdout_tail=[{}]",
                output.status, stderr, stdout_tail
            )));
        }

        Ok(AgentResponse {
            output: parsed.output,
            stderr,
            items: Vec::new(),
            token_usage: parsed.token_usage,
            model: model.to_string(),
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
                    "sandbox setup failed for claude: {error}"
                ))
            })?;

        // Dump full args (truncate each to 120 chars) so we can diagnose
        // exactly what is being passed to the Claude CLI process.
        let args_debug: Vec<String> = wrapped_command
            .args
            .iter()
            .enumerate()
            .map(|(i, a)| {
                let s = a.to_string_lossy();
                if s.len() > 120 {
                    format!("[{i}] {}…({} chars)", &s[..120], s.len())
                } else {
                    format!("[{i}] {s}")
                }
            })
            .collect();
        tracing::info!(
            program = %wrapped_command.program.display(),
            arg_count = wrapped_command.args.len(),
            prompt_len = req.prompt.len(),
            args = %args_debug.join(" | "),
            "claude execute_stream: full command args"
        );

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

        // ETXTBSY (error 26) occurs on Linux when a security scanner or indexer
        // briefly opens the executable for writing after it is written. Retry once.
        let spawn_result = cmd.spawn();
        let child = match spawn_result {
            Ok(child) => child,
            Err(ref e) if e.raw_os_error() == Some(26) => {
                tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                cmd.spawn().map_err(|error| {
                    harness_core::error::HarnessError::AgentExecution(format!(
                        "failed to run claude: {error}"
                    ))
                })?
            }
            Err(error) => {
                return Err(harness_core::error::HarnessError::AgentExecution(format!(
                    "failed to run claude: {error}"
                )));
            }
        };
        let mut child = crate::ManagedChild::new(child, "claude execute_stream");

        let stderr_capture = Arc::new(Mutex::new(String::new()));
        let mut stderr_task = None;
        if let Some(stderr) = child.inner_mut().stderr.take() {
            let agent = self.name().to_string();
            let captured = Arc::clone(&stderr_capture);
            stderr_task = Some(tokio::spawn(async move {
                filter_agent_stderr_with_capture(stderr, &agent, Some(captured)).await;
            }));
        }

        let idle_timeout = self
            .stream_timeout_secs
            .filter(|&s| s > 0)
            .map(std::time::Duration::from_secs);
        let stream_result = stream_claude_code_output(child.inner_mut(), &tx, idle_timeout).await;
        let stream_send_failed = matches!(
            &stream_result,
            Err(harness_core::error::HarnessError::AgentExecution(message))
                if message.contains("stream send failed")
        );
        let stream_process_exited = matches!(
            &stream_result,
            Err(harness_core::error::HarnessError::AgentExecution(message))
                if message.contains("claude exited with")
        );
        if stream_result.is_err() && !stream_process_exited {
            child.terminate_now();
        }
        if stream_send_failed {
            return Err(stream_result.expect_err("stream send failures return an error"));
        }
        if let Some(stderr_task) = stderr_task {
            let _ = stderr_task.await;
        }
        if let Err(error) = stream_result {
            let stderr = captured_stderr_tail(&stderr_capture);
            return Err(enrich_stream_exit_error(error, &stderr));
        }
        send_stream_item(&tx, StreamItem::Done, self.name(), "done").await?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "claude_tests.rs"]
mod tests;
