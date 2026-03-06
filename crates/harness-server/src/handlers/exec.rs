use crate::http::AppState;
use harness_core::ExecPlanId;
use harness_protocol::{RpcResponse, INTERNAL_ERROR};
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
            if let Err(e) = state.exec_plan_db.upsert(&plan).await {
                return RpcResponse::error(id, INTERNAL_ERROR, e.to_string());
            }
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
    match state.exec_plan_db.get(&plan_id).await {
        Ok(Some(plan)) => match serde_json::to_value(&plan) {
            Ok(v) => RpcResponse::success(id, v),
            Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
        },
        Ok(None) => RpcResponse::error(id, INTERNAL_ERROR, "plan not found"),
        Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
    }
}

pub async fn exec_plan_update(
    state: &AppState,
    id: Option<serde_json::Value>,
    plan_id: ExecPlanId,
    updates: serde_json::Value,
) -> RpcResponse {
    let mut plan = match state.exec_plan_db.get(&plan_id).await {
        Ok(Some(plan)) => plan,
        Ok(None) => return RpcResponse::error(id, INTERNAL_ERROR, "plan not found"),
        Err(e) => return RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
    };

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
        _ => return RpcResponse::error(id, INTERNAL_ERROR, format!("unknown action: {action}")),
    }

    if let Err(e) = state.exec_plan_db.upsert(&plan).await {
        return RpcResponse::error(id, INTERNAL_ERROR, e.to_string());
    }

    match serde_json::to_value(&plan) {
        Ok(v) => RpcResponse::success(id, v),
        Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
    }
}
