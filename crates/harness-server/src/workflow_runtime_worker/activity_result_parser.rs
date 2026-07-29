use harness_workflow::runtime::ActivityResult;
use serde_json::{Map, Value};

pub(super) struct StructuredActivityResultError {
    pub error: String,
    pub extracted_activity: Option<String>,
}

pub(super) fn parse_activity_result_json(
    block: &str,
    expected_activity: &str,
) -> Result<ActivityResult, StructuredActivityResultError> {
    let value: Value =
        serde_json::from_str(block).map_err(|error| StructuredActivityResultError {
            error: format!("activity result JSON is invalid: {error}"),
            extracted_activity: None,
        })?;
    let extracted_activity = value
        .get("activity")
        .and_then(Value::as_str)
        .map(str::to_string);
    validate_activity_result_shape(&value).map_err(|error| StructuredActivityResultError {
        error,
        extracted_activity: extracted_activity.clone(),
    })?;
    let result: ActivityResult =
        serde_json::from_value(value).map_err(|error| StructuredActivityResultError {
            error: format!("activity result JSON could not be deserialized: {error}"),
            extracted_activity: extracted_activity.clone(),
        })?;
    if result.activity != expected_activity {
        return Err(StructuredActivityResultError {
            error: format!(
                "activity result JSON reported activity `{}`, expected `{expected_activity}`",
                result.activity
            ),
            extracted_activity: Some(result.activity),
        });
    }
    Ok(result)
}

fn validate_activity_result_shape(value: &Value) -> Result<(), String> {
    let object = expect_object(value, "$")?;
    reject_unknown_fields(
        object,
        "$",
        &[
            "activity",
            "status",
            "summary",
            "artifacts",
            "signals",
            "validation",
            "error",
            "error_kind",
        ],
    )?;
    expect_required_string(object, "activity", "$.activity")?;
    expect_required_string(object, "summary", "$.summary")?;
    expect_required_string_enum(
        object,
        "status",
        "$.status",
        &["succeeded", "failed", "blocked", "cancelled"],
    )?;
    expect_optional_string_or_null(object, "error", "$.error")?;
    expect_optional_string_enum_or_null(
        object,
        "error_kind",
        "$.error_kind",
        &[
            "retryable",
            "timeout",
            "fatal",
            "configuration",
            "external_dependency",
            "unknown",
        ],
    )?;
    validate_artifacts(object)?;
    validate_signals(object)?;
    validate_validation(object)?;
    Ok(())
}

fn validate_artifacts(object: &Map<String, Value>) -> Result<(), String> {
    let Some(value) = object.get("artifacts") else {
        return Ok(());
    };
    let artifacts = expect_array(value, "$.artifacts")?;
    for (index, artifact) in artifacts.iter().enumerate() {
        let path = format!("$.artifacts[{index}]");
        let artifact = expect_object(artifact, &path)?;
        reject_unknown_fields(artifact, &path, &["artifact_type", "artifact"])?;
        expect_required_string(artifact, "artifact_type", &format!("{path}.artifact_type"))?;
        if !artifact.contains_key("artifact") {
            return Err(format!("{path}.artifact is required"));
        }
    }
    Ok(())
}

fn validate_signals(object: &Map<String, Value>) -> Result<(), String> {
    let Some(value) = object.get("signals") else {
        return Ok(());
    };
    let signals = expect_array(value, "$.signals")?;
    for (index, signal) in signals.iter().enumerate() {
        let path = format!("$.signals[{index}]");
        let signal = expect_object(signal, &path)?;
        reject_unknown_fields(signal, &path, &["signal_type", "signal"])?;
        expect_required_string(signal, "signal_type", &format!("{path}.signal_type"))?;
        let payload_path = format!("{path}.signal");
        let payload = signal
            .get("signal")
            .ok_or_else(|| format!("{payload_path} is required"))?;
        expect_object(payload, &payload_path)?;
    }
    Ok(())
}

fn validate_validation(object: &Map<String, Value>) -> Result<(), String> {
    let Some(value) = object.get("validation") else {
        return Ok(());
    };
    let records = expect_array(value, "$.validation")?;
    for (index, record) in records.iter().enumerate() {
        let path = format!("$.validation[{index}]");
        let record = expect_object(record, &path)?;
        reject_unknown_fields(record, &path, &["command", "status", "reason"])?;
        expect_required_string(record, "command", &format!("{path}.command"))?;
        expect_required_string(record, "status", &format!("{path}.status"))?;
        expect_optional_string_or_null(record, "reason", &format!("{path}.reason"))?;
    }
    Ok(())
}

fn expect_required_string(
    object: &Map<String, Value>,
    field: &str,
    path: &str,
) -> Result<(), String> {
    let value = object
        .get(field)
        .ok_or_else(|| format!("{path} is required"))?;
    expect_string(value, path).map(|_| ())
}

fn expect_required_string_enum(
    object: &Map<String, Value>,
    field: &str,
    path: &str,
    allowed: &[&str],
) -> Result<(), String> {
    let value = object
        .get(field)
        .ok_or_else(|| format!("{path} is required"))?;
    let value = expect_string(value, path)?;
    if allowed.contains(&value) {
        Ok(())
    } else {
        Err(format!(
            "{path} expected one of [{}], got `{value}`",
            allowed.join(", ")
        ))
    }
}

fn expect_optional_string_enum_or_null(
    object: &Map<String, Value>,
    field: &str,
    path: &str,
    allowed: &[&str],
) -> Result<(), String> {
    let Some(value) = object.get(field) else {
        return Ok(());
    };
    if value.is_null() {
        return Ok(());
    }
    let value = expect_string(value, path)?;
    if allowed.contains(&value) {
        Ok(())
    } else {
        Err(format!(
            "{path} expected one of [{}] or null, got `{value}`",
            allowed.join(", ")
        ))
    }
}

fn expect_optional_string_or_null(
    object: &Map<String, Value>,
    field: &str,
    path: &str,
) -> Result<(), String> {
    let Some(value) = object.get(field) else {
        return Ok(());
    };
    if value.is_null() {
        return Ok(());
    }
    expect_string(value, path).map(|_| ())
}

fn expect_string<'a>(value: &'a Value, path: &str) -> Result<&'a str, String> {
    value
        .as_str()
        .ok_or_else(|| format!("{path} expected string, got {}", json_type_name(value)))
}

fn expect_array<'a>(value: &'a Value, path: &str) -> Result<&'a Vec<Value>, String> {
    value
        .as_array()
        .ok_or_else(|| format!("{path} expected array, got {}", json_type_name(value)))
}

fn expect_object<'a>(value: &'a Value, path: &str) -> Result<&'a Map<String, Value>, String> {
    value
        .as_object()
        .ok_or_else(|| format!("{path} expected object, got {}", json_type_name(value)))
}

fn reject_unknown_fields(
    object: &Map<String, Value>,
    path: &str,
    allowed: &[&str],
) -> Result<(), String> {
    for key in object.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(format!("{path}.{key} is not allowed"));
        }
    }
    Ok(())
}

fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}
