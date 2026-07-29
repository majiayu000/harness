use harness_core::config::workflow::{RuntimeDispatchProfileOverride, WorkflowConfig};
use harness_workflow::runtime::{RuntimeJob, RuntimeProfile, WorkflowInstance};

use super::super::data_helpers::activity_name;

const DEFAULT_RUNTIME_TURN_TIMEOUT_SECS: u64 = 3600;
const DEFAULT_PR_FEEDBACK_INSPECT_TIMEOUT_SECS: u64 = 3600;

pub(super) fn runtime_profile_with_timeout_fallback(
    mut profile: RuntimeProfile,
    workflow_config: &WorkflowConfig,
    workflow: Option<&WorkflowInstance>,
    job: &RuntimeJob,
) -> RuntimeProfile {
    if profile.timeout_secs.is_none() {
        profile.timeout_secs = runtime_timeout_fallback(workflow_config, workflow, job);
    }
    profile
}

pub(super) fn runtime_timeout_fallback(
    workflow_config: &WorkflowConfig,
    workflow: Option<&WorkflowInstance>,
    job: &RuntimeJob,
) -> Option<u64> {
    let runtime_dispatch = &workflow_config.runtime_dispatch;
    let workflow_definition_id = workflow.map(|workflow| workflow.definition_id.as_str());
    let activity = activity_name(job);

    workflow_definition_id
        .and_then(|definition_id| {
            runtime_dispatch
                .workflow_activity_profiles
                .get(definition_id)
                .and_then(|profiles| profiles.get(&activity))
        })
        .and_then(profile_timeout)
        .or_else(|| {
            runtime_dispatch
                .activity_profiles
                .get(&activity)
                .and_then(profile_timeout)
        })
        .or_else(|| {
            workflow_definition_id.and_then(|definition_id| {
                runtime_dispatch
                    .workflow_profiles
                    .get(definition_id)
                    .and_then(profile_timeout)
            })
        })
        .or(runtime_dispatch.timeout_secs)
        .or_else(|| default_runtime_timeout(&activity))
}

fn profile_timeout(profile: &RuntimeDispatchProfileOverride) -> Option<u64> {
    profile.timeout_secs
}

fn default_runtime_timeout(activity: &str) -> Option<u64> {
    Some(match activity {
        "inspect_pr_feedback" => DEFAULT_PR_FEEDBACK_INSPECT_TIMEOUT_SECS,
        _ => DEFAULT_RUNTIME_TURN_TIMEOUT_SECS,
    })
}
