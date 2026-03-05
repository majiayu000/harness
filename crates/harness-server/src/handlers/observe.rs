use crate::http::AppState;
use harness_protocol::{RpcResponse, INTERNAL_ERROR};
use std::path::PathBuf;

pub async fn event_log(
    state: &AppState,
    id: Option<serde_json::Value>,
    event: harness_core::Event,
) -> RpcResponse {
    match state.events.log(&event) {
        Ok(event_id) => RpcResponse::success(
            id,
            serde_json::json!({ "logged": true, "event_id": event_id }),
        ),
        Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
    }
}

pub async fn event_query(
    state: &AppState,
    id: Option<serde_json::Value>,
    filters: harness_core::EventFilters,
) -> RpcResponse {
    match state.events.query(&filters) {
        Ok(events) => match serde_json::to_value(&events) {
            Ok(v) => RpcResponse::success(id, v),
            Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
        },
        Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
    }
}

pub async fn metrics_collect(
    state: &AppState,
    id: Option<serde_json::Value>,
    project_root: PathBuf,
) -> RpcResponse {
    let project_root = match crate::handlers::validate_project_root(&project_root) {
        Ok(p) => p,
        Err(e) => return RpcResponse::error(id, INTERNAL_ERROR, e),
    };

    // scan -> persist -> query -> grade
    let violations = {
        let rules = state.rules.read().await;
        rules.scan(&project_root).await.unwrap_or_default()
    };
    crate::handlers::persist_violations(&state.events, &violations);

    let evts = match state.events.query(&harness_core::EventFilters::default()) {
        Ok(e) => e,
        Err(e) => return RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
    };

    let violation_count = violations.len();
    let report = harness_observe::QualityGrader::grade(&evts, violation_count);
    match serde_json::to_value(&report) {
        Ok(v) => RpcResponse::success(id, v),
        Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
    }
}

pub async fn metrics_query(
    state: &AppState,
    id: Option<serde_json::Value>,
    filters: harness_core::MetricFilters,
) -> RpcResponse {
    let event_filters = harness_core::EventFilters {
        since: filters.since,
        until: filters.until,
        ..Default::default()
    };
    let events = state.events.query(&event_filters);
    match events {
        Ok(evts) => {
            let violation_count = evts
                .iter()
                .filter(|e| e.hook == "rule_check")
                .count();
            let report = harness_observe::QualityGrader::grade(&evts, violation_count);
            match serde_json::to_value(&report) {
                Ok(v) => RpcResponse::success(id, v),
                Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
            }
        }
        Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
    }
}
