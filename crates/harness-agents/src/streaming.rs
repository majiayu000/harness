use harness_core::{agent::StreamItem, error::HarnessError, types::Item};
use std::sync::{Arc, Mutex};
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
const MAX_CAPTURED_STDERR_CHARS: usize = 4000;
const MAX_STREAM_FAILURE_OUTPUT_CHARS: usize = 500;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StderrHandling {
    ClassifyWarnings,
    DiagnosticsOnly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StderrLineDisposition {
    Debug,
    Warning,
}

/// Truncate a string to at most `max_bytes`, snapping to a char boundary.
fn truncate_to_char_boundary(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

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
    // Codex CLI telemetry failures are agent-internal and are not actionable
    // server warnings.
    "warn codex_analytics::client:",
    "error codex_analytics::client:",
    "codex_analytics::client:",
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

/// Detect code-content lines emitted by Codex to stderr (e.g. `   7        "error",`).
/// These contain source code keywords but are not real errors.
fn looks_like_code_content(line: &str) -> bool {
    let trimmed = line.trim_start();
    let had_indent = trimmed.len() != line.len();
    // Lines starting with a digit followed by whitespace are numbered source lines.
    if trimmed
        .as_bytes()
        .first()
        .is_some_and(|b| b.is_ascii_digit())
        && trimmed
            .find(|c: char| !c.is_ascii_digit())
            .is_some_and(|pos| {
                trimmed
                    .as_bytes()
                    .get(pos)
                    .is_some_and(|b| b.is_ascii_whitespace())
            })
    {
        return true;
    }

    // Diff hunks and inline patch content frequently contain words like
    // "warn"/"error" but should stay debug-only.
    if trimmed.starts_with('+')
        || trimmed.starts_with('-')
        || trimmed.starts_with("@@")
        || trimmed.starts_with("diff --git")
    {
        return true;
    }

    if had_indent {
        const INDENTED_CODE_PREFIXES: &[&str] = &[
            "\"",
            "//",
            "/*",
            "#[",
            "tracing::",
            "sqlx::",
            "tokio::",
            "let ",
            "fn ",
            "pub ",
            "impl ",
            "struct ",
            "enum ",
            "match ",
            "if ",
            "for ",
            "while ",
            "loop",
            "return ",
            "Err(",
            "Ok(",
            "Some(",
            "None",
            ".",
        ];
        if INDENTED_CODE_PREFIXES
            .iter()
            .any(|prefix| trimmed.starts_with(prefix))
        {
            return true;
        }
        if trimmed.ends_with('{')
            || trimmed.ends_with('}')
            || trimmed.ends_with(',')
            || trimmed.ends_with(");")
            || trimmed.ends_with(")]")
            || trimmed.ends_with("=>")
        {
            return true;
        }
    }

    false
}

fn looks_like_git_commit_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    let Some((hash, rest)) = trimmed.split_once(' ') else {
        return false;
    };
    (7..=40).contains(&hash.len())
        && hash.chars().all(|c| c.is_ascii_hexdigit())
        && !rest.is_empty()
}

fn looks_like_search_result(line: &str) -> bool {
    let trimmed = line.trim_start();
    let Some((path, rest)) = trimmed.split_once(':') else {
        return false;
    };
    let Some((line_no, _tail)) = rest.split_once(':') else {
        return false;
    };
    !path.is_empty()
        && !path.contains(char::is_whitespace)
        && !line_no.is_empty()
        && line_no.chars().all(|c| c.is_ascii_digit())
        && (path.starts_with("./")
            || path.starts_with('/')
            || path.contains('/')
            || path.ends_with(".rs")
            || path.ends_with(".md")
            || path.ends_with(".toml"))
}

fn looks_like_markdown_prompt_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with('#')
        || trimmed.starts_with("- ")
        || trimmed.starts_with("* ")
        || trimmed.starts_with("> ")
        || trimmed.starts_with("Pass: ")
        || trimmed.starts_with("Warn: ")
        || trimmed.starts_with("Block: ")
}

fn is_agent_internal(line: &str) -> bool {
    if looks_like_code_content(line)
        || looks_like_git_commit_line(line)
        || looks_like_search_result(line)
        || looks_like_markdown_prompt_line(line)
    {
        return true;
    }
    let lower = line.to_lowercase();
    AGENT_INTERNAL_PREFIXES
        .iter()
        .any(|prefix| lower.starts_with(prefix))
}

fn tail_chars(s: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    // char_indices().rev().nth(max_chars - 1) finds the byte offset of the
    // (max_chars)-th char from the end in a single pass — no Vec<char> needed.
    match s.char_indices().rev().nth(max_chars - 1) {
        Some((idx, _)) => s[idx..].to_string(),
        None => s.to_string(),
    }
}

fn append_stderr_capture(captured: &Arc<Mutex<String>>, line: &str) {
    let mut guard = captured
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    guard.push_str(line);
    guard.push('\n');
    if guard.chars().rev().nth(MAX_CAPTURED_STDERR_CHARS).is_some() {
        *guard = tail_chars(&guard, MAX_CAPTURED_STDERR_CHARS);
    }
}

fn stderr_line_disposition(line: &str, handling: StderrHandling) -> Option<StderrLineDisposition> {
    let trimmed = truncate_to_char_boundary(line, MAX_STDERR_LINE_LEN);
    if trimmed.is_empty() {
        return None;
    }
    if handling == StderrHandling::DiagnosticsOnly || is_agent_internal(trimmed) {
        return Some(StderrLineDisposition::Debug);
    }
    let lower = trimmed.to_lowercase();
    if STDERR_ERROR_KEYWORDS.iter().any(|kw| lower.contains(kw)) {
        Some(StderrLineDisposition::Warning)
    } else {
        Some(StderrLineDisposition::Debug)
    }
}

pub(crate) fn captured_stderr_tail(captured: &Arc<Mutex<String>>) -> String {
    captured
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .trim()
        .to_string()
}

pub(crate) fn enrich_stream_exit_error(error: HarnessError, stderr_tail: &str) -> HarnessError {
    match error {
        HarnessError::AgentExecution(message)
            if message.contains(" exited with ")
                && !message.contains("stderr=[")
                && !stderr_tail.trim().is_empty() =>
        {
            HarnessError::AgentExecution(format!("{message}: stderr=[{stderr_tail}]"))
        }
        other => other,
    }
}

async fn drain_agent_stderr_with_capture(
    stderr: tokio::process::ChildStderr,
    agent_name: &str,
    captured: Option<Arc<Mutex<String>>>,
    handling: StderrHandling,
) {
    let reader = BufReader::new(stderr);
    let mut lines = reader.lines();

    while let Ok(Some(line)) = lines.next_line().await {
        let trimmed = truncate_to_char_boundary(&line, MAX_STDERR_LINE_LEN);
        if trimmed.is_empty() {
            continue;
        }
        if let Some(captured) = captured.as_ref() {
            append_stderr_capture(captured, trimmed);
        }
        match stderr_line_disposition(trimmed, handling) {
            Some(StderrLineDisposition::Warning) => tracing::warn!(agent = agent_name, "{trimmed}"),
            Some(StderrLineDisposition::Debug) => tracing::debug!(agent = agent_name, "{trimmed}"),
            None => {}
        }
    }
}

pub(crate) async fn filter_agent_stderr_with_capture(
    stderr: tokio::process::ChildStderr,
    agent_name: &str,
    captured: Option<Arc<Mutex<String>>>,
) {
    drain_agent_stderr_with_capture(
        stderr,
        agent_name,
        captured,
        StderrHandling::ClassifyWarnings,
    )
    .await;
}

pub(crate) async fn capture_agent_stderr_diagnostics(
    stderr: tokio::process::ChildStderr,
    agent_name: &str,
    captured: Option<Arc<Mutex<String>>>,
) {
    drain_agent_stderr_with_capture(
        stderr,
        agent_name,
        captured,
        StderrHandling::DiagnosticsOnly,
    )
    .await;
}

/// Read agent stderr line-by-line. Lines matching error keywords are logged
/// at warn level; agent-internal progress lines are always debug.
#[cfg(test)]
pub(crate) async fn filter_agent_stderr(stderr: tokio::process::ChildStderr, agent_name: &str) {
    filter_agent_stderr_with_capture(stderr, agent_name, None).await;
}

/// Log stderr captured from a non-streaming `output()` call.
fn log_captured_stderr_with_mode(stderr: &str, agent_name: &str, handling: StderrHandling) {
    for line in stderr.lines() {
        let trimmed = truncate_to_char_boundary(line, MAX_STDERR_LINE_LEN);
        match stderr_line_disposition(trimmed, handling) {
            Some(StderrLineDisposition::Warning) => tracing::warn!(agent = agent_name, "{trimmed}"),
            Some(StderrLineDisposition::Debug) => tracing::debug!(agent = agent_name, "{trimmed}"),
            None => {}
        }
    }
}

pub(crate) fn log_captured_stderr(stderr: &str, agent_name: &str) {
    log_captured_stderr_with_mode(stderr, agent_name, StderrHandling::ClassifyWarnings);
}

pub(crate) fn log_captured_stderr_diagnostics(stderr: &str, agent_name: &str) {
    log_captured_stderr_with_mode(stderr, agent_name, StderrHandling::DiagnosticsOnly);
}

pub(crate) async fn send_stream_item(
    tx: &Sender<StreamItem>,
    item: StreamItem,
    agent_name: &str,
    item_label: &'static str,
) -> harness_core::error::Result<()> {
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
) -> harness_core::error::Result<String> {
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
                    // Kill entire process group to prevent orphaned grandchild
                    // processes (e.g. cargo test binaries) from running forever.
                    #[cfg(unix)]
                    crate::kill_process_group(child);
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
        let stdout_tail = tail_chars(&output, MAX_STREAM_FAILURE_OUTPUT_CHARS);
        return Err(HarnessError::AgentExecution(if stdout_tail.is_empty() {
            format!("{agent_name} exited with {status}")
        } else {
            format!("{agent_name} exited with {status}: stdout_tail=[{stdout_tail}]")
        }));
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
}
