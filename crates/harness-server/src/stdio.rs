use crate::http::AppState;
use crate::router;
use harness_protocol::{codec, RpcResponse};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

async fn process_line(state: &AppState, line: &str) -> anyhow::Result<String> {
    let response = match codec::decode_request(line) {
        Ok(req) => router::handle_request(state, req).await,
        Err(e) => RpcResponse::error(
            None,
            harness_protocol::PARSE_ERROR,
            format!("parse error: {e}"),
        ),
    };

    Ok(codec::encode_response(&response)?)
}

/// Serve JSON-RPC over stdio (one JSON object per line).
pub async fn serve(state: &AppState) -> anyhow::Result<()> {
    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let reader = BufReader::new(stdin);
    let mut lines = reader.lines();

    tracing::info!("harness: stdio server started");

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }

        let out = process_line(state, &line).await?;
        stdout.write_all(out.as_bytes()).await?;
        stdout.write_all(b"\n").await?;
        stdout.flush().await?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{http::AppState, server::HarnessServer, thread_manager::ThreadManager};
    use harness_agents::AgentRegistry;
    use harness_core::HarnessConfig;
    use harness_protocol::{Method, RpcRequest};
    use std::sync::Arc;
    use tokio::sync::RwLock;

    async fn make_test_state(dir: &std::path::Path) -> anyhow::Result<AppState> {
        let server = Arc::new(HarnessServer::new(
            HarnessConfig::default(),
            ThreadManager::new(),
            AgentRegistry::new("test"),
        ));
        let tasks = crate::task_runner::TaskStore::open(&dir.join("tasks.db")).await?;
        let events = Arc::new(harness_observe::EventStore::new(dir)?);
        let signal_detector = harness_gc::SignalDetector::new(
            harness_gc::signal_detector::SignalThresholds::default(),
            harness_core::ProjectId::new(),
        );
        let draft_store = harness_gc::DraftStore::new(dir)?;
        let gc_agent = Arc::new(harness_gc::GcAgent::new(
            harness_gc::gc_agent::GcConfig::default(),
            signal_detector,
            draft_store,
        ));
        let thread_db = crate::thread_db::ThreadDb::open(&dir.join("threads.db")).await?;

        Ok(AppState {
            server,
            tasks,
            skills: Arc::new(RwLock::new(harness_skills::SkillStore::new())),
            rules: Arc::new(RwLock::new(harness_rules::engine::RuleEngine::new())),
            events,
            gc_agent,
            plans: Arc::new(RwLock::new(std::collections::HashMap::new())),
            thread_db: Some(thread_db),
            interceptors: vec![],
        })
    }

    #[tokio::test]
    async fn stdio_processes_initialize_then_initialized() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let state = make_test_state(dir.path()).await?;

        let init_line = serde_json::to_string(&RpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: Method::Initialize,
        })?;
        let init_out = process_line(&state, &init_line).await?;
        let init_resp: harness_protocol::RpcResponse = codec::decode_response(&init_out)?;
        assert!(
            init_resp.error.is_none(),
            "initialize failed: {:?}",
            init_resp.error
        );
        let init_result = init_resp
            .result
            .ok_or_else(|| anyhow::anyhow!("initialize response missing result"))?;
        assert!(
            init_result["capabilities"].is_object(),
            "initialize should return capabilities"
        );

        let initialized_line = serde_json::to_string(&RpcRequest {
            jsonrpc: "2.0".to_string(),
            id: None,
            method: Method::Initialized,
        })?;
        let initialized_out = process_line(&state, &initialized_line).await?;
        let initialized_resp: harness_protocol::RpcResponse =
            codec::decode_response(&initialized_out)?;
        assert!(
            initialized_resp.error.is_none(),
            "initialized failed: {:?}",
            initialized_resp.error
        );
        assert!(
            initialized_resp.result.is_some(),
            "initialized should return success payload"
        );
        Ok(())
    }
}
