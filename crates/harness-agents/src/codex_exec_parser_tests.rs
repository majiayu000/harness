use super::*;
use harness_core::types::{Item, TokenUsage};

#[test]
fn parse_exec_agent_message_completion() {
    let line =
        r#"{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"hi"}}"#;
    let event = parse_codex_exec_event_line(line).expect("event should parse");
    match event {
        ParsedCodexExecEvent::ItemCompleted { item_id, item } => {
            assert_eq!(item_id, "item_0");
            assert_eq!(
                item,
                Item::AgentReasoning {
                    content: "hi".into()
                }
            );
        }
        other => panic!("expected item completion, got {other:?}"),
    }
}

#[test]
fn parse_exec_command_item_started() {
    let line = r#"{"type":"item.started","item":{"id":"item_0","type":"command_execution","command":"pwd","aggregated_output":"","exit_code":null,"status":"in_progress"}}"#;
    let event = parse_codex_exec_event_line(line).expect("event should parse");
    match event {
        ParsedCodexExecEvent::ItemStarted { item } => {
            assert_eq!(
                item,
                Item::ShellCommand {
                    command: "pwd".into(),
                    exit_code: None,
                    stdout: String::new(),
                    stderr: String::new(),
                }
            );
        }
        other => panic!("expected item start, got {other:?}"),
    }
}

#[test]
fn parse_exec_command_output_delta() {
    let line = r#"{"type":"item.command_execution.output_delta","item_id":"item_0","delta":"cargo check\n"}"#;
    let event = parse_codex_exec_event_line(line).expect("event should parse");
    match event {
        ParsedCodexExecEvent::ToolOutputDelta { item_id, text } => {
            assert_eq!(item_id, "item_0");
            assert_eq!(text, "cargo check\n");
        }
        other => panic!("expected tool output delta, got {other:?}"),
    }
}

#[test]
fn parse_exec_warning_and_error() {
    let warning = parse_codex_exec_event_line(r#"{"type":"warning","message":"careful"}"#)
        .expect("warning should parse");
    let error =
        parse_codex_exec_event_line(r#"{"type":"error","error":{"message":"something failed"}}"#)
            .expect("error should parse");

    assert!(matches!(
        warning,
        ParsedCodexExecEvent::Warning { ref message } if message == "careful"
    ));
    assert!(matches!(
        error,
        ParsedCodexExecEvent::Error { ref message } if message == "something failed"
    ));
}

#[test]
fn parse_exec_item_completed_error() {
    let line =
        r#"{"type":"item.completed","item":{"id":"item_0","type":"error","message":"bad config"}}"#;
    let event = parse_codex_exec_event_line(line).expect("event should parse");

    assert!(matches!(
        event,
        ParsedCodexExecEvent::Error { ref message } if message == "bad config"
    ));
}

#[test]
fn parse_exec_item_events_are_never_silently_dropped() {
    // Formerly these were ignored (fail open). Tool-shaped kinds now map to
    // tool-call items and everything else surfaces as an unknown kind.
    for item_type in ["mcp_tool_call", "file_change"] {
        let line =
            format!(r#"{{"type":"item.started","item":{{"id":"item_0","type":"{item_type}"}}}}"#);
        let event = parse_codex_exec_event_line(&line).expect("event should parse");
        assert!(
            matches!(
                event,
                ParsedCodexExecEvent::ItemStarted {
                    item: Item::ToolCall { .. }
                }
            ),
            "{item_type} must surface as a tool call, got {event:?}"
        );
    }
    let line = r#"{"type":"item.started","item":{"id":"item_0","type":"todo_list"}}"#;
    let event = parse_codex_exec_event_line(line).expect("event should parse");
    assert!(
        matches!(event, ParsedCodexExecEvent::UnknownItemKind { ref item_type } if item_type == "todo_list"),
        "unmapped kinds must surface, got {event:?}"
    );
}

#[test]
fn parse_exec_output_surfaces_item_completed_error() {
    let stdout =
        r#"{"type":"item.completed","item":{"id":"item_0","type":"error","message":"bad config"}}"#;
    let parsed = parse_codex_exec_output(stdout).expect("stdout should parse");

    assert_eq!(parsed.structured_error.as_deref(), Some("bad config"));
}

#[test]
fn explicit_completion_outweighs_preceding_structured_error() {
    let stdout = concat!(
        r#"{"type":"error","message":"structured output schema rejected"}"#,
        "\n",
        r#"{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"recovered"}}"#,
        "\n",
        r#"{"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":1}}"#,
    );

    let parsed = parse_codex_exec_output(stdout).expect("stdout should parse");

    assert_eq!(parsed.output, "recovered");
    assert_eq!(
        parsed.structured_error.as_deref(),
        Some("structured output schema rejected")
    );
    assert!(!parsed.explicit_failure);
}

#[test]
fn missing_terminal_fails_with_preceding_error_evidence() {
    let parsed = parse_codex_exec_output(r#"{"type":"error","message":"authentication failed"}"#)
        .expect("stdout should parse");

    assert!(parsed.explicit_failure);
    assert_eq!(
        parsed.structured_error.as_deref(),
        Some("authentication failed")
    );
}

#[test]
fn contradictory_terminal_events_fail_closed() {
    let stdout = concat!(
        r#"{"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":1}}"#,
        "\n",
        r#"{"type":"turn.failed","message":"late failure"}"#,
    );

    let parsed = parse_codex_exec_output(stdout).expect("stdout should parse");

    assert!(parsed.explicit_failure);
    assert_eq!(
        parsed.structured_error.as_deref(),
        Some("codex emitted contradictory terminal events")
    );
}

#[test]
fn parse_exec_output_ignores_unknown_item_events() {
    let stdout = concat!(
        r#"{"type":"item.started","item":{"id":"item_1","type":"mcp_tool_call","server":"github"}}"#,
        "\n",
        r#"{"type":"item.started","item":{"id":"item_2","type":"file_change","path":"src/lib.rs"}}"#,
        "\n",
        r#"{"type":"item.started","item":{"id":"item_3","type":"todo_list","items":[]}}"#,
        "\n",
        r#"{"type":"item.completed","item":{"id":"item_4","type":"agent_message","text":"done"}}"#,
        "\n",
        r#"{"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":2}}"#,
    );

    let parsed = parse_codex_exec_output(stdout).expect("stdout should parse");

    assert_eq!(parsed.output, "done");
    assert_eq!(parsed.token_usage.total_tokens, 3);
}

#[test]
fn parse_exec_turn_completed_usage() {
    let line = r#"{"type":"turn.completed","usage":{"input_tokens":10,"cached_input_tokens":4,"output_tokens":3,"reasoning_output_tokens":2}}"#;
    let event = parse_codex_exec_event_line(line).expect("event should parse");
    match event {
        ParsedCodexExecEvent::TurnCompleted { usage: Some(usage) } => {
            assert_eq!(
                usage,
                TokenUsage {
                    input_tokens: 10,
                    output_tokens: 3,
                    total_tokens: 13,
                    cost_usd: 0.0,
                }
            );
        }
        other => panic!("expected completed turn usage, got {other:?}"),
    }
}

#[test]
fn parse_exec_output_deduplicates_completed_agent_message_after_delta() {
    let stdout = concat!(
        r#"{"type":"item.delta","item_id":"item_0","delta":"he"}"#,
        "\n",
        r#"{"type":"item.delta","item_id":"item_0","delta":"llo"}"#,
        "\n",
        r#"{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"hello"}}"#,
        "\n",
        r#"{"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":2}}"#,
    );

    let parsed = parse_codex_exec_output(stdout).expect("stdout should parse");
    assert_eq!(parsed.output, "hello");
    assert_eq!(parsed.token_usage.total_tokens, 3);
}

#[test]
fn parse_exec_mcp_tool_call_maps_to_tool_call_item() {
    let line = r#"{"type":"item.completed","item":{"id":"item_3","type":"mcp_tool_call","name":"fetch_url","arguments":{"url":"https://example.com"},"output":{"status":"ok"}}}"#;
    let event = parse_codex_exec_event_line(line).expect("event should parse");
    match event {
        ParsedCodexExecEvent::ItemCompleted { item_id, item } => {
            assert_eq!(item_id, "item_3");
            assert_eq!(
                item,
                Item::ToolCall {
                    name: "fetch_url".into(),
                    input: serde_json::json!({"url": "https://example.com"}),
                    output: Some(serde_json::json!({"status": "ok"})),
                }
            );
        }
        other => panic!("expected tool call completion, got {other:?}"),
    }
}

#[test]
fn parse_exec_web_search_and_file_change_map_to_tool_call_items() {
    for (item_type, name_field) in [("web_search", "web_search"), ("file_change", "file_change")] {
        let line =
            format!(r#"{{"type":"item.started","item":{{"id":"item_4","type":"{item_type}"}}}}"#);
        let event = parse_codex_exec_event_line(&line).expect("event should parse");
        match event {
            ParsedCodexExecEvent::ItemStarted { item } => match item {
                Item::ToolCall { name, .. } => assert_eq!(name, name_field),
                other => panic!("expected tool call item, got {other:?}"),
            },
            other => panic!("expected item start, got {other:?}"),
        }
    }
}

#[test]
fn parse_exec_unknown_item_kind_surfaces_instead_of_being_ignored() {
    let line = r#"{"type":"item.started","item":{"id":"item_5","type":"novel_side_effect"}}"#;
    let event = parse_codex_exec_event_line(line).expect("event should parse");
    match event {
        ParsedCodexExecEvent::UnknownItemKind { item_type } => {
            assert_eq!(item_type, "novel_side_effect");
        }
        other => panic!("unknown item kinds must surface, got {other:?}"),
    }

    let missing_type = r#"{"type":"item.completed","item":{"id":"item_6"}}"#;
    let event = parse_codex_exec_event_line(missing_type).expect("event should parse");
    match event {
        ParsedCodexExecEvent::UnknownItemKind { item_type } => {
            assert_eq!(item_type, "missing_item_type");
        }
        other => panic!("typeless items must surface, got {other:?}"),
    }
}
