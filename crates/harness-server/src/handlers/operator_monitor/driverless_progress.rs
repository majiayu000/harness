use crate::http::AppState;
use harness_workflow::runtime::DriverlessProgressInstance;
use serde::Serialize;

use super::WORKFLOW_SAMPLE_LIMIT;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(super) struct DriverlessProgressEvidence {
    classification: &'static str,
    workflow_id: String,
    definition_id: String,
    state: String,
    age_secs: u64,
    provenance_status: &'static str,
}

impl From<DriverlessProgressInstance> for DriverlessProgressEvidence {
    fn from(instance: DriverlessProgressInstance) -> Self {
        Self {
            classification: "driverless_progress",
            workflow_id: instance.workflow_id,
            definition_id: instance.definition_id,
            state: instance.state,
            age_secs: instance.age_secs,
            provenance_status: instance.provenance_status.as_str(),
        }
    }
}

pub(super) async fn list_driverless_progress(
    state: &AppState,
) -> anyhow::Result<Vec<DriverlessProgressEvidence>> {
    let Some(store) = state.core.workflow_runtime_store.as_ref() else {
        return Ok(Vec::new());
    };
    let workflow_cfg =
        harness_core::config::workflow::load_workflow_config(&state.core.project_root)?;
    let limit = i64::from(workflow_cfg.storage.workflow_watchdog_batch_size)
        .clamp(1, WORKFLOW_SAMPLE_LIMIT);
    Ok(store
        .list_driverless_progress_instances(limit)
        .await?
        .into_iter()
        .map(DriverlessProgressEvidence::from)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers;
    use axum::{body::to_bytes, routing::get, Router};
    use harness_workflow::runtime::{
        DriverlessProgressProvenanceStatus, WorkflowInstance, WorkflowRuntimeStore,
        WorkflowSubject, GITHUB_ISSUE_PR_DEFINITION_ID,
    };
    use serde_json::Value;
    use std::sync::Arc;

    #[test]
    fn projection_exposes_stable_classification_and_provenance() {
        let projection = DriverlessProgressEvidence::from(DriverlessProgressInstance {
            workflow_id: "workflow-1".to_string(),
            definition_id: "github_issue_pr".to_string(),
            state: "implementing".to_string(),
            age_secs: 42,
            provenance_status: DriverlessProgressProvenanceStatus::MissingStateEntryProvenance,
        });
        let value = serde_json::to_value(projection).expect("projection should serialize");

        assert_eq!(value["classification"], "driverless_progress");
        assert_eq!(value["workflow_id"], "workflow-1");
        assert_eq!(value["definition_id"], "github_issue_pr");
        assert_eq!(value["state"], "implementing");
        assert_eq!(value["age_secs"], 42);
        assert_eq!(value["provenance_status"], "missing_state_entry_provenance");
    }

    #[tokio::test]
    async fn operator_monitor_exposes_bounded_driverless_progress_when_watchdog_is_disabled(
    ) -> anyhow::Result<()> {
        let _lock = test_helpers::HOME_LOCK.lock().await;
        let dir = test_helpers::tempdir_in_home("harness-test-driverless-progress-monitor-")?;
        std::fs::write(
            dir.path().join("WORKFLOW.md"),
            "---\nstorage:\n  workflow_watchdog_enabled: false\n  workflow_watchdog_batch_size: 1\n---\nBody\n",
        )?;
        let mut state = test_helpers::make_test_state(dir.path()).await?;
        let store = Arc::new(
            WorkflowRuntimeStore::open_with_database_url(
                &harness_core::config::dirs::default_db_path(dir.path(), "workflow_runtime"),
                Some(&test_helpers::test_database_url()?),
            )
            .await?,
        );
        for id in ["driverless-a", "driverless-b"] {
            store
                .upsert_instance(
                    &WorkflowInstance::new(
                        GITHUB_ISSUE_PR_DEFINITION_ID,
                        1,
                        "implementing",
                        WorkflowSubject::new("issue", format!("issue:{id}")),
                    )
                    .with_id(id.to_string()),
                )
                .await?;
        }
        state.core.workflow_runtime_store = Some(store);

        let app = Router::new()
            .route("/api/operator-monitor", get(super::super::operator_monitor))
            .with_state(Arc::new(state));
        let request = axum::http::Request::builder()
            .uri("/api/operator-monitor")
            .body(axum::body::Body::empty())?;
        let response = tower::ServiceExt::oneshot(app, request).await?;
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await?)?;
        let rows = body["driverless_progress"]
            .as_array()
            .expect("driverless progress should be an array");

        assert_eq!(rows.len(), 1, "configured monitoring bound should apply");
        assert_eq!(rows[0]["classification"], "driverless_progress");
        assert_eq!(rows[0]["definition_id"], "github_issue_pr");
        assert_eq!(rows[0]["state"], "implementing");
        assert_eq!(
            rows[0]["provenance_status"],
            "missing_state_entry_provenance"
        );
        assert!(body["stuck_workflows"]
            .as_array()
            .is_some_and(Vec::is_empty));
        Ok(())
    }
}
