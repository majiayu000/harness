use harness_workflow::runtime::ActivityResult;
use serde_json::{Map, Value};
const JSON_PAYLOAD_ENCODING: &str = "harness.runtime.json_payload.v1";
pub(super) struct StructuredActivityResultError {
    pub error: String,
    pub extracted_activity: Option<String>,
}
pub(super) fn parse_activity_result_json(
    block: &str,
    expected_activity: &str,
) -> Result<ActivityResult, StructuredActivityResultError> {
    let mut value: Value =
        serde_json::from_str(block).map_err(|error| StructuredActivityResultError {
            error: format!("activity result JSON is invalid: {error}"),
            extracted_activity: None,
        })?;
    let extracted_activity = value
        .get("activity")
        .and_then(Value::as_str)
        .map(str::to_string);
    normalize_strict_output_payloads(&mut value).map_err(|error| {
        StructuredActivityResultError {
            error,
            extracted_activity: extracted_activity.clone(),
        }
    })?;
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
fn normalize_strict_output_payloads(value: &mut Value) -> Result<(), String> {
    let Some(object) = value.as_object_mut() else {
        return Ok(());
    };
    normalize_payload_records(object, "artifacts", "artifact")?;
    normalize_payload_records(object, "signals", "signal")
}
fn normalize_payload_records(
    object: &mut Map<String, Value>,
    array_field: &str,
    payload_field: &str,
) -> Result<(), String> {
    let Some(Value::Array(records)) = object.get_mut(array_field) else {
        return Ok(());
    };
    for (index, record) in records.iter_mut().enumerate() {
        let path = format!("$.{array_field}[{index}].{payload_field}");
        let record = record
            .as_object_mut()
            .ok_or_else(|| format!("$.{array_field}[{index}] expected object"))?;
        let Some(payload) = record.get_mut(payload_field) else {
            continue;
        };
        let Some(payload_object) = payload.as_object() else {
            continue;
        };
        if payload_object.get("encoding").and_then(Value::as_str) == Some(JSON_PAYLOAD_ENCODING) {
            let json = payload_object
                .get("json")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("{path}.json is required"))?;
            *payload = serde_json::from_str(json)
                .map_err(|error| format!("{path}.json is not valid JSON: {error}"))?;
        }
    }
    Ok(())
}
fn validate_activity_result_shape(value: &Value) -> Result<(), String> {
    let object = expect_object(value, "$")?;
    expect_required_string(object, "activity", "$.activity")?;
    expect_required_string(object, "summary", "$.summary")?;
    expect_required_string(object, "status", "$.status")?;
    expect_optional_string_or_null(object, "error", "$.error")?;
    expect_optional_string_or_null(object, "error_kind", "$.error_kind")?;
    validate_array_records(object, "artifacts", |artifact, path| {
        expect_required_string(artifact, "artifact_type", &format!("{path}.artifact_type"))?;
        if !artifact.contains_key("artifact") {
            return Err(format!("{path}.artifact is required"));
        }
        Ok(())
    })?;
    validate_array_records(object, "signals", |signal, path| {
        expect_required_string(signal, "signal_type", &format!("{path}.signal_type"))?;
        let payload_path = format!("{path}.signal");
        expect_object(
            signal
                .get("signal")
                .ok_or_else(|| format!("{payload_path} is required"))?,
            &payload_path,
        )?;
        Ok(())
    })?;
    validate_array_records(object, "validation", |record, path| {
        expect_required_string(record, "command", &format!("{path}.command"))?;
        expect_required_string(record, "status", &format!("{path}.status"))?;
        expect_optional_string_or_null(record, "reason", &format!("{path}.reason"))?;
        Ok(())
    })?;
    Ok(())
}
fn validate_array_records(
    object: &Map<String, Value>,
    field: &str,
    mut validate_record: impl FnMut(&Map<String, Value>, &str) -> Result<(), String>,
) -> Result<(), String> {
    let Some(value) = object.get(field) else {
        return Ok(());
    };
    for (index, record) in expect_array(value, &format!("$.{field}"))?
        .iter()
        .enumerate()
    {
        let path = format!("$.{field}[{index}]");
        let record = expect_object(record, &path)?;
        validate_record(record, &path)?;
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
fn expect_optional_string_or_null(
    object: &Map<String, Value>,
    field: &str,
    path: &str,
) -> Result<(), String> {
    let Some(value) = object.get(field).filter(|value| !value.is_null()) else {
        return Ok(());
    };
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
