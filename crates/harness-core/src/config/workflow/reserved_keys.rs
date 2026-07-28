use super::super::completion_evidence_config_field;
use serde_yaml::Value;

pub(super) fn reject_misplaced_completion_evidence_fields(
    value: &Value,
    source_path: &str,
) -> anyhow::Result<()> {
    if let Some(field) = find_reserved_key(value, false) {
        anyhow::bail!(
            "failed to parse merged workflow front matter ({source_path}): unknown field `{field}`"
        );
    }
    Ok(())
}

fn find_reserved_key(value: &Value, in_key: bool) -> Option<&'static str> {
    match value {
        Value::String(field) if in_key => completion_evidence_config_field(field),
        Value::Mapping(fields) => fields.iter().find_map(|(field, nested)| {
            find_reserved_key(field, true).or_else(|| find_reserved_key(nested, in_key))
        }),
        Value::Sequence(values) => values
            .iter()
            .find_map(|value| find_reserved_key(value, in_key)),
        Value::Tagged(tagged) => {
            // Serde exposes a YAML tag as an enum variant, so its label is the
            // first piece of compound-key content and must precede the payload.
            let tag_field = in_key.then(|| tagged.tag.to_string()).and_then(|tag| {
                let tag = tag.strip_prefix('!').unwrap_or(&tag);
                completion_evidence_config_field(tag)
            });
            tag_field.or_else(|| find_reserved_key(&tagged.value, in_key))
        }
        _ => None,
    }
}
