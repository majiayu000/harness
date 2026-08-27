use super::{
    append_continuation_context_prompt, pretty_json, repo_memory_prompt_section,
    strip_model_facing_audit_sections,
};
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
    if definition.definition_hash().starts_with("builtin:") {
        return Ok(());
    }
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
    if let Some(policy) =
        crate::workflow_runtime_worker::executor_contract::classifier_policy_for_job(job)?
    {
        let pinned_policy = definition
            .classifier_activity_policy(expected_activity)
            .and_then(|activity_policy| activity_policy.classifier.as_ref())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "declarative workflow '{}' classifier activity '{}' has no pinned policy",
                    workflow.id,
                    expected_activity
                )
            })?;
        if pinned_policy != &policy {
            anyhow::bail!(
                "declarative workflow '{}' classifier job policy does not match its pinned definition",
                workflow.id
            );
        }
        apply_classifier_packet(packet, job, expected_activity, &policy)?;
        return Ok(());
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

fn apply_classifier_packet(
    packet: &mut Value,
    job: &RuntimeJob,
    activity: &str,
    policy: &harness_core::config::workflow::WorkflowClassifierPolicy,
) -> anyhow::Result<()> {
    let classifier_input = job
        .input
        .pointer("/classifier/input")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("classifier runtime job is missing its input snapshot"))?;
    harness_workflow::runtime::validate_classifier_input(&classifier_input)?;
    let runtime_job = packet.get("runtime_job").cloned().unwrap_or(Value::Null);
    let mut result_schema = packet
        .get("activity_result_schema")
        .cloned()
        .unwrap_or_else(|| json!({}));
    result_schema["classifier_output_contract"] = json!({
        "exact_count": 1,
        "artifact_type": harness_workflow::runtime::CLASSIFIER_OUTPUT_ARTIFACT,
        "payload": {
            "schema": harness_workflow::runtime::CLASSIFIER_OUTPUT_SCHEMA,
            "verdict": policy.verdicts,
            "rationale": "non-empty string",
            "evidence_refs": "JSON pointers rooted under /classifier_input"
        },
        "signals": "must be empty; Harness derives routing from the validated assessment"
    });
    *packet = json!({
        "schema": "harness.runtime.classifier_prompt_packet.v1",
        "runtime_job": runtime_job,
        "classifier_input": classifier_input,
        "classifier_policy": {
            "activity": activity,
            "verdicts": policy.verdicts,
            "instructions": policy.instructions,
            "policy_sha256": job.input.pointer("/classifier/policy_sha256"),
        },
        "activity_result_schema": result_schema,
        "required_structured_output": {
            "classifier_output": "Exactly one classifier_output artifact matching classifier_output_contract.",
            "signals": "Return an empty array.",
            "validation": "Return an empty array. Do not run repository commands."
        },
        "activity_policy": {
            "activity": activity,
            "classifier": policy,
        }
    });
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
    if let Some(classifier) = policy.get("classifier") {
        prompt.push_str(
            "\nClassifier execution contract:\n- Use only classifier_input facts.\n- Do not call tools, inspect the repository, or mutate external state.\n- Return exactly one classifier_output artifact and no signals.\n",
        );
        prompt.push_str(&pretty_json(classifier));
        prompt.push('\n');
    }
}

pub(crate) fn build_runtime_job_prompt(
    prompt_packet: &Value,
    prompt_task_request: Option<&str>,
) -> String {
    let workflow_prompt_template = prompt_packet
        .pointer("/workflow_file/prompt_template")
        .and_then(Value::as_str)
        .filter(|template| !template.trim().is_empty())
        .map(ToOwned::to_owned);
    let mut model_packet = prompt_packet.clone();
    if let Some(workflow_file) = model_packet
        .get_mut("workflow_file")
        .and_then(Value::as_object_mut)
    {
        workflow_file.remove("prompt_template");
    }
    strip_model_facing_audit_sections(&mut model_packet);
    let prompt_packet_json = pretty_json(&model_packet);
    if prompt_packet.get("schema").and_then(Value::as_str)
        == Some("harness.runtime.classifier_prompt_packet.v1")
    {
        return format!(
            "You are executing a Harness classifier activity.\n\n\
             Classifier contract:\n\
             - Use only facts present in classifier_input.\n\
             - Do not call tools, inspect a repository, use outside knowledge, or mutate external state.\n\
             - Choose exactly one verdict declared by classifier_policy.\n\
             - Return exactly one classifier_output artifact and no signals or validation records.\n\
             - Return a raw JSON activity result when the transport enforces output-schema; otherwise put it in a final fenced `harness-activity-result` block.\n\n\
             Classifier prompt packet:\n{prompt_packet_json}\n"
        );
    }
    let activity = prompt_packet
        .pointer("/runtime_job/activity")
        .and_then(Value::as_str)
        .unwrap_or("workflow_activity");
    let project_root = prompt_packet
        .pointer("/project/root")
        .and_then(Value::as_str)
        .unwrap_or("");
    let runtime_profile = prompt_packet
        .pointer("/runtime_job/runtime_profile")
        .and_then(Value::as_str)
        .unwrap_or("");
    let job_id = prompt_packet
        .pointer("/runtime_job/id")
        .and_then(Value::as_str)
        .unwrap_or("");
    let mut prompt = format!(
        "You are executing a Harness workflow runtime job.\n\n\
         Runtime contract:\n\
         - Treat the workflow database as the source of orchestration state, but do not edit workflow tables directly.\n\
         - Harness server only manages lifecycle. You, the agent, perform repository and GitHub work when the activity requires it.\n\
         - Follow the project instructions loaded by the runtime.\n\
         - Use the prompt packet activity_result_schema to shape your final summary.\n\
         - When returning structured activity output, return a raw JSON object matching activity_result_schema when your transport enforces output-schema; otherwise put the JSON object in a final fenced `harness-activity-result` block matching activity_result_schema.\n\
         - The structured result activity field must match this runtime job activity exactly.\n\
         - Return a concise final summary appropriate to the activity. Include changed files and validation commands only when repository code changes were requested; for discovery and planning activities, report inspected inputs, emitted signals, and remaining blockers.\n\n\
         Project root: {project_root}\n\
         Runtime job id: {job_id}\n\
         Runtime profile: {runtime_profile}\n\
         Activity: {activity}\n\n\
         Prompt packet:\n{prompt_packet_json}\n",
    );
    append_continuation_context_prompt(&mut prompt, prompt_packet);
    if let Some(repo_memory_section) = repo_memory_prompt_section(prompt_packet) {
        prompt.push_str(&repo_memory_section);
    }
    append_activity_policy_prompt(&mut prompt, prompt_packet);
    if let Some(prompt_task_request) = prompt_task_request {
        prompt.push_str("\nPrompt task request:\n");
        prompt.push_str(prompt_task_request);
        prompt.push('\n');
    }
    if let Some(template) = workflow_prompt_template {
        prompt.push_str("\nRepository workflow prompt template:\n");
        prompt.push_str(&template);
        prompt.push('\n');
    }
    prompt
}
