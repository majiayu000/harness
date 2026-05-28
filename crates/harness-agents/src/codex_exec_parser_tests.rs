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
fn parse_exec_unknown_item_events_are_ignored() {
    for item_type in ["mcp_tool_call", "file_change", "todo_list"] {
        let line =
            format!(r#"{{"type":"item.started","item":{{"id":"item_0","type":"{item_type}"}}}}"#);
        let event = parse_codex_exec_event_line(&line).expect("event should parse");

        assert!(matches!(event, ParsedCodexExecEvent::Ignore));
    }
}

#[test]
fn parse_exec_output_surfaces_item_completed_error() {
    let stdout =
        r#"{"type":"item.completed","item":{"id":"item_0","type":"error","message":"bad config"}}"#;
    let parsed = parse_codex_exec_output(stdout).expect("stdout should parse");

    assert_eq!(parsed.structured_error.as_deref(), Some("bad config"));
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
        ParsedCodexExecEvent::TokenUsage { usage } => {
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
        other => panic!("expected token usage, got {other:?}"),
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
