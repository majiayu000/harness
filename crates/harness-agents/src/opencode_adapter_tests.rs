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
        ParsedAcpMessage::Event(AgentEvent::ItemStartedKind {
            item_type: "tool_call".into()
        })
    );

    let completed = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"tool_call_update","toolCallId":"call_1","status":"completed"}}}"#;
    assert_eq!(
        parse_acp_message(completed).unwrap(),
        ParsedAcpMessage::Event(AgentEvent::ItemCompletedKind)
    );
}

#[test]
fn parse_usage_update_notification() {
    assert!(OpenCodeAcpAdapter::new(PathBuf::from("opencode")).reports_usage_cost());
    let line = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"usage_update","used":53000,"size":200000,"cost":{"amount":0.045,"currency":"USD"}}}}"#;
    assert_eq!(
        parse_acp_message(line),
        Some(ParsedAcpMessage::SessionCost { cost_usd: 0.045 })
    );

    let without_cost = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"usage_update","used":10,"size":12}}}"#;
    assert_eq!(
        parse_acp_message(without_cost),
        Some(ParsedAcpMessage::Ignore)
    );
    let non_usd = line.replace("USD", "EUR");
    assert_eq!(parse_acp_message(&non_usd), Some(ParsedAcpMessage::Ignore));
    let zero_usd = line.replace("0.045", "0.0");
    assert_eq!(
        parse_acp_message(&zero_usd),
        Some(ParsedAcpMessage::SessionCost { cost_usd: 0.0 })
    );
}

#[test]
fn cumulative_session_cost_requires_an_observed_turn_baseline() {
    assert_eq!(turn_cost(0.25, Some(0.0)).unwrap(), Some(0.25));
    assert_eq!(turn_cost(0.5, Some(0.25)).unwrap(), Some(0.25));
    assert_eq!(turn_cost(0.25, Some(0.25)).unwrap(), Some(0.0));
    assert_eq!(turn_cost(0.5, None).unwrap(), None);
    assert!(turn_cost(0.25, Some(0.5)).is_err());
}

#[test]
fn parse_permission_request() {
    let line = r#"{"jsonrpc":"2.0","id":7,"method":"session/request_permission","params":{"sessionId":"s1","toolCall":{"toolCallId":"call_1","title":"Run tests","kind":"execute","rawInput":{"command":"cargo test -p harness-agents"}},"options":[{"optionId":"allow-custom","name":"Allow once","kind":"allow_once"},{"optionId":"deny-custom","name":"Reject","kind":"reject_once"}]}}"#;
    let message = parse_acp_message(line).unwrap();
    assert_eq!(
        message,
        ParsedAcpMessage::PermissionRequest {
            id: json!(7),
            command: "cargo test -p harness-agents".into(),
            options: vec![
                json!({"optionId":"allow-custom","name":"Allow once","kind":"allow_once"}),
                json!({"optionId":"deny-custom","name":"Reject","kind":"reject_once"}),
            ],
        }
    );
}

#[test]
fn permission_response_uses_offered_option_without_persistent_escalation() {
    let options = vec![
        json!({"optionId":"forever","kind":"allow_always"}),
        json!({"optionId":"one-time","kind":"allow_once"}),
        json!({"optionId":"no","kind":"reject_once"}),
    ];
    assert_eq!(
        permission_response(&json!("7"), &options, &ApprovalDecision::Accept).unwrap(),
        json!({"jsonrpc":"2.0","id":"7","result":{"outcome":{"outcome":"selected","optionId":"one-time"}}})
    );
    assert_eq!(
        permission_response(
            &json!(7),
            &options,
            &ApprovalDecision::Reject {
                reason: "not now".into()
            }
        )
        .unwrap(),
        json!({"jsonrpc":"2.0","id":7,"result":{"outcome":{"outcome":"selected","optionId":"no"}}})
    );
    assert!(permission_response(&json!(7), &options[..1], &ApprovalDecision::Accept).is_err());
}

#[test]
fn parse_response() {
    let line = r#"{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}"#;
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
        ParsedAcpMessage::RpcError { id, error } => {
            assert_eq!(id, 2);
            assert_eq!(error["code"], -32602);
            assert_eq!(error["message"], "Invalid params");
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
fn permission_ids_preserve_json_rpc_id_types() {
    assert_eq!(request_id_string(&json!(42)), "42");
    assert_eq!(request_id_string(&json!("42")), r#""42""#);
    assert_ne!(
        request_id_string(&json!(42)),
        request_id_string(&json!("42"))
    );
}

#[tokio::test]
async fn stdout_reader_recognizes_container_canary_marker() -> anyhow::Result<()> {
    let mut child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(format!(
            "printf '%s\\n' '{}'",
            crate::spawn_contract::egress::CONTAINER_EGRESS_CANARY_VERIFIED,
        ))
        .stdout(std::process::Stdio::piped())
        .spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("missing stdout"))?;
    let mut lines = BufReader::new(stdout).lines();

    let message = OpenCodeAcpAdapter::read_next_message(&mut lines).await?;

    assert_eq!(
        message,
        Some(ParsedAcpMessage::Event(
            AgentEvent::EgressVerifiedAtDispatch
        ))
    );
    Ok(())
}

fn test_turn_request() -> AgentRequest {
    AgentRequest {
        prompt: "ping".to_string(),
        prompt_layers: None,
        project_root: PathBuf::from("/tmp"),
        permission_mode: Default::default(),
        model: None,
        reasoning_effort: None,
        execution_phase: None,
        sandbox_mode: None,
        approval_policy: None,
        allowed_tools: None,
        max_budget_usd: None,
        context: vec![],
        timeout_secs: None,
        env_vars: std::collections::HashMap::new(),
        capability_token: None,
    }
}

#[test]
fn session_new_uses_prepared_child_workspace() {
    let request = test_turn_request();
    let params = session_new_params(&request, std::path::Path::new("/workspace"));
    assert_eq!(params["cwd"], "/workspace");
    assert_eq!(params["mcpServers"], json!([]));
}

#[cfg(unix)]
mod lifecycle_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn fake_acp_server(dir: &std::path::Path, turn_script: &str) -> anyhow::Result<PathBuf> {
        let path = dir.join("fake-opencode");
        std::fs::write(
            &path,
            format!(
                r#"#!/bin/sh
IFS= read -r initialize || exit 1
printf '%s\n' '{{"id":1,"result":{{"protocolVersion":1}}}}'
IFS= read -r initialized || exit 1
IFS= read -r session_new || exit 1
printf '%s\n' '{{"id":2,"result":{{"sessionId":"s1"}}}}'
IFS= read -r prompt || exit 1
{turn_script}
while IFS= read -r ignored; do :; done
"#
            ),
        )?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
        Ok(path)
    }

    async fn next_permission(rx: &mut mpsc::Receiver<AgentEvent>) -> anyhow::Result<String> {
        tokio::time::timeout(Duration::from_secs(3), async {
            while let Some(event) = rx.recv().await {
                if let AgentEvent::ApprovalRequest { id, .. } = event {
                    return Ok(id);
                }
            }
            anyhow::bail!("ACP stream closed without a permission request")
        })
        .await?
    }

    fn request_for(dir: &std::path::Path) -> AgentRequest {
        let mut request = test_turn_request();
        request.project_root = dir.to_path_buf();
        request.permission_mode = harness_core::config::agents::AgentPermissionMode::Full;
        request.sandbox_mode = Some(SandboxMode::DangerFullAccess);
        request.timeout_secs = Some(2);
        request.env_vars.insert(
            "HARNESS_TEST_ACP_RESPONSES".into(),
            dir.join("responses.jsonl").display().to_string(),
        );
        request
    }

    #[tokio::test]
    async fn interrupt_before_permission_is_read_cancels_only_that_turn() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let server = fake_acp_server(
            dir.path(),
            r#"
printf '%s\n' '{"method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"ready"}}}}'
IFS= read -r cancel || exit 1
printf '%s\n' "$cancel" >> "$HARNESS_TEST_ACP_RESPONSES"
printf '%s\n' '{"id":7,"method":"session/request_permission","params":{"sessionId":"s1","toolCall":{"toolCallId":"late","title":"Late request"},"options":[{"optionId":"allow-late","kind":"allow_once","name":"Allow"}]}}'
IFS= read -r response || exit 1
printf '%s\n' "$response" >> "$HARNESS_TEST_ACP_RESPONSES"
printf '%s\n' '{"id":3,"result":{"stopReason":"cancelled"}}'
IFS= read -r prompt || exit 1
printf '%s\n' '{"id":8,"method":"session/request_permission","params":{"sessionId":"s1","toolCall":{"toolCallId":"next","title":"Next turn request"},"options":[{"optionId":"allow-next","kind":"allow_once","name":"Allow"}]}}'
IFS= read -r response || exit 1
printf '%s\n' "$response" >> "$HARNESS_TEST_ACP_RESPONSES"
printf '%s\n' '{"id":4,"result":{"stopReason":"end_turn"}}'
"#,
        )?;
        let adapter = Arc::new(OpenCodeAcpAdapter::new(server));
        let outcome = async {
            let (tx, mut rx) = mpsc::channel(16);
            let running = {
                let adapter = adapter.clone();
                let request = request_for(dir.path());
                tokio::spawn(async move { adapter.start_turn(request, tx).await })
            };
            tokio::time::timeout(Duration::from_secs(3), async {
                while let Some(event) = rx.recv().await {
                    if matches!(event, AgentEvent::MessageDelta { ref text } if text == "ready") {
                        return Ok(());
                    }
                }
                anyhow::bail!("ACP stream closed before cancellation barrier")
            })
            .await??;
            adapter.interrupt().await?;
            tokio::time::timeout(Duration::from_secs(3), running).await???;
            while let Some(event) = rx.recv().await {
                assert!(!matches!(event, AgentEvent::ApprovalRequest { .. }));
            }
            assert!(adapter
                .respond_approval("7".into(), ApprovalDecision::Accept)
                .await
                .is_err());

            let (tx, mut rx) = mpsc::channel(16);
            let running = {
                let adapter = adapter.clone();
                let request = request_for(dir.path());
                tokio::spawn(async move { adapter.start_turn(request, tx).await })
            };
            adapter
                .respond_approval(next_permission(&mut rx).await?, ApprovalDecision::Accept)
                .await?;
            tokio::time::timeout(Duration::from_secs(3), running).await???;
            let responses = std::fs::read_to_string(dir.path().join("responses.jsonl"))?
                .lines()
                .map(serde_json::from_str::<Value>)
                .collect::<Result<Vec<_>, _>>()?;
            assert_eq!(responses[0]["method"], "session/cancel");
            assert_eq!(responses[1], cancelled_permission_response(&json!(7)));
            assert_eq!(responses[2]["id"], 8);
            assert_eq!(responses[2]["result"]["outcome"]["optionId"], "allow-next");
            Ok(())
        }
        .await;
        adapter.terminate_and_drain().await?;
        assert!(!adapter.state.lock().await.turn_cancelled);
        outcome
    }

    #[tokio::test]
    async fn reused_session_reports_turn_cost_without_context_tokens() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let server = fake_acp_server(
            dir.path(),
            r#"
printf '%s\n' '{"method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"usage_update","used":53000,"size":200000}}}'
printf '%s\n' '{"method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"usage_update","used":53000,"size":200000,"cost":{"amount":0.25,"currency":"USD"}}}}'
printf '%s\n' '{"id":3,"result":{"stopReason":"end_turn"}}'
IFS= read -r prompt || exit 1
printf '%s\n' '{"method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"usage_update","used":60000,"size":200000,"cost":{"amount":0.375,"currency":"USD"}}}}'
printf '%s\n' '{"id":4,"result":{"stopReason":"end_turn"}}'
IFS= read -r prompt || exit 1
printf '%s\n' '{"method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"usage_update","used":60000,"size":200000}}}'
printf '%s\n' '{"id":5,"result":{"stopReason":"end_turn"}}'
IFS= read -r prompt || exit 1
printf '%s\n' '{"method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"usage_update","used":60000,"size":200000,"cost":{"amount":0.75,"currency":"USD"}}}}'
printf '%s\n' '{"id":6,"result":{"stopReason":"end_turn"}}'
"#,
        )?;
        let adapter = OpenCodeAcpAdapter::new(server);
        let outcome = async {
            for expected_cost in [Some(0.25), Some(0.125), None, None] {
                let (tx, mut rx) = mpsc::channel(16);
                adapter.start_turn(request_for(dir.path()), tx).await?;
                let mut costs = Vec::new();
                while let Some(event) = rx.recv().await {
                    match event {
                        AgentEvent::CostReported { cost_usd } => costs.push(cost_usd),
                        AgentEvent::TokenUsage { .. } => {
                            anyhow::bail!("ACP context occupancy was emitted as consumed tokens")
                        }
                        _ => {}
                    }
                }
                assert_eq!(costs, expected_cost.into_iter().collect::<Vec<_>>());
            }
            Ok(())
        }
        .await;
        adapter.terminate_and_drain().await?;
        outcome
    }

    #[tokio::test]
    async fn permission_round_trip_preserves_options_and_numeric_string_ids() -> anyhow::Result<()>
    {
        let dir = tempfile::tempdir()?;
        let server = fake_acp_server(
            dir.path(),
            r#"
printf '%s\n' '{"id":7,"method":"session/request_permission","params":{"sessionId":"s1","toolCall":{"toolCallId":"call_1","title":"Run tests"},"options":[{"optionId":"allow-custom","name":"Allow once","kind":"allow_once"}]}}'
printf '%s\n' '{"id":"7","method":"session/request_permission","params":{"sessionId":"s1","toolCall":{"toolCallId":"call_2","title":"Delete output"},"options":[{"optionId":"deny-custom","name":"Reject","kind":"reject_once"}]}}'
IFS= read -r response || exit 1
printf '%s\n' "$response" >> "$HARNESS_TEST_ACP_RESPONSES"
IFS= read -r response || exit 1
printf '%s\n' "$response" >> "$HARNESS_TEST_ACP_RESPONSES"
printf '%s\n' '{"id":3,"result":{"stopReason":"end_turn"}}'
"#,
        )?;
        let adapter = Arc::new(OpenCodeAcpAdapter::new(server));
        let (tx, mut rx) = mpsc::channel(8);
        let request = request_for(dir.path());
        let execution = tokio::spawn({
            let adapter = adapter.clone();
            async move { adapter.start_turn(request, tx).await }
        });
        let result = async {
            let first = next_permission(&mut rx).await?;
            adapter.respond_approval(first.clone(), ApprovalDecision::Accept).await?;
            let second = next_permission(&mut rx).await?;
            assert_ne!(first, second);
            adapter.respond_approval(second, ApprovalDecision::Reject { reason: "not requested".into() }).await?;
            tokio::time::timeout(Duration::from_secs(3), execution).await???;
            assert!(adapter.respond_approval(first, ApprovalDecision::Accept).await.is_err());
            let replies: Vec<Value> = std::fs::read_to_string(dir.path().join("responses.jsonl"))?
                .lines().map(serde_json::from_str).collect::<Result<_, _>>()?;
            assert_eq!(replies, vec![
                json!({"jsonrpc":"2.0","id":7,"result":{"outcome":{"outcome":"selected","optionId":"allow-custom"}}}),
                json!({"jsonrpc":"2.0","id":"7","result":{"outcome":{"outcome":"selected","optionId":"deny-custom"}}}),
            ]);
            Ok::<(), anyhow::Error>(())
        }.await;
        adapter.terminate_and_drain().await?;
        result
    }

    #[tokio::test]
    async fn interrupt_cancels_pending_permission_response() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let server = fake_acp_server(
            dir.path(),
            r#"
printf '%s\n' '{"id":"permission","method":"session/request_permission","params":{"sessionId":"s1","toolCall":{"toolCallId":"call_1","title":"Run tests"},"options":[{"optionId":"allow-custom","name":"Allow once","kind":"allow_once"}]}}'
IFS= read -r response || exit 1
printf '%s\n' "$response" >> "$HARNESS_TEST_ACP_RESPONSES"
IFS= read -r response || exit 1
printf '%s\n' "$response" >> "$HARNESS_TEST_ACP_RESPONSES"
printf '%s\n' '{"id":3,"result":{"stopReason":"cancelled"}}'
"#,
        )?;
        let adapter = Arc::new(OpenCodeAcpAdapter::new(server));
        let (tx, mut rx) = mpsc::channel(8);
        let request = request_for(dir.path());
        let execution = tokio::spawn({
            let adapter = adapter.clone();
            async move { adapter.start_turn(request, tx).await }
        });
        let result = async {
            let permission = next_permission(&mut rx).await?;
            adapter.interrupt().await?;
            tokio::time::timeout(Duration::from_secs(3), execution).await???;
            assert!(adapter.respond_approval(permission, ApprovalDecision::Accept).await.is_err());
            let replies: Vec<Value> = std::fs::read_to_string(dir.path().join("responses.jsonl"))?
                .lines().map(serde_json::from_str).collect::<Result<_, _>>()?;
            assert_eq!(replies[0]["method"], "session/cancel");
            assert_eq!(replies[1], json!({"jsonrpc":"2.0","id":"permission","result":{"outcome":{"outcome":"cancelled"}}}));
            assert!(adapter.state.lock().await.pending_permissions.is_empty());
            Ok::<(), anyhow::Error>(())
        }.await;
        adapter.terminate_and_drain().await?;
        result
    }
}
