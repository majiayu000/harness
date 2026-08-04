use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
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
    /// Hashes of the values covered by `entries` and `legacy_entries`.
    ///
    /// The durable store validates these before every instance write. This
    /// makes an in-memory `workflow.data` mutation that bypasses the
    /// provenance-bearing API fail closed even when it overwrites an already
    /// classified pointer.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub value_digests: BTreeMap<String, String>,
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
            value_digests: BTreeMap::new(),
        }
    }

    pub(crate) fn migrated_from_persisted_data(data: &Value) -> anyhow::Result<Self> {
        let mut provenance = Self::new();
        match data {
            Value::Object(object) => {
                provenance
                    .legacy_entries
                    .extend(object.keys().map(|key| workflow_data_pointer("", key)));
            }
            // Any non-object document is grandfathered whole, including the
            // `null` produced by a pre-sidecar row that omitted `data`
            // entirely. Recording nothing for `null` would leave the root
            // uncovered, so the row would fail every coverage check and could
            // never be dispatched, written, or repaired after upgrade.
            _ => {
                provenance.legacy_entries.insert(String::new());
            }
        }
        provenance.refresh_value_digests(data)?;
        Ok(provenance)
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
        self.value_digests
            .retain(|entry, _| entry != &pointer && !entry.starts_with(&descendant_prefix));
        self.entries.insert(pointer.clone(), provenance);
    }

    /// Copy every classification covering `pointer` into `target`.
    ///
    /// Used when a writer replaces the whole `workflow.data` document but
    /// leaves some fields byte-identical. Those fields were not authored by
    /// this write, so their existing classification — including a
    /// grandfathered legacy marker — must survive rather than being reassigned
    /// by the new writer's field mapping.
    ///
    /// Returns `false` when this sidecar has no coverage for the pointer at
    /// all, in which case the caller must classify it as a fresh write.
    pub(crate) fn carry_over_coverage_into(&self, pointer: &str, target: &mut Self) -> bool {
        let descendant_prefix = if pointer.is_empty() {
            "/".to_string()
        } else {
            format!("{pointer}/")
        };
        let mut carried = false;
        for (entry, provenance) in &self.entries {
            if entry == pointer || entry.starts_with(&descendant_prefix) {
                target.entries.insert(entry.clone(), *provenance);
                carried = true;
            }
        }
        for entry in &self.legacy_entries {
            if entry == pointer || entry.starts_with(&descendant_prefix) {
                target.legacy_entries.insert(entry.clone());
                carried = true;
            }
        }
        if carried {
            return true;
        }
        // No exact or descendant entry, so an ancestor covers this field.
        // Legacy is checked first: when an ancestor is grandfathered, keeping
        // the field fenced is the conservative reading.
        if self.is_legacy(pointer) {
            target.legacy_entries.insert(pointer.to_string());
            return true;
        }
        if let Some(provenance) = self.provenance_for(pointer) {
            target.entries.insert(pointer.to_string(), provenance);
            return true;
        }
        false
    }

    pub(crate) fn remove_classification(&mut self, pointer: &str) {
        let descendant_prefix = if pointer.is_empty() {
            "/".to_string()
        } else {
            format!("{pointer}/")
        };
        self.entries
            .retain(|entry, _| entry != pointer && !entry.starts_with(&descendant_prefix));
        self.legacy_entries
            .retain(|entry| entry != pointer && !entry.starts_with(&descendant_prefix));
        self.value_digests
            .retain(|entry, _| entry != pointer && !entry.starts_with(&descendant_prefix));
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

    pub(crate) fn refresh_value_digests(&mut self, data: &Value) -> anyhow::Result<()> {
        self.value_digests.clear();
        for pointer in self
            .entries
            .keys()
            .chain(self.legacy_entries.iter())
            .cloned()
            .collect::<Vec<_>>()
        {
            let value = data.pointer(&pointer).ok_or_else(|| {
                anyhow::anyhow!("workflow.data provenance pointer `{pointer}` does not exist")
            })?;
            self.value_digests
                .insert(pointer, workflow_data_digest(value)?);
        }
        Ok(())
    }

    pub(crate) fn validate_persisted_data(&self, data: &Value) -> anyhow::Result<()> {
        if self.schema != WORKFLOW_DATA_PROVENANCE_SCHEMA {
            anyhow::bail!(
                "workflow.data provenance schema `{}` is not supported",
                self.schema
            );
        }
        for pointer in self.entries.keys().chain(self.legacy_entries.iter()) {
            let value = data.pointer(pointer).ok_or_else(|| {
                anyhow::anyhow!("workflow.data provenance pointer `{pointer}` does not exist")
            })?;
            let expected = self.value_digests.get(pointer).ok_or_else(|| {
                anyhow::anyhow!(
                    "workflow.data provenance pointer `{pointer}` has no durable value digest"
                )
            })?;
            let actual = workflow_data_digest(value)?;
            if actual != *expected {
                anyhow::bail!(
                    "workflow.data pointer `{pointer}` changed outside the classified write API"
                );
            }
        }
        validate_value_coverage("", data, self)
    }
}

fn validate_value_coverage(
    pointer: &str,
    value: &Value,
    provenance: &WorkflowDataProvenance,
) -> anyhow::Result<()> {
    let covered = provenance.provenance_for(pointer).is_some() || provenance.is_legacy(pointer);
    if covered && !provenance.has_descendant_entry(pointer) {
        return Ok(());
    }
    if pointer.is_empty() && value.as_object().is_some_and(serde_json::Map::is_empty) {
        return Ok(());
    }
    if covered || provenance.has_descendant_entry(pointer) {
        match value {
            Value::Object(object) => {
                for (key, child) in object {
                    validate_value_coverage(
                        &workflow_data_pointer(pointer, key),
                        child,
                        provenance,
                    )?;
                }
                return Ok(());
            }
            Value::Array(items) => {
                for (index, child) in items.iter().enumerate() {
                    validate_value_coverage(
                        &workflow_data_pointer(pointer, &index.to_string()),
                        child,
                        provenance,
                    )?;
                }
                return Ok(());
            }
            _ if covered => return Ok(()),
            _ => {}
        }
    }
    anyhow::bail!("unclassified workflow.data field `{pointer}`")
}

fn workflow_data_digest(value: &Value) -> anyhow::Result<String> {
    let encoded = serde_json::to_vec(value)?;
    Ok(format!("sha256:{:x}", Sha256::digest(encoded)))
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
        let mut provenance = WorkflowDataProvenance::migrated_from_persisted_data(
            &serde_json::json!({"snapshot": {"body": "old"}}),
        )
        .expect("migration boundary");
        provenance.classify("/snapshot/head_oid", DataProvenance::Server);
        provenance
            .refresh_value_digests(&serde_json::json!({
                "snapshot": {"body": "old", "head_oid": "abc"}
            }))
            .expect("digest refresh");

        assert!(provenance.is_legacy("/snapshot/body"));
        assert_eq!(
            provenance.provenance_for("/snapshot/head_oid"),
            Some(DataProvenance::Server)
        );
        assert!(provenance.has_descendant_entry("/snapshot"));
        assert!(provenance.migrated_at.is_some());
    }
}
