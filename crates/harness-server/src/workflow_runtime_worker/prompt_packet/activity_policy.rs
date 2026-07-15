use super::pretty_json;
use harness_core::config::workflow::WorkflowDocument;
use harness_workflow::runtime::{
    declarative_workflow_definition_for_instance, DeclarativeWorkflowDefinition, RuntimeJob,
    WorkflowInstance,
};
use serde_json::{json, Value};
use std::sync::Arc;

use crate::workflow_runtime_worker::data_helpers::activity_name;

pub(super) fn apply_activity_policy(
    packet: &mut Value,
    job: &RuntimeJob,
    workflow: Option<&WorkflowInstance>,
    workflow_document: &WorkflowDocument,
) {
    apply_activity_policy_with_resolver(packet, job, workflow, workflow_document, |workflow| {
        declarative_workflow_definition_for_instance(workflow)
    });
}

pub(super) fn apply_activity_policy_with_resolver(
    packet: &mut Value,
    job: &RuntimeJob,
    workflow: Option<&WorkflowInstance>,
    workflow_document: &WorkflowDocument,
    resolve_definition: impl FnOnce(&WorkflowInstance) -> Option<Arc<DeclarativeWorkflowDefinition>>,
) {
    let Some(workflow) = workflow else {
        return;
    };
    let Some(definition) = resolve_definition(workflow) else {
        return;
    };
    let activity = activity_name(job);
    if definition
        .policy()
        .states
        .get(&workflow.state)
        .and_then(|state| state.activity.as_deref())
        != Some(activity.as_str())
    {
        return;
    }
    let Some(policy) = workflow_document.config.activities.get(&activity) else {
        return;
    };

    let mut binding = json!({
        "activity": activity,
        "validation": &policy.validation,
    });
    if let Some(prompt) = policy.prompt.as_deref() {
        binding["prompt"] = json!(prompt);
    }
    if !policy.validation.is_empty() {
        packet["activity_result_schema"]["validation_contract"] = json!({
            "required_commands": &policy.validation,
            "report_each_command": true,
            "required_status": "successful",
        });
        packet["required_structured_output"]["validation_commands"] = json!({
            "required": &policy.validation,
            "format": "Report one validation record per command with its actual status.",
        });
    }
    packet["activity_policy"] = binding;
}

pub(super) fn append_activity_policy_prompt(prompt: &mut String, prompt_packet: &Value) {
    let Some(policy) = prompt_packet.get("activity_policy") else {
        return;
    };
    if let Some(instructions) = policy
        .get("prompt")
        .and_then(Value::as_str)
        .filter(|instructions| !instructions.trim().is_empty())
    {
        prompt.push_str("\nActivity policy instructions:\n");
        prompt.push_str(instructions);
        prompt.push('\n');
    }
    if let Some(validation) = policy
        .get("validation")
        .and_then(Value::as_array)
        .filter(|commands| !commands.is_empty())
    {
        prompt.push_str("\nActivity policy validation commands (run and report each command):\n");
        prompt.push_str(&pretty_json(validation));
        prompt.push('\n');
    }
}
