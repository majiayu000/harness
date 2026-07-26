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

static NUL_CHARACTERS_REMOVED: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct SanitizationStats {
    nul_characters_removed: u64,
    affected_nodes: u64,
}

impl SanitizationStats {
    fn record_node(&mut self, nul_characters_removed: u64) {
        self.nul_characters_removed = self
            .nul_characters_removed
            .saturating_add(nul_characters_removed);
        self.affected_nodes = self.affected_nodes.saturating_add(1);
    }
}

/// Total number of NUL characters removed since process start.
#[cfg(test)]
pub(crate) fn nul_characters_removed_total() -> u64 {
    NUL_CHARACTERS_REMOVED.load(Ordering::Relaxed)
}

fn record_nul_characters_removed(count: u64) -> u64 {
    match NUL_CHARACTERS_REMOVED.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_add(count))
    }) {
        Ok(previous) => previous.saturating_add(count),
        Err(current) => current,
    }
}

/// Serialize `value` as a JSON string safe to bind to a Postgres `jsonb` column.
///
/// NUL characters are removed from string contents and object keys. Removal is
/// data loss, so it is counted and logged at error level. Logs contain only the
/// serialized type and scalar counts, never values, object keys, or paths.
pub(crate) fn to_jsonb_string<T>(value: &T) -> anyhow::Result<String>
where
    T: Serialize + ?Sized,
{
    let mut json = serde_json::to_value(value)?;
    let stats = strip_nul(&mut json)?;

    if stats.nul_characters_removed > 0 {
        let total_nul_characters_removed =
            record_nul_characters_removed(stats.nul_characters_removed);
        tracing::error!(
            entity_type = std::any::type_name::<T>(),
            nul_characters_removed = stats.nul_characters_removed,
            affected_nodes = stats.affected_nodes,
            total_nul_characters_removed,
            "stripped NUL characters before jsonb write; stored value differs from the input"
        );
    }

    Ok(serde_json::to_string(&json)?)
}

/// Remove NUL characters in place and return bounded scalar statistics.
fn strip_nul(value: &mut Value) -> anyhow::Result<SanitizationStats> {
    let mut stats = SanitizationStats::default();
    strip_nul_into(value, &mut stats)?;
    Ok(stats)
}

fn strip_nul_into(value: &mut Value, stats: &mut SanitizationStats) -> anyhow::Result<()> {
    match value {
        Value::String(text) => {
            let removed = text.chars().filter(|ch| *ch == '\0').count() as u64;
            if removed > 0 {
                text.retain(|ch| ch != '\0');
                stats.record_node(removed);
            }
        }
        Value::Array(items) => {
            for item in items {
                strip_nul_into(item, stats)?;
            }
        }
        Value::Object(entries) => {
            if entries.keys().any(|key| key.contains('\0')) {
                let mut rekeyed = Map::with_capacity(entries.len());
                for (key, item) in std::mem::take(entries) {
                    let sanitized_key = key.replace('\0', "");
                    if rekeyed.contains_key(&sanitized_key) {
                        anyhow::bail!(
                            "refusing jsonb sanitization because object keys would collide"
                        );
                    }
                    let removed = key.chars().filter(|ch| *ch == '\0').count() as u64;
                    if removed > 0 {
                        stats.record_node(removed);
                    }
                    rekeyed.insert(sanitized_key, item);
                }
                *entries = rekeyed;
            }

            for item in entries.values_mut() {
                strip_nul_into(item, stats)?;
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
    Ok(())
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
    fn every_removed_nul_is_counted() {
        let stats = sanitization_stats(serde_json::json!({
            "summary": "a\0b\0c"
        }));

        assert_eq!(stats.nul_characters_removed, 2);
        assert_eq!(stats.affected_nodes, 1);
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

    #[test]
    fn colliding_sanitized_keys_fail_closed() {
        let value = serde_json::json!({
            "ab": "original",
            "a\0b": "would overwrite"
        });

        let error = to_jsonb_string(&value)
            .expect_err("sanitizing colliding object keys must not discard either value");

        assert!(
            error.to_string().contains("collide"),
            "collision error must explain the failure without revealing keys: {error}"
        );
        assert!(
            !error.to_string().contains("would overwrite"),
            "collision error must not reveal values: {error}"
        );
    }

    fn sanitization_stats(mut value: Value) -> SanitizationStats {
        match strip_nul(&mut value) {
            Ok(stats) => stats,
            Err(error) => panic!("test fixture must not contain colliding keys: {error}"),
        }
    }

    #[test]
    fn characters_and_affected_nodes_are_counted_separately() {
        let stats = sanitization_stats(serde_json::json!({
            "a": "x\0y",
            "b": ["ok", "z\0\0"],
            "c": { "k\0": "clean" }
        }));

        assert_eq!(stats.nul_characters_removed, 4);
        assert_eq!(stats.affected_nodes, 3);
    }

    #[test]
    fn clean_values_report_nothing() {
        let stats = sanitization_stats(serde_json::json!({
            "a": "xy",
            "b": content_with_escaped_literal()
        }));

        assert_eq!(stats, SanitizationStats::default());
    }

    /// The process-wide counter is monotonic; other tests share it, so this
    /// only asserts that a sanitization advances it.
    #[test]
    fn sanitization_advances_the_character_counter() {
        let before = nul_characters_removed_total();

        to_jsonb_string(&serde_json::json!({ "a": "x\0y", "b": "z\0" })).unwrap();

        assert!(nul_characters_removed_total() >= before + 2);
    }
}
