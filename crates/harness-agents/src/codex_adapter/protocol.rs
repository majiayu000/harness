use crate::codex::{parse_codex_error_item_message, parse_codex_item, parse_codex_token_usage};
use harness_core::agent::{AgentEvent, ApprovalDecision, TurnRequest};
use harness_core::config::agents::SandboxMode;
use serde_json::{json, Value};
use std::path::Path;

use super::{ParsedCodexMessage, MAX_PROTOCOL_LINE_PREVIEW};

pub(super) fn protocol_line_preview(line: &str) -> String {
    let mut chars = line.chars();
    let mut preview: String = chars.by_ref().take(MAX_PROTOCOL_LINE_PREVIEW).collect();
    if chars.next().is_some() {
        preview.push_str("...");
    }
    preview
}

pub(super) fn notification_payload(method: &str, params: Value) -> Value {
    json!({
        "method": method,
        "params": params,
    })
}

pub(super) fn approval_decision_result(decision: ApprovalDecision) -> Value {
    match decision {
        ApprovalDecision::Accept => json!({ "decision": "accept" }),
        ApprovalDecision::Reject { reason } => json!({
            "decision": "decline",
            "reason": reason,
        }),
    }
}

pub(super) fn sandbox_mode_value(mode: Option<SandboxMode>) -> Option<String> {
    mode.map(|value| {
        match value {
            SandboxMode::ReadOnly | SandboxMode::ReadOnlyWithNetwork => "read-only",
            SandboxMode::WorkspaceWrite => "workspace-write",
            SandboxMode::DangerFullAccess => "danger-full-access",
        }
        .to_string()
    })
}

pub(super) fn sandbox_policy_value(
    mode: Option<SandboxMode>,
    project_root: &Path,
) -> Option<Value> {
    mode.map(|value| match value {
        SandboxMode::ReadOnly => json!({ "type": "readOnly" }),
        SandboxMode::ReadOnlyWithNetwork => json!({
            "type": "readOnly",
            "networkAccess": true,
        }),
        SandboxMode::WorkspaceWrite => json!({
            "type": "workspaceWrite",
            "writableRoots": [project_root],
        }),
        SandboxMode::DangerFullAccess => json!({ "type": "dangerFullAccess" }),
    })
}

pub(super) fn thread_start_params(req: &TurnRequest, child_workspace: &Path) -> Value {
    json!({
        "cwd": child_workspace,
        "model": req.model,
        "sandbox": sandbox_mode_value(req.sandbox_mode),
        "approvalPolicy": req.approval_policy,
        "ephemeral": true,
    })
}

pub(super) fn turn_start_params(
    req: &TurnRequest,
    thread_id: &str,
    child_workspace: &Path,
) -> Value {
    json!({
        "threadId": thread_id,
        "cwd": child_workspace,
        "model": req.model,
        "effort": req.reasoning_effort,
        "sandboxPolicy": sandbox_policy_value(req.sandbox_mode, child_workspace),
        "approvalPolicy": req.approval_policy,
        "input": [
            {
                "type": "text",
                "text": req.prompt,
            }
        ],
    })
}

pub(super) fn response_id_matches(actual: &Value, expected: u64) -> bool {
    actual.as_u64() == Some(expected) || actual.as_str() == Some(&expected.to_string())
}

fn request_id_string(id: &Value) -> String {
    match id {
        Value::String(value) => value.clone(),
        other => other.to_string(),
    }
}

pub(super) fn thread_id_from_result(result: &Value) -> Option<String> {
    result
        .get("thread")
        .and_then(|thread| thread.get("id"))
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .or_else(|| {
            result
                .get("id")
                .and_then(Value::as_str)
                .map(ToString::to_string)
        })
}

fn parse_app_server_agent_event(
    method: &str,
    params: &Value,
    id: Option<&Value>,
) -> ParsedCodexMessage {
    match method {
        "thread/started" => {
            let thread_id = params
                .get("thread")
                .and_then(|thread| thread.get("id"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            ParsedCodexMessage::ThreadStarted { thread_id }
        }
        "turn/started" => {
            let turn_id = params
                .get("turn")
                .and_then(|turn| turn.get("id"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            ParsedCodexMessage::TurnStarted { turn_id }
        }
        "item/agentMessage/delta" => ParsedCodexMessage::Event(AgentEvent::MessageDelta {
            text: params
                .get("delta")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        }),
        "item/commandExecution/outputDelta" => {
            ParsedCodexMessage::Event(AgentEvent::ToolOutputDelta {
                item_id: params
                    .get("itemId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                text: params
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            })
        }
        "item/started" => params
            .get("item")
            .and_then(parse_codex_item)
            .map(|item| ParsedCodexMessage::Event(AgentEvent::ItemStartedPayload { item }))
            .unwrap_or(ParsedCodexMessage::Ignore),
        "item/completed" => params
            .get("item")
            .map(|item| {
                if let Some(message) = parse_codex_error_item_message(item) {
                    ParsedCodexMessage::Event(AgentEvent::Error { message })
                } else {
                    parse_codex_item(item)
                        .map(|item| {
                            ParsedCodexMessage::Event(AgentEvent::ItemCompletedPayload { item })
                        })
                        .unwrap_or(ParsedCodexMessage::Ignore)
                }
            })
            .unwrap_or(ParsedCodexMessage::Ignore),
        "thread/tokenUsage/updated" => params
            .get("tokenUsage")
            .and_then(|usage| usage.get("total"))
            .and_then(parse_codex_token_usage)
            .map(|usage| ParsedCodexMessage::Event(AgentEvent::TokenUsage { usage }))
            .unwrap_or(ParsedCodexMessage::Ignore),
        "warning" => ParsedCodexMessage::Event(AgentEvent::Warning {
            message: params
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown warning")
                .to_string(),
        }),
        "error" => ParsedCodexMessage::Event(AgentEvent::Error {
            message: params
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
                .or_else(|| params.get("message").and_then(Value::as_str))
                .unwrap_or("unknown error")
                .to_string(),
        }),
        "turn/completed" => ParsedCodexMessage::Event(AgentEvent::TurnCompleted {
            output: params
                .get("turn")
                .and_then(|turn| turn.get("items"))
                .and_then(Value::as_array)
                .and_then(|items| items.iter().rev().find_map(parse_codex_item))
                .and_then(|item| match item {
                    harness_core::types::Item::AgentReasoning { content } => Some(content),
                    _ => None,
                })
                .unwrap_or_default(),
        }),
        "item/commandExecution/requestApproval"
        | "item/fileChange/requestApproval"
        | "item/permissions/requestApproval" => {
            ParsedCodexMessage::Event(AgentEvent::ApprovalRequest {
                id: id.map(request_id_string).unwrap_or_default(),
                command: params
                    .get("command")
                    .and_then(Value::as_str)
                    .or_else(|| params.get("reason").and_then(Value::as_str))
                    .unwrap_or(method)
                    .to_string(),
            })
        }
        _ => ParsedCodexMessage::Ignore,
    }
}

pub fn parse_codex_message(line: &str) -> Option<ParsedCodexMessage> {
    let value: Value = serde_json::from_str(line).ok()?;

    if let Some(method) = value.get("method").and_then(Value::as_str) {
        let params = value.get("params").cloned().unwrap_or(Value::Null);
        return Some(parse_app_server_agent_event(
            method,
            &params,
            value.get("id"),
        ));
    }

    if let Some(id) = value.get("id") {
        if let Some(error) = value.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error")
                .to_string();
            return Some(ParsedCodexMessage::Event(AgentEvent::Error { message }));
        }
        if let Some(result) = value.get("result") {
            return Some(ParsedCodexMessage::Response {
                id: id.clone(),
                result: result.clone(),
            });
        }
    }

    Some(ParsedCodexMessage::Ignore)
}
