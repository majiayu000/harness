use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

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
            entries: BTreeMap::new(),
        }
    }

    pub fn with_entry(mut self, pointer: impl Into<String>, provenance: DataProvenance) -> Self {
        self.entries.insert(pointer.into(), provenance);
        self
    }

    pub fn provenance_for(&self, pointer: &str) -> Option<DataProvenance> {
        nearest_entry(pointer).find_map(|candidate| self.entries.get(candidate).copied())
    }

    pub fn has_descendant_entry(&self, pointer: &str) -> bool {
        let prefix = if pointer.is_empty() {
            "/".to_string()
        } else {
            format!("{pointer}/")
        };
        self.entries.keys().any(|entry| entry.starts_with(&prefix))
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
}
