use super::data_helpers::{activity_name, prompt_task_request_for_job, PromptTaskRequest};
use super::prompt_packet::{
    build_runtime_job_prompt, build_runtime_prompt_packet, prompt_packet_digest,
    PromptPacketConfigurationError,
};
use crate::http::AppState;
use harness_workflow::runtime::{RuntimeJob, QUALITY_GATE_ACTIVITY};
use serde_json::{json, Value};
use std::path::Path;

pub(crate) struct PreparedRemotePrompt {
    pub response: Value,
    pub evidence: Value,
}

/// Prepare instructions only: remote workspaces are never accessed by the server.
pub(crate) async fn prepare(
    state: &AppState,
    job: &RuntimeJob,
    workspace: &str,
) -> anyhow::Result<PreparedRemotePrompt> {
    if job.input.pointer("/command/agent_contract").is_some()
        || job.input.pointer("/command/exact_replay").is_some()
        || matches!(activity_name(job).as_str(), QUALITY_GATE_ACTIVITY)
    {
        return Err(PromptPacketConfigurationError::new(
            "this activity requires raw claim execution; omit execution_workspace",
        )
        .into());
    }
    if matches!(
        activity_name(job).as_str(),
        "start_child_workflow" | "inspect_pr_feedback"
    ) {
        return Err(PromptPacketConfigurationError::new(
            "server-owned activity cannot be rendered for a remote agent",
        )
        .into());
    }
    let store = state
        .core
        .workflow_runtime_store
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("workflow runtime store unavailable"))?;
    let workflow = super::job_context::workflow_for_job(state, job)
        .await?
        .ok_or_else(|| anyhow::anyhow!("runtime workflow is missing"))?;
    if activity_name(job) == "merge_pr"
        && super::server_merge::server_merge_execution_enabled(state, job, Some(&workflow))
    {
        return Err(PromptPacketConfigurationError::new(
            "server-owned merge execution cannot be rendered for a remote agent",
        )
        .into());
    }
    let source_root = super::job_context::project_root_for_job(state, job, Some(&workflow))?;
    let document = harness_core::config::workflow::load_workflow_document(&source_root)
        .map_err(PromptPacketConfigurationError::from)?;
    let profile = super::executor::runtime_profile_with_timeout_fallback(
        super::runtime_profile::runtime_profile_for_job(job)
            .map_err(PromptPacketConfigurationError::from)?,
        &document.config,
        Some(&workflow),
        job,
    );
    let request = prompt_task_request_for_job(job, Some(store)).await?;
    if let PromptTaskRequest::PayloadUnavailable { prompt_ref } = &request {
        return Err(PromptPacketConfigurationError::new(format!(
            "runtime prompt payload unavailable: {prompt_ref}"
        ))
        .into());
    }
    let memory = super::repo_memory_prompt::repo_memory_for_prompt_packet(
        document.config.memory.enabled,
        Some(store),
        Some(&workflow),
        job,
        request.prompt_text(),
    )
    .await;
    let packet = build_runtime_prompt_packet(
        store.definition_registry(),
        job,
        Some(&workflow),
        Path::new(workspace),
        &source_root,
        &profile,
        None,
        &document,
        &memory.records,
        request.prompt_text(),
    )?;
    let digest = prompt_packet_digest(&packet);
    let prompt = build_runtime_job_prompt(&packet, request.prompt_text());
    Ok(PreparedRemotePrompt {
        response: json!({
            "prompt": prompt,
            "prompt_packet_digest": digest,
            "activity_result_schema": packet["activity_result_schema"],
        }),
        evidence: json!({
            "prompt_packet_digest": digest,
            "prompt_packet": packet,
            "repo_memory_degradation": memory.degradation,
        }),
    })
}

pub(crate) fn error_kind(error: &anyhow::Error) -> harness_workflow::runtime::ActivityErrorKind {
    if error
        .downcast_ref::<PromptPacketConfigurationError>()
        .is_some()
    {
        harness_workflow::runtime::ActivityErrorKind::Configuration
    } else {
        harness_workflow::runtime::ActivityErrorKind::ExternalDependency
    }
}
