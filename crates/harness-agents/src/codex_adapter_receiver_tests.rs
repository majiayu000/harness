use super::*;
use harness_core::agent::AgentAdapter;
use std::collections::HashMap;

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
        state.stdout_lines = Some(BufReader::new(stdout).lines());
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
