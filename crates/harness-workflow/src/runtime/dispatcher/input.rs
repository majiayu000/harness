use super::*;

pub(super) fn author_trust_class_from_data(
    data: &serde_json::Value,
) -> anyhow::Result<Option<IsolationTrustClass>> {
    let Some(value) = data.get("author_trust_class") else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    serde_json::from_value(value.clone())
        .map(Some)
        .with_context(|| format!("invalid author_trust_class in workflow metadata: {value}"))
}

pub(super) fn retry_not_before_for_command(
    command: &WorkflowCommandRecord,
) -> anyhow::Result<Option<DateTime<Utc>>> {
    let Some(raw) = command
        .command
        .command
        .get("retry_not_before")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
    else {
        return Ok(None);
    };
    DateTime::parse_from_rfc3339(raw)
        .map(|value| Some(value.with_timezone(&Utc)))
        .with_context(|| {
            format!(
                "workflow command {} has invalid retry_not_before",
                command.id
            )
        })
}
