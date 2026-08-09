use harness_core::agent::StreamItem;
use std::process::Stdio;

fn output_script(payload: &str, include_canary_marker: bool) -> String {
    if include_canary_marker {
        format!(
            "printf '%s\\n%s\\n' '{}' '{payload}'",
            crate::spawn_contract::egress::CONTAINER_EGRESS_CANARY_VERIFIED,
        )
    } else {
        format!("printf '%s\\n' '{payload}'")
    }
}

#[tokio::test]
async fn codex_waits_for_container_canary_marker() -> anyhow::Result<()> {
    let payload = r#"{"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":2}}"#;
    let mut child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(output_script(payload, true))
        .stdout(Stdio::piped())
        .spawn()?;
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);

    let parsed =
        crate::codex::codex_exec_parser::stream_codex_exec_output(&mut child, &tx, None, true)
            .await?;

    assert!(matches!(
        rx.recv().await,
        Some(StreamItem::EgressVerifiedAtDispatch)
    ));
    assert_eq!(parsed.token_usage.total_tokens, 3);
    Ok(())
}

#[tokio::test]
async fn codex_rejects_missing_container_canary_marker() -> anyhow::Result<()> {
    let payload = r#"{"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":2}}"#;
    let mut child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(output_script(payload, false))
        .stdout(Stdio::piped())
        .spawn()?;
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);

    let error =
        crate::codex::codex_exec_parser::stream_codex_exec_output(&mut child, &tx, None, true)
            .await
            .expect_err("missing canary marker must fail");

    assert!(error
        .to_string()
        .contains("before the container egress canary"));
    assert!(!matches!(
        rx.try_recv(),
        Ok(StreamItem::EgressVerifiedAtDispatch)
    ));
    Ok(())
}

#[tokio::test]
async fn claude_waits_for_container_canary_marker() -> anyhow::Result<()> {
    let payload = r#"{"type":"result","subtype":"success","result":"done"}"#;
    let mut child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(output_script(payload, true))
        .stdout(Stdio::piped())
        .spawn()?;
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);

    let parsed =
        crate::claude_stream::stream_claude_code_output(&mut child, &tx, None, true).await?;

    assert!(matches!(
        rx.recv().await,
        Some(StreamItem::EgressVerifiedAtDispatch)
    ));
    assert_eq!(parsed.output, "done");
    Ok(())
}
