//! Shared JSONB serialization for Postgres.
//!
//! PostgreSQL rejects NUL characters inside `jsonb` values, so they must be
//! removed before a write. Doing that with a textual `String::replace` over
//! already-serialized JSON is unsafe: the six-character escape serde emits for
//! a NUL is also a suffix of the seven-character text serde emits for a
//! *literal* backslash followed by `u0000`. A textual replace matches inside
//! that escaped literal and leaves a dangling backslash behind, corrupting the
//! document. Stripping therefore happens at the JSON value level, and every
//! removal is counted and logged.

use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;
use serde_json::{Map, Value};

static NUL_SANITIZATIONS: AtomicU64 = AtomicU64::new(0);

/// Total number of values sanitized since process start.
pub(crate) fn nul_sanitizations_total() -> u64 {
    NUL_SANITIZATIONS.load(Ordering::Relaxed)
}

/// Serialize `value` as a JSON string safe to bind to a Postgres `jsonb` column.
///
/// NUL characters are removed from string contents and object keys. Removal is
/// data loss, so it is counted and logged at error level with the serialized
/// type and the JSON pointers that were affected — never the values themselves,
/// which may carry secrets.
pub(crate) fn to_jsonb_string<T>(value: &T) -> anyhow::Result<String>
where
    T: Serialize + ?Sized,
{
    let mut json = serde_json::to_value(value)?;
    let mut affected = Vec::new();
    strip_nul(&mut json, &mut String::new(), &mut affected);

    if !affected.is_empty() {
        NUL_SANITIZATIONS.fetch_add(affected.len() as u64, Ordering::Relaxed);
        tracing::error!(
            entity_type = std::any::type_name::<T>(),
            removals = affected.len(),
            pointers = ?affected,
            total_sanitizations = nul_sanitizations_total(),
            "stripped NUL characters before jsonb write; stored value differs from the input"
        );
    }

    Ok(serde_json::to_string(&json)?)
}

/// Remove NUL characters in place, recording an RFC 6901 pointer per affected node.
fn strip_nul(value: &mut Value, path: &mut String, affected: &mut Vec<String>) {
    match value {
        Value::String(text) => {
            if text.contains('\0') {
                text.retain(|ch| ch != '\0');
                affected.push(path.clone());
            }
        }
        Value::Array(items) => {
            for (index, item) in items.iter_mut().enumerate() {
                let restore = path.len();
                path.push('/');
                path.push_str(&index.to_string());
                strip_nul(item, path, affected);
                path.truncate(restore);
            }
        }
        Value::Object(entries) => {
            if entries.keys().any(|key| key.contains('\0')) {
                let mut rekeyed = Map::with_capacity(entries.len());
                for (key, item) in std::mem::take(entries) {
                    if key.contains('\0') {
                        let restore = path.len();
                        path.push('/');
                        path.push_str(&escape_token(&key));
                        affected.push(path.clone());
                        path.truncate(restore);
                    }
                    // A rekey can collide with an existing key; last write wins,
                    // matching the previous textual behavior.
                    rekeyed.insert(key.replace('\0', ""), item);
                }
                *entries = rekeyed;
            }

            for (key, item) in entries.iter_mut() {
                let restore = path.len();
                path.push('/');
                path.push_str(&escape_token(key));
                strip_nul(item, path, affected);
                path.truncate(restore);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

/// RFC 6901 reference-token escaping.
fn escape_token(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The six characters serde emits for a NUL, built without writing the
    /// sequence into this source file.
    fn nul_escape() -> String {
        format!("{}u0000", '\\')
    }

    /// Content holding a literal backslash + `u0000`, which must survive intact.
    fn content_with_escaped_literal() -> String {
        format!("literal {} mention", nul_escape())
    }

    /// Reproduces the defect this module replaces (GH-1795).
    ///
    /// The former implementation textually replaced the NUL escape in the
    /// serialized output. When the input holds a literal backslash + `u0000`,
    /// serde escapes the backslash, so the serialized text holds seven
    /// characters. The replace then matches the trailing six and leaves a
    /// dangling backslash, which is not a valid JSON escape.
    #[test]
    fn legacy_textual_replace_corrupts_escaped_literal() {
        let value = serde_json::json!({ "summary": content_with_escaped_literal() });

        let legacy = serde_json::to_string(&value)
            .unwrap()
            .replace(&nul_escape(), "");

        assert!(
            serde_json::from_str::<Value>(&legacy).is_err(),
            "legacy replace was expected to produce invalid JSON, got: {legacy}"
        );
    }

    #[test]
    fn escaped_literal_survives_untouched() {
        let value = serde_json::json!({ "summary": content_with_escaped_literal() });

        let encoded = to_jsonb_string(&value).unwrap();
        let decoded: Value = serde_json::from_str(&encoded).unwrap();

        assert_eq!(decoded, value, "content without a real NUL must round-trip");
    }

    #[test]
    fn real_nul_is_stripped_from_strings() {
        let value = serde_json::json!({ "summary": "a\0b" });

        let encoded = to_jsonb_string(&value).unwrap();
        let decoded: Value = serde_json::from_str(&encoded).unwrap();

        assert_eq!(decoded, serde_json::json!({ "summary": "ab" }));
        assert!(!encoded.contains(&nul_escape()));
    }

    #[test]
    fn real_nul_is_stripped_from_nested_values_and_keys() {
        let value = serde_json::json!({
            "outer": { "in\0ner": ["ok", "x\0y"] }
        });

        let encoded = to_jsonb_string(&value).unwrap();
        let decoded: Value = serde_json::from_str(&encoded).unwrap();

        assert_eq!(
            decoded,
            serde_json::json!({ "outer": { "inner": ["ok", "xy"] } })
        );
    }

    fn affected_pointers(mut value: Value) -> Vec<String> {
        let mut affected = Vec::new();
        strip_nul(&mut value, &mut String::new(), &mut affected);
        affected
    }

    #[test]
    fn every_affected_node_is_reported_by_pointer() {
        let affected = affected_pointers(serde_json::json!({
            "a": "x\0y",
            "b": ["ok", "z\0"],
            "c": { "k\0": "clean" }
        }));

        assert_eq!(affected, vec!["/a", "/b/1", "/c/k\0"]);
    }

    #[test]
    fn clean_values_report_nothing() {
        let affected = affected_pointers(serde_json::json!({
            "a": "xy",
            "b": content_with_escaped_literal()
        }));

        assert!(affected.is_empty(), "unexpected reports: {affected:?}");
    }

    /// The process-wide counter is monotonic; other tests share it, so this
    /// only asserts that a sanitization advances it.
    #[test]
    fn sanitization_advances_the_counter() {
        let before = nul_sanitizations_total();

        to_jsonb_string(&serde_json::json!({ "a": "x\0y", "b": "z\0" })).unwrap();

        assert!(nul_sanitizations_total() >= before + 2);
    }
}
