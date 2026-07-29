use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

pub const WORKFLOW_DATA_PROVENANCE_SCHEMA: &str = "harness.workflow.data_provenance.v1";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DataProvenance {
    Server,
    Agent,
    External,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkflowDataProvenance {
    #[serde(default = "default_schema")]
    pub schema: String,
    /// Durable boundary between grandfathered rows and provenance-aware writes.
    ///
    /// `None` is accepted only for sidecars written by the pre-migration PR
    /// implementation. New sidecars always persist this timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub migrated_at: Option<DateTime<Utc>>,
    /// JSON-pointer roots that existed when provenance tracking was introduced.
    ///
    /// They remain fenced as legacy data after later classified writes. This
    /// prevents the first post-deployment write from turning untouched history
    /// into an apparent writer defect.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub legacy_entries: BTreeSet<String>,
    #[serde(default)]
    pub entries: BTreeMap<String, DataProvenance>,
}

impl Default for WorkflowDataProvenance {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkflowDataProvenance {
    pub fn new() -> Self {
        Self {
            schema: WORKFLOW_DATA_PROVENANCE_SCHEMA.to_string(),
            migrated_at: Some(Utc::now()),
            legacy_entries: BTreeSet::new(),
            entries: BTreeMap::new(),
        }
    }

    pub fn migrated_from(data: &Value) -> Self {
        let mut provenance = Self::new();
        match data {
            Value::Object(object) => {
                provenance
                    .legacy_entries
                    .extend(object.keys().map(|key| workflow_data_pointer("", key)));
            }
            value if !value.is_null() => {
                provenance.legacy_entries.insert(String::new());
            }
            _ => {}
        }
        provenance
    }

    pub fn with_entry(mut self, pointer: impl Into<String>, provenance: DataProvenance) -> Self {
        self.classify(pointer, provenance);
        self
    }

    pub fn classify(&mut self, pointer: impl Into<String>, provenance: DataProvenance) {
        let pointer = pointer.into();
        let descendant_prefix = if pointer.is_empty() {
            "/".to_string()
        } else {
            format!("{pointer}/")
        };
        self.entries
            .retain(|entry, _| !entry.starts_with(&descendant_prefix));
        self.legacy_entries
            .retain(|entry| entry != &pointer && !entry.starts_with(&descendant_prefix));
        self.entries.insert(pointer.clone(), provenance);
    }

    pub fn provenance_for(&self, pointer: &str) -> Option<DataProvenance> {
        nearest_entry(pointer).find_map(|candidate| self.entries.get(candidate).copied())
    }

    pub fn is_legacy(&self, pointer: &str) -> bool {
        nearest_entry(pointer).any(|candidate| self.legacy_entries.contains(candidate))
    }

    pub fn has_descendant_entry(&self, pointer: &str) -> bool {
        let prefix = if pointer.is_empty() {
            "/".to_string()
        } else {
            format!("{pointer}/")
        };
        self.entries.keys().any(|entry| entry.starts_with(&prefix))
            || self
                .legacy_entries
                .iter()
                .any(|entry| entry.starts_with(&prefix))
    }
}

fn default_schema() -> String {
    WORKFLOW_DATA_PROVENANCE_SCHEMA.to_string()
}

fn nearest_entry(pointer: &str) -> impl Iterator<Item = &str> {
    let mut candidates = Vec::new();
    let mut current = pointer;
    loop {
        candidates.push(current);
        let Some((parent, _)) = current.rsplit_once('/') else {
            break;
        };
        current = parent;
    }
    candidates.into_iter()
}

pub fn workflow_data_pointer(parent: &str, key: &str) -> String {
    let escaped = key.replace('~', "~0").replace('/', "~1");
    if parent.is_empty() {
        format!("/{escaped}")
    } else {
        format!("{parent}/{escaped}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provenance_lookup_uses_exact_field_before_ancestor() {
        let provenance = WorkflowDataProvenance::new()
            .with_entry("/continuation", DataProvenance::Agent)
            .with_entry("/mixed/server_fact", DataProvenance::Server);

        assert_eq!(
            provenance.provenance_for("/continuation/last_summary"),
            Some(DataProvenance::Agent)
        );
        assert_eq!(
            provenance.provenance_for("/mixed/server_fact"),
            Some(DataProvenance::Server)
        );
        assert_eq!(provenance.provenance_for("/mixed"), None);
        assert!(provenance.has_descendant_entry("/mixed"));
    }

    #[test]
    fn migration_boundary_survives_more_specific_classified_writes() {
        let mut provenance = WorkflowDataProvenance::migrated_from(
            &serde_json::json!({"snapshot": {"body": "old"}}),
        );
        provenance.classify("/snapshot/head_oid", DataProvenance::Server);

        assert!(provenance.is_legacy("/snapshot/body"));
        assert_eq!(
            provenance.provenance_for("/snapshot/head_oid"),
            Some(DataProvenance::Server)
        );
        assert!(provenance.has_descendant_entry("/snapshot"));
        assert!(provenance.migrated_at.is_some());
    }
}
