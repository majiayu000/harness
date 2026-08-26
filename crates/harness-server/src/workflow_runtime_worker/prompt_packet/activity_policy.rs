use super::pretty_json;
use harness_core::config::workflow::WorkflowDocument;
use harness_workflow::runtime::{
    DeclarativeDefinitionResolution, RuntimeJob, WorkflowDefinitionRegistry, WorkflowInstance,
};
use serde_json::{json, Value};

use crate::workflow_runtime_worker::data_helpers::activity_name;

pub(super) fn apply_activity_policy(
    registry: &WorkflowDefinitionRegistry,
    packet: &mut Value,
    job: &RuntimeJob,
    workflow: Option<&WorkflowInstance>,
    workflow_document: &WorkflowDocument,
) -> anyhow::Result<()> {
    apply_activity_policy_with_resolver(packet, job, workflow, workflow_document, |workflow| {
        registry.resolve_declarative_definition(workflow)
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
    let built_in = definition.definition_hash().starts_with("builtin:");
    let activity = activity_name(job);
    let state_policy = definition
        .policy()
        .states
        .get(&workflow.state)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "declarative workflow '{}' cannot dispatch from non-active state '{}'",
                workflow.id,
                workflow.state
            )
        })?;
    let expected_activity = state_policy.activity.as_deref().ok_or_else(|| {
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
    let pinned_classifier_policy =
        crate::workflow_runtime_worker::classifier::pinned_classifier_activity_policy(
            &definition,
            workflow,
            expected_activity,
        )?;
    let policy = if definition.requires_server_classifier_assessment(&workflow.state) {
        pinned_classifier_policy.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "declarative workflow '{}' classifier activity '{}' has no pinned dispatch policy",
                workflow.id,
                expected_activity
            )
        })?
    } else {
        match workflow_document.config.activities.get(expected_activity) {
            Some(policy) => policy,
            None if built_in && !is_required_builtin_classifier(expected_activity) => return Ok(()),
            None => {
                anyhow::bail!(
                    "declarative workflow '{}' activity '{}' is missing from WORKFLOW.md at dispatch",
                    workflow.id,
                    expected_activity
                )
            }
        }
    };
    if built_in && policy.classifier.is_none() {
        if is_required_builtin_classifier(expected_activity) {
            anyhow::bail!(
                "built-in classifier activity '{}' must declare a classifier policy in WORKFLOW.md",
                expected_activity
            );
        }
        return Ok(());
    }
    if let Some(classifier) = policy.classifier.as_ref() {
        classifier.validate_routes(
            expected_activity,
            state_policy.on_success.as_deref(),
            state_policy.on_failure.as_deref(),
            &state_policy.on_signal,
        )?;
    }

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
    if let Some(classifier) = policy.classifier.as_ref() {
        binding["classifier"] = serde_json::to_value(classifier)?;
        packet["classifier_facts"] = json!({
            "facts": job.input.get("scope_facts").cloned().unwrap_or(Value::Null),
            "provenance": {
                "issue_plan": "agent_authored_schema_validated",
                "server_issue_snapshot": "server_observed_github_rest",
                "server_pr_snapshot": "server_observed_github_graphql",
                "workflow_data": "see workflow.data_provenance",
            }
        });
        packet["activity_result_schema"]["classifier_contract"] = json!({
            "artifact_type": "classifier_output",
            "exactly_one": true,
            "verdicts": &classifier.verdicts,
            "fields": {
                "verdict": "one declared verdict",
                "rationale": "non-empty explanation grounded in the prompt packet",
                "evidence_refs": "optional prompt-packet JSON pointers"
            },
            "agent_signals_are_ignored": true,
        });
        packet["required_structured_output"]["classifier_output"] = json!({
            "required": true,
            "format": "Return exactly one artifact_type `classifier_output`; Harness validates it and authors the workflow signal.",
        });
        // Classifier instructions are pinned in workflow instance data. Do not
        // expose mutable checkout policy or prompt text beside that attested
        // policy, because a later WORKFLOW.md edit must not influence a turn
        // whose policy digest names the original snapshot.
        if let Some(workflow_file) = packet
            .get_mut("workflow_file")
            .and_then(Value::as_object_mut)
        {
            workflow_file.remove("config");
            workflow_file.remove("prompt_template");
        }
    }
    packet["activity_policy"] = binding;
    Ok(())
}

fn is_required_builtin_classifier(activity: &str) -> bool {
    matches!(
        activity,
        harness_workflow::runtime::CHANGE_SCOPE_REVIEW_ACTIVITY
    )
}

pub(super) fn is_classifier_activity(prompt_packet: &Value) -> bool {
    prompt_packet
        .pointer("/activity_policy/classifier")
        .is_some_and(Value::is_object)
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
