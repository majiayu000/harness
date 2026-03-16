use crate::{http::AppState, validate_root};
use harness_core::ExecPlanId;
use harness_protocol::{RpcResponse, INTERNAL_ERROR, NOT_FOUND};
use std::path::PathBuf;

pub async fn exec_plan_init(
    state: &AppState,
    id: Option<serde_json::Value>,
    spec: String,
    project_root: PathBuf,
) -> RpcResponse {
    let project_root = validate_root!(&project_root, id);
    match harness_exec::ExecPlan::from_spec(&spec, &project_root) {
        Ok(plan) => {
            let plan_id = plan.id.clone();
            let db = match &state.core.plan_db {
                Some(db) => db,
                None => return RpcResponse::error(id, INTERNAL_ERROR, "plan store unavailable"),
            };
            if let Err(e) = db.upsert(&plan).await {
                return RpcResponse::error(
                    id,
                    INTERNAL_ERROR,
                    format!("failed to persist plan: {e}"),
                );
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
    if let Some(db) = &state.core.plan_db {
        match db.get(&plan_id).await {
            Ok(Some(plan)) => match serde_json::to_value(&plan) {
                Ok(v) => RpcResponse::success(id, v),
                Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
            },
            Ok(None) => RpcResponse::error(id, NOT_FOUND, "plan not found"),
            Err(e) => RpcResponse::error(id, INTERNAL_ERROR, format!("db error: {e}")),
        }
    } else {
        RpcResponse::error(id, NOT_FOUND, "plan not found")
    }
}

pub async fn exec_plan_update(
    state: &AppState,
    id: Option<serde_json::Value>,
    plan_id: ExecPlanId,
    updates: serde_json::Value,
) -> RpcResponse {
    let db = match &state.core.plan_db {
        Some(db) => db,
        None => return RpcResponse::error(id, INTERNAL_ERROR, "plan store unavailable"),
    };

    let action = updates
        .get("action")
        .and_then(|a| a.as_str())
        .unwrap_or("")
        .to_string();

    // Validate the action before entering the lock so we can return early cheaply.
    match action.as_str() {
        "activate" | "complete" | "abandon" | "add_milestone" | "log_decision" => {}
        _ => return RpcResponse::error(id, INTERNAL_ERROR, format!("unknown action: {action}")),
    }

    let result = db
        .update_in_txn(&plan_id, |plan| match action.as_str() {
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
            _ => {}
        })
        .await;

    match result {
        Ok(Some(plan)) => match serde_json::to_value(&plan) {
            Ok(v) => RpcResponse::success(id, v),
            Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
        },
        Ok(None) => RpcResponse::error(id, NOT_FOUND, "plan not found"),
        Err(e) => RpcResponse::error(id, INTERNAL_ERROR, format!("db error: {e}")),
    }
}
