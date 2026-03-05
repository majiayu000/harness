use crate::http::AppState;
use harness_protocol::{RpcResponse, INTERNAL_ERROR};
use std::path::PathBuf;

pub async fn rule_load(
    state: &AppState,
    id: Option<serde_json::Value>,
    project_root: PathBuf,
) -> RpcResponse {
    let project_root = match crate::handlers::validate_project_root(&project_root) {
        Ok(p) => p,
        Err(e) => return RpcResponse::error(id, INTERNAL_ERROR, e),
    };
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
    let project_root = match crate::handlers::validate_project_root(&project_root) {
        Ok(p) => p,
        Err(e) => return RpcResponse::error(id, INTERNAL_ERROR, e),
    };
    let rules = crate::handlers::snapshot_rule_engine(state.rules.as_ref()).await;
    let result = match files {
        Some(f) => {
            // Validate each file is within the project root to prevent path traversal.
            let mut validated = Vec::with_capacity(f.len());
            for file in &f {
                match crate::handlers::validate_file_in_root(file, &project_root) {
                    Ok(p) => validated.push(p),
                    Err(e) => return RpcResponse::error(id, INTERNAL_ERROR, e),
                }
            }
            rules.scan_files(&project_root, &validated).await
        }
        None => rules.scan(&project_root).await,
    };
    match result {
        Ok(violations) => {
            crate::handlers::persist_violations(&state.events, &violations);
            match serde_json::to_value(&violations) {
                Ok(v) => RpcResponse::success(id, v),
                Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
            }
        }
        Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
    }
}
