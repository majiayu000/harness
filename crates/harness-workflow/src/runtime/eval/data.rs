use serde_json::{json, Value};

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
