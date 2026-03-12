use harness_core::{HarnessError, Item, StreamItem};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
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

/// Read stdout from a spawned child process, stream deltas via `tx`,
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

    let mut reader = BufReader::new(stdout);
    let mut output = String::new();
    let mut chunk = [0_u8; 1024];

    loop {
        let read = reader.read(&mut chunk).await.map_err(|error| {
            HarnessError::AgentExecution(format!("failed reading {agent_name} stdout: {error}"))
        })?;
        if read == 0 {
            break;
        }

        let delta = String::from_utf8_lossy(&chunk[..read]).to_string();
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
    use tokio::process::Command;

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
