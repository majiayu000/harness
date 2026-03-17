use harness_core::{HarnessError, Item, StreamItem};
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

/// Read agent stderr line-by-line. Lines matching error keywords are logged
/// at warn level; all others at debug level (invisible by default).
pub(crate) async fn filter_agent_stderr(stderr: tokio::process::ChildStderr, agent_name: &str) {
    let reader = BufReader::new(stderr);
    let mut lines = reader.lines();

    while let Ok(Some(line)) = lines.next_line().await {
        let trimmed = &line[..line.len().min(MAX_STDERR_LINE_LEN)];
        if trimmed.is_empty() {
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
pub(crate) async fn stream_child_output(
    child: &mut tokio::process::Child,
    tx: &Sender<StreamItem>,
    agent_name: &str,
) -> harness_core::Result<String> {
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| HarnessError::AgentExecution(format!("{agent_name} stdout unavailable")))?;

    let mut lines = BufReader::new(stdout).lines();
    let mut output = String::new();

    loop {
        let maybe_line = lines.next_line().await.map_err(|error| {
            HarnessError::AgentExecution(format!("failed reading {agent_name} stdout: {error}"))
        })?;
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
        let result = stream_child_output(&mut child, &tx, "test-agent")
            .await
            .expect("stream_child_output should succeed");

        assert!(
            result.contains("line1") && result.contains("line2") && result.contains("line3"),
            "returned output must contain all lines, got: {result:?}"
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
        assert!(
            delta_count >= 3,
            "expected at least 3 deltas, got {delta_count}"
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
        stream_child_output(&mut child, &tx, "test-agent")
            .await
            .expect("stream_child_output should succeed");
        drop(tx);

        let mut items = Vec::new();
        while let Some(item) = rx.recv().await {
            items.push(item);
        }

        let first_delta_pos = items
            .iter()
            .position(|item| matches!(item, StreamItem::MessageDelta { .. }))
            .expect("at least one MessageDelta expected");
        let completed_pos = items
            .iter()
            .position(|item| matches!(item, StreamItem::ItemCompleted { .. }))
            .expect("ItemCompleted expected");
        assert!(
            first_delta_pos < completed_pos,
            "MessageDelta must precede ItemCompleted"
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
        stream_child_output(&mut child, &tx, "test-agent")
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
                assert!(
                    content.contains("hello") && content.contains("world"),
                    "ItemCompleted content must include all lines, got: {content:?}"
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
        let result = stream_child_output(&mut child, &tx, "test-agent").await;
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
            stream_child_output(&mut child, &tx, "test-agent"),
        )
        .await
        .expect("stream_child_output should not hang");

        assert!(result.is_err(), "expected Err when receiver is dropped");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("stream send failed"),
            "error must report send failure, got: {msg}"
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
