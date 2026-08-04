use anyhow::Context;
use harness_core::prompts::wrap_external_data;
use harness_workflow::runtime::{
    workflow_data_pointer, DataProvenance, WorkflowDataProvenance, WorkflowInstance,
    WORKFLOW_DATA_PROVENANCE_SCHEMA,
};
use serde_json::{json, Map, Value};

use super::{pretty_json, REPO_MEMORY_PROMPT_PREAMBLE};

const WORKFLOW_UNTRUSTED_DATA_SCHEMA: &str = "harness.runtime.workflow_untrusted_data.v1";
const CONTINUATION_CONTEXT_SCHEMA: &str = "harness.runtime.continuation_context.v2";

struct RenderedWorkflowData {
    trusted: Value,
    untrusted: Option<Value>,
}

struct RenderedValue {
    trusted: Option<Value>,
    untrusted: Option<Value>,
}

pub(super) fn workflow_prompt_value(
    workflow: &WorkflowInstance,
    job_input: &Value,
) -> anyhow::Result<Value> {
    let rendered = render_workflow_data(workflow, job_input)?;
    let mut value = json!({
        "id": workflow.id,
        "definition_id": workflow.definition_id,
        "definition_version": workflow.definition_version,
        "state": workflow.state,
        "version": workflow.version,
        "subject": workflow.subject,
        "parent_workflow_id": workflow.parent_workflow_id,
        "data": rendered.trusted,
    });
    if let Some(untrusted) = rendered.untrusted {
        value["untrusted_data"] = untrusted;
    }
    Ok(value)
}

fn render_workflow_data(
    workflow: &WorkflowInstance,
    job_input: &Value,
) -> anyhow::Result<RenderedWorkflowData> {
    let mut data = workflow.data.clone();
    super::remove_duplicated_command_field(&mut data, job_input, "additional_prompt");
    let mut degradation = Vec::new();
    let rendered = match &workflow.data_provenance {
        Some(provenance) => {
            if provenance.schema != WORKFLOW_DATA_PROVENANCE_SCHEMA {
                anyhow::bail!(
                    "workflow.data provenance schema `{}` is not supported",
                    provenance.schema
                );
            }
            render_value("", &data, provenance, &mut degradation)?
        }
        None => render_legacy_root(&data, &mut degradation)?,
    };
    let trusted = rendered.trusted.unwrap_or_else(|| json!({}));
    let untrusted = rendered.untrusted.map(|fields| {
        let fenced_field_count = count_fenced_fields(&fields);
        let mut value = json!({
            "schema": WORKFLOW_UNTRUSTED_DATA_SCHEMA,
            "preamble": REPO_MEMORY_PROMPT_PREAMBLE,
            "fenced_field_count": fenced_field_count,
            "fields": fields,
        });
        if !degradation.is_empty() {
            value["degradation"] = Value::Array(degradation);
        }
        value
    });
    Ok(RenderedWorkflowData { trusted, untrusted })
}

fn render_value(
    pointer: &str,
    value: &Value,
    provenance: &WorkflowDataProvenance,
    degradation: &mut Vec<Value>,
) -> anyhow::Result<RenderedValue> {
    // A more-specific classification always wins over an inherited container
    // classification, so mixed objects and arrays must be partitioned first.
    if provenance.has_descendant_entry(pointer) {
        return render_descendant_classified_container(pointer, value, provenance, degradation);
    }

    match provenance.provenance_for(pointer) {
        Some(DataProvenance::Server) => {
            return Ok(RenderedValue {
                trusted: Some(value.clone()),
                untrusted: None,
            });
        }
        Some(DataProvenance::Agent | DataProvenance::External) => {
            return Ok(RenderedValue {
                trusted: None,
                untrusted: Some(Value::String(fence_untrusted_value(value))),
            });
        }
        None => {}
    }

    if provenance.is_legacy(pointer) {
        degradation.push(json!({
            "pointer": pointer,
            "reason": "legacy_unclassified_workflow_data"
        }));
        return Ok(RenderedValue {
            trusted: None,
            untrusted: Some(Value::String(fence_untrusted_value(value))),
        });
    }

    // An *uncovered* empty container has no fields to classify, so there is
    // nothing to fence and no writer defect to report. This exception must
    // stay below the provenance lookups: an explicitly Agent- or
    // External-classified empty container still belongs behind the fence, and
    // a legacy one still owes its degradation evidence.
    if empty_container(value) {
        return Ok(RenderedValue {
            trusted: Some(value.clone()),
            untrusted: None,
        });
    }

    anyhow::bail!("unclassified workflow.data field `{pointer}`");
}

fn render_descendant_classified_container(
    pointer: &str,
    value: &Value,
    provenance: &WorkflowDataProvenance,
    degradation: &mut Vec<Value>,
) -> anyhow::Result<RenderedValue> {
    match value {
        Value::Object(object) => {
            render_descendant_classified_object(pointer, object, provenance, degradation)
        }
        Value::Array(array) => {
            render_descendant_classified_array(pointer, array, provenance, degradation)
        }
        _ => anyhow::bail!(
            "workflow.data field `{pointer}` has descendant provenance but is not a container"
        ),
    }
}

fn render_descendant_classified_object(
    pointer: &str,
    object: &Map<String, Value>,
    provenance: &WorkflowDataProvenance,
    degradation: &mut Vec<Value>,
) -> anyhow::Result<RenderedValue> {
    let mut trusted = Map::new();
    let mut untrusted = Map::new();
    for (key, child) in object {
        let child_pointer = workflow_data_pointer(pointer, key);
        let rendered = render_value(&child_pointer, child, provenance, degradation)?;
        if let Some(value) = rendered.trusted {
            trusted.insert(key.clone(), value);
        }
        if let Some(value) = rendered.untrusted {
            untrusted.insert(key.clone(), value);
        }
    }
    Ok(RenderedValue {
        trusted: object_or_none(trusted),
        untrusted: object_or_none(untrusted),
    })
}

fn render_descendant_classified_array(
    pointer: &str,
    array: &[Value],
    provenance: &WorkflowDataProvenance,
    degradation: &mut Vec<Value>,
) -> anyhow::Result<RenderedValue> {
    let mut trusted = vec![Value::Null; array.len()];
    let mut untrusted = vec![Value::Null; array.len()];
    let mut has_trusted = false;
    let mut has_untrusted = false;
    for (index, child) in array.iter().enumerate() {
        let child_pointer = workflow_data_pointer(pointer, &index.to_string());
        let rendered = render_value(&child_pointer, child, provenance, degradation)?;
        if let Some(value) = rendered.trusted {
            trusted[index] = value;
            has_trusted = true;
        }
        if let Some(value) = rendered.untrusted {
            untrusted[index] = value;
            has_untrusted = true;
        }
    }
    Ok(RenderedValue {
        trusted: has_trusted.then_some(Value::Array(trusted)),
        untrusted: has_untrusted.then_some(Value::Array(untrusted)),
    })
}

fn render_legacy_root(
    value: &Value,
    degradation: &mut Vec<Value>,
) -> anyhow::Result<RenderedValue> {
    let object = value
        .as_object()
        .context("legacy workflow.data must be a JSON object")?;
    let mut untrusted = Map::new();
    for (key, child) in object {
        let pointer = workflow_data_pointer("", key);
        degradation.push(json!({
            "pointer": pointer,
            "reason": "legacy_unclassified_workflow_data"
        }));
        untrusted.insert(key.to_string(), Value::String(fence_untrusted_value(child)));
    }
    Ok(RenderedValue {
        trusted: Some(json!({})),
        untrusted: object_or_none(untrusted),
    })
}

fn object_or_none(object: Map<String, Value>) -> Option<Value> {
    if object.is_empty() {
        None
    } else {
        Some(Value::Object(object))
    }
}

fn empty_container(value: &Value) -> bool {
    value.as_object().is_some_and(Map::is_empty) || value.as_array().is_some_and(Vec::is_empty)
}

pub(super) fn fence_untrusted_value(value: &Value) -> String {
    match value {
        Value::String(value) => wrap_external_data(value),
        Value::Null => wrap_external_data("null"),
        _ => wrap_external_data(&pretty_json(value)),
    }
}

fn count_fenced_fields(value: &Value) -> usize {
    match value {
        Value::String(_) => 1,
        Value::Object(object) => object.values().map(count_fenced_fields).sum(),
        Value::Array(array) => array.iter().map(count_fenced_fields).sum(),
        _ => 0,
    }
}

pub(super) fn prompt_continuation_context(workflow: Option<&WorkflowInstance>) -> Option<Value> {
    let continuation = workflow?.data.get("continuation")?;
    let attempt = continuation.get("attempt")?.as_u64()?;
    if attempt <= 1 {
        return None;
    }
    let previous_external_state = continuation
        .get("last_external_state")
        .unwrap_or(&Value::Null);
    let previous_summary = continuation.get("last_summary").unwrap_or(&Value::Null);
    Some(json!({
        "schema": CONTINUATION_CONTEXT_SCHEMA,
        "preamble": REPO_MEMORY_PROMPT_PREAMBLE,
        "attempt": attempt,
        "previous_external_state": fence_untrusted_value(previous_external_state),
        "previous_summary": fence_untrusted_value(previous_summary),
    }))
}

pub(super) fn append_continuation_context_prompt(prompt: &mut String, prompt_packet: &Value) {
    let Some(context) = prompt_packet.get("continuation_context") else {
        return;
    };
    let attempt = context.get("attempt").and_then(Value::as_u64).unwrap_or(0);
    let preamble = context
        .get("preamble")
        .and_then(Value::as_str)
        .unwrap_or(REPO_MEMORY_PROMPT_PREAMBLE);
    let previous_state = context
        .get("previous_external_state")
        .and_then(Value::as_str)
        .unwrap_or("<external_data>\nnull\n</external_data>");
    let previous_summary = context
        .get("previous_summary")
        .and_then(Value::as_str)
        .unwrap_or("<external_data>\nnull\n</external_data>");
    prompt.push_str(&format!(
        "\nContinuation context:\n```external-data\n{preamble}\nAttempt: {attempt}\nPrevious external state:\n{previous_state}\nPrevious attempt summary:\n{previous_summary}\n```\n"
    ));
}
