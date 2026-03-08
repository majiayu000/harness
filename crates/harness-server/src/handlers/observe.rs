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
        match rules.scan(&project_root).await {
            Ok(violations) => violations,
            Err(err) => {
                tracing::error!(
                    project_root = %project_root.display(),
                    error = %err,
                    "metrics/collect: rules scan failed"
                );
                return RpcResponse::error(id, INTERNAL_ERROR, err.to_string());
            }
        }
    };
    state.events.persist_rule_scan(&project_root, &violations);

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{http::build_app_state, server::HarnessServer, thread_manager::ThreadManager};
    use harness_agents::AgentRegistry;
    use harness_core::{EventFilters, GuardId, HarnessConfig, Language};
    use harness_rules::engine::Guard;
    use std::path::Path;
    use std::sync::Arc;

    use crate::test_helpers::tempdir_in_home;

    async fn make_test_state(project_root: &Path, data_dir: &Path) -> anyhow::Result<AppState> {
        let mut config = HarnessConfig::default();
        config.server.project_root = project_root.to_path_buf();
        config.server.data_dir = data_dir.to_path_buf();
        let server = Arc::new(HarnessServer::new(
            config,
            ThreadManager::new(),
            AgentRegistry::new("test"),
        ));
        build_app_state(server).await
    }

    async fn register_failing_guard(state: &AppState) {
        let mut rules = state.rules.write().await;
        rules.register_guard(Guard {
            id: GuardId::from_str("FAIL-SCAN-GUARD"),
            script_path: PathBuf::from("fail\0scan.sh"),
            language: Language::Common,
            rules: vec![],
        });
    }

    #[tokio::test]
    async fn metrics_collect_returns_internal_error_when_scan_fails() -> anyhow::Result<()> {
        let project_root = tempdir_in_home("metrics-scan-fail-root-")?;
        let data_dir = tempfile::tempdir()?;
        let state = make_test_state(project_root.path(), data_dir.path()).await?;
        register_failing_guard(&state).await;

        let response = metrics_collect(
            &state,
            Some(serde_json::json!(1)),
            project_root.path().to_path_buf(),
        )
        .await;

        let error = response
            .error
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("expected metrics/collect to return an error"))?;
        assert_eq!(error.code, INTERNAL_ERROR);
        assert!(
            !error.message.is_empty(),
            "scan failure should provide a non-empty error message"
        );
        assert!(
            response.result.is_none(),
            "scan failure must not return a success payload"
        );

        let events = state.events.query(&EventFilters::default())?;
        assert!(
            events.iter().all(|event| event.hook != "rule_scan"),
            "scan failure should not persist a rule_scan event"
        );
        Ok(())
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
                .rev()
                .find(|e| e.hook == "rule_scan")
                .map(|scan| {
                    evts.iter()
                        .filter(|e| e.hook == "rule_check" && e.session_id == scan.session_id)
                        .count()
                })
                .unwrap_or(0);
            let report = harness_observe::QualityGrader::grade(&evts, violation_count);
            match serde_json::to_value(&report) {
                Ok(v) => RpcResponse::success(id, v),
                Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
            }
        }
        Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
    }
}
