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
use harness_core::types::{Decision, Event, EventFilters, ProjectId, SessionId, ThreadId};
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

pub(crate) async fn record_context_composition(
    state: &AppState,
    thread_id: &ThreadId,
    project: ProjectId,
    task_profile: harness_context::TaskProfile,
) -> Result<(), String> {
    let mode = state.core.server.config.context.mode.into();
    let request = ComposeRequest {
        thread_id: thread_id.clone(),
        run_id: current_run_id(),
        project,
        task_profile,
        budget_hint: state.core.server.config.context.budget_tokens,
    };
    let composer = build_composer(state, mode).await;
    let outcome = composer.compose(&request);
    let (manifest, decision, reason, fatal_error) = match outcome {
        Ok(composition) => {
            let mut manifest = composition.manifest;
            if mode == ComposeMode::Enforce {
                let message = "context enforce mode is not available until composed injection replaces legacy injection paths";
                manifest
                    .warnings
                    .push("context_enforce_not_wired".to_string());
                tracing::error!(thread_id = %thread_id, "context enforce mode requested before injection wiring is available");
                (
                    manifest,
                    Decision::Block,
                    Some("context_enforce_not_wired".to_string()),
                    Some(message.to_string()),
                )
            } else {
                let decision =
                    if manifest.provider_errors.is_empty() && manifest.warnings.is_empty() {
                        Decision::Pass
                    } else {
                        Decision::Warn
                    };
                (manifest, decision, None, None)
            }
        }
        Err(error) => {
            tracing::error!(thread_id = %thread_id, error = %error, "context composition failed");
            let fatal_error = if mode == ComposeMode::Enforce {
                Some(error.to_string())
            } else {
                None
            };
            (
                error.manifest().clone(),
                Decision::Block,
                Some("compose_error".to_string()),
                fatal_error,
            )
        }
    };

    if let Err(error) = log_manifest_event(state, thread_id, manifest, decision, reason).await {
        tracing::warn!(thread_id = %thread_id, error = %error, "context manifest logging failed");
    }
    if let Some(error) = fatal_error {
        return Err(error);
    }
    Ok(())
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

async fn log_manifest_event(
    state: &AppState,
    thread_id: &ThreadId,
    manifest: ComposeManifest,
    decision: Decision,
    reason: Option<String>,
) -> anyhow::Result<()> {
    let detail = serde_json::to_string(&manifest)?;
    let mut event = Event::new(
        SessionId::new(),
        CONTEXT_MANIFEST_HOOK,
        &thread_id.to_string(),
        decision,
    );
    event.run_id = manifest.run_id.clone();
    event.reason = reason;
    event.detail = Some(detail);
    state.observability.events.log(&event).await?;
    Ok(())
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
