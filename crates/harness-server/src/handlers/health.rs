use crate::http::AppState;
use harness_core::{EventFilters, Violation};
use harness_observe::{health::generate_health_report, stats};
use harness_protocol::{RpcResponse, INTERNAL_ERROR};
use std::path::PathBuf;

pub async fn health_check(
    state: &AppState,
    id: Option<serde_json::Value>,
    project_root: PathBuf,
) -> RpcResponse {
    let project_root = match crate::handlers::validate_project_root(&project_root) {
        Ok(p) => p,
        Err(e) => return RpcResponse::error(id, INTERNAL_ERROR, e),
    };

    let events = match state.events.query(&EventFilters::default()) {
        Ok(evts) => evts,
        Err(e) => return RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
    };

    let violations: Vec<Violation> = {
        let rules = state.rules.read().await;
        rules.scan(&project_root).await.unwrap_or_default()
    };
    crate::handlers::persist_violations(&state.events, &violations);

    let report = generate_health_report(&events, &violations);
    match serde_json::to_value(&report) {
        Ok(v) => RpcResponse::success(id, v),
        Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
    }
}

pub async fn stats_query(
    state: &AppState,
    id: Option<serde_json::Value>,
    since: Option<chrono::DateTime<chrono::Utc>>,
    until: Option<chrono::DateTime<chrono::Utc>>,
) -> RpcResponse {
    let filters = EventFilters { since, until, ..Default::default() };
    let events = match state.events.query(&filters) {
        Ok(evts) => evts,
        Err(e) => return RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
    };

    let hook_stats = stats::aggregate_hook_stats(&events);
    let trends = stats::compute_trends(&events, 7);
    let rule_stats = stats::aggregate_rule_stats(&events);

    match serde_json::to_value(serde_json::json!({
        "hook_stats": hook_stats,
        "trends": trends,
        "rule_stats": rule_stats,
    })) {
        Ok(v) => RpcResponse::success(id, v),
        Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
    }
}
