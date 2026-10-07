use super::tests::{test_turn_request, write_app_server_stub};
use super::*;

const HANDSHAKE: &str = r#"
IFS= read -r initialize || exit 1
printf '%s\n' '{"id":999,"error":{"code":-32600,"message":"unrelated initialize response"}}'
printf '%s\n' '{"id":1,"result":{}}'
IFS= read -r initialized || exit 1
IFS= read -r thread_start || exit 1
printf '%s\n' '{"method":"thread/started","params":{"thread":{"id":"thread-1"}}}'
printf '%s\n' '{"id":2,"result":{"thread":{"id":"thread-1"}}}'
IFS= read -r turn_start || exit 1
"#;

const TURN_STARTED: &str = r#"
printf '%s\n' '{"id":3,"result":{"turn":{"id":"turn-1"}}}'
printf '%s\n' '{"method":"turn/started","params":{"threadId":"thread-1","turn":{"id":"turn-1","status":"inProgress","items":[]}}}'
"#;

const COMPLETE_AFTER_GATE: &str = r#"
while [ ! -e "$HARNESS_TEST_COMPLETE_GATE" ]; do sleep 0.01; done
printf '%s\n' '{"method":"turn/completed","params":{"threadId":"thread-1","turn":{"id":"turn-1","status":"completed","items":[]}}}'
"#;

fn deadlines() -> AttemptDeadlines {
    AttemptDeadlines {
        absolute_init: Duration::from_secs(5),
        frame_write: Duration::from_secs(2),
        stop_cleanup: Duration::from_secs(2),
    }
}

async fn wait_for_event(
    rx: &mut mpsc::Receiver<AgentEvent>,
    predicate: impl Fn(&AgentEvent) -> bool,
) -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = rx.recv().await {
            if predicate(&event) {
                return Ok(());
            }
        }
        anyhow::bail!("Codex stream closed before expected test event")
    })
    .await?
}

#[test]
fn rpc_error_preserves_request_id_type_and_error_data() {
    for id in [json!(4), json!(999), json!("4")] {
        let error =
            json!({"code":-32600,"message":"steer rejected","data":{"reason":"stale turn"}});
        let frame = json!({"id":id,"error":error}).to_string();
        assert_eq!(
            parse_codex_message(&frame),
            Some(ParsedCodexMessage::RpcError { id, error })
        );
    }
}

#[tokio::test]
async fn steering_rejection_reaches_caller_and_turn_still_completes() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let stub = write_app_server_stub(
        dir.path(),
        &format!(
            r#"
{HANDSHAKE}
{TURN_STARTED}
IFS= read -r steer || exit 1
printf '%s\n' '{{"id":999,"error":{{"code":-32600,"message":"unrelated request failed"}}}}'
printf '%s\n' '{{"id":4,"error":{{"code":-32600,"message":"steer rejected"}}}}'
IFS= read -r steer || exit 1
printf '%s\n' '{{"id":5,"result":{{"turnId":"turn-1"}}}}'
{COMPLETE_AFTER_GATE}
"#
        ),
    )?;
    let adapter = Arc::new(CodexAdapter::new(stub).with_deadlines(deadlines()));
    let gate = dir.path().join("complete");
    let mut request = test_turn_request(dir.path().to_path_buf());
    request.env_vars.insert(
        "HARNESS_TEST_COMPLETE_GATE".into(),
        gate.display().to_string(),
    );
    let (tx, mut rx) = mpsc::channel(16);
    let running = {
        let adapter = adapter.clone();
        tokio::spawn(async move { adapter.start_turn(request, tx).await })
    };
    let outcome = async {
        wait_for_event(&mut rx, |event| matches!(event, AgentEvent::TurnStarted)).await?;
        let error = adapter
            .steer("first instruction".into())
            .await
            .expect_err("steering rejection must reach its caller");
        assert!(error
            .to_string()
            .contains("request 4 failed: steer rejected"));
        assert_eq!(
            adapter.state.lock().await.active_turn_id.as_deref(),
            Some("turn-1")
        );
        adapter.steer("second instruction".into()).await?;
        std::fs::write(&gate, "")?;
        tokio::time::timeout(Duration::from_secs(5), running).await???;
        let mut saw_diagnostic = false;
        let mut completed = false;
        while let Some(event) = rx.recv().await {
            match event {
                AgentEvent::Diagnostic { message, .. } => {
                    saw_diagnostic |= message.contains("request 999 failed")
                }
                AgentEvent::TurnCompleted { .. } => completed = true,
                AgentEvent::Error { message } => {
                    anyhow::bail!("auxiliary failure ended the turn: {message}")
                }
                _ => {}
            }
        }
        assert!(saw_diagnostic, "unmatched errors remain visible");
        assert!(completed, "explicit turn completion must still be consumed");
        Ok(())
    }
    .await;
    adapter.terminate_and_drain().await?;
    outcome
}

#[tokio::test]
async fn matching_initialize_thread_and_turn_start_errors_fail_the_attempt() -> anyhow::Result<()> {
    let cases = [
        (1, r#"IFS= read -r initialize || exit 1"#.to_string()),
        (
            2,
            r#"
IFS= read -r initialize || exit 1
printf '%s\n' '{"id":1,"result":{}}'
IFS= read -r initialized || exit 1
IFS= read -r thread_start || exit 1
printf '%s\n' '{"method":"thread/started","params":{"thread":{"id":"thread-1"}}}'
"#
            .to_string(),
        ),
        (3, HANDSHAKE.to_string()),
    ];
    for (id, prefix) in cases {
        let dir = tempfile::tempdir()?;
        let response = json!({"id":id,"error":{"code":-32602,"message":"start rejected"}});
        let stub = write_app_server_stub(
            dir.path(),
            &format!("{prefix}\nprintf '%s\\n' '{response}'"),
        )?;
        let adapter = CodexAdapter::new(stub).with_deadlines(deadlines());
        let (tx, _rx) = mpsc::channel(16);
        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            adapter.start_turn(test_turn_request(dir.path().to_path_buf()), tx),
        )
        .await;
        let cleaned_up = adapter.state.lock().await.child.is_none();
        adapter.terminate_and_drain().await?;
        let error =
            outcome?.expect_err("a matching setup/start request failure must fail the attempt");
        assert!(
            error
                .to_string()
                .contains(&format!("request {id} failed: start rejected")),
            "{error}"
        );
        assert!(
            cleaned_up,
            "failed setup/start attempts must drain their child"
        );
    }
    Ok(())
}

#[tokio::test]
async fn steering_timeout_is_bounded_and_late_response_does_not_end_turn() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let stub = write_app_server_stub(
        dir.path(),
        &format!(
            r#"
{HANDSHAKE}
{TURN_STARTED}
IFS= read -r steer || exit 1
printf '%s\n' '{{"method":"item/agentMessage/delta","params":{{"delta":"steer received"}}}}'
IFS= read -r steer || exit 1
printf '%s\n' '{{"id":4,"error":{{"code":-32600,"message":"late steering rejection"}}}}'
printf '%s\n' '{{"id":5,"result":{{"turnId":"turn-1"}}}}'
{COMPLETE_AFTER_GATE}
"#
        ),
    )?;
    let adapter = Arc::new(CodexAdapter::new(stub).with_deadlines(AttemptDeadlines {
        frame_write: Duration::from_millis(300),
        ..deadlines()
    }));
    let gate = dir.path().join("complete");
    let mut request = test_turn_request(dir.path().to_path_buf());
    request.env_vars.insert(
        "HARNESS_TEST_COMPLETE_GATE".into(),
        gate.display().to_string(),
    );
    let (tx, mut rx) = mpsc::channel(16);
    let running = {
        let adapter = adapter.clone();
        tokio::spawn(async move { adapter.start_turn(request, tx).await })
    };
    let outcome = async {
        wait_for_event(&mut rx, |event| matches!(event, AgentEvent::TurnStarted)).await?;
        let steering = {
            let adapter = adapter.clone();
            tokio::spawn(async move { adapter.steer("lost response".into()).await })
        };
        wait_for_event(
            &mut rx,
            |event| matches!(event, AgentEvent::MessageDelta { text } if text == "steer received"),
        )
        .await?;
        let overlap = adapter
            .steer("overlapping request".into())
            .await
            .expect_err("only one outstanding steer is allowed");
        assert!(overlap.to_string().contains("already pending"));
        let error = tokio::time::timeout(Duration::from_secs(2), steering)
            .await??
            .expect_err("a missing acknowledgment must time out");
        assert!(error.to_string().contains("response deadline elapsed"));
        assert!(adapter.state.lock().await.pending_steer.is_none());
        adapter.steer("retry after timeout".into()).await?;
        std::fs::write(&gate, "")?;
        tokio::time::timeout(Duration::from_secs(5), running).await???;
        let mut completed = false;
        while let Some(event) = rx.recv().await {
            assert!(!matches!(event, AgentEvent::Error { .. }));
            completed |= matches!(event, AgentEvent::TurnCompleted { .. });
        }
        assert!(completed);
        Ok(())
    }
    .await;
    adapter.terminate_and_drain().await?;
    outcome
}
