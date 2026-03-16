use crate::{http::AppState, validate_root};
use harness_protocol::{RpcResponse, INTERNAL_ERROR};
use std::path::PathBuf;

pub async fn event_log(
    state: &AppState,
    id: Option<serde_json::Value>,
    event: harness_core::Event,
) -> RpcResponse {
    match state.observability.events.log(&event).await {
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
    match state.observability.events.query(&filters).await {
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
    let project_root = validate_root!(&project_root, id);

    // scan -> persist -> query -> grade
    let violations = {
        let rules = state.engines.rules.read().await;
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
    state
        .observability
        .events
        .persist_rule_scan(&project_root, &violations)
        .await;

    let evts = match state
        .observability
        .events
        .query(&harness_core::EventFilters::default())
        .await
    {
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
        let mut rules = state.engines.rules.write().await;
        rules.register_guard(Guard {
            id: GuardId::from_str("FAIL-SCAN-GUARD"),
            script_path: PathBuf::from("fail\0scan.sh"),
            language: Language::Common,
            rules: vec![],
        });
    }

    #[tokio::test]
    async fn event_log_returns_logged_true_and_event_id() -> anyhow::Result<()> {
        let project_root = tempdir_in_home("observe-event-log-root-")?;
        let data_dir = tempfile::tempdir()?;
        let state = make_test_state(project_root.path(), data_dir.path()).await?;

        let session_id = harness_core::SessionId::new();
        let event = harness_core::Event::new(
            session_id.clone(),
            "pre_tool_use",
            "Bash",
            harness_core::Decision::Pass,
        );

        let response = event_log(&state, Some(serde_json::json!(1)), event.clone()).await;

        assert!(
            response.error.is_none(),
            "event_log should succeed: {:?}",
            response.error
        );
        let result = response
            .result
            .ok_or_else(|| anyhow::anyhow!("missing result"))?;
        assert_eq!(result["logged"], serde_json::json!(true));
        let returned_id = result["event_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing event_id"))?;
        assert_eq!(returned_id, event.id.as_str());
        Ok(())
    }

    #[tokio::test]
    async fn event_query_returns_previously_logged_events() -> anyhow::Result<()> {
        let project_root = tempdir_in_home("observe-event-query-root-")?;
        let data_dir = tempfile::tempdir()?;
        let state = make_test_state(project_root.path(), data_dir.path()).await?;

        let session_id = harness_core::SessionId::new();
        let event = harness_core::Event::new(
            session_id.clone(),
            "post_tool_use",
            "Read",
            harness_core::Decision::Pass,
        );
        state.observability.events.log(&event).await?;

        let filters = harness_core::EventFilters {
            session_id: Some(session_id),
            ..Default::default()
        };
        let response = event_query(&state, Some(serde_json::json!(2)), filters).await;

        assert!(
            response.error.is_none(),
            "event_query should succeed: {:?}",
            response.error
        );
        let events_val = response
            .result
            .ok_or_else(|| anyhow::anyhow!("missing result"))?;
        let events_arr = events_val
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("expected JSON array"))?;
        assert_eq!(events_arr.len(), 1);
        assert_eq!(
            events_arr[0]["id"],
            serde_json::json!(event.id.as_str()),
            "returned event id should match logged event"
        );
        Ok(())
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
    async fn metrics_collect_emits_rule_scan_anchor_event() -> anyhow::Result<()> {
        // Regression test for issue #82: the handler path must write a
        // `rule_scan` anchor event so that session-scoped violation counting
        // in metrics_query works correctly.
        let project_root = tempdir_in_home("metrics-scan-anchor-root-")?;
        let data_dir = tempfile::tempdir()?;
        let state = make_test_state(project_root.path(), data_dir.path()).await?;

        let response = metrics_collect(
            &state,
            Some(serde_json::json!(1)),
            project_root.path().to_path_buf(),
        )
        .await;

        assert!(
            response.error.is_none(),
            "metrics_collect should succeed: {:?}",
            response.error
        );

        let events = state
            .observability
            .events
            .query(&EventFilters {
                hook: Some("rule_scan".to_string()),
                ..Default::default()
            })
            .await?;
        assert_eq!(
            events.len(),
            1,
            "metrics_collect must emit exactly one rule_scan anchor event"
        );
        assert_eq!(
            events[0].hook, "rule_scan",
            "anchor event hook must be 'rule_scan'"
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
    let events = state.observability.events.query(&event_filters).await;
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
