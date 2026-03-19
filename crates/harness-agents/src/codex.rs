use crate::cloud_setup;
use crate::streaming::{
    filter_agent_stderr, log_captured_stderr, send_stream_item, stream_child_output,
};
use async_trait::async_trait;
use harness_core::SandboxMode;
use harness_core::{
    AgentRequest, AgentResponse, Capability, CodeAgent, CodexAgentConfig, CodexCloudConfig,
    StreamItem, TokenUsage,
};
use harness_sandbox::{wrap_command, SandboxSpec};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::Command;

pub struct CodexAgent {
    pub cli_path: PathBuf,
    pub cloud: CodexCloudConfig,
    pub sandbox_mode: SandboxMode,
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
            cloud,
            sandbox_mode,
        }
    }

    pub fn from_config(config: CodexAgentConfig, sandbox_mode: SandboxMode) -> Self {
        Self::with_cloud(config.cli_path, config.cloud, sandbox_mode)
    }

    async fn run_setup_phase(&self, project_root: &Path) -> harness_core::Result<()> {
        cloud_setup::run_setup_phase(&self.cloud, project_root).await
    }

    fn base_args(&self, req: &AgentRequest) -> Vec<OsString> {
        let mut args = vec![
            OsString::from("exec"),
            OsString::from("--skip-git-repo-check"),
            OsString::from("-s"),
            OsString::from(codex_sandbox_mode(self.sandbox_mode)),
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

#[async_trait]
impl CodeAgent for CodexAgent {
    fn name(&self) -> &str {
        "codex"
    }

    fn capabilities(&self) -> Vec<Capability> {
        vec![Capability::Read, Capability::Write, Capability::Execute]
    }

    async fn execute(&self, req: AgentRequest) -> harness_core::Result<AgentResponse> {
        self.run_setup_phase(&req.project_root).await?;

        let base_args = self.base_args(&req);
        let sandbox_spec = SandboxSpec::new(self.sandbox_mode, &req.project_root);
        let wrapped_command =
            wrap_command(&self.cli_path, &base_args, &sandbox_spec).map_err(|error| {
                harness_core::HarnessError::AgentExecution(format!(
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
        crate::strip_claude_env(&mut cmd);
        cmd.envs(&req.env_vars);

        if self.cloud.enabled {
            for key in &self.cloud.setup_secret_env {
                cmd.env_remove(key);
            }
        }

        let child = cmd.spawn().map_err(|err| {
            harness_core::HarnessError::AgentExecution(format!("failed to run codex: {err}"))
        })?;
        let output = child.wait_with_output().await.map_err(|err| {
            harness_core::HarnessError::AgentExecution(format!("failed to wait for codex: {err}"))
        })?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        log_captured_stderr(&stderr, self.name());

        if !output.status.success() {
            return Err(harness_core::HarnessError::AgentExecution(format!(
                "codex exited with {}: {stderr}",
                output.status
            )));
        }

        Ok(AgentResponse {
            output: stdout,
            stderr,
            items: Vec::new(),
            token_usage: TokenUsage::default(),
            model: "codex".to_string(),
            exit_code: output.status.code(),
        })
    }

    async fn execute_stream(
        &self,
        req: AgentRequest,
        tx: tokio::sync::mpsc::Sender<StreamItem>,
    ) -> harness_core::Result<()> {
        self.run_setup_phase(&req.project_root).await?;

        let base_args = self.base_args(&req);
        let sandbox_spec = SandboxSpec::new(self.sandbox_mode, &req.project_root);
        let wrapped_command =
            wrap_command(&self.cli_path, &base_args, &sandbox_spec).map_err(|error| {
                harness_core::HarnessError::AgentExecution(format!(
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
        crate::strip_claude_env(&mut cmd);
        cmd.envs(&req.env_vars);

        if self.cloud.enabled {
            for key in &self.cloud.setup_secret_env {
                cmd.env_remove(key);
            }
        }

        let mut child = cmd.spawn().map_err(|error| {
            harness_core::HarnessError::AgentExecution(format!("failed to run codex: {error}"))
        })?;

        if let Some(stderr) = child.stderr.take() {
            let agent = self.name().to_string();
            tokio::spawn(async move {
                filter_agent_stderr(stderr, &agent).await;
            });
        }

        stream_child_output(&mut child, &tx, self.name()).await?;
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
    use harness_core::Item;
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
printf 'hello\n'
sleep 0.2
printf 'world\n'
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
    async fn execute_stream_cancel_path_converges_when_receiver_dropped_mid_stream() {
        let (dir, script) = write_executable_script(
            r#"
printf 'first\n'
sleep 2
printf 'second\n'
sleep 2
printf 'third\n'
"#,
        );
        let agent = CodexAgent::new(script, SandboxMode::DangerFullAccess);
        let request = AgentRequest {
            prompt: "ignored".to_string(),
            project_root: dir.path().to_path_buf(),
            ..AgentRequest::default()
        };

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let handle = tokio::spawn(async move { agent.execute_stream(request, tx).await });

        let first = timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("timed out waiting for first stream item");

        // On slow CI the process may exit before the first item arrives.
        // Either way, dropping the receiver must cause convergence.
        if let Some(item) = first {
            assert!(
                matches!(item, StreamItem::MessageDelta { .. }),
                "expected first event to be delta, got {item:?}"
            );
        }

        drop(rx);

        let result = timeout(Duration::from_secs(15), handle)
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
        let marker = dir.path().join("timeout-marker.txt");
        let script = dir.path().join("mock-codex-timeout.sh");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\nset -eu\nsleep 5\necho reached > \"{}\"\n",
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

        let timed = timeout(
            Duration::from_millis(500),
            agent.execute_stream(request, tx),
        )
        .await;
        assert!(timed.is_err(), "expected timeout on long-running stream");

        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !marker.exists(),
            "process should be killed when stream future is dropped on timeout"
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
}
