use crate::{http::AppState, validate_root};
use harness_protocol::{RpcResponse, INTERNAL_ERROR};
use std::path::PathBuf;

pub async fn rule_load(
    state: &AppState,
    id: Option<serde_json::Value>,
    project_root: PathBuf,
) -> RpcResponse {
    let project_root = validate_root!(&project_root, id);
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
    let project_root = validate_root!(&project_root, id);
    let file_count = files.as_ref().map_or(0, |paths| paths.len());
    let result = {
        let rules = state.rules.read().await;
        if let Err(err) = rules.validate_scan_request(files.as_deref()) {
            tracing::warn!(
                project_root = %project_root.display(),
                guard_count = rules.guards().len(),
                file_count,
                error = %err,
                "rule/check rejected before scan"
            );
            return RpcResponse::error(id, INTERNAL_ERROR, err.to_string());
        }
        match files {
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
        }
    };
    match result {
        Ok(violations) => {
            let guard_count = {
                let rules = state.rules.read().await;
                rules.guards().len()
            };
            tracing::info!(
                project_root = %project_root.display(),
                guard_count,
                file_count,
                violation_count = violations.len(),
                "rule/check scan completed"
            );
            state.events.persist_rule_scan(&project_root, &violations);
            match serde_json::to_value(&violations) {
                Ok(v) => RpcResponse::success(id, v),
                Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
            }
        }
        Err(e) => {
            tracing::warn!(
                project_root = %project_root.display(),
                file_count,
                error = %e,
                "rule/check scan failed"
            );
            RpcResponse::error(id, INTERNAL_ERROR, e.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::rule_check;
    use crate::{http::AppState, server::HarnessServer, thread_manager::ThreadManager};
    use harness_agents::AgentRegistry;
    use harness_core::{EventFilters, GuardId, Language};
    use harness_protocol::INTERNAL_ERROR;
    use harness_rules::engine::{Guard, WARN_EMPTY_SCAN_INPUT, WARN_NO_GUARDS_REGISTERED};
    use std::path::PathBuf;
    use std::sync::{
        atomic::{AtomicBool, AtomicU64},
        Arc,
    };
    use tokio::sync::{broadcast, RwLock};

    async fn make_test_state(dir: &std::path::Path) -> anyhow::Result<AppState> {
        let server = Arc::new(HarnessServer::new(
            harness_core::HarnessConfig::default(),
            ThreadManager::new(),
            AgentRegistry::new("test"),
        ));
        let tasks = crate::task_runner::TaskStore::open(&dir.join("tasks.db")).await?;
        let events = Arc::new(harness_observe::EventStore::new(dir)?);
        let signal_detector = harness_gc::SignalDetector::new(
            server.config.gc.signal_thresholds.clone().into(),
            harness_core::ProjectId::new(),
        );
        let draft_store = harness_gc::DraftStore::new(dir)?;
        let gc_agent = Arc::new(harness_gc::GcAgent::new(
            harness_gc::gc_agent::GcConfig::default(),
            signal_detector,
            draft_store,
        ));
        let thread_db = crate::thread_db::ThreadDb::open(&dir.join("threads.db")).await?;
        let (notification_tx, _) = broadcast::channel(64);
        Ok(AppState {
            server,
            project_root: dir.to_path_buf(),
            tasks,
            skills: Arc::new(RwLock::new(harness_skills::SkillStore::new())),
            rules: Arc::new(RwLock::new(harness_rules::engine::RuleEngine::new())),
            events,
            gc_agent,
            plans: Arc::new(RwLock::new(std::collections::HashMap::new())),
            thread_db: Some(thread_db),
            plan_db: None,
            interceptors: vec![],
            notification_tx,
            notification_lagged_total: Arc::new(AtomicU64::new(0)),
            notification_lag_log_every: 1,
            notify_tx: None,
            initialized: Arc::new(AtomicBool::new(true)),
            workspace_mgr: None,
            feishu_intake: None,
            task_queue: Arc::new(crate::task_queue::TaskQueue::new(&Default::default())),
        })
    }

    use crate::test_helpers::tempdir_in_home;

    #[tokio::test]
    async fn rule_check_returns_warning_when_no_guards_registered() -> anyhow::Result<()> {
        let dir = tempdir_in_home("rule-check-no-guard-")?;
        let state = make_test_state(dir.path()).await?;
        let project_root = dir.path().to_path_buf();

        let response = rule_check(&state, Some(serde_json::json!(1)), project_root, None).await;

        let error = response
            .error
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("expected rule/check to fail without guards"))?;
        assert_eq!(error.code, INTERNAL_ERROR);
        assert!(
            error.message.contains(WARN_NO_GUARDS_REGISTERED),
            "expected warning message in error: {}",
            error.message
        );
        assert!(
            response.result.is_none(),
            "warning path must not return result"
        );

        let events = state.events.query(&EventFilters {
            hook: Some("rule_scan".to_string()),
            ..Default::default()
        })?;
        assert!(
            events.is_empty(),
            "warning path should not persist rule_scan events"
        );
        Ok(())
    }

    #[tokio::test]
    async fn rule_check_returns_warning_for_empty_scan_input() -> anyhow::Result<()> {
        let dir = tempdir_in_home("rule-check-empty-input-")?;
        let state = make_test_state(dir.path()).await?;
        {
            let mut rules = state.rules.write().await;
            rules.register_guard(Guard {
                id: GuardId::from_str("TEST-GUARD"),
                script_path: PathBuf::from("unused-guard.sh"),
                language: Language::Common,
                rules: vec![],
            });
        }

        let response = rule_check(
            &state,
            Some(serde_json::json!(1)),
            dir.path().to_path_buf(),
            Some(Vec::new()),
        )
        .await;

        let error = response
            .error
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("expected rule/check to fail for empty scan input"))?;
        assert_eq!(error.code, INTERNAL_ERROR);
        assert!(
            error.message.contains(WARN_EMPTY_SCAN_INPUT),
            "expected warning message in error: {}",
            error.message
        );
        assert!(
            response.result.is_none(),
            "warning path must not return result"
        );
        Ok(())
    }
}
