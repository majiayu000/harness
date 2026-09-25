use super::attempt::AttemptDeadlines;
use super::*;
use harness_core::agent::AgentAdapter;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

#[tokio::test]
#[cfg(unix)]
async fn closed_event_receiver_kills_and_reaps_app_server_process_group() -> anyhow::Result<()> {
    let mut command = tokio::process::Command::new("sh");
    command
        .arg("-c")
        .arg(
            r#"printf '%s\n' '{"method":"turn/started","params":{"threadId":"thread-1","turn":{"id":"turn-1"}}}'; sleep 60 & wait"#,
        )
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped());
    crate::set_process_group(&mut command);
    let mut child = command.spawn()?;
    let pid = child
        .id()
        .ok_or_else(|| anyhow::anyhow!("stub app-server should have a pid"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("stdout should be piped"))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("stdin should be piped"))?;

    let adapter = CodexAdapter::new(PathBuf::from("codex"));
    {
        let mut state = adapter.state.lock().await;
        state.child = Some(crate::ManagedChild::new(
            child,
            "codex app-server receiver test",
        ));
        state.stdin = Some(stdin);
        state.stdout_lines = Some(adapter.wrap_stdout(stdout));
        state.thread_id = Some("thread-1".into());
        state.child_workspace = Some(PathBuf::from("/tmp/project"));
    }
    let request = AgentRequest {
        prompt: "ping".into(),
        prompt_layers: None,
        project_root: PathBuf::from("/tmp/project"),
        permission_mode: Default::default(),
        model: None,
        reasoning_effort: None,
        execution_phase: None,
        sandbox_mode: None,
        approval_policy: None,
        allowed_tools: None,
        max_budget_usd: None,
        context: Vec::new(),
        timeout_secs: Some(5),
        env_vars: HashMap::new(),
        capability_token: None,
    };
    adapter.state.lock().await.spawn_policy_fingerprint = Some(
        crate::spawn_contract::adapter_spawn_policy_fingerprint(&request, adapter.sandbox_mode),
    );
    let (tx, rx) = mpsc::channel(1);
    drop(rx);

    let error = adapter
        .start_turn(request, tx)
        .await
        .expect_err("closed receiver must fail and reset the app-server");
    assert!(format!("{error}").contains("event receiver closed"));

    let state = adapter.state.lock().await;
    assert!(state.child.is_none());
    assert!(state.stdin.is_none());
    assert!(state.stdout_lines.is_none());
    assert!(state.thread_id.is_none());
    assert!(state.active_turn_id.is_none());
    assert!(state.child_workspace.is_none());
    drop(state);
    assert!(
        !crate::process_group_has_members(pid),
        "app-server process group {pid} survived receiver closure"
    );
    Ok(())
}

#[tokio::test]
#[cfg(unix)]
async fn full_event_receiver_does_not_block_stop_or_cleanup() -> anyhow::Result<()> {
    let mut command = tokio::process::Command::new("sh");
    // Emit turn/started then flood deltas so a capacity-1 live receiver blocks on send.
    command
        .arg("-c")
        .arg(
            r#"
printf '%s\n' '{"method":"turn/started","params":{"threadId":"thread-1","turn":{"id":"turn-1","status":"inProgress","items":[]}}}'
i=0
while [ "$i" -lt 200 ]; do
  printf '%s\n' '{"method":"item/agentMessage/delta","params":{"itemId":"item-1","threadId":"thread-1","turnId":"turn-1","delta":"x"}}'
  i=$((i + 1))
done
sleep 60 &
wait
"#,
        )
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped());
    crate::set_process_group(&mut command);
    let mut child = command.spawn()?;
    let pid = child
        .id()
        .ok_or_else(|| anyhow::anyhow!("stub app-server should have a pid"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("stdout should be piped"))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("stdin should be piped"))?;

    let adapter = Arc::new(CodexAdapter::new(PathBuf::from("codex")).with_deadlines(
        AttemptDeadlines {
            absolute_init: Duration::from_secs(5),
            frame_write: Duration::from_secs(2),
            stop_cleanup: Duration::from_secs(2),
        },
    ));
    {
        let mut state = adapter.state.lock().await;
        state.child = Some(crate::ManagedChild::new(
            child,
            "codex app-server full receiver test",
        ));
        state.stdin = Some(stdin);
        state.stdout_lines = Some(adapter.wrap_stdout(stdout));
        state.thread_id = Some("thread-1".into());
        state.child_workspace = Some(PathBuf::from("/tmp/project"));
    }
    let request = AgentRequest {
        prompt: "ping".into(),
        prompt_layers: None,
        project_root: PathBuf::from("/tmp/project"),
        permission_mode: Default::default(),
        model: None,
        reasoning_effort: None,
        execution_phase: None,
        sandbox_mode: None,
        approval_policy: None,
        allowed_tools: None,
        max_budget_usd: None,
        context: Vec::new(),
        timeout_secs: Some(5),
        env_vars: HashMap::new(),
        capability_token: None,
    };
    adapter.state.lock().await.spawn_policy_fingerprint = Some(
        crate::spawn_contract::adapter_spawn_policy_fingerprint(&request, adapter.sandbox_mode),
    );

    // Keep the receiver alive without draining so tx.send blocks once capacity fills.
    let (tx, _rx) = mpsc::channel(1);
    let turn: tokio::task::JoinHandle<harness_core::error::Result<()>> = {
        let adapter = Arc::clone(&adapter);
        tokio::spawn(async move { adapter.as_ref().start_turn(request, tx).await })
    };

    let started = Instant::now();
    while adapter.state.lock().await.active_turn_id.is_none() {
        if started.elapsed() > Duration::from_secs(15) {
            let _ = adapter.terminate_and_drain().await;
            let _ = turn.await;
            panic!("timed out waiting for remote turn id under a full receiver");
        }
        tokio::task::yield_now().await;
    }

    // Give the turn loop a chance to block on the full channel before stop.
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    let stop_started = Instant::now();
    adapter
        .terminate_and_drain()
        .await
        .expect("full receiver must not prevent stop/cleanup success");
    assert!(
        stop_started.elapsed() < Duration::from_secs(2),
        "terminate_and_drain must finish within the stop budget despite a full event receiver"
    );

    let turn_result = turn.await.expect("join");
    assert!(
        turn_result.is_err(),
        "turn blocked on a full receiver must end when stop cancels it"
    );

    let state = adapter.state.lock().await;
    assert!(state.child.is_none(), "child must be reaped after stop");
    assert!(state.stdin.is_none());
    assert!(state.stdout_lines.is_none());
    assert!(state.active_attempt.is_none());
    drop(state);
    assert!(
        !crate::process_group_has_members(pid),
        "app-server process group {pid} survived full-receiver stop"
    );
    Ok(())
}
