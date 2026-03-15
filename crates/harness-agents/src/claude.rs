use crate::streaming::{
    filter_agent_stderr, log_captured_stderr, send_stream_item, stream_child_output,
};
use async_trait::async_trait;
use harness_core::SandboxMode;
use harness_core::{
    AgentRequest, AgentResponse, Capability, CodeAgent, ReasoningBudget, StreamItem, TokenUsage,
};
use harness_sandbox::{wrap_command, SandboxSpec};
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::process::Command;

pub struct ClaudeCodeAgent {
    pub cli_path: PathBuf,
    pub default_model: String,
    pub sandbox_mode: SandboxMode,
    /// Per-phase model selection. When set, model is chosen based on
    /// `req.execution_phase`. Falls back to `req.model` or `default_model`.
    pub reasoning_budget: Option<ReasoningBudget>,
}

impl ClaudeCodeAgent {
    pub fn new(cli_path: PathBuf, default_model: String, sandbox_mode: SandboxMode) -> Self {
        Self {
            cli_path,
            default_model,
            sandbox_mode,
            reasoning_budget: None,
        }
    }

    /// Attach a ReasoningBudget for per-phase model selection.
    pub fn with_reasoning_budget(mut self, budget: ReasoningBudget) -> Self {
        self.reasoning_budget = Some(budget);
        self
    }

    fn resolve_model<'a>(&'a self, req: &'a AgentRequest) -> &'a str {
        if let (Some(budget), Some(phase)) = (&self.reasoning_budget, req.execution_phase) {
            return budget.model_for_phase(phase);
        }
        req.model.as_deref().unwrap_or(&self.default_model)
    }

    fn base_args(&self, req: &AgentRequest) -> Vec<OsString> {
        let model = self.resolve_model(req);
        let mut base_args = vec![
            OsString::from("-p"),
            OsString::from("--dangerously-skip-permissions"),
            OsString::from("--output-format"),
            OsString::from("text"),
            OsString::from("--model"),
            OsString::from(model),
            OsString::from("--verbose"),
        ];

        if !req.allowed_tools.is_empty() {
            base_args.push(OsString::from("--allowedTools"));
            base_args.push(OsString::from(req.allowed_tools.join(",")));
        }

        if let Some(budget) = req.max_budget_usd {
            base_args.push(OsString::from("--max-budget-usd"));
            base_args.push(OsString::from(budget.to_string()));
        }

        base_args.push(OsString::from(req.prompt.clone()));
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

    async fn execute(&self, req: AgentRequest) -> harness_core::Result<AgentResponse> {
        let model = self.resolve_model(&req).to_string();
        let base_args = self.base_args(&req);

        let sandbox_spec = SandboxSpec::new(self.sandbox_mode, &req.project_root);
        let wrapped_command =
            wrap_command(&self.cli_path, &base_args, &sandbox_spec).map_err(|error| {
                harness_core::HarnessError::AgentExecution(format!(
                    "sandbox setup failed for claude: {error}"
                ))
            })?;

        let mut cmd = Command::new(&wrapped_command.program);
        cmd.args(&wrapped_command.args)
            .current_dir(&req.project_root)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        crate::strip_claude_env(&mut cmd);
        crate::process_group::apply_process_group_management(&mut cmd);

        let child = cmd.spawn().map_err(|e| {
            harness_core::HarnessError::AgentExecution(format!("failed to run claude: {e}"))
        })?;
        let _pg_guard = crate::process_group::ProcessGroupGuard::new(child.id());

        let output = child.wait_with_output().await.map_err(|e| {
            harness_core::HarnessError::AgentExecution(format!("failed to run claude: {e}"))
        })?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        log_captured_stderr(&stderr, self.name());

        if !output.status.success() {
            return Err(harness_core::HarnessError::AgentExecution(format!(
                "claude exited with {}: {stderr}",
                output.status
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
    ) -> harness_core::Result<()> {
        let base_args = self.base_args(&req);
        let sandbox_spec = SandboxSpec::new(self.sandbox_mode, &req.project_root);
        let wrapped_command =
            wrap_command(&self.cli_path, &base_args, &sandbox_spec).map_err(|error| {
                harness_core::HarnessError::AgentExecution(format!(
                    "sandbox setup failed for claude: {error}"
                ))
            })?;

        let mut cmd = Command::new(&wrapped_command.program);
        cmd.args(&wrapped_command.args)
            .current_dir(&req.project_root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        crate::strip_claude_env(&mut cmd);
        crate::process_group::apply_process_group_management(&mut cmd);

        let mut child = cmd.spawn().map_err(|error| {
            harness_core::HarnessError::AgentExecution(format!("failed to run claude: {error}"))
        })?;
        let _pg_guard = crate::process_group::ProcessGroupGuard::new(child.id());

        if let Some(stderr) = child.stderr.take() {
            let agent = self.name().to_string();
            tokio::spawn(async move {
                filter_agent_stderr(stderr, &agent).await;
            });
        }

        stream_child_output(&mut child, &tx, self.name()).await?;
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
    use harness_core::{ExecutionPhase, Item, ReasoningBudget};
    use std::fs;
    use std::time::Duration;
    use tokio::time::timeout;

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

        assert_eq!(agent.resolve_model(&req_planning), "claude-opus-4-20250514");
        assert_eq!(
            agent.resolve_model(&req_execution),
            "claude-sonnet-4-20250514"
        );
        assert_eq!(
            agent.resolve_model(&req_validation),
            "claude-opus-4-20250514"
        );
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
        let (dir, script) = write_executable_script("sleep 5\n");
        let marker = dir.path().join("timeout-marker.txt");
        // Script must keep stdout open (via continuous output) so the stream
        // reader does not return early before the timeout fires.
        fs::write(
            &script,
            format!(
                "#!/bin/sh\nset -eu\nwhile true; do echo waiting; sleep 1; done\necho reached > \"{}\"\n",
                marker.display()
            ),
        )
        .expect("rewrite timeout script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&script)
                .expect("script metadata")
                .permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&script, perms).expect("set executable permissions");
        }

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

        let timed = timeout(Duration::from_secs(2), agent.execute_stream(request, tx)).await;
        assert!(timed.is_err(), "expected timeout on long-running stream");

        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            !marker.exists(),
            "process should be killed when stream future is dropped on timeout"
        );
    }
}
