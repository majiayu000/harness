use crate::streaming::{
    captured_stderr_tail, enrich_stream_exit_error, filter_agent_stderr_with_capture,
    log_captured_stderr, send_stream_item, stream_child_output,
};
use async_trait::async_trait;
use harness_core::config::agents::SandboxMode;
use harness_core::{
    agent::AgentRequest, agent::AgentResponse, agent::CodeAgent, agent::StreamItem,
    types::Capability, types::ReasoningBudget, types::TokenUsage,
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
            OsString::from("text"),
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

        let output = child.wait_with_output().await.map_err(|e| {
            harness_core::error::HarnessError::AgentExecution(format!(
                "failed to wait for claude: {e}"
            ))
        })?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        log_captured_stderr(&stderr, self.name());

        if !output.status.success() {
            let stdout_tail: String = if stdout.chars().count() > 500 {
                stdout
                    .chars()
                    .rev()
                    .take(500)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect()
            } else {
                stdout.clone()
            };
            return Err(harness_core::error::HarnessError::AgentExecution(format!(
                "claude exited with {}: stderr=[{}] stdout_tail=[{}]",
                output.status, stderr, stdout_tail
            )));
        }

        Ok(AgentResponse {
            output: stdout,
            stderr,
            items: Vec::new(),
            token_usage: TokenUsage::default(),
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
        let mut child = match spawn_result {
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

        let stderr_capture = Arc::new(Mutex::new(String::new()));
        let mut stderr_task = None;
        if let Some(stderr) = child.stderr.take() {
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
        let stream_result = stream_child_output(&mut child, &tx, self.name(), idle_timeout).await;
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
        if let Err(error) = stream_result {
            let stderr = captured_stderr_tail(&stderr_capture);
            return Err(enrich_stream_exit_error(error, &stderr));
        }
        send_stream_item(
            &tx,
            StreamItem::TokenUsage {
                usage: TokenUsage::default(),
            },
            self.name(),
            "token_usage",
        )
        .await?;
        send_stream_item(&tx, StreamItem::Done, self.name(), "done").await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::{types::ExecutionPhase, types::Item, types::ReasoningBudget};
    use std::fs;
    use std::time::Duration;
    use tokio::time::timeout;

    fn args_to_strings(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|a| a.to_string_lossy().to_string())
            .collect()
    }

    async fn wait_for_path(path: &std::path::Path, timeout_duration: Duration) -> bool {
        timeout(timeout_duration, async {
            loop {
                if path.exists() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .is_ok()
    }

    #[test]
    fn base_args_full_profile_uses_dangerously_skip_permissions() {
        let agent = ClaudeCodeAgent::new(
            PathBuf::from("claude"),
            "test-model".to_string(),
            SandboxMode::DangerFullAccess,
        );
        let req = AgentRequest {
            allowed_tools: None, // Full profile — no restriction
            ..AgentRequest::default()
        };
        let args = args_to_strings(&agent.base_args(&req));
        assert!(
            args.contains(&"--dangerously-skip-permissions".to_string()),
            "Full profile must use --dangerously-skip-permissions; got: {args:?}"
        );
        assert!(
            !args.contains(&"--allowedTools".to_string()),
            "Full profile must NOT use --allowedTools; got: {args:?}"
        );
    }

    #[test]
    fn base_args_standard_profile_uses_allowed_tools_flag() {
        let agent = ClaudeCodeAgent::new(
            PathBuf::from("claude"),
            "test-model".to_string(),
            SandboxMode::DangerFullAccess,
        );
        let req = AgentRequest {
            allowed_tools: Some(vec![
                "Read".to_string(),
                "Write".to_string(),
                "Edit".to_string(),
                "Bash".to_string(),
            ]),
            ..AgentRequest::default()
        };
        let args = args_to_strings(&agent.base_args(&req));
        assert!(
            args.contains(&"--allowedTools".to_string()),
            "Standard profile must use --allowedTools; got: {args:?}"
        );
        assert!(
            args.contains(&"--permission-mode".to_string()),
            "Standard profile must use --permission-mode; got: {args:?}"
        );
        let permission_mode_value = args
            .iter()
            .skip_while(|a| *a != "--permission-mode")
            .nth(1)
            .cloned()
            .unwrap_or_default();
        assert_eq!(
            permission_mode_value, "bypassPermissions",
            "--permission-mode value must be bypassPermissions (camelCase); got: {permission_mode_value:?}"
        );
        assert!(
            !args.contains(&"--dangerously-skip-permissions".to_string()),
            "Standard profile must NOT use --dangerously-skip-permissions; got: {args:?}"
        );
        let tools_value = args
            .iter()
            .skip_while(|a| *a != "--allowedTools")
            .nth(1)
            .cloned()
            .unwrap_or_default();
        assert!(
            tools_value.contains("Read")
                && tools_value.contains("Write")
                && tools_value.contains("Edit")
                && tools_value.contains("Bash"),
            "allowedTools value must list all Standard tools; got: {tools_value:?}"
        );
    }

    #[test]
    fn base_args_read_only_profile_uses_allowed_tools_flag() {
        let agent = ClaudeCodeAgent::new(
            PathBuf::from("claude"),
            "test-model".to_string(),
            SandboxMode::DangerFullAccess,
        );
        let req = AgentRequest {
            allowed_tools: Some(vec![
                "Read".to_string(),
                "Grep".to_string(),
                "Glob".to_string(),
            ]),
            ..AgentRequest::default()
        };
        let args = args_to_strings(&agent.base_args(&req));
        assert!(
            args.contains(&"--allowedTools".to_string()),
            "ReadOnly profile must use --allowedTools; got: {args:?}"
        );
        assert!(
            !args.contains(&"--dangerously-skip-permissions".to_string()),
            "ReadOnly profile must NOT use --dangerously-skip-permissions; got: {args:?}"
        );
        let tools_value = args
            .iter()
            .skip_while(|a| *a != "--allowedTools")
            .nth(1)
            .cloned()
            .unwrap_or_default();
        assert!(
            tools_value.contains("Read")
                && tools_value.contains("Grep")
                && tools_value.contains("Glob"),
            "allowedTools value must list all ReadOnly tools; got: {tools_value:?}"
        );
    }

    #[test]
    fn base_args_never_contains_both_permission_flags() {
        let agent = ClaudeCodeAgent::new(
            PathBuf::from("claude"),
            "test-model".to_string(),
            SandboxMode::DangerFullAccess,
        );
        for (label, allowed_tools) in [
            ("full", None),
            (
                "standard",
                Some(vec!["Read".to_string(), "Bash".to_string()]),
            ),
        ] {
            let req = AgentRequest {
                allowed_tools,
                ..AgentRequest::default()
            };
            let args = args_to_strings(&agent.base_args(&req));
            let has_skip = args.contains(&"--dangerously-skip-permissions".to_string());
            let has_allowed = args.contains(&"--allowedTools".to_string());
            assert!(
                !(has_skip && has_allowed),
                "Both --dangerously-skip-permissions and --allowedTools present for {label} profile: {args:?}"
            );
        }
    }

    #[test]
    fn resolve_model_uses_phase_when_budget_configured() {
        let budget = ReasoningBudget::default();
        let agent = ClaudeCodeAgent::new(
            PathBuf::from("claude"),
            "default-model".to_string(),
            SandboxMode::DangerFullAccess,
        )
        .with_reasoning_budget(budget);

        let req_planning = AgentRequest {
            execution_phase: Some(ExecutionPhase::Planning),
            ..AgentRequest::default()
        };
        let req_execution = AgentRequest {
            execution_phase: Some(ExecutionPhase::Execution),
            ..AgentRequest::default()
        };
        let req_validation = AgentRequest {
            execution_phase: Some(ExecutionPhase::Validation),
            ..AgentRequest::default()
        };
        let req_no_phase = AgentRequest::default();

        assert_eq!(agent.resolve_model(&req_planning), "claude-opus-4-6");
        assert_eq!(agent.resolve_model(&req_execution), "claude-sonnet-4-6");
        assert_eq!(agent.resolve_model(&req_validation), "claude-opus-4-6");
        // No phase → falls back to default_model
        assert_eq!(agent.resolve_model(&req_no_phase), "default-model");
    }

    #[test]
    fn resolve_model_falls_back_to_req_model_when_no_budget() {
        let agent = ClaudeCodeAgent::new(
            PathBuf::from("claude"),
            "default-model".to_string(),
            SandboxMode::DangerFullAccess,
        );

        let req_with_model = AgentRequest {
            model: Some("explicit-model".to_string()),
            execution_phase: Some(ExecutionPhase::Planning),
            ..AgentRequest::default()
        };
        let req_no_model = AgentRequest {
            execution_phase: Some(ExecutionPhase::Planning),
            ..AgentRequest::default()
        };

        // No budget → req.model takes precedence over phase
        assert_eq!(agent.resolve_model(&req_with_model), "explicit-model");
        // No budget, no req.model → default_model
        assert_eq!(agent.resolve_model(&req_no_model), "default-model");
    }

    #[test]
    fn base_args_uses_request_reasoning_effort_when_phase_is_absent() {
        let agent = ClaudeCodeAgent::new(
            PathBuf::from("claude"),
            "default-model".to_string(),
            SandboxMode::DangerFullAccess,
        );
        let req = AgentRequest {
            reasoning_effort: Some("medium".to_string()),
            ..AgentRequest::default()
        };

        let args = args_to_strings(&agent.base_args(&req));
        assert!(args
            .windows(2)
            .any(|window| window == ["--effort", "medium"]));
    }

    #[test]
    fn with_reasoning_budget_sets_field() {
        let budget = ReasoningBudget::default();
        let agent = ClaudeCodeAgent::new(
            PathBuf::from("claude"),
            "default-model".to_string(),
            SandboxMode::DangerFullAccess,
        )
        .with_reasoning_budget(budget);
        assert!(agent.reasoning_budget.is_some());
    }

    fn write_executable_script(script_body: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("create tempdir");
        let path = dir.path().join("mock-claude.sh");
        let script = format!("#!/bin/sh\nset -eu\n{script_body}\n");
        fs::write(&path, &script).expect("write script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&path).expect("script metadata").permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&path, perms).expect("set executable permissions");
        }
        (dir, path)
    }

    #[tokio::test]
    async fn execute_stream_returns_error_when_channel_closed() {
        let agent = ClaudeCodeAgent::new(
            PathBuf::from("/usr/bin/true"),
            "test-model".to_string(),
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
printf 'hello\n'
sleep 0.2
printf 'world\n'
"#,
        );
        let agent = ClaudeCodeAgent::new(
            script,
            "test-model".to_string(),
            SandboxMode::DangerFullAccess,
        );
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
    async fn execute_stream_classifies_quota_failure_from_stdout_tail() {
        let (dir, script) = write_executable_script(
            r#"
printf "You've hit your limit · resets 3pm (Asia/Shanghai)\n"
exit 1
"#,
        );
        let agent = ClaudeCodeAgent::new(
            script,
            "test-model".to_string(),
            SandboxMode::DangerFullAccess,
        );
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

        assert_eq!(
            err.turn_failure().expect("turn failure").kind,
            harness_core::types::TurnFailureKind::Quota
        );
        assert!(
            err.to_string()
                .contains("stdout_tail=[You've hit your limit"),
            "streamed claude failures must preserve stdout tail, got: {err}"
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
        let agent = ClaudeCodeAgent::new(
            script,
            "test-model".to_string(),
            SandboxMode::DangerFullAccess,
        );
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
            matches!(
                err.turn_failure().expect("turn failure").kind,
                harness_core::types::TurnFailureKind::Quota
            ),
            "expected late stderr to influence streamed exit classification, got: {err}"
        );
    }

    #[tokio::test]
    async fn execute_stream_cancel_path_converges_when_receiver_dropped_mid_stream() {
        let (dir, script) = write_executable_script(
            r#"
printf 'first\n'
sleep 0.3
printf 'second\n'
"#,
        );
        let agent = ClaudeCodeAgent::new(
            script,
            "test-model".to_string(),
            SandboxMode::DangerFullAccess,
        );
        let request = AgentRequest {
            prompt: "ignored".to_string(),
            project_root: dir.path().to_path_buf(),
            ..AgentRequest::default()
        };

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let handle = tokio::spawn(async move { agent.execute_stream(request, tx).await });

        let first = timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("timed out waiting for first stream item")
            .expect("stream closed before first item");
        assert!(
            matches!(first, StreamItem::MessageDelta { .. }),
            "expected first event to be delta, got {first:?}"
        );

        drop(rx);

        let result = timeout(Duration::from_secs(10), handle)
            .await
            .expect("execute_stream task should converge after cancellation")
            .expect("join should succeed");
        let err = result.expect_err("receiver drop should surface send failure");
        assert!(
            err.to_string().contains("stream send failed"),
            "expected stream send failure after cancellation, got: {err}"
        );
    }

    #[tokio::test]
    async fn execute_stream_timeout_drop_does_not_leave_hanging_process() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let started_marker = dir.path().join("timeout-started.txt");
        let marker = dir.path().join("timeout-marker.txt");
        let script = dir.path().join("mock-claude-timeout.sh");
        // sync_all() ensures the kernel flushes dirty pages before exec;
        // without it, Linux can return ETXTBSY on some CI kernels.
        {
            use std::io::Write;
            let mut f = fs::File::create(&script).expect("create timeout script");
            f.write_all(
                format!(
                    "#!/bin/sh\nset -eu\necho started > \"{}\"\nsleep 5\necho reached > \"{}\"\n",
                    started_marker.display(),
                    marker.display()
                )
                .as_bytes(),
            )
            .expect("write timeout script");
            f.sync_all().expect("sync timeout script");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&script)
                .expect("script metadata")
                .permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&script, perms).expect("set executable permissions");
        }

        // On Linux CI kernels (tmpfs), the inode write-access count may not be
        // fully settled immediately after close() + fsync. A short sleep lets
        // the kernel scheduler process the fd-close before execve() is called,
        // preventing ETXTBSY (os error 26). 200 ms is empirically sufficient
        // even on heavily loaded CI runners where 50 ms proved flaky.
        tokio::time::sleep(Duration::from_millis(200)).await;

        let agent = ClaudeCodeAgent::new(
            script,
            "test-model".to_string(),
            SandboxMode::DangerFullAccess,
        );
        let request = AgentRequest {
            prompt: "ignored".to_string(),
            project_root: dir.path().to_path_buf(),
            ..AgentRequest::default()
        };
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let handle = tokio::spawn(async move { agent.execute_stream(request, tx).await });

        if !wait_for_path(&started_marker, Duration::from_secs(10)).await {
            let outcome = timeout(Duration::from_secs(1), handle).await;
            panic!("stream process did not stay alive long enough to observe startup: {outcome:?}");
        }

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
}
