use super::*;

#[test]
fn parse_assistant_message() {
    let line = r#"{"type": "assistant", "message": "Let me read the file..."}"#;
    let event = parse_stream_json_line(line).unwrap();
    match event {
        AgentEvent::MessageDelta { text } => {
            assert_eq!(text, "Let me read the file...");
        }
        other => panic!("expected MessageDelta, got {other:?}"),
    }
}

#[test]
fn parse_assistant_message_content_blocks() {
    let line = r#"{"type":"assistant","message":{"content":[{"type":"thinking","text":"hidden"},{"type":"text","text":"Hello "},{"type":"text","text":"world"}]}}"#;
    let Some(event) = parse_stream_json_line(line) else {
        panic!("assistant content blocks should parse");
    };
    match event {
        AgentEvent::MessageDelta { text } => {
            assert_eq!(text, "Hello world");
        }
        other => panic!("expected MessageDelta, got {other:?}"),
    }
}

#[test]
fn parse_tool_use() {
    let line = r#"{"type": "tool_use", "name": "Read", "input": {"path": "src/main.rs"}}"#;
    let event = parse_stream_json_line(line).unwrap();
    match event {
        AgentEvent::ToolCall { name, input } => {
            assert_eq!(name, "Read");
            assert_eq!(input["path"], "src/main.rs");
        }
        other => panic!("expected ToolCall, got {other:?}"),
    }
}

#[test]
fn parse_tool_result() {
    let line = r#"{"type": "tool_result", "output": "file contents here"}"#;
    let event = parse_stream_json_line(line).unwrap();
    assert!(matches!(event, AgentEvent::ItemCompletedKind));
}

#[test]
fn parse_result_event() {
    let line = r#"{"type": "result", "result": "Done, bug fixed."}"#;
    let event = parse_stream_json_line(line).unwrap();
    match event {
        AgentEvent::TurnCompleted { output } => {
            assert_eq!(output, "Done, bug fixed.");
        }
        other => panic!("expected TurnCompleted, got {other:?}"),
    }
}

#[test]
fn parse_assistant_tool_use_blocks_surface_tool_calls() {
    let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Let me check."},{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"ls"}},{"type":"text","text":"Now reading."}]}}"#;
    let events = parse_stream_json_events(line);
    assert_eq!(events.len(), 3, "expected delta+tool+delta, got {events:?}");
    assert!(
        matches!(&events[0], AgentEvent::MessageDelta { text } if text == "Let me check."),
        "got {events:?}"
    );
    match &events[1] {
        AgentEvent::ToolCall { name, input } => {
            assert_eq!(name, "Bash");
            assert_eq!(input["command"], "ls");
        }
        other => panic!("expected ToolCall, got {other:?}"),
    }
    assert!(
        matches!(&events[2], AgentEvent::MessageDelta { text } if text == "Now reading."),
        "got {events:?}"
    );
}

#[test]
fn parse_assistant_tool_use_only_block_is_not_dropped() {
    let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_1","name":"Read","input":{"file_path":"src/main.rs"}}]}}"#;
    let events = parse_stream_json_events(line);
    assert_eq!(events.len(), 1, "got {events:?}");
    assert!(
        matches!(&events[0], AgentEvent::ToolCall { name, .. } if name == "Read"),
        "got {events:?}"
    );
}

#[test]
fn parse_result_with_is_error_reports_failure() {
    let line = r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"command crashed"}"#;
    let event = parse_stream_json_line(line).unwrap();
    match event {
        AgentEvent::Error { message } => {
            assert!(message.contains("error_during_execution"), "{message}");
            assert!(message.contains("command crashed"), "{message}");
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[test]
fn parse_result_with_error_subtype_reports_failure_without_is_error() {
    let line = r#"{"type":"result","subtype":"error_max_turns"}"#;
    let event = parse_stream_json_line(line).unwrap();
    match event {
        AgentEvent::Error { message } => {
            assert!(message.contains("error_max_turns"), "{message}");
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[test]
fn parse_result_success_subtype_still_completes() {
    let line = r#"{"type":"result","subtype":"success","is_error":false,"result":"done"}"#;
    let event = parse_stream_json_line(line).unwrap();
    assert!(
        matches!(event, AgentEvent::TurnCompleted { ref output } if output == "done"),
        "got {event:?}"
    );
}

#[test]
fn parse_result_usage_with_cache_fields() {
    let line = r#"{"type":"result","result":"Done","usage":{"input_tokens":10,"output_tokens":3,"cache_read_input_tokens":4,"cache_creation_input_tokens":2}}"#;
    let usage = parse_stream_json_usage(line).expect("usage should parse");
    assert_eq!(usage.input_tokens, 10);
    assert_eq!(usage.output_tokens, 3);
    assert_eq!(usage.total_tokens, 19);
}

#[test]
fn parse_result_usage_allows_missing_cache_fields() {
    let line = r#"{"type":"result","result":"Done","usage":{"input_tokens":10,"output_tokens":3}}"#;
    let usage = parse_stream_json_usage(line).expect("usage should parse");
    assert_eq!(usage.input_tokens, 10);
    assert_eq!(usage.output_tokens, 3);
    assert_eq!(usage.total_tokens, 13);
}

#[test]
fn parse_result_usage_allows_zero_tokens() {
    let line = r#"{"type":"result","result":"Done","usage":{"input_tokens":0,"output_tokens":0}}"#;
    let usage = parse_stream_json_usage(line).expect("usage should parse");
    assert_eq!(usage.total_tokens, 0);
}

#[test]
fn parse_result_usage_ignores_malformed_json() {
    assert!(parse_stream_json_usage("{not-json").is_none());
}

#[test]
fn parse_error_event() {
    let line = r#"{"type": "error", "error": "rate limit exceeded"}"#;
    let event = parse_stream_json_line(line).unwrap();
    match event {
        AgentEvent::Error { message } => {
            assert_eq!(message, "rate limit exceeded");
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[test]
fn parse_unknown_type_returns_none() {
    let line = r#"{"type": "system_prompt", "text": "you are helpful"}"#;
    assert!(parse_stream_json_line(line).is_none());
}

#[test]
fn parse_invalid_json_returns_none() {
    assert!(parse_stream_json_line("not json").is_none());
    assert!(parse_stream_json_line("").is_none());
}

#[test]
fn parse_missing_type_returns_none() {
    let line = r#"{"message": "no type field"}"#;
    assert!(parse_stream_json_line(line).is_none());
}
