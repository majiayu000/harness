use super::*;

fn agent() -> CursorAgent {
    CursorAgent::from_config(CursorAgentConfig::default(), SandboxMode::DangerFullAccess)
}

fn request() -> AgentRequest {
    AgentRequest {
        prompt: "--a prompt with 'quotes' and $(literal)".into(),
        permission_mode: AgentPermissionMode::Full,
        allowed_tools: None,
        ..Default::default()
    }
}

#[test]
fn cursor_args_preserve_prompt_and_model() {
    let mut req = request();
    req.model = Some("selected-model".into());
    let args = agent().args(&req).unwrap();
    assert_eq!(
        args,
        [
            "--print",
            "--force",
            "--trust",
            "--output-format",
            "stream-json",
            "--model",
            "selected-model",
            "--",
            &req.prompt
        ]
        .map(OsString::from)
    );
}

#[test]
fn cursor_rejects_unenforceable_request_before_spawn() {
    for tools in [Some(vec![]), Some(vec!["Read".into()])] {
        let mut req = request();
        req.allowed_tools = tools;
        assert!(agent()
            .args(&req)
            .unwrap_err()
            .to_string()
            .contains("scoped tool allowlists"));
    }
    let mut req = request();
    req.permission_mode = AgentPermissionMode::Scoped;
    assert!(agent().args(&req).is_err());
    let mut req = request();
    req.max_budget_usd = Some(1.0);
    assert!(agent().args(&req).unwrap_err().to_string().contains("USD"));
    assert!(!agent().reports_usage_cost());
    assert!(!agent()
        .agent_contract_capabilities()
        .missing_for_enforcement()
        .is_empty());
}

#[test]
fn cursor_stream_preserves_messages_tools_and_authoritative_result() {
    let mut output = CursorOutput::default();
    let events = output
        .parse(r#"{"type":"system","subtype":"init","model":"Composer"}"#)
        .unwrap();
    assert!(matches!(&events[1], AgentEvent::ModelReported { model, .. } if model == "Composer"));
    let events = output
        .parse(r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Checking."}]}}"#)
        .unwrap();
    assert_eq!(
        events,
        vec![AgentEvent::ItemCompleted {
            item: Item::AgentReasoning {
                content: "Checking.".into()
            }
        }]
    );
    let events = output.parse(r#"{"type":"tool_call","subtype":"started","call_id":"t1","tool_call":{"readToolCall":{"args":{"path":"README.md"}}}}"#).unwrap();
    assert!(
        matches!(&events[0], AgentEvent::ToolCall { name, input } if name == "readToolCall" && input["args"]["path"] == "README.md")
    );
    output
        .parse(r#"{"type":"result","subtype":"success","is_error":false,"result":"Final result"}"#)
        .unwrap();
    assert_eq!(output.output.as_deref(), Some("Final result"));
}

#[test]
fn cursor_malformed_and_error_events_cannot_report_success() {
    for line in [
        "not json",
        "{}",
        r#"{"type":"result","subtype":"success","is_error":false}"#,
        r#"{"type":"result","subtype":"error","is_error":true,"result":"quota exceeded"}"#,
        r#"{"type":"assistant","message":{}}"#,
    ] {
        assert!(CursorOutput::default().parse(line).is_err(), "{line}");
    }
}

#[cfg(unix)]
fn fixture(script: &str) -> (tempfile::TempDir, CursorAgent, AgentRequest) {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cursor-fixture");
    std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    let backend = CursorAgent::from_config(
        CursorAgentConfig {
            cli_path: path,
            ..Default::default()
        },
        SandboxMode::DangerFullAccess,
    );
    let mut req = request();
    req.project_root = dir.path().to_path_buf();
    (dir, backend, req)
}

#[cfg(unix)]
#[tokio::test]
async fn cursor_execute_and_stream_require_terminal_success_and_exit_zero() {
    let (_dir, backend, req) = fixture("printf '%s\\n' '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"OK\"}'");
    assert_eq!(backend.execute(req.clone()).await.unwrap().output, "OK");
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    backend.execute_stream(req, tx).await.unwrap();
    assert_eq!(
        rx.recv().await,
        Some(AgentEvent::TurnCompleted {
            output: "OK".into()
        })
    );
    assert_eq!(rx.recv().await, Some(AgentEvent::Done));
    for script in ["exit 0", "echo authentication-failed >&2; exit 1", "printf '%s\\n' '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"OK\"}'; exit 1"] {
        let (_dir, backend, req) = fixture(script);
        assert!(backend.execute(req).await.is_err());
    }
}

#[cfg(unix)]
#[tokio::test]
async fn cursor_timeout_and_malformed_stream_terminate_child() {
    for script in ["sleep 30", "echo malformed; sleep 30"] {
        let (_dir, backend, mut req) = fixture(script);
        req.timeout_secs = Some(1);
        let result = tokio::time::timeout(Duration::from_secs(5), backend.execute(req)).await;
        assert!(result
            .expect("failure must terminate the child without waiting for its natural exit")
            .is_err());
    }
}

#[test]
fn cursor_persists_intermediate_messages_and_ignores_tool_metadata() {
    let mut state = CursorOutput::default();
    state.parse(r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Inspecting source."}]}}"#).unwrap();
    let events = state.parse(r#"{"type":"tool_call","subtype":"completed","call_id":"call-1","tool_call":{"readToolCall":{"args":{"path":"lib.rs"},"result":{"content":"source"}},"startedAtMs":10,"completedAtMs":20,"toolCallId":"call-1","hookAdditionalContexts":[]}}"#).unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(state.items.len(), 2);
    assert!(
        matches!(&state.items[0], Item::AgentReasoning { content } if content == "Inspecting source.")
    );
    assert!(
        matches!(&state.items[1], Item::ToolCall { name, output: Some(_), .. } if name == "readToolCall")
    );
    state.parse(r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Tests passed."}]}}"#).unwrap();
    state
        .parse(r#"{"type":"result","subtype":"success","is_error":false,"result":"Final report."}"#)
        .unwrap();
    assert_eq!(state.items.len(), 3);
    assert_eq!(state.output.as_deref(), Some("Final report."));
}
