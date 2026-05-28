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
    let agent = CodexAgent::with_cloud(PathBuf::from("codex"), cloud, SandboxMode::WorkspaceWrite);
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
async fn setup_secret_is_available_in_setup_but_removed_for_agent_phase() -> anyhow::Result<()> {
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
        codex_sandbox_mode(SandboxMode::ReadOnlyWithNetwork),
        "read-only"
    );
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
fn base_args_uses_request_reasoning_effort_and_sandbox_without_approval_flag() {
    let agent = CodexAgent::new(PathBuf::from("codex"), SandboxMode::DangerFullAccess);
    let request = AgentRequest {
        prompt: "ping".to_string(),
        project_root: PathBuf::from("/tmp/project"),
        reasoning_effort: Some("medium".to_string()),
        sandbox_mode: Some(SandboxMode::ReadOnly),
        approval_policy: Some("on-request".to_string()),
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
    assert!(!args.iter().any(|arg| arg == "-a" || arg == "on-request"));
    assert_eq!(args.last().map(String::as_str), Some("ping"));
}

#[test]
fn base_args_configures_read_only_with_network_profile() {
    let agent = CodexAgent::new(PathBuf::from("codex"), SandboxMode::DangerFullAccess);
    let request = AgentRequest {
        prompt: "ping".to_string(),
        project_root: PathBuf::from("/tmp/project"),
        sandbox_mode: Some(SandboxMode::ReadOnlyWithNetwork),
        ..Default::default()
    };

    let args: Vec<String> = agent
        .base_args(&request)
        .iter()
        .map(|value| value.to_string_lossy().to_string())
        .collect();

    assert!(!args.iter().any(|arg| arg == "-s"));
    assert!(args.windows(2).any(|window| {
        window
            == [
                "-c",
                "default_permissions=\"harness_read_only_with_network\"",
            ]
    }));
    assert!(args.windows(2).any(|window| {
        window == [
            "-c",
            "permissions.harness_read_only_with_network.filesystem={\":minimal\"=\"read\",\":project_roots\"={\".\"=\"read\"}}",
        ]
    }));
    assert!(args.windows(2).any(|window| {
        window
            == [
                "-c",
                "permissions.harness_read_only_with_network.network.enabled=true",
            ]
    }));
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
