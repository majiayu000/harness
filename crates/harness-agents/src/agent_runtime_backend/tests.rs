use super::*;
use agent_runtime::core::{NativeOption, PendingRequest, ReplyVia, RequestId, RequestKind};

/// A shell-script agent registered with agent-runtime through an override
/// file: no real agent, no quota. `$1` is the prompt.
fn overlay(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("agents.toml");
    std::fs::write(
        &path,
        r#"
[[agents]]
id = "script"
[[agents.auth]]
method = "none"
programmatic = "allowed"
source = "test"
[[agents.channels]]
id = "script-print"
kind = "one_shot"
driver = "one_shot"
permission = { method = "launch", mode = "none" }
[agents.channels.launch]
program = "/bin/sh"
[agents.channels.oneshot]
args = ["-c", "case \"$1\" in *FAIL*) printf '{\"type\":\"end\",\"error\":{\"message\":\"429 quota used up\"}}\\n';; *SILENT*) printf '{\"type\":\"end\",\"stop\":\"end_turn\"}\\n';; *HANG*) exit 0;; *) printf '{\"type\":\"text\",\"data\":\"PONG\"}\\n{\"type\":\"usage\",\"in\":3,\"out\":1}\\n{\"type\":\"end\",\"stop\":\"end_turn\"}\\n';; esac", "sh", "{prompt}"]
[[agents.channels.oneshot.rules]]
when = { "/type" = "text" }
emit = "text"
text = "/data"
[[agents.channels.oneshot.rules]]
when = { "/type" = "usage" }
emit = "usage"
input = "/in"
output = "/out"
[[agents.channels.oneshot.rules]]
when = { "/type" = "end" }
emit = "end"
stop = "/stop"
error = "/error"
message = "/error/message"
"#,
    )
    .expect("write overlay");
    path
}

fn backend(dir: &std::path::Path) -> (AgentRuntimeBackend, std::path::PathBuf) {
    let log = dir.join("trial.jsonl");
    let config = AgentRuntimeBackendConfig {
        enabled_for: vec!["claude".into()],
        agent_ids: [("claude".to_string(), "script".to_string())].into(),
        config_files: vec![overlay(dir)],
        trial_log: Some(log.clone()),
        ..Default::default()
    };
    let runtime = runtime_from_config(&config).expect("runtime");
    (
        AgentRuntimeBackend::new("claude", &config, SandboxMode::WorkspaceWrite, runtime),
        log,
    )
}

fn request(dir: &std::path::Path, prompt: &str) -> AgentRequest {
    AgentRequest {
        prompt: prompt.into(),
        project_root: dir.to_path_buf(),
        ..Default::default()
    }
}

fn lines(log: &std::path::Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).expect("json line"))
        .collect()
}

#[tokio::test]
async fn completed_turn_returns_output_and_logs_a_clean_call() {
    let dir = tempfile::tempdir().expect("tmp");
    let (backend, log) = backend(dir.path());
    let response = backend
        .execute(request(dir.path(), "hello"))
        .await
        .expect("ok");
    assert_eq!(response.output, "PONG");
    assert_eq!(response.token_usage.input_tokens, 3);
    let record = &lines(&log)[0];
    assert_eq!(record["harness_agent"], "claude");
    assert_eq!(record["runtime_agent"], "script");
    assert_eq!(record["channel"], "script-print");
    assert_eq!(record["outcome"], "completed");
    assert_eq!(record["reported_success"], true);
    assert_eq!(record["suspect_false_success"], false);
    assert_eq!(record["leftovers"], 0);
}

#[tokio::test]
async fn quota_failure_is_an_error_never_success() {
    let dir = tempfile::tempdir().expect("tmp");
    let (backend, log) = backend(dir.path());
    let error = backend
        .execute(request(dir.path(), "FAIL now"))
        .await
        .expect_err("failure");
    assert!(error.to_string().contains("usage limit"), "{error}");
    let record = &lines(&log)[0];
    assert_eq!(record["outcome"], "failed");
    assert_eq!(record["failure_class"], "usage_limit");
    assert_eq!(record["reported_success"], false);
}

#[tokio::test]
async fn end_without_output_is_unknown_and_an_error() {
    let dir = tempfile::tempdir().expect("tmp");
    let (backend, log) = backend(dir.path());
    let error = backend
        .execute(request(dir.path(), "SILENT"))
        .await
        .expect_err("unknown");
    assert!(error.to_string().contains("did not complete"), "{error}");
    assert_eq!(lines(&log)[0]["outcome"], "unknown");
}

#[tokio::test]
async fn process_exit_without_an_end_signal_is_an_error() {
    let dir = tempfile::tempdir().expect("tmp");
    let (backend, log) = backend(dir.path());
    assert!(backend.execute(request(dir.path(), "HANG")).await.is_err());
    assert_eq!(lines(&log)[0]["reported_success"], false);
}

#[tokio::test]
async fn stream_forwards_text_and_ends_with_done() {
    let dir = tempfile::tempdir().expect("tmp");
    let (backend, _) = backend(dir.path());
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    backend
        .execute_stream(request(dir.path(), "hi"), tx)
        .await
        .expect("ok");
    let mut items = Vec::new();
    while let Some(item) = rx.recv().await {
        items.push(item);
    }
    assert!(items
        .iter()
        .any(|i| matches!(i, StreamItem::MessageDelta { text } if text == "PONG")));
    assert!(matches!(items.last(), Some(StreamItem::Done)));
}

#[tokio::test]
async fn an_unknown_agent_is_an_error_and_still_logged() {
    let dir = tempfile::tempdir().expect("tmp");
    let log = dir.path().join("trial.jsonl");
    let config = AgentRuntimeBackendConfig {
        agent_ids: [("claude".to_string(), "no-such-agent".to_string())].into(),
        trial_log: Some(log.clone()),
        ..Default::default()
    };
    let runtime = runtime_from_config(&config).expect("runtime");
    let backend = AgentRuntimeBackend::new("claude", &config, SandboxMode::WorkspaceWrite, runtime);
    assert!(backend.execute(request(dir.path(), "x")).await.is_err());
    assert_eq!(lines(&log)[0]["outcome"], "none");
}

fn pending(raw: serde_json::Value) -> PendingRequest {
    PendingRequest {
        id: RequestId("r".into()),
        kind: RequestKind::Permission,
        title: None,
        options: vec![
            NativeOption {
                id: "once".into(),
                label: "Allow".into(),
                kind: "allow_once".into(),
            },
            NativeOption {
                id: "always".into(),
                label: "Always".into(),
                kind: "allow_always".into(),
            },
            NativeOption {
                id: "no".into(),
                label: "No".into(),
                kind: "reject_once".into(),
            },
        ],
        reply_via: ReplyVia::Rpc,
        live_only: true,
        raw,
    }
}

#[test]
fn approvals_follow_the_allowlist_and_never_pick_always() {
    let scoped = AgentRequest {
        allowed_tools: Some(vec!["Read".into(), "Edit".into(), "Write".into()]),
        ..Default::default()
    };
    let grants = approval::Grants::from_request(&scoped);
    let edit = pending(serde_json::json!({"tool_name": "Edit"}));
    let bash = pending(serde_json::json!({"tool_name": "Bash"}));
    let odd = pending(serde_json::json!({"something": "else"}));
    assert!(matches!(grants.decide(&edit), Some(agent_runtime::Reply::Select(id)) if id == "once"));
    assert!(grants.decide(&bash).is_none(), "Bash is not granted");
    assert!(
        grants.decide(&odd).is_none(),
        "unrecognised requests are never approved"
    );
    assert!(matches!(approval::reject(&bash), agent_runtime::Reply::Select(id) if id == "no"));
    let full = approval::Grants::from_request(&AgentRequest {
        allowed_tools: None,
        ..Default::default()
    });
    assert!(matches!(full.decide(&odd), Some(agent_runtime::Reply::Select(id)) if id == "once"));
}
