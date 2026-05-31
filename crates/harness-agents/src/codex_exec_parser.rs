use crate::streaming::send_stream_item;
use harness_core::agent::StreamItem;
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
    TokenUsage { usage: TokenUsage },
    Warning { message: String },
    Error { message: String },
    Ignore,
}

#[derive(Debug, Default)]
pub(crate) struct ParsedCodexExecOutput {
    pub(crate) output: String,
    pub(crate) items: Vec<Item>,
    pub(crate) token_usage: TokenUsage,
    pub(crate) warnings: Vec<String>,
    pub(crate) structured_error: Option<String>,
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
        "turn.completed" => value
            .get("usage")
            .and_then(parse_codex_token_usage)
            .map(|usage| ParsedCodexExecEvent::TokenUsage { usage })
            .or(Some(ParsedCodexExecEvent::Ignore)),
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
        ParsedCodexExecEvent::TokenUsage { usage } => {
            parsed.token_usage = usage.clone();
            emitted_items.push(StreamItem::TokenUsage { usage });
        }
        ParsedCodexExecEvent::Warning { message } => {
            parsed.warnings.push(message.clone());
            emitted_items.push(StreamItem::Warning { message });
        }
        ParsedCodexExecEvent::Error { message } => {
            parsed.structured_error = Some(message.clone());
            emitted_items.push(StreamItem::Error { message });
        }
        ParsedCodexExecEvent::Ignore => {}
    }
}

pub(crate) fn parse_codex_exec_output(
    stdout: &str,
) -> harness_core::error::Result<ParsedCodexExecOutput> {
    let mut parsed = ParsedCodexExecOutput::default();
    let mut seen_message_deltas = HashSet::new();

    for line in stdout.lines() {
        let event = parse_codex_exec_event_line(line).ok_or_else(|| {
            harness_core::error::HarnessError::AgentExecution(format!(
                "failed to parse codex json line: {line}"
            ))
        })?;
        let mut ignored = Vec::new();
        apply_codex_exec_event(&mut parsed, &mut seen_message_deltas, event, &mut ignored);
    }

    Ok(parsed)
}

pub(crate) async fn stream_codex_exec_output(
    child: &mut tokio::process::Child,
    tx: &tokio::sync::mpsc::Sender<StreamItem>,
    idle_timeout: Option<Duration>,
) -> harness_core::error::Result<ParsedCodexExecOutput> {
    let stdout = child.stdout.take().ok_or_else(|| {
        harness_core::error::HarnessError::AgentExecution("codex stdout unavailable".into())
    })?;
    let mut lines = BufReader::new(stdout).lines();
    let mut parsed = ParsedCodexExecOutput::default();
    let mut seen_message_deltas = HashSet::new();

    loop {
        let maybe_line = if let Some(duration) = idle_timeout {
            tokio::time::timeout(duration, lines.next_line())
                .await
                .map_err(|_| {
                    #[cfg(unix)]
                    crate::kill_process_group(child);
                    harness_core::error::HarnessError::AgentExecution(format!(
                        "codex stream idle timeout after {}s: zombie connection terminated",
                        duration.as_secs()
                    ))
                })?
                .map_err(|error| {
                    harness_core::error::HarnessError::AgentExecution(format!(
                        "failed reading codex stdout: {error}"
                    ))
                })?
        } else {
            lines.next_line().await.map_err(|error| {
                harness_core::error::HarnessError::AgentExecution(format!(
                    "failed reading codex stdout: {error}"
                ))
            })?
        };
        let Some(line) = maybe_line else {
            break;
        };
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
        );
        for item in emitted_items {
            let item_label = match &item {
                StreamItem::ItemStarted { .. } => "item_started",
                StreamItem::MessageDelta { .. } => "message_delta",
                StreamItem::ToolOutputDelta { .. } => "tool_output_delta",
                StreamItem::ItemCompleted { .. } => "item_completed",
                StreamItem::TokenUsage { .. } => "token_usage",
                StreamItem::Warning { .. } => "warning",
                StreamItem::Error { .. } => "error",
                StreamItem::ApprovalRequest { .. } => "approval_request",
                StreamItem::Done => "done",
            };
            send_stream_item(tx, item, "codex", item_label).await?;
        }
    }

    Ok(parsed)
}

#[cfg(test)]
#[path = "codex_exec_parser_tests.rs"]
mod tests;
