use crate::{http::AppState, validate_root};
use harness_core::{EventFilters, Violation};
use harness_observe::{health::generate_health_report, stats};
use harness_protocol::{RpcResponse, INTERNAL_ERROR};
use std::path::PathBuf;

pub async fn health_check(
    state: &AppState,
    id: Option<serde_json::Value>,
    project_root: PathBuf,
) -> RpcResponse {
    let project_root = validate_root!(&project_root, id);

    // Query historical events before persisting the current scan to avoid the
    // just-persisted rule_check events inflating the quality stability score.
    let events = match state
        .observability
        .events
        .query(&EventFilters::default())
        .await
    {
        Ok(evts) => evts,
        Err(e) => return RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
    };

    let violations: Vec<Violation> = {
        let rules = state.engines.rules.read().await;
        match rules.scan(&project_root).await {
            Ok(violations) => violations,
            Err(err) => {
                tracing::error!(
                    project_root = %project_root.display(),
                    error = %err,
                    "health/check: rules scan failed"
                );
                return RpcResponse::error(id, INTERNAL_ERROR, err.to_string());
            }
        }
    };
    state
        .observability
        .events
        .persist_rule_scan(&project_root, &violations)
        .await;

    let report = generate_health_report(&events, &violations);
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
    use harness_core::{GuardId, HarnessConfig, Language};
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
        let mut rules = state.engines.rules.write().await;
        rules.register_guard(Guard {
            id: GuardId::from_str("FAIL-SCAN-GUARD"),
            script_path: PathBuf::from("fail\0scan.sh"),
            language: Language::Common,
            rules: vec![],
        });
    }

    #[tokio::test]
    async fn health_check_returns_internal_error_when_scan_fails() -> anyhow::Result<()> {
        let project_root = tempdir_in_home("health-scan-fail-root-")?;
        let data_dir = tempfile::tempdir()?;
        let state = make_test_state(project_root.path(), data_dir.path()).await?;
        register_failing_guard(&state).await;

        let response = health_check(
            &state,
            Some(serde_json::json!(1)),
            project_root.path().to_path_buf(),
        )
        .await;

        let error = response
            .error
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("expected health/check to return an error"))?;
        assert_eq!(error.code, INTERNAL_ERROR);
        assert!(
            !error.message.is_empty(),
            "scan failure should provide a non-empty error message"
        );
        assert!(
            response.result.is_none(),
            "scan failure must not return a success payload"
        );

        let events = state
            .observability
            .events
            .query(&EventFilters::default())
            .await?;
        assert!(
            events.iter().all(|event| event.hook != "rule_scan"),
            "scan failure should not persist a rule_scan event"
        );
        Ok(())
    }

    #[tokio::test]
    async fn health_check_emits_rule_scan_anchor_event() -> anyhow::Result<()> {
        // Regression test for issue #82: the handler path must write a
        // `rule_scan` anchor event so that session-scoped violation counting
        // in metrics_query works correctly.
        let project_root = tempdir_in_home("health-scan-anchor-root-")?;
        let data_dir = tempfile::tempdir()?;
        let state = make_test_state(project_root.path(), data_dir.path()).await?;

        let response = health_check(
            &state,
            Some(serde_json::json!(1)),
            project_root.path().to_path_buf(),
        )
        .await;

        assert!(
            response.error.is_none(),
            "health_check should succeed: {:?}",
            response.error
        );

        let events = state
            .observability
            .events
            .query(&harness_core::EventFilters {
                hook: Some("rule_scan".to_string()),
                ..Default::default()
            })
            .await?;
        assert_eq!(
            events.len(),
            1,
            "health_check must emit exactly one rule_scan anchor event"
        );
        assert_eq!(
            events[0].hook, "rule_scan",
            "anchor event hook must be 'rule_scan'"
        );
        Ok(())
    }
}

pub async fn stats_query(
    state: &AppState,
    id: Option<serde_json::Value>,
    since: Option<chrono::DateTime<chrono::Utc>>,
    until: Option<chrono::DateTime<chrono::Utc>>,
) -> RpcResponse {
    let filters = EventFilters {
        since,
        until,
        ..Default::default()
    };
    let events = match state.observability.events.query(&filters).await {
        Ok(evts) => evts,
        Err(e) => return RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
    };

    let hook_stats = stats::aggregate_hook_stats(&events);
    let trends = stats::compute_trends(&events, 7);
    let rule_stats = stats::aggregate_rule_stats(&events);
    let rule_trends = stats::compute_rule_trends(&events, 7);

    match serde_json::to_value(serde_json::json!({
        "hook_stats": hook_stats,
        "trends": trends,
        "rule_stats": rule_stats,
        "rule_trends": rule_trends,
    })) {
        Ok(v) => RpcResponse::success(id, v),
        Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
    }
}
