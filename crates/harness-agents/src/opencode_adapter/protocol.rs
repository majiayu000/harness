use harness_core::agent::AgentEvent;
use harness_core::types::TokenUsage;
use serde_json::{json, Value};

const MAX_PROTOCOL_LINE_PREVIEW: usize = 240;

pub(super) fn protocol_line_preview(line: &str) -> String {
    let mut chars = line.chars();
    let mut preview: String = chars.by_ref().take(MAX_PROTOCOL_LINE_PREVIEW).collect();
    if chars.next().is_some() {
        preview.push_str("...");
    }
    preview
}

#[derive(Debug, Clone, PartialEq)]
pub enum ParsedAcpMessage {
    Event(AgentEvent),
    Response { id: Value, result: Value },
    Ignore,
}

/// Parse one ACP JSON-RPC line from `opencode acp` stdout.
pub fn parse_acp_message(line: &str) -> Option<ParsedAcpMessage> {
    let value: Value = serde_json::from_str(line).ok()?;
    if value.get("method").is_some() {
        return parse_acp_notification(&value);
    }
    if value.get("id").is_some() {
        if value.get("error").is_some() {
            return Some(ParsedAcpMessage::Response {
                id: value.get("id").cloned()?,
                result: value.get("error").cloned()?,
            });
        }
        return Some(ParsedAcpMessage::Response {
            id: value.get("id").cloned()?,
            result: value.get("result").cloned()?,
        });
    }
    None
}

fn parse_acp_notification(value: &Value) -> Option<ParsedAcpMessage> {
    let method = value.get("method")?.as_str()?;
    let params = value.get("params").cloned().unwrap_or(Value::Null);
    match method {
        "session/update" => {
            let update = params.get("update")?;
            let session_update = update.get("sessionUpdate")?.as_str()?;
            match session_update {
                "agent_message_chunk" => {
                    let text = update
                        .pointer("/content/text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    Some(ParsedAcpMessage::Event(AgentEvent::MessageDelta { text }))
                }
                "tool_call" => {
                    let name = update
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or("tool")
                        .to_string();
                    let tool_call_id = update
                        .get("toolCallId")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let input = json!({ "toolCallId": tool_call_id });
                    Some(ParsedAcpMessage::Event(AgentEvent::ToolCall {
                        name,
                        input,
                    }))
                }
                "tool_call_update" => {
                    let status = update
                        .get("status")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    match status {
                        "in_progress" => {
                            Some(ParsedAcpMessage::Event(AgentEvent::ItemStartedKind {
                                item_type: "tool_call".into(),
                            }))
                        }
                        "completed" | "error" => {
                            Some(ParsedAcpMessage::Event(AgentEvent::ItemCompletedKind))
                        }
                        _ => Some(ParsedAcpMessage::Ignore),
                    }
                }
                "usage_update" => {
                    let used = update.get("used").and_then(Value::as_u64).unwrap_or(0);
                    let size = update.get("size").and_then(Value::as_u64).unwrap_or(0);
                    let cost = update
                        .pointer("/cost/amount")
                        .and_then(Value::as_f64)
                        .unwrap_or(0.0);
                    let usage = TokenUsage {
                        input_tokens: used,
                        output_tokens: 0,
                        total_tokens: size,
                        cost_usd: cost,
                    };
                    Some(ParsedAcpMessage::Event(AgentEvent::TokenUsage { usage }))
                }
                _ => Some(ParsedAcpMessage::Ignore),
            }
        }
        "session/request_permission" => {
            let id = value.get("id")?.clone();
            let command = params
                .get("prompt")
                .and_then(Value::as_str)
                .unwrap_or("permission requested")
                .to_string();
            Some(ParsedAcpMessage::Event(AgentEvent::ApprovalRequest {
                id: request_id_string(&id),
                command,
            }))
        }
        _ => Some(ParsedAcpMessage::Ignore),
    }
}

pub(super) fn request_id_string(id: &Value) -> String {
    match id {
        Value::String(value) => value.clone(),
        other => other.to_string(),
    }
}

pub(super) fn response_id_matches(actual: &Value, expected: u64) -> bool {
    actual.as_u64() == Some(expected) || actual.as_str() == Some(&expected.to_string())
}

pub(super) fn request_id_from_string(id: &str) -> Value {
    serde_json::from_str(id).unwrap_or_else(|_| Value::String(id.to_string()))
}
