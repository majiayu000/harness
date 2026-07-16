use super::*;

#[tokio::test]
async fn execute_stream_kills_descendant_that_keeps_stderr_pipe_open_after_root_exit() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let descendant_marker = dir.path().join("stream-descendant-started.txt");
    let marker = dir.path().join("stream-descendant-reached.txt");
    let (script_dir, script) = write_executable_script(&format!(
        r#"
( echo descendant > "{}"; sleep 1; echo reached > "{}"; sleep 30 ) >&2 &
while [ ! -f "{}" ]; do sleep 0.01; done
printf 'root done\n'
"#,
        descendant_marker.display(),
        marker.display(),
        descendant_marker.display()
    ));
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
    let mut handle = tokio::spawn(async move { agent.execute_stream(request, tx).await });
    if !wait_for_path(&descendant_marker, Duration::from_secs(20)).await {
        handle.abort();
        let abort_outcome = timeout(Duration::from_secs(2), &mut handle).await;
        panic!(
            "timed out waiting for descendant startup marker at `{}`; abort outcome: {abort_outcome:?}",
            descendant_marker.display()
        );
    }
    timeout(Duration::from_secs(5), handle)
        .await
        .expect("execute_stream should not wait for descendant-held stderr")
        .expect("execute_stream task should join")
        .expect("stream execution should succeed");

    assert!(
        descendant_marker.exists(),
        "descendant should start before process-group cleanup runs"
    );
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(
        !marker.exists(),
        "stream cleanup should kill descendants before they can mutate the workspace"
    );
    drop(script_dir);
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
    let descendant_marker = dir.path().join("timeout-descendant-started.txt");
    let marker = dir.path().join("timeout-marker.txt");
    let script = dir.path().join("mock-claude-timeout.sh");
    // sync_all() ensures the kernel flushes dirty pages before exec;
    // without it, Linux can return ETXTBSY on some CI kernels.
    {
        use std::io::Write;
        let mut f = fs::File::create(&script).expect("create timeout script");
        f.write_all(
            format!(
                "#!/bin/sh\nset -eu\necho started > \"{}\"\n( echo descendant > \"{}\"; sleep 1; echo reached > \"{}\" ) &\nsleep 5\n",
                started_marker.display(),
                descendant_marker.display(),
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
    if !wait_for_path(&descendant_marker, Duration::from_secs(10)).await {
        let outcome = timeout(Duration::from_secs(1), handle).await;
        panic!("stream process did not start descendant before cancellation: {outcome:?}");
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

    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(
        !marker.exists(),
        "process group descendant should be killed when stream future is dropped"
    );
}
