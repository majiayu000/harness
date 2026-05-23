use super::*;
use harness_core::{agent::StreamItem, types::Item};
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;

// ── stream_child_output ──────────────────────────────────────────────────

/// stream_child_output returns Ok and collects all lines when the child
/// exits successfully.
#[tokio::test]
async fn stream_child_output_collects_all_lines_and_returns_output() {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg("printf 'line1\\nline2\\nline3\\n'")
        .stdout(std::process::Stdio::piped())
        .stdin(std::process::Stdio::null())
        .spawn()
        .expect("spawn sh");

    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    let result = stream_child_output(&mut child, &tx, "test-agent", None)
        .await
        .expect("stream_child_output should succeed");

    assert_eq!(
        result, "line1\nline2\nline3\n",
        "returned output must be exactly the three lines with newlines, got: {result:?}"
    );

    // Drain channel to collect all sent items.
    drop(tx);
    let mut items = Vec::new();
    while let Ok(item) = rx.try_recv() {
        items.push(item);
    }

    let delta_count = items
        .iter()
        .filter(|item| matches!(item, StreamItem::MessageDelta { .. }))
        .count();
    assert_eq!(
        delta_count, 3,
        "expected exactly 3 deltas (one per line), got {delta_count}"
    );
}

/// Every line from the child process becomes a MessageDelta before
/// ItemCompleted is emitted.
#[tokio::test]
async fn stream_child_output_emits_deltas_before_item_completed() {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg("printf 'alpha\\nbeta\\n'")
        .stdout(std::process::Stdio::piped())
        .stdin(std::process::Stdio::null())
        .spawn()
        .expect("spawn sh");

    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    stream_child_output(&mut child, &tx, "test-agent", None)
        .await
        .expect("stream_child_output should succeed");
    drop(tx);

    let mut items = Vec::new();
    while let Some(item) = rx.recv().await {
        items.push(item);
    }

    let last_delta_pos = items
        .iter()
        .rposition(|item| matches!(item, StreamItem::MessageDelta { .. }))
        .expect("at least one MessageDelta expected");
    let completed_pos = items
        .iter()
        .position(|item| matches!(item, StreamItem::ItemCompleted { .. }))
        .expect("ItemCompleted expected");
    assert!(
            last_delta_pos < completed_pos,
            "all MessageDeltas must precede ItemCompleted (last delta at {last_delta_pos}, completed at {completed_pos})"
        );
}

/// The ItemCompleted payload must carry the full accumulated output.
#[tokio::test]
async fn stream_child_output_item_completed_contains_full_output() {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg("printf 'hello\\nworld\\n'")
        .stdout(std::process::Stdio::piped())
        .stdin(std::process::Stdio::null())
        .spawn()
        .expect("spawn sh");

    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    stream_child_output(&mut child, &tx, "test-agent", None)
        .await
        .expect("stream_child_output should succeed");
    drop(tx);

    let mut items = Vec::new();
    while let Some(item) = rx.recv().await {
        items.push(item);
    }

    let completed = items
        .iter()
        .find(|item| matches!(item, StreamItem::ItemCompleted { .. }))
        .expect("ItemCompleted expected");
    match completed {
        StreamItem::ItemCompleted {
            item: Item::AgentReasoning { content },
        } => {
            assert_eq!(
                    content, "hello\nworld\n",
                    "ItemCompleted content must be exactly the full accumulated output, got: {content:?}"
                );
        }
        other => panic!("unexpected ItemCompleted payload: {other:?}"),
    }
}

/// stream_child_output must return Err when the child exits with a non-zero
/// status code.
#[tokio::test]
async fn stream_child_output_fails_on_nonzero_exit() {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg("printf \"You've hit your limit\\n\"; exit 1")
        .stdout(std::process::Stdio::piped())
        .stdin(std::process::Stdio::null())
        .spawn()
        .expect("spawn sh");

    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    let result = stream_child_output(&mut child, &tx, "test-agent", None).await;
    assert!(result.is_err(), "expected Err on non-zero exit");
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("test-agent"),
        "error message must identify the agent, got: {msg}"
    );
    assert!(
        msg.contains("stdout_tail=[You've hit your limit"),
        "error message must preserve stdout tail for failure classification, got: {msg}"
    );
}

/// stream_child_output must propagate a send error when the receiver has
/// been dropped before any output arrives.
#[tokio::test]
async fn stream_child_output_fails_when_channel_closed_before_output() {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg("printf 'some output\\n'")
        .stdout(std::process::Stdio::piped())
        .stdin(std::process::Stdio::null())
        .spawn()
        .expect("spawn sh");

    let (tx, rx) = tokio::sync::mpsc::channel::<StreamItem>(1);
    drop(rx);

    let result = timeout(
        Duration::from_secs(5),
        stream_child_output(&mut child, &tx, "test-agent", None),
    )
    .await
    .expect("stream_child_output should not hang");

    assert!(result.is_err(), "expected Err when receiver is dropped");
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("stream send failed"),
        "error must report send failure, got: {msg}"
    );

    // Reap the child to avoid zombie processes on Unix.
    // kill() may fail if the process already exited naturally; log but don't fail.
    if let Err(e) = child.kill().await {
        eprintln!("child kill (may already have exited): {e}");
    }
    if let Err(e) = child.wait().await {
        eprintln!("child wait: {e}");
    }
}

/// stream_child_output returns Err with a "zombie" message when no output
/// arrives within the configured idle timeout.
#[tokio::test]
async fn stream_child_output_idle_timeout_terminates_zombie() {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg("sleep 60")
        .stdout(std::process::Stdio::piped())
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn sh");

    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    let result = stream_child_output(
        &mut child,
        &tx,
        "test-agent",
        Some(Duration::from_millis(200)),
    )
    .await;

    assert!(result.is_err(), "expected Err on idle timeout");
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("zombie"),
        "error must mention zombie connection, got: {msg}"
    );
    assert!(
        msg.contains("test-agent"),
        "error must identify the agent, got: {msg}"
    );
}

// ── filter_agent_stderr ──────────────────────────────────────────────────

/// Spawn a child whose stderr contains mixed lines, run filter_agent_stderr,
/// and verify it completes without panicking (behavioral smoke test).
#[tokio::test]
async fn filter_agent_stderr_drains_without_panic() {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(
            "echo 'Compiling foo v0.1' >&2; \
                 echo 'error[E0308]: mismatched types' >&2; \
                 echo 'warning: unused import' >&2; \
                 echo 'test result: ok. 5 passed' >&2",
        )
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn sh");

    let stderr = child.stderr.take().expect("stderr piped");
    filter_agent_stderr(stderr, "test-agent").await;
    child.wait().await.expect("child wait");
}

#[test]
fn log_captured_stderr_does_not_panic_on_empty() {
    log_captured_stderr("", "agent");
}

#[test]
fn log_captured_stderr_truncates_long_lines() {
    let long_line = "x".repeat(2000);
    // Should not panic
    log_captured_stderr(&long_line, "agent");
}

#[test]
fn diagnostics_only_stderr_does_not_promote_markdown_context_lines() {
    let disposition = stderr_line_disposition(
        "110:- Full cargo test --workspace ... -Dwarnings",
        StderrHandling::DiagnosticsOnly,
    );
    assert_eq!(disposition, Some(StderrLineDisposition::Debug));
}

#[test]
fn warning_classifier_preserves_real_warning_lines() {
    let disposition =
        stderr_line_disposition("warning: unused import", StderrHandling::ClassifyWarnings);
    assert_eq!(disposition, Some(StderrLineDisposition::Warning));
}

#[test]
fn looks_like_code_content_matches_indented_rust_fragments() {
    assert!(looks_like_code_content(
        "                        tracing::warn!("
    ));
    assert!(looks_like_code_content(
        "                                error = %e,"
    ));
    assert!(looks_like_code_content(
            "                                \"scheduler: failed to load registry project config, skipping\""
        ));
}

#[test]
fn looks_like_code_content_matches_patch_lines() {
    assert!(looks_like_code_content(
        "+        // Incomplete %XX should be left as-is, not panic"
    ));
    assert!(looks_like_code_content(
        "+                Err(e) => tracing::warn!("
    ));
}

#[test]
fn looks_like_code_content_does_not_hide_real_warning_lines() {
    assert!(!looks_like_code_content("warning: unused import"));
    assert!(!looks_like_code_content(
        "agent execution failed: codex exited with status 1"
    ));
}

#[test]
fn codex_analytics_stderr_is_agent_internal() {
    assert!(is_agent_internal(
        "WARN codex_analytics::client: events failed with status 403 Forbidden"
    ));
}

#[test]
fn looks_like_git_commit_line_matches_history_output() {
    assert!(looks_like_git_commit_line(
        "7f84b9a fix(lifecycle): add Item::Error for agent-not-found and stall paths"
    ));
    assert!(looks_like_git_commit_line(
            "fb15c77 Merge pull request #352 from majiayu000/feat/issue-88-warn-missing-preflight-skill"
        ));
}

#[test]
fn looks_like_search_result_matches_path_line_output() {
    assert!(looks_like_search_result(
            "./rules/go/quality.md:20:Return errors instead. `panic` only in `main()` for truly unrecoverable states."
        ));
    assert!(looks_like_search_result(
            "./crates/harness-server/src/http/background.rs:382:                            format!(\"startup recovery: failed to acquire concurrency permit: {e}\");"
        ));
    assert!(!looks_like_search_result(
        "thread 'main' panicked at src/main.rs:10:5:"
    ));
}

#[test]
fn looks_like_markdown_prompt_line_matches_headings_and_bullets() {
    assert!(looks_like_markdown_prompt_line("## Warnings"));
    assert!(looks_like_markdown_prompt_line(
        "- <action to resolve block/warn>"
    ));
    assert!(looks_like_markdown_prompt_line(
        "Pass: <N> | Warn: <N> | Block: <N>"
    ));
}
