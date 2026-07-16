use super::pretty_json;
use harness_core::config::workflow::WorkflowDocument;
use harness_workflow::runtime::{
    resolve_declarative_definition, DeclarativeDefinitionResolution, RuntimeJob, WorkflowInstance,
};
use serde_json::{json, Value};

use crate::workflow_runtime_worker::data_helpers::activity_name;

pub(super) fn apply_activity_policy(
    packet: &mut Value,
    job: &RuntimeJob,
    workflow: Option<&WorkflowInstance>,
    workflow_document: &WorkflowDocument,
) -> anyhow::Result<()> {
    apply_activity_policy_with_resolver(packet, job, workflow, workflow_document, |workflow| {
        resolve_declarative_definition(workflow)
    })
}

pub(super) fn apply_activity_policy_with_resolver(
    packet: &mut Value,
    job: &RuntimeJob,
    workflow: Option<&WorkflowInstance>,
    workflow_document: &WorkflowDocument,
    resolve_definition: impl FnOnce(&WorkflowInstance) -> DeclarativeDefinitionResolution,
) -> anyhow::Result<()> {
    let Some(workflow) = workflow else {
        return Ok(());
    };
    let definition = match resolve_definition(workflow) {
        DeclarativeDefinitionResolution::NotDeclarative => return Ok(()),
        DeclarativeDefinitionResolution::Resolved(definition) => definition,
        DeclarativeDefinitionResolution::PinError(error) => anyhow::bail!(
            "declarative workflow '{}' has an invalid definition pin while binding activity policy: {error:?}",
            workflow.id
        ),
    };
    let activity = activity_name(job);
    let expected_activity = definition
        .policy()
        .states
        .get(&workflow.state)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "declarative workflow '{}' cannot dispatch from non-active state '{}'",
                workflow.id,
                workflow.state
            )
        })?
        .activity
        .as_deref()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "declarative workflow '{}' state '{}' has no activity for runtime dispatch",
                workflow.id,
                workflow.state
            )
        })?;
    if activity != expected_activity {
        anyhow::bail!(
            "declarative workflow '{}' state '{}' expects activity '{}', got '{}'",
            workflow.id,
            workflow.state,
            expected_activity,
            activity
        );
    }
    let policy = workflow_document
        .config
        .activities
        .get(expected_activity)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "declarative workflow '{}' activity '{}' is missing from WORKFLOW.md at dispatch",
                workflow.id,
                expected_activity
            )
        })?;

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
    Ok(())
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
