use crate::streaming::send_stream_item;
use harness_core::agent::{AgentDiagnosticSeverity, StreamItem};
use harness_core::types::{Item, TokenUsage};
use serde_json::Value;
use std::collections::HashSet;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};

#[derive(Debug)]
pub(crate) enum ParsedCodexExecEvent {
    MessageDelta { item_id: String, text: String },
    ToolOutputDelta { item_id: String, text: String },
    ItemStarted { item: Item },
    ItemCompleted { item_id: String, item: Item },
    Warning { message: String },
    Error { message: String },
    TurnCompleted { usage: Option<TokenUsage> },
    TurnFailed { message: String },
    Ignore,
}

#[derive(Debug, Default)]
pub(crate) struct ParsedCodexExecOutput {
    pub(crate) output: String,
    pub(crate) items: Vec<Item>,
    pub(crate) token_usage: TokenUsage,
    pub(crate) warnings: Vec<String>,
    pub(crate) structured_error: Option<String>,
    pub(crate) explicit_failure: bool,
}

#[derive(Debug, Default)]
enum CodexExecTerminal {
    #[default]
    Pending,
    Completed,
    Failed,
}

fn json_str_field<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(|field| field.as_str()))
}

pub(crate) fn parse_codex_item(item: &Value) -> Option<Item> {
    match json_str_field(item, &["type"])? {
        "agent_message" | "agentMessage" => Some(Item::AgentReasoning {
            content: json_str_field(item, &["text"])?.to_string(),
        }),
        "command_execution" | "commandExecution" => Some(Item::ShellCommand {
            command: json_str_field(item, &["command"])?.to_string(),
            exit_code: item
                .get("exit_code")
                .or_else(|| item.get("exitCode"))
                .and_then(|field| field.as_i64())
                .and_then(|code| i32::try_from(code).ok()),
            stdout: json_str_field(item, &["aggregated_output", "aggregatedOutput"])
                .unwrap_or_default()
                .to_string(),
            stderr: String::new(),
        }),
        _ => None,
    }
}

pub(crate) fn parse_codex_error_item_message(item: &Value) -> Option<String> {
    if json_str_field(item, &["type"])? != "error" {
        return None;
    }

    Some(
        json_str_field(item, &["message"])
            .or_else(|| {
                item.get("error")
                    .and_then(|error| json_str_field(error, &["message"]))
            })
            .unwrap_or("unknown error")
            .to_string(),
    )
}

pub(crate) fn parse_codex_token_usage(usage: &Value) -> Option<TokenUsage> {
    let input_tokens = usage
        .get("input_tokens")
        .or_else(|| usage.get("inputTokens"))
        .and_then(|field| field.as_u64())?;
    let output_tokens = usage
        .get("output_tokens")
        .or_else(|| usage.get("outputTokens"))
        .and_then(|field| field.as_u64())?;
    let total_tokens = usage
        .get("total_tokens")
        .or_else(|| usage.get("totalTokens"))
        .and_then(|field| field.as_u64())
        .unwrap_or(input_tokens.saturating_add(output_tokens));

    Some(TokenUsage {
        input_tokens,
        output_tokens,
        total_tokens,
        cost_usd: 0.0,
    })
}

pub(crate) fn parse_codex_exec_event_line(line: &str) -> Option<ParsedCodexExecEvent> {
    let value: Value = serde_json::from_str(line).ok()?;
    let event_type = json_str_field(&value, &["type"])?;

    match event_type {
        "thread.started" | "turn.started" => Some(ParsedCodexExecEvent::Ignore),
        "warning" => Some(ParsedCodexExecEvent::Warning {
            message: json_str_field(&value, &["message"])
                .or_else(|| value.get("warning").and_then(Value::as_str))
                .unwrap_or("unknown warning")
                .to_string(),
        }),
        "error" => Some(ParsedCodexExecEvent::Error {
            message: json_str_field(&value, &["message"])
                .or_else(|| {
                    value
                        .get("error")
                        .and_then(|error| json_str_field(error, &["message"]))
                })
                .unwrap_or("unknown error")
                .to_string(),
        }),
        "turn.completed" => Some(ParsedCodexExecEvent::TurnCompleted {
            usage: value.get("usage").and_then(parse_codex_token_usage),
        }),
        "turn.failed" => Some(ParsedCodexExecEvent::TurnFailed {
            message: json_str_field(&value, &["message"])
                .or_else(|| {
                    value
                        .get("error")
                        .and_then(|error| json_str_field(error, &["message"]))
                })
                .unwrap_or("codex turn failed")
                .to_string(),
        }),
        "item.started" | "item.completed" => {
            let Some(item_value) = value.get("item") else {
                return Some(ParsedCodexExecEvent::Ignore);
            };
            if let Some(message) = parse_codex_error_item_message(item_value) {
                return Some(ParsedCodexExecEvent::Error { message });
            }
            let Some(item) = parse_codex_item(item_value) else {
                return Some(ParsedCodexExecEvent::Ignore);
            };
            if event_type == "item.started" {
                Some(ParsedCodexExecEvent::ItemStarted { item })
            } else {
                Some(ParsedCodexExecEvent::ItemCompleted {
                    item_id: json_str_field(item_value, &["id"])
                        .unwrap_or_default()
                        .to_string(),
                    item,
                })
            }
        }
        "item.delta" | "item/agentMessage/delta" | "item.agent_message.delta" => {
            Some(ParsedCodexExecEvent::MessageDelta {
                item_id: json_str_field(&value, &["item_id", "itemId"])?.to_string(),
                text: json_str_field(&value, &["delta", "text"])?.to_string(),
            })
        }
        "item/commandExecution/outputDelta"
        | "item.command_execution.output_delta"
        | "item.command_output_delta" => Some(ParsedCodexExecEvent::ToolOutputDelta {
            item_id: json_str_field(&value, &["item_id", "itemId"])?.to_string(),
            text: json_str_field(&value, &["delta", "text"])?.to_string(),
        }),
        _ => Some(ParsedCodexExecEvent::Ignore),
    }
}

fn apply_codex_exec_event(
    parsed: &mut ParsedCodexExecOutput,
    seen_message_deltas: &mut HashSet<String>,
    event: ParsedCodexExecEvent,
    emitted_items: &mut Vec<StreamItem>,
    terminal: &mut CodexExecTerminal,
) {
    match event {
        ParsedCodexExecEvent::MessageDelta { item_id, text } => {
            seen_message_deltas.insert(item_id);
            parsed.output.push_str(&text);
            emitted_items.push(StreamItem::MessageDelta { text });
        }
        ParsedCodexExecEvent::ToolOutputDelta { item_id, text } => {
            emitted_items.push(StreamItem::ToolOutputDelta { item_id, text });
        }
        ParsedCodexExecEvent::ItemStarted { item } => {
            emitted_items.push(StreamItem::ItemStarted { item });
        }
        ParsedCodexExecEvent::ItemCompleted { item_id, item } => {
            if let Item::AgentReasoning { content } = &item {
                if !seen_message_deltas.contains(&item_id) {
                    parsed.output.push_str(content);
                    emitted_items.push(StreamItem::MessageDelta {
                        text: content.clone(),
                    });
                }
            } else {
                parsed.items.push(item.clone());
            }
            emitted_items.push(StreamItem::ItemCompleted { item });
        }
        ParsedCodexExecEvent::Warning { message } => {
            parsed.warnings.push(message.clone());
            emitted_items.push(StreamItem::Warning { message });
        }
        ParsedCodexExecEvent::Error { message } => {
            parsed.structured_error = Some(message.clone());
            emitted_items.push(StreamItem::Diagnostic {
                severity: AgentDiagnosticSeverity::Error,
                message,
            });
        }
        ParsedCodexExecEvent::TurnCompleted { usage } => {
            if let Some(usage) = usage {
                parsed.token_usage = usage.clone();
                emitted_items.push(StreamItem::TokenUsage { usage });
            }
            apply_codex_terminal(
                parsed,
                emitted_items,
                terminal,
                CodexExecTerminal::Completed,
            );
        }
        ParsedCodexExecEvent::TurnFailed { message } => {
            parsed.structured_error = Some(message);
            apply_codex_terminal(parsed, emitted_items, terminal, CodexExecTerminal::Failed);
        }
        ParsedCodexExecEvent::Ignore => {}
    }
}

fn apply_codex_terminal(
    parsed: &mut ParsedCodexExecOutput,
    emitted_items: &mut Vec<StreamItem>,
    terminal: &mut CodexExecTerminal,
    next: CodexExecTerminal,
) {
    if matches!(terminal, CodexExecTerminal::Pending) {
        *terminal = next;
        return;
    }

    let message = "codex emitted contradictory terminal events".to_string();
    parsed.structured_error = Some(message.clone());
    parsed.explicit_failure = true;
    *terminal = CodexExecTerminal::Failed;
    emitted_items.push(StreamItem::Error { message });
}

fn finish_codex_exec_output(parsed: &mut ParsedCodexExecOutput, terminal: CodexExecTerminal) {
    match terminal {
        CodexExecTerminal::Pending => {
            parsed.explicit_failure = true;
            parsed.structured_error.get_or_insert_with(|| {
                "codex stream ended without an authoritative terminal event".to_string()
            });
        }
        CodexExecTerminal::Completed => parsed.explicit_failure = false,
        CodexExecTerminal::Failed => parsed.explicit_failure = true,
    }
}

pub(crate) fn parse_codex_exec_output(
    stdout: &str,
) -> harness_core::error::Result<ParsedCodexExecOutput> {
    let mut parsed = ParsedCodexExecOutput::default();
    let mut seen_message_deltas = HashSet::new();
    let mut terminal = CodexExecTerminal::default();

    for line in stdout.lines() {
        if line == crate::spawn_contract::egress::CONTAINER_EGRESS_CANARY_VERIFIED {
            continue;
        }
        let event = parse_codex_exec_event_line(line).ok_or_else(|| {
            harness_core::error::HarnessError::AgentExecution(format!(
                "failed to parse codex json line: {line}"
            ))
        })?;
        let mut ignored = Vec::new();
        apply_codex_exec_event(
            &mut parsed,
            &mut seen_message_deltas,
            event,
            &mut ignored,
            &mut terminal,
        );
    }

    finish_codex_exec_output(&mut parsed, terminal);

    Ok(parsed)
}

pub(crate) async fn stream_codex_exec_output(
    child: &mut tokio::process::Child,
    tx: &tokio::sync::mpsc::Sender<StreamItem>,
    idle_timeout: Option<Duration>,
    await_container_egress_canary: bool,
) -> harness_core::error::Result<ParsedCodexExecOutput> {
    let stdout = child.stdout.take().ok_or_else(|| {
        harness_core::error::HarnessError::AgentExecution("codex stdout unavailable".into())
    })?;
    let mut lines = BufReader::new(stdout).lines();
    let mut parsed = ParsedCodexExecOutput::default();
    let mut seen_message_deltas = HashSet::new();
    let mut terminal = CodexExecTerminal::default();
    let mut container_egress_verified = !await_container_egress_canary;
    enum StreamRead {
        Line(std::io::Result<Option<String>>),
        Exited(std::io::Result<std::process::ExitStatus>),
    }

    loop {
        let read_or_exit = async {
            tokio::select! {
                biased;
                line = lines.next_line() => StreamRead::Line(line),
                status = child.wait() => StreamRead::Exited(status),
            }
        };
        let read = if let Some(duration) = idle_timeout {
            tokio::time::timeout(duration, read_or_exit)
                .await
                .map_err(|_| {
                    #[cfg(unix)]
                    crate::kill_process_group(child);
                    harness_core::error::HarnessError::AgentExecution(format!(
                        "codex stream idle timeout after {}s: zombie connection terminated",
                        duration.as_secs()
                    ))
                })?
        } else {
            read_or_exit.await
        };
        let maybe_line = match read {
            StreamRead::Line(line) => line.map_err(|error| {
                harness_core::error::HarnessError::AgentExecution(format!(
                    "failed reading codex stdout: {error}"
                ))
            })?,
            StreamRead::Exited(status) => {
                status.map_err(|error| {
                    harness_core::error::HarnessError::AgentExecution(format!(
                        "failed waiting for codex process: {error}"
                    ))
                })?;
                #[cfg(unix)]
                crate::kill_process_group(child);
                break;
            }
        };
        let Some(line) = maybe_line else {
            break;
        };
        if line == crate::spawn_contract::egress::CONTAINER_EGRESS_CANARY_VERIFIED {
            if !container_egress_verified {
                send_stream_item(
                    tx,
                    StreamItem::EgressVerifiedAtDispatch,
                    "codex",
                    "egress_verification",
                )
                .await?;
                container_egress_verified = true;
            }
            continue;
        }
        let event = parse_codex_exec_event_line(&line).ok_or_else(|| {
            harness_core::error::HarnessError::AgentExecution(format!(
                "failed to parse codex json line: {line}"
            ))
        })?;
        let mut emitted_items = Vec::new();
        apply_codex_exec_event(
            &mut parsed,
            &mut seen_message_deltas,
            event,
            &mut emitted_items,
            &mut terminal,
        );
        for item in emitted_items {
            let item_label = match &item {
                StreamItem::EgressVerifiedAtDispatch => "egress_verification",
                StreamItem::TurnStarted => "turn_started",
                StreamItem::ItemStarted { .. } => "item_started",
                StreamItem::ItemStartedKind { .. } => "item_started",
                StreamItem::MessageDelta { .. } => "message_delta",
                StreamItem::ToolOutputDelta { .. } => "tool_output_delta",
                StreamItem::ToolCall { .. } => "tool_call",
                StreamItem::ItemCompleted { .. } => "item_completed",
                StreamItem::ItemCompletedKind => "item_completed",
                StreamItem::TokenUsage { .. } => "token_usage",
                StreamItem::Warning { .. } => "warning",
                StreamItem::Diagnostic { .. } => "diagnostic",
                StreamItem::TurnCancelled { .. } => "turn_cancelled",
                StreamItem::Error { .. } => "error",
                StreamItem::TurnCompleted { .. } => "turn_completed",
                StreamItem::ApprovalRequest { .. } => "approval_request",
                StreamItem::Done => "done",
            };
            send_stream_item(tx, item, "codex", item_label).await?;
        }
    }

    if !container_egress_verified {
        return Err(harness_core::error::HarnessError::AgentExecution(
            "codex exited before the container egress canary reported success".into(),
        ));
    }

    finish_codex_exec_output(&mut parsed, terminal);

    Ok(parsed)
}

#[cfg(test)]
#[path = "codex_exec_parser_tests.rs"]
mod tests;
