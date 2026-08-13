use serde_json::Value;

pub(super) fn dependency_override(data: &Value, reason: &str) -> String {
    let recovery = format!(
        "Operator requested workflow runtime unblock after overriding the dependency gate. Recovery reason: {reason}"
    );
    data.get("additional_prompt")
        .and_then(Value::as_str)
        .filter(|prompt| !prompt.trim().is_empty())
        .map_or(recovery.clone(), |prompt| format!("{prompt}\n\n{recovery}"))
}
