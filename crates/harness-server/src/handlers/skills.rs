use crate::http::AppState;
use harness_core::SkillId;
use harness_protocol::{RpcResponse, INTERNAL_ERROR, NOT_FOUND};

pub async fn skill_create(
    state: &AppState,
    id: Option<serde_json::Value>,
    name: String,
    content: String,
) -> RpcResponse {
    // Reject names that could traverse outside the skills directory when used as a filename.
    if name.contains('/') || name.contains('\\') || name.contains("..") || name.is_empty() {
        return RpcResponse::error(
            id,
            INTERNAL_ERROR,
            "skill name must not contain path separators or '..'",
        );
    }
    let mut skills = state.engines.skills.write().await;
    let skill = skills.create(name, content).clone();
    match serde_json::to_value(&skill) {
        Ok(v) => RpcResponse::success(id, v),
        Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
    }
}

pub async fn skill_list(
    state: &AppState,
    id: Option<serde_json::Value>,
    query: Option<String>,
) -> RpcResponse {
    let skills = state.engines.skills.read().await;
    let result = match query {
        Some(q) => skills.search(&q).into_iter().cloned().collect::<Vec<_>>(),
        None => skills.list().to_vec(),
    };
    match serde_json::to_value(&result) {
        Ok(v) => RpcResponse::success(id, v),
        Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
    }
}

pub async fn skill_get(
    state: &AppState,
    id: Option<serde_json::Value>,
    skill_id: SkillId,
) -> RpcResponse {
    let skills = state.engines.skills.read().await;
    match skills.get(&skill_id) {
        Some(skill) => match serde_json::to_value(skill) {
            Ok(v) => RpcResponse::success(id, v),
            Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
        },
        None => RpcResponse::error(id, NOT_FOUND, "skill not found"),
    }
}

pub async fn skill_delete(
    state: &AppState,
    id: Option<serde_json::Value>,
    skill_id: SkillId,
) -> RpcResponse {
    let mut skills = state.engines.skills.write().await;
    let deleted = skills.delete(&skill_id);
    RpcResponse::success(id, serde_json::json!({ "deleted": deleted }))
}

pub async fn skill_stats(state: &AppState, id: Option<serde_json::Value>) -> RpcResponse {
    let skills = state.engines.skills.read().await;
    let stats: Vec<serde_json::Value> = skills
        .list()
        .iter()
        .map(|s| {
            serde_json::json!({
                "name": s.name,
                "usage_count": s.usage_count,
                "last_used": s.last_used,
            })
        })
        .collect();
    RpcResponse::success(id, serde_json::json!(stats))
}
