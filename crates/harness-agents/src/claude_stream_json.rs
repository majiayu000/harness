//! Parsers for Claude Code `--output-format stream-json` JSONL lines.
//!
//! These were historically part of `claude_adapter.rs`; when the unreachable
//! `ClaudeAdapter` turn path was removed (GH-1786), the parsers moved here —
//! they are shared stream-format knowledge, not adapter state.

use harness_core::agent::AgentEvent;
use harness_core::types::TokenUsage;
use harness_observe::usage::parse_result_usage_metrics;
use serde_json::Value;

/// Parse a single line of Claude Code `--output-format stream-json` output.
///
/// Returns the first event on the line; use [`parse_stream_json_events`] when a
/// line can carry several (e.g. an assistant message mixing text and tool_use
/// content blocks).
pub fn parse_stream_json_line(line: &str) -> Option<AgentEvent> {
    parse_stream_json_events(line).into_iter().next()
}

/// Parse a single line of Claude Code `--output-format stream-json` output into
/// every event it carries, in content-block order.
///
/// Returns an empty vec for unrecognized event types (forward compatibility).
pub fn parse_stream_json_events(line: &str) -> Vec<AgentEvent> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return Vec::new();
    };
    let Some(event_type) = v.get("type").and_then(Value::as_str) else {
        return Vec::new();
    };

    match event_type {
        "assistant" => v
            .get("message")
            .map(parse_assistant_events)
            .unwrap_or_default(),
        "tool_use" => {
            let Some(name) = v.get("name").and_then(Value::as_str) else {
                return Vec::new();
            };
            let input = v.get("input").cloned().unwrap_or(serde_json::Value::Null);
            vec![AgentEvent::ToolCall {
                name: name.to_string(),
                input,
            }]
        }
        "tool_result" => vec![AgentEvent::ItemCompletedKind],
        "result" => match parse_result_failure_value(&v) {
            Some(message) => vec![AgentEvent::Error { message }],
            None => {
                let output = v
                    .get("result")
                    .and_then(|r| r.as_str())
                    .unwrap_or("")
                    .to_string();
                vec![AgentEvent::TurnCompleted { output }]
            }
        },
        "error" => {
            let message = v
                .get("error")
                .and_then(|e| e.as_str())
                .unwrap_or("unknown error")
                .to_string();
            vec![AgentEvent::Error { message }]
        }
        _ => Vec::new(),
    }
}

/// Detect a terminal `result` event that reports failure (`is_error` or an
/// `error*` subtype). The Claude CLI emits these with exit code 0, so callers
/// must not rely on the process status to notice the failure.
pub(crate) fn parse_stream_json_result_failure(line: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    if v.get("type").and_then(Value::as_str) != Some("result") {
        return None;
    }
    parse_result_failure_value(&v)
}

fn parse_result_failure_value(v: &Value) -> Option<String> {
    let subtype = v.get("subtype").and_then(Value::as_str).unwrap_or("");
    let is_error =
        v.get("is_error").and_then(Value::as_bool) == Some(true) || subtype.starts_with("error");
    if !is_error {
        return None;
    }

    let detail = v
        .get("result")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            v.get("error")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        });
    let subtype_label = if subtype.is_empty() {
        "unknown"
    } else {
        subtype
    };
    Some(match detail {
        Some(detail) => format!("claude result reported failure ({subtype_label}): {detail}"),
        None => format!("claude result reported failure ({subtype_label})"),
    })
}

fn parse_assistant_events(message: &Value) -> Vec<AgentEvent> {
    if let Some(text) = message.as_str() {
        return vec![AgentEvent::MessageDelta {
            text: text.to_string(),
        }];
    }

    let Some(content) = message.get("content").and_then(Value::as_array) else {
        return Vec::new();
    };

    let mut events = Vec::new();
    let mut text_buf = String::new();
    for block in content {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    text_buf.push_str(text);
                }
            }
            Some("tool_use") => {
                if let Some(name) = block.get("name").and_then(Value::as_str) {
                    if !text_buf.is_empty() {
                        events.push(AgentEvent::MessageDelta {
                            text: std::mem::take(&mut text_buf),
                        });
                    }
                    let input = block
                        .get("input")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null);
                    events.push(AgentEvent::ToolCall {
                        name: name.to_string(),
                        input,
                    });
                }
            }
            _ => {}
        }
    }
    if !text_buf.is_empty() {
        events.push(AgentEvent::MessageDelta { text: text_buf });
    }
    events
}

pub fn parse_stream_json_usage(line: &str) -> Option<TokenUsage> {
    let usage = parse_result_usage_metrics(line)?;

    Some(TokenUsage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        total_tokens: usage.total_tokens(),
        cost_usd: 0.0,
    })
}

#[cfg(test)]
#[path = "claude_stream_json_tests.rs"]
mod tests;
