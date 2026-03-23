use harness_core::{HarnessError, Item, StreamItem};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc::Sender;

const STDERR_ERROR_KEYWORDS: &[&str] = &[
    "error",
    "warn",
    "warning",
    "failed",
    "fatal",
    "panic",
    "exception",
];
const MAX_STDERR_LINE_LEN: usize = 1000;

/// Agent-internal line prefixes that should never surface as WARN.
/// These are progress/reasoning lines emitted by Codex and similar agents,
/// not actionable errors for the harness operator.
const AGENT_INTERNAL_PREFIXES: &[&str] = &[
    // ISO-8601 timestamps from Codex internal tracing
    "2025-",
    "2026-",
    "2027-",
    // Codex session header lines
    "--------",
    "workdir:",
    "model:",
    "provider:",
    "approval:",
    "sandbox:",
    "reasoning effort:",
    "reasoning summaries:",
    "session id:",
    "mcp startup:",
    // Codex reasoning/exec progress lines
    "codex ",
    "exec ",
    "tokens used",
    // Cargo/rustc build output that agents emit to stderr
    "compiling ",
    "   compiling ",
    "    checking ",
    "    finished ",
];

fn is_agent_internal(line: &str) -> bool {
    let lower = line.to_lowercase();
    AGENT_INTERNAL_PREFIXES
        .iter()
        .any(|prefix| lower.starts_with(prefix))
}

/// Read agent stderr line-by-line. Lines matching error keywords are logged
/// at warn level; agent-internal progress lines are always debug.
pub(crate) async fn filter_agent_stderr(stderr: tokio::process::ChildStderr, agent_name: &str) {
    let reader = BufReader::new(stderr);
    let mut lines = reader.lines();

    while let Ok(Some(line)) = lines.next_line().await {
        let trimmed = &line[..line.len().min(MAX_STDERR_LINE_LEN)];
        if trimmed.is_empty() {
            continue;
        }
        if is_agent_internal(trimmed) {
            tracing::debug!(agent = agent_name, "{trimmed}");
            continue;
        }
        let lower = trimmed.to_lowercase();
        if STDERR_ERROR_KEYWORDS.iter().any(|kw| lower.contains(kw)) {
            tracing::warn!(agent = agent_name, "{trimmed}");
        } else {
            tracing::debug!(agent = agent_name, "{trimmed}");
        }
    }
}

/// Log stderr captured from a non-streaming `output()` call.
pub(crate) fn log_captured_stderr(stderr: &str, agent_name: &str) {
    for line in stderr.lines() {
        let trimmed = &line[..line.len().min(MAX_STDERR_LINE_LEN)];
        if trimmed.is_empty() {
            continue;
        }
        if is_agent_internal(trimmed) {
            tracing::debug!(agent = agent_name, "{trimmed}");
            continue;
        }
        let lower = trimmed.to_lowercase();
        if STDERR_ERROR_KEYWORDS.iter().any(|kw| lower.contains(kw)) {
            tracing::warn!(agent = agent_name, "{trimmed}");
        } else {
            tracing::debug!(agent = agent_name, "{trimmed}");
        }
    }
}

pub(crate) async fn send_stream_item(
    tx: &Sender<StreamItem>,
    item: StreamItem,
    agent_name: &str,
    item_label: &'static str,
) -> harness_core::Result<()> {
    tx.send(item).await.map_err(|err| {
        tracing::error!(
            agent = agent_name,
            stream_item = item_label,
            error = %err,
            "failed to send stream item"
        );
        HarnessError::AgentExecution(format!(
            "{agent_name} stream send failed while sending {item_label}: {err}"
        ))
    })
}

/// Read stdout from a spawned child process line-by-line, stream deltas via `tx`,
/// wait for exit, and return the collected output.
///
/// `idle_timeout` sets the maximum time to wait for the next line. When a line
/// is not received within the timeout the subprocess is considered a zombie: an
/// error is returned and the caller's `kill_on_drop(true)` child is dropped,
/// terminating the process.
pub(crate) async fn stream_child_output(
    child: &mut tokio::process::Child,
    tx: &Sender<StreamItem>,
    agent_name: &str,
    idle_timeout: Option<Duration>,
) -> harness_core::Result<String> {
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| HarnessError::AgentExecution(format!("{agent_name} stdout unavailable")))?;

    let mut lines = BufReader::new(stdout).lines();
    let mut output = String::new();

    loop {
        let maybe_line = if let Some(dur) = idle_timeout {
            tokio::time::timeout(dur, lines.next_line())
                .await
                .map_err(|_| {
                    HarnessError::AgentExecution(format!(
                        "{agent_name} stream idle timeout after {}s: zombie connection terminated",
                        dur.as_secs()
                    ))
                })?
                .map_err(|error| {
                    HarnessError::AgentExecution(format!(
                        "failed reading {agent_name} stdout: {error}"
                    ))
                })?
        } else {
            lines.next_line().await.map_err(|error| {
                HarnessError::AgentExecution(format!("failed reading {agent_name} stdout: {error}"))
            })?
        };
        let Some(line) = maybe_line else {
            break;
        };
        let delta = format!("{line}\n");
        output.push_str(&delta);
        send_stream_item(
            tx,
            StreamItem::MessageDelta { text: delta },
            agent_name,
            "message_delta",
        )
        .await?;
    }

    let status = child.wait().await.map_err(|error| {
        HarnessError::AgentExecution(format!("failed waiting for {agent_name} process: {error}"))
    })?;
    if !status.success() {
        return Err(HarnessError::AgentExecution(format!(
            "{agent_name} exited with {status}"
        )));
    }

    send_stream_item(
        tx,
        StreamItem::ItemCompleted {
            item: Item::AgentReasoning {
                content: output.clone(),
            },
        },
        agent_name,
        "item_completed",
    )
    .await?;

    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::{Item, StreamItem};
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
            .arg("exit 1")
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
}
