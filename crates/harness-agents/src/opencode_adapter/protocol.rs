use harness_core::agent::{AgentEvent, ApprovalDecision};
use harness_core::error::HarnessError;
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
    Response {
        id: Value,
        result: Value,
    },
    RpcError {
        id: Value,
        error: Value,
    },
    SessionCost {
        cost_usd: f64,
    },
    PermissionRequest {
        id: Value,
        command: String,
        options: Vec<Value>,
    },
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
            return Some(ParsedAcpMessage::RpcError {
                id: value.get("id").cloned()?,
                error: value.get("error").cloned()?,
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
                    // ACP v1 used/size describe context occupancy/capacity, not
                    // consumed tokens. Cost is an independent session snapshot.
                    Some(
                        if update.pointer("/cost/currency").and_then(Value::as_str) == Some("USD") {
                            match update.pointer("/cost/amount").and_then(Value::as_f64) {
                                Some(cost_usd) => ParsedAcpMessage::SessionCost { cost_usd },
                                None => ParsedAcpMessage::Ignore,
                            }
                        } else {
                            ParsedAcpMessage::Ignore
                        },
                    )
                }
                _ => Some(ParsedAcpMessage::Ignore),
            }
        }
        "session/request_permission" => {
            let id = value.get("id")?.clone();
            let tool_call = params.get("toolCall")?;
            let options = params.get("options")?.as_array()?.clone();
            let command = tool_call
                .pointer("/rawInput/command")
                .and_then(Value::as_str)
                .or_else(|| tool_call.get("title").and_then(Value::as_str))
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| {
                    format!(
                        "Approve tool call {}",
                        tool_call
                            .get("toolCallId")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown")
                    )
                });
            Some(ParsedAcpMessage::PermissionRequest {
                id,
                command,
                options,
            })
        }
        _ => Some(ParsedAcpMessage::Ignore),
    }
}

pub(super) fn request_id_string(id: &Value) -> String {
    // JSON encoding keeps numeric 7 and string "7" distinct in the public
    // opaque approval ID. Replies use the original Value stored with the request.
    id.to_string()
}

pub(super) fn cancelled_permission_response(id: &Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": { "outcome": { "outcome": "cancelled" } },
    })
}

pub(super) fn turn_cost(
    session_cost_usd: f64,
    previous_turn_cost_usd: Option<f64>,
) -> harness_core::error::Result<Option<f64>> {
    let Some(previous_turn_cost_usd) = previous_turn_cost_usd else {
        // A reused session with an unobserved previous turn has no reliable
        // baseline. Do not attribute its entire cumulative cost to this turn.
        return Ok(None);
    };
    let cost_usd = session_cost_usd - previous_turn_cost_usd;
    if cost_usd < 0.0 {
        return Err(HarnessError::AgentExecution(
            "opencode cumulative session cost fell below the previous turn's cost".into(),
        ));
    }
    Ok(Some(cost_usd))
}

pub(super) fn permission_response(
    id: &Value,
    options: &[Value],
    decision: &ApprovalDecision,
) -> harness_core::error::Result<Value> {
    // Accept/Reject applies only to this request; it must not silently choose
    // an allow_always/reject_always option that changes future permissions.
    let kind = match decision {
        ApprovalDecision::Accept => "allow_once",
        ApprovalDecision::Reject { .. } => "reject_once",
    };
    let option_id = options
        .iter()
        .find(|option| option.get("kind").and_then(Value::as_str) == Some(kind))
        .and_then(|option| option.get("optionId"))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            HarnessError::Unsupported(format!(
                "opencode permission request did not offer a {kind} option"
            ))
        })?;
    Ok(json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": { "outcome": { "outcome": "selected", "optionId": option_id } },
    }))
}

pub(super) fn acp_error_message(error: &Value, fallback: &str) -> String {
    error
        .get("message")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|message| !message.is_empty())
        .unwrap_or(fallback)
        .to_string()
}

pub(super) fn response_id_matches(actual: &Value, expected: u64) -> bool {
    actual.as_u64() == Some(expected) || actual.as_str() == Some(&expected.to_string())
}
