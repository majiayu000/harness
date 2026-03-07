use crate::http::AppState;
use harness_core::ExecPlanId;
use harness_protocol::{RpcResponse, INTERNAL_ERROR, NOT_FOUND};
use std::path::PathBuf;

pub async fn exec_plan_init(
    state: &AppState,
    id: Option<serde_json::Value>,
    spec: String,
    project_root: PathBuf,
) -> RpcResponse {
    let project_root = match crate::handlers::validate_project_root(&project_root) {
        Ok(p) => p,
        Err(e) => return RpcResponse::error(id, INTERNAL_ERROR, e),
    };
    match harness_exec::ExecPlan::from_spec(&spec, &project_root) {
        Ok(plan) => {
            let plan_id = plan.id.clone();
            if let Some(db) = &state.plan_db {
                if let Err(e) = db.upsert(&plan).await {
                    tracing::warn!("failed to persist plan: {e}");
                }
            }
            let mut plans = state.plans.write().await;
            plans.insert(plan_id.clone(), plan);
            RpcResponse::success(id, serde_json::json!({ "plan_id": plan_id }))
        }
        Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
    }
}

pub async fn exec_plan_status(
    state: &AppState,
    id: Option<serde_json::Value>,
    plan_id: ExecPlanId,
) -> RpcResponse {
    let plans = state.plans.read().await;
    match plans.get(&plan_id) {
        Some(plan) => match serde_json::to_value(plan) {
            Ok(v) => RpcResponse::success(id, v),
            Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
        },
        None => RpcResponse::error(id, NOT_FOUND, "plan not found"),
    }
}

pub async fn exec_plan_update(
    state: &AppState,
    id: Option<serde_json::Value>,
    plan_id: ExecPlanId,
    updates: serde_json::Value,
) -> RpcResponse {
    let mut plans = state.plans.write().await;
    match plans.get_mut(&plan_id) {
        Some(plan) => {
            let action = updates.get("action").and_then(|a| a.as_str()).unwrap_or("");
            match action {
                "activate" => plan.activate(),
                "complete" => plan.complete(),
                "abandon" => plan.abandon(),
                "add_milestone" => {
                    if let Some(desc) = updates.get("description").and_then(|d| d.as_str()) {
                        plan.add_milestone(desc.to_string());
                    }
                }
                "log_decision" => {
                    let decision = updates
                        .get("decision")
                        .and_then(|d| d.as_str())
                        .unwrap_or("");
                    let rationale = updates
                        .get("rationale")
                        .and_then(|r| r.as_str())
                        .unwrap_or("");
                    plan.log_decision(decision, rationale);
                }
                _ => {
                    return RpcResponse::error(
                        id,
                        INTERNAL_ERROR,
                        format!("unknown action: {action}"),
                    )
                }
            }
            if let Some(db) = &state.plan_db {
                if let Err(e) = db.upsert(plan).await {
                    tracing::warn!("failed to persist plan update: {e}");
                }
            }
            match serde_json::to_value(&*plan) {
                Ok(v) => RpcResponse::success(id, v),
                Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
            }
        }
        None => RpcResponse::error(id, NOT_FOUND, "plan not found"),
    }
}
