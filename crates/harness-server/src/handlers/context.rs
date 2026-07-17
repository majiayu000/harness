use crate::http::AppState;
use harness_context::{
    providers::{
        ContractProvider, ErrorProvider, ExecPlanProvider, GcDraftsProvider, RulesProvider,
        SkillsProvider, TaskBriefProvider,
    },
    ComposeConfig, ComposeManifest, ComposeMode, ComposeRequest, Composition, ContextComposer,
    ContextItem,
};
use harness_core::run_id::RunIdentity;
use harness_core::types::{EventFilters, ThreadId};
use harness_protocol::methods::{RpcResponse, INTERNAL_ERROR, NOT_FOUND};

const CONTEXT_MANIFEST_HOOK: &str = "context_manifest";

pub async fn context_preview(
    state: &AppState,
    id: Option<serde_json::Value>,
    mut request: ComposeRequest,
    supplied_items: Vec<ContextItem>,
) -> RpcResponse {
    request.run_id = request.run_id.or_else(current_run_id);
    let composer = build_composer(state, ComposeMode::Preview).await;
    match composer.compose_supplied(&request, supplied_items) {
        Ok(composition) => RpcResponse::success(id, composition_response(composition)),
        Err(error) => RpcResponse::error(id, INTERNAL_ERROR, error.to_string()),
    }
}

pub async fn context_manifest_get(
    state: &AppState,
    id: Option<serde_json::Value>,
    thread_id: ThreadId,
) -> RpcResponse {
    let filters = EventFilters {
        hook: Some(CONTEXT_MANIFEST_HOOK.to_string()),
        tool: Some(thread_id.to_string()),
        ..Default::default()
    };
    match state.observability.events.query(&filters).await {
        Ok(events) => {
            let Some(event) = events.into_iter().last() else {
                return RpcResponse::error(id, NOT_FOUND, "context manifest not found");
            };
            let Some(detail) = event.detail else {
                return RpcResponse::error(
                    id,
                    INTERNAL_ERROR,
                    "context manifest event missing detail",
                );
            };
            match serde_json::from_str::<ComposeManifest>(&detail) {
                Ok(manifest) => {
                    RpcResponse::success(id, serde_json::json!({ "manifest": manifest }))
                }
                Err(error) => RpcResponse::error(
                    id,
                    INTERNAL_ERROR,
                    format!("failed to decode context manifest: {error}"),
                ),
            }
        }
        Err(error) => RpcResponse::error(id, INTERNAL_ERROR, error.to_string()),
    }
}

async fn build_composer(state: &AppState, mode: ComposeMode) -> ContextComposer {
    let mut config = ComposeConfig::from(&state.core.server.config.context);
    config.mode = mode;

    let rules = {
        let rules = state.engines.rules.read().await;
        rules.rules().to_vec()
    };
    let skills = {
        let skills = state.engines.skills.read().await;
        skills.list().to_vec()
    };
    let plans = state
        .core
        .plan_cache
        .iter()
        .map(|entry| entry.value().clone())
        .collect::<Vec<_>>();
    let composer = ContextComposer::new(config)
        .with_provider(Box::new(ContractProvider))
        .with_provider(Box::new(ExecPlanProvider::new(plans)))
        .with_provider(Box::new(RulesProvider::new(rules)))
        .with_provider(Box::new(SkillsProvider::new(skills)))
        .with_provider(Box::new(TaskBriefProvider));

    match state.engines.gc_agent.drafts() {
        Ok(drafts) => composer.with_provider(Box::new(GcDraftsProvider::new(drafts))),
        Err(error) => {
            tracing::error!(error = %error, "context gc-drafts provider snapshot failed");
            composer.with_provider(Box::new(ErrorProvider::new(
                "gc-drafts",
                format!("snapshot failed: {error}"),
            )))
        }
    }
}

fn composition_response(composition: Composition) -> serde_json::Value {
    serde_json::json!({
        "rendered": composition.rendered,
        "manifest": composition.manifest,
    })
}

fn current_run_id() -> Option<harness_core::run_id::RunId> {
    RunIdentity::from_env()
        .ok()
        .flatten()
        .map(|identity| identity.run_id)
}
