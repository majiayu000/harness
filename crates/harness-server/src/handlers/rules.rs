use crate::http::AppState;
use harness_protocol::{RpcResponse, INTERNAL_ERROR};
use std::path::PathBuf;

pub async fn rule_load(
    state: &AppState,
    id: Option<serde_json::Value>,
    project_root: PathBuf,
) -> RpcResponse {
    let mut rules = state.rules.write().await;
    match rules.load(&project_root) {
        Ok(()) => {
            let count = rules.rules().len();
            RpcResponse::success(id, serde_json::json!({ "rules_count": count }))
        }
        Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
    }
}

pub async fn rule_check(
    state: &AppState,
    id: Option<serde_json::Value>,
    project_root: PathBuf,
    files: Option<Vec<PathBuf>>,
) -> RpcResponse {
    let result = {
        let rules = state.rules.read().await;
        match files {
            Some(f) => rules.scan_files(&project_root, &f).await,
            None => rules.scan(&project_root).await,
        }
    };
    match result {
        Ok(violations) => {
            let session_id = harness_core::SessionId::new();
            for violation in &violations {
                let decision = match violation.severity {
                    harness_core::Severity::Critical | harness_core::Severity::High => {
                        harness_core::Decision::Block
                    }
                    harness_core::Severity::Medium => harness_core::Decision::Warn,
                    harness_core::Severity::Low => harness_core::Decision::Pass,
                };
                let mut event = harness_core::Event::new(
                    session_id.clone(),
                    "rule_check",
                    violation.rule_id.as_str(),
                    decision,
                );
                event.reason = Some(violation.message.clone());
                event.detail = Some(format!(
                    "{}:{}",
                    violation.file.display(),
                    violation
                        .line
                        .map(|l| l.to_string())
                        .unwrap_or_default()
                ));
                if let Err(e) = state.events.log(&event) {
                    tracing::warn!("failed to log rule violation event: {e}");
                }
            }
            match serde_json::to_value(&violations) {
                Ok(v) => RpcResponse::success(id, v),
                Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
            }
        }
        Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
    }
}
