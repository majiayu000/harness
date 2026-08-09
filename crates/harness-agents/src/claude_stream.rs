use crate::claude_stream_json::{
    parse_stream_json_events, parse_stream_json_result_failure, parse_stream_json_usage,
};
use crate::streaming::send_stream_item;
use harness_core::{
    agent::{AgentEvent, StreamItem},
    error::HarnessError,
    types::{Item, TokenUsage},
};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};

const MAX_STREAM_FAILURE_OUTPUT_CHARS: usize = 500;

#[derive(Debug, Default)]
pub(crate) struct ParsedClaudeStreamOutput {
    pub(crate) raw_stdout: String,
    pub(crate) output: String,
    pub(crate) token_usage: TokenUsage,
    /// Terminal `result` failure reported by the CLI (emitted with exit code
    /// 0), so success cannot be inferred from the process status alone.
    pub(crate) failure: Option<String>,
    completed: bool,
}

pub(crate) fn claude_stdout_tail(s: &str) -> String {
    let max_chars = MAX_STREAM_FAILURE_OUTPUT_CHARS;
    if max_chars == 0 {
        return String::new();
    }
    match s.char_indices().rev().nth(max_chars - 1) {
        Some((idx, _)) => s[idx..].to_string(),
        None => s.to_string(),
    }
}

fn is_json_line(line: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(line).is_ok()
}

fn push_plaintext_line(
    line: &str,
    parsed: &mut ParsedClaudeStreamOutput,
    emitted_items: &mut Vec<StreamItem>,
) {
    let delta = format!("{line}\n");
    parsed.output.push_str(&delta);
    emitted_items.push(StreamItem::MessageDelta { text: delta });
}

fn apply_claude_stream_line(
    line: &str,
    parsed: &mut ParsedClaudeStreamOutput,
    emitted_items: &mut Vec<StreamItem>,
) {
    if line.trim().is_empty() {
        return;
    }

    let usage = parse_stream_json_usage(line);
    let events = parse_stream_json_events(line);
    if usage.is_none() && events.is_empty() && !is_json_line(line) {
        push_plaintext_line(line, parsed, emitted_items);
        return;
    }

    if let Some(usage) = usage {
        parsed.token_usage = usage.clone();
        emitted_items.push(StreamItem::TokenUsage { usage });
    }

    if let Some(failure) = parse_stream_json_result_failure(line) {
        // Do not also emit a StreamItem::Error here: the failure is reported
        // exactly once, through the Err path of the execution call, so the
        // turn lifecycle does not persist the same error twice.
        parsed.failure = Some(failure);
        parsed.completed = true;
        return;
    }

    for event in events {
        apply_claude_stream_event(event, parsed, emitted_items);
    }
}

fn apply_claude_stream_event(
    event: AgentEvent,
    parsed: &mut ParsedClaudeStreamOutput,
    emitted_items: &mut Vec<StreamItem>,
) {
    match event {
        AgentEvent::MessageDelta { text } => {
            parsed.output.push_str(&text);
            emitted_items.push(StreamItem::MessageDelta { text });
        }
        AgentEvent::ToolOutputDelta { item_id, text } => {
            emitted_items.push(StreamItem::ToolOutputDelta { item_id, text });
        }
        AgentEvent::ItemStartedPayload { item } => {
            emitted_items.push(StreamItem::ItemStarted { item });
        }
        AgentEvent::ItemCompletedPayload { item } => {
            emitted_items.push(StreamItem::ItemCompleted { item });
        }
        AgentEvent::ApprovalRequest { id, command } => {
            emitted_items.push(StreamItem::ApprovalRequest { id, command });
        }
        AgentEvent::Warning { message } => {
            emitted_items.push(StreamItem::Warning { message });
        }
        AgentEvent::Error { message } => {
            emitted_items.push(StreamItem::Error { message });
        }
        AgentEvent::TurnCompleted { output } => {
            if !output.is_empty() {
                if parsed.output.is_empty() {
                    emitted_items.push(StreamItem::MessageDelta {
                        text: output.clone(),
                    });
                }
                parsed.output = output;
            }
            emitted_items.push(StreamItem::ItemCompleted {
                item: Item::AgentReasoning {
                    content: parsed.output.clone(),
                },
            });
            parsed.completed = true;
        }
        AgentEvent::ToolCall { name, input } => {
            emitted_items.push(StreamItem::ItemCompleted {
                item: Item::ToolCall {
                    name,
                    input,
                    output: None,
                },
            });
        }
        AgentEvent::EgressVerifiedAtDispatch
        | AgentEvent::TurnStarted
        | AgentEvent::ItemStarted { .. }
        | AgentEvent::ItemCompleted
        | AgentEvent::TokenUsage { .. } => {}
    }
}

pub(crate) fn parse_claude_stream_output(stdout: &str) -> ParsedClaudeStreamOutput {
    let mut parsed = ParsedClaudeStreamOutput::default();
    for line in stdout.lines() {
        if line == crate::spawn_contract::egress::CONTAINER_EGRESS_CANARY_VERIFIED {
            continue;
        }
        parsed.raw_stdout.push_str(line);
        parsed.raw_stdout.push('\n');
        let mut emitted_items = Vec::new();
        apply_claude_stream_line(line, &mut parsed, &mut emitted_items);
    }
    parsed
}

fn stream_item_label(item: &StreamItem) -> &'static str {
    match item {
        StreamItem::EgressVerifiedAtDispatch => "egress_verification",
        StreamItem::ItemStarted { .. } => "item_started",
        StreamItem::MessageDelta { .. } => "message_delta",
        StreamItem::ToolOutputDelta { .. } => "tool_output_delta",
        StreamItem::ItemCompleted { .. } => "item_completed",
        StreamItem::TokenUsage { .. } => "token_usage",
        StreamItem::Warning { .. } => "warning",
        StreamItem::Error { .. } => "error",
        StreamItem::ApprovalRequest { .. } => "approval_request",
        StreamItem::Done => "done",
    }
}

async fn read_next_claude_line(
    lines: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    child: &mut tokio::process::Child,
    idle_timeout: Option<Duration>,
) -> harness_core::error::Result<Option<String>> {
    if let Some(duration) = idle_timeout {
        tokio::time::timeout(duration, lines.next_line())
            .await
            .map_err(|_| {
                #[cfg(unix)]
                crate::kill_process_group(child);
                HarnessError::AgentExecution(format!(
                    "claude stream idle timeout after {}s: zombie connection terminated",
                    duration.as_secs()
                ))
            })?
            .map_err(|error| {
                HarnessError::AgentExecution(format!("failed reading claude stdout: {error}"))
            })
    } else {
        lines.next_line().await.map_err(|error| {
            HarnessError::AgentExecution(format!("failed reading claude stdout: {error}"))
        })
    }
}

pub(crate) async fn stream_claude_code_output(
    child: &mut tokio::process::Child,
    tx: &tokio::sync::mpsc::Sender<StreamItem>,
    idle_timeout: Option<Duration>,
    await_container_egress_canary: bool,
) -> harness_core::error::Result<ParsedClaudeStreamOutput> {
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| HarnessError::AgentExecution("claude stdout unavailable".into()))?;
    let mut lines = BufReader::new(stdout).lines();
    let mut parsed = ParsedClaudeStreamOutput::default();
    let mut container_egress_verified = !await_container_egress_canary;

    while let Some(line) = read_next_claude_line(&mut lines, child, idle_timeout).await? {
        if line == crate::spawn_contract::egress::CONTAINER_EGRESS_CANARY_VERIFIED {
            if !container_egress_verified {
                send_stream_item(
                    tx,
                    StreamItem::EgressVerifiedAtDispatch,
                    "claude",
                    "egress_verification",
                )
                .await?;
                container_egress_verified = true;
            }
            continue;
        }
        parsed.raw_stdout.push_str(&line);
        parsed.raw_stdout.push('\n');
        let mut emitted_items = Vec::new();
        apply_claude_stream_line(&line, &mut parsed, &mut emitted_items);
        for item in emitted_items {
            let item_label = stream_item_label(&item);
            send_stream_item(tx, item, "claude", item_label).await?;
        }
    }

    let status = child.wait().await.map_err(|error| {
        HarnessError::AgentExecution(format!("failed waiting for claude process: {error}"))
    })?;
    if !status.success() {
        let tail_source = if parsed.output.is_empty() {
            &parsed.raw_stdout
        } else {
            &parsed.output
        };
        let stdout_tail = claude_stdout_tail(tail_source);
        return Err(HarnessError::AgentExecution(if stdout_tail.is_empty() {
            format!("claude exited with {status}")
        } else {
            format!("claude exited with {status}: stdout_tail=[{stdout_tail}]")
        }));
    }

    if !container_egress_verified {
        return Err(HarnessError::AgentExecution(
            "claude exited before the container egress canary reported success".into(),
        ));
    }

    if let Some(failure) = &parsed.failure {
        return Err(HarnessError::AgentExecution(failure.clone()));
    }

    if !parsed.completed {
        send_stream_item(
            tx,
            StreamItem::ItemCompleted {
                item: Item::AgentReasoning {
                    content: parsed.output.clone(),
                },
            },
            "claude",
            "item_completed",
        )
        .await?;
    }

    Ok(parsed)
}
