use super::*;
use harness_core::agent::AgentEvent;

#[test]
fn parse_message_chunk_notification() {
    let line = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"agent_message_chunk","messageId":"m1","content":{"type":"text","text":"hello"}}}}"#;
    let message = parse_acp_message(line).unwrap();
    assert_eq!(
        message,
        ParsedAcpMessage::Event(AgentEvent::MessageDelta {
            text: "hello".into()
        })
    );
}

#[test]
fn parse_tool_call_notification() {
    let line = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"tool_call","toolCallId":"call_1","title":"bash","kind":"execute","status":"pending"}}}"#;
    let message = parse_acp_message(line).unwrap();
    match message {
        ParsedAcpMessage::Event(AgentEvent::ToolCall { name, input }) => {
            assert_eq!(name, "bash");
            assert_eq!(input["toolCallId"], "call_1");
        }
        other => panic!("unexpected message: {other:?}"),
    }
}

#[test]
fn parse_tool_call_update_status_transitions() {
    let in_progress = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"tool_call_update","toolCallId":"call_1","status":"in_progress"}}}"#;
    assert_eq!(
        parse_acp_message(in_progress).unwrap(),
        ParsedAcpMessage::Event(AgentEvent::ItemStarted {
            item_type: "tool_call".into()
        })
    );

    let completed = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"tool_call_update","toolCallId":"call_1","status":"completed"}}}"#;
    assert_eq!(
        parse_acp_message(completed).unwrap(),
        ParsedAcpMessage::Event(AgentEvent::ItemCompleted)
    );
}

#[test]
fn parse_usage_update_notification() {
    let line = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"usage_update","used":53000,"size":200000,"cost":{"amount":0.045,"currency":"USD"}}}}"#;
    let message = parse_acp_message(line).unwrap();
    match message {
        ParsedAcpMessage::Event(AgentEvent::TokenUsage { usage }) => {
            assert_eq!(usage.input_tokens, 53000);
            assert_eq!(usage.total_tokens, 200000);
            assert!((usage.cost_usd - 0.045).abs() < f64::EPSILON);
        }
        other => panic!("unexpected message: {other:?}"),
    }
}

#[test]
fn parse_permission_request() {
    let line = r#"{"jsonrpc":"2.0","id":7,"method":"session/request_permission","params":{"sessionId":"s1","prompt":"run git push"}}"#;
    let message = parse_acp_message(line).unwrap();
    assert_eq!(
        message,
        ParsedAcpMessage::Event(AgentEvent::ApprovalRequest {
            id: "7".into(),
            command: "run git push".into()
        })
    );
}

#[test]
fn parse_response() {
    let line = r#"{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn","usage":{"inputTokens":100,"outputTokens":5,"totalTokens":105}}}"#;
    let message = parse_acp_message(line).unwrap();
    match message {
        ParsedAcpMessage::Response { id, result } => {
            assert_eq!(id, 3);
            assert_eq!(result["stopReason"], "end_turn");
        }
        other => panic!("unexpected message: {other:?}"),
    }
}

#[test]
fn parse_error_response() {
    let line = r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32602,"message":"Invalid params"}}"#;
    let message = parse_acp_message(line).unwrap();
    match message {
        ParsedAcpMessage::Response { id, result } => {
            assert_eq!(id, 2);
            assert_eq!(result["message"], "Invalid params");
        }
        other => panic!("unexpected message: {other:?}"),
    }
}

#[test]
fn ignores_unknown_updates_and_garbage() {
    let unknown = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"available_commands_update","availableCommands":[]}}}"#;
    assert_eq!(
        parse_acp_message(unknown).unwrap(),
        ParsedAcpMessage::Ignore
    );
    assert!(parse_acp_message("not json").is_none());
    assert!(parse_acp_message("").is_none());
}

#[test]
fn session_config_options_carry_model() {
    let mut req = test_turn_request();
    req.model = Some("anthropic/claude-sonnet-4".into());
    assert_eq!(
        session_config_options(&req),
        vec![json!({ "id": "model", "value": "anthropic/claude-sonnet-4" })]
    );
    req.model = None;
    let empty: Vec<Value> = vec![];
    assert_eq!(session_config_options(&req), empty);
}

#[test]
fn request_id_string_round_trip() {
    let value = json!(42);
    let id = request_id_string(&value);
    assert_eq!(id, "42");
    assert_eq!(request_id_from_string(&id), value);
    assert_eq!(
        request_id_from_string("not-a-number"),
        Value::String("not-a-number".into())
    );
}

fn test_turn_request() -> TurnRequest {
    TurnRequest {
        prompt: "ping".to_string(),
        prompt_layers: None,
        project_root: PathBuf::from("/tmp"),
        model: None,
        reasoning_effort: None,
        execution_phase: None,
        sandbox_mode: None,
        approval_policy: None,
        allowed_tools: None,
        context: vec![],
        timeout_secs: None,
        env_vars: std::collections::HashMap::new(),
        capability_token: None,
    }
}
