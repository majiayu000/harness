use crate::runtime::{DataProvenance, WorkflowInstance};
use serde_json::{json, Value};

pub fn server_owned_eval_metadata(instance: &WorkflowInstance) -> Option<&Value> {
    let eval = instance.data.get("eval")?;
    let provenance = instance.data_provenance.as_ref()?.provenance_for("/eval")?;
    if provenance != DataProvenance::Server
        || eval
            .get("eval_run_id")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        || eval
            .get("case_id")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
    {
        return None;
    }
    Some(eval)
}

pub(super) fn eval_cleanup_data(
    mut data: Value,
    eval_run_id: &str,
    case_id: &str,
    reason: &str,
) -> Value {
    if !data.is_object() {
        data = json!({});
    }
    let Some(object) = data.as_object_mut() else {
        return data;
    };
    let eval = object
        .entry("eval".to_string())
        .or_insert_with(|| json!({}));
    if !eval.is_object() {
        *eval = json!({});
    }
    if let Some(eval_object) = eval.as_object_mut() {
        eval_object.insert(
            "cleanup".to_string(),
            json!({
                "status": "cancelled",
                "eval_run_id": eval_run_id,
                "case_id": case_id,
                "reason": reason,
            }),
        );
    }
    data
}
