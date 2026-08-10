use super::{
    AgentStackComponent, AgentStackComponentError, AgentStackComponentParseError,
    AgentStackEntryClass, AgentStackInventory, AgentStackObservationClass,
    AgentStackSelectionState, AgentStackSourceScope, AgentStackTrustLevel,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;

pub const AGENT_STACK_SNAPSHOT_SCHEMA_VERSION: &str = "agent-stack-snapshot/v0.1";
pub const AGENT_STACK_DIFF_SCHEMA_VERSION: &str = "agent-stack-diff/v0.1";

const REPOSITORY_OBSERVATION_BOUNDARIES: &[&str] = &[
    "collects repository-observed Agent Stack components selected by the shared inventory rules",
    "does not collect runtime, runner, user-global, admin, or system observations",
    "does not classify repository inventory as runtime-observed or execute external tools",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStackSnapshotScope {
    Repository,
}

impl AgentStackSnapshotScope {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Repository => "repository",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentStackSnapshotEntry {
    component: AgentStackComponent,
    entry_class: AgentStackEntryClass,
}

impl AgentStackSnapshotEntry {
    pub fn new(component: AgentStackComponent, entry_class: AgentStackEntryClass) -> Self {
        Self {
            component,
            entry_class,
        }
    }

    pub fn component(&self) -> &AgentStackComponent {
        &self.component
    }

    pub fn entry_class(&self) -> &AgentStackEntryClass {
        &self.entry_class
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentStackSnapshot {
    schema_version: String,
    scope: AgentStackSnapshotScope,
    observation_boundaries: Vec<String>,
    components: Vec<AgentStackSnapshotEntry>,
}

impl AgentStackSnapshot {
    pub fn repository(
        components: Vec<AgentStackSnapshotEntry>,
    ) -> Result<Self, AgentStackSnapshotError> {
        let mut snapshot = Self {
            schema_version: AGENT_STACK_SNAPSHOT_SCHEMA_VERSION.to_owned(),
            scope: AgentStackSnapshotScope::Repository,
            observation_boundaries: repository_observation_boundaries(),
            components,
        };
        snapshot.sort_components();
        snapshot.validate()?;
        Ok(snapshot)
    }

    pub fn from_inventory(
        inventory: &AgentStackInventory,
    ) -> Result<Self, AgentStackSnapshotError> {
        let components = inventory
            .entries()
            .iter()
            .map(|entry| {
                AgentStackSnapshotEntry::new(entry.component().clone(), entry.entry_class().clone())
            })
            .collect();
        Self::repository(components)
    }

    pub fn from_json(value: &str) -> Result<Self, AgentStackSnapshotParseError> {
        let envelope: SnapshotVersionEnvelope =
            serde_json::from_str(value).map_err(AgentStackSnapshotParseError::Syntax)?;
        if envelope.schema_version.as_deref() != Some(AGENT_STACK_SNAPSHOT_SCHEMA_VERSION) {
            return Err(AgentStackSnapshotError::UnsupportedSchemaVersion.into());
        }

        let wire: WireSnapshot =
            serde_json::from_str(value).map_err(AgentStackSnapshotParseError::Syntax)?;
        let mut components = Vec::with_capacity(wire.components.len());
        for wire_entry in wire.components {
            let component_json = serde_json::to_string(&wire_entry.component)
                .map_err(AgentStackSnapshotParseError::Syntax)?;
            let component = AgentStackComponent::from_json(&component_json).map_err(|error| {
                AgentStackSnapshotParseError::Validation(match error {
                    AgentStackComponentParseError::Syntax(_) => {
                        AgentStackSnapshotError::InvalidComponentShape
                    }
                    AgentStackComponentParseError::Validation(error) => {
                        AgentStackSnapshotError::InvalidComponent(error)
                    }
                })
            })?;
            components.push(AgentStackSnapshotEntry::new(
                component,
                wire_entry.entry_class,
            ));
        }
        let mut snapshot = Self {
            schema_version: wire.schema_version,
            scope: wire.scope,
            observation_boundaries: wire.observation_boundaries,
            components,
        };
        snapshot.sort_components();
        snapshot.validate()?;
        Ok(snapshot)
    }

    pub fn validate(&self) -> Result<(), AgentStackSnapshotError> {
        if self.schema_version != AGENT_STACK_SNAPSHOT_SCHEMA_VERSION {
            return Err(AgentStackSnapshotError::UnsupportedSchemaVersion);
        }
        if self.observation_boundaries.is_empty() {
            return Err(AgentStackSnapshotError::MissingObservationBoundaries);
        }

        let mut seen = BTreeSet::new();
        for entry in &self.components {
            entry
                .component
                .validate()
                .map_err(AgentStackSnapshotError::InvalidComponent)?;
            if !matches!(self.scope, AgentStackSnapshotScope::Repository)
                || entry.component.source().scope() != AgentStackSourceScope::Repository
                || entry.component.observation_class()
                    != AgentStackObservationClass::RepositoryObserved
                || entry.component.selection_state() != AgentStackSelectionState::Discovered
                || entry.component.trust_level() != AgentStackTrustLevel::RepositoryObserved
            {
                return Err(AgentStackSnapshotError::ComponentOutsideScope {
                    component_id: entry.component.component_id().as_str().to_owned(),
                });
            }
            let component_id = entry.component.component_id().as_str().to_owned();
            if !seen.insert(component_id.clone()) {
                return Err(AgentStackSnapshotError::DuplicateComponentId { component_id });
            }
            match entry.entry_class {
                AgentStackEntryClass::RegularFile { .. } => {
                    if entry.component.integrity().is_none() {
                        return Err(AgentStackSnapshotError::MissingRegularFileIntegrity {
                            component_id,
                        });
                    }
                }
                AgentStackEntryClass::DirectoryPresence => {
                    if entry.component.integrity().is_some() {
                        return Err(AgentStackSnapshotError::DirectoryIntegrityPresent {
                            component_id,
                        });
                    }
                }
            }
        }
        Ok(())
    }

    pub fn schema_version(&self) -> &str {
        &self.schema_version
    }

    pub const fn scope(&self) -> AgentStackSnapshotScope {
        self.scope
    }

    pub fn observation_boundaries(&self) -> &[String] {
        &self.observation_boundaries
    }

    pub fn components(&self) -> &[AgentStackSnapshotEntry] {
        &self.components
    }

    fn sort_components(&mut self) {
        self.components.sort_by(|left, right| {
            left.component
                .component_id()
                .as_str()
                .cmp(right.component.component_id().as_str())
        });
    }
}

#[derive(Debug, Error)]
pub enum AgentStackSnapshotParseError {
    #[error("the Agent Stack snapshot JSON has invalid syntax or shape")]
    Syntax(#[source] serde_json::Error),
    #[error(transparent)]
    Validation(#[from] AgentStackSnapshotError),
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum AgentStackSnapshotError {
    #[error("the Agent Stack snapshot schema version is unsupported")]
    UnsupportedSchemaVersion,
    #[error("the Agent Stack snapshot observation boundaries are missing")]
    MissingObservationBoundaries,
    #[error("the Agent Stack snapshot component has invalid JSON shape")]
    InvalidComponentShape,
    #[error("the Agent Stack snapshot component is invalid")]
    InvalidComponent(#[source] AgentStackComponentError),
    #[error("the Agent Stack snapshot component is outside the snapshot scope: {component_id}")]
    ComponentOutsideScope { component_id: String },
    #[error("the Agent Stack snapshot contains a duplicate component: {component_id}")]
    DuplicateComponentId { component_id: String },
    #[error("the Agent Stack snapshot regular-file component has no integrity: {component_id}")]
    MissingRegularFileIntegrity { component_id: String },
    #[error("the Agent Stack snapshot directory-presence component has integrity: {component_id}")]
    DirectoryIntegrityPresent { component_id: String },
}

#[derive(Deserialize)]
struct SnapshotVersionEnvelope {
    schema_version: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireSnapshot {
    schema_version: String,
    scope: AgentStackSnapshotScope,
    observation_boundaries: Vec<String>,
    components: Vec<WireSnapshotEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireSnapshotEntry {
    component: serde_json::Value,
    entry_class: AgentStackEntryClass,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStackSnapshotChangeKind {
    Added,
    Removed,
    Modified,
}

impl AgentStackSnapshotChangeKind {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Removed => "removed",
            Self::Modified => "modified",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStackSnapshotChangedField {
    Kind,
    Source,
    ObservationClass,
    SelectionState,
    Integrity,
    Capabilities,
    TrustLevel,
    Freshness,
    EntryClass,
}

impl AgentStackSnapshotChangedField {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Kind => "kind",
            Self::Source => "source",
            Self::ObservationClass => "observation_class",
            Self::SelectionState => "selection_state",
            Self::Integrity => "integrity",
            Self::Capabilities => "capabilities",
            Self::TrustLevel => "trust_level",
            Self::Freshness => "freshness",
            Self::EntryClass => "entry_class",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentStackSnapshotChange {
    kind: AgentStackSnapshotChangeKind,
    component_id: String,
    changed_fields: Vec<AgentStackSnapshotChangedField>,
    before: Option<AgentStackSnapshotEntry>,
    after: Option<AgentStackSnapshotEntry>,
}

impl AgentStackSnapshotChange {
    fn added(after: AgentStackSnapshotEntry) -> Self {
        Self {
            kind: AgentStackSnapshotChangeKind::Added,
            component_id: after.component.component_id().as_str().to_owned(),
            changed_fields: Vec::new(),
            before: None,
            after: Some(after),
        }
    }

    fn removed(before: AgentStackSnapshotEntry) -> Self {
        Self {
            kind: AgentStackSnapshotChangeKind::Removed,
            component_id: before.component.component_id().as_str().to_owned(),
            changed_fields: Vec::new(),
            before: Some(before),
            after: None,
        }
    }

    fn modified(
        before: AgentStackSnapshotEntry,
        after: AgentStackSnapshotEntry,
        changed_fields: Vec<AgentStackSnapshotChangedField>,
    ) -> Self {
        Self {
            kind: AgentStackSnapshotChangeKind::Modified,
            component_id: before.component.component_id().as_str().to_owned(),
            changed_fields,
            before: Some(before),
            after: Some(after),
        }
    }

    pub const fn kind(&self) -> AgentStackSnapshotChangeKind {
        self.kind
    }

    pub fn component_id(&self) -> &str {
        &self.component_id
    }

    pub fn changed_fields(&self) -> &[AgentStackSnapshotChangedField] {
        &self.changed_fields
    }

    pub fn before(&self) -> Option<&AgentStackSnapshotEntry> {
        self.before.as_ref()
    }

    pub fn after(&self) -> Option<&AgentStackSnapshotEntry> {
        self.after.as_ref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentStackSnapshotDiff {
    schema_version: String,
    scope: AgentStackSnapshotScope,
    baseline_schema_version: String,
    candidate_schema_version: String,
    counts: AgentStackSnapshotDiffCounts,
    changes: Vec<AgentStackSnapshotChange>,
}

impl AgentStackSnapshotDiff {
    pub fn schema_version(&self) -> &str {
        &self.schema_version
    }

    pub const fn scope(&self) -> AgentStackSnapshotScope {
        self.scope
    }

    pub const fn counts(&self) -> &AgentStackSnapshotDiffCounts {
        &self.counts
    }

    pub fn changes(&self) -> &[AgentStackSnapshotChange] {
        &self.changes
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentStackSnapshotDiffCounts {
    added: usize,
    removed: usize,
    modified: usize,
    unchanged: usize,
}

impl AgentStackSnapshotDiffCounts {
    pub const fn added(&self) -> usize {
        self.added
    }

    pub const fn removed(&self) -> usize {
        self.removed
    }

    pub const fn modified(&self) -> usize {
        self.modified
    }

    pub const fn unchanged(&self) -> usize {
        self.unchanged
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum AgentStackSnapshotDiffError {
    #[error(
        "cannot diff Agent Stack snapshots with different schema versions: baseline={baseline}, candidate={candidate}"
    )]
    IncompatibleSchemaVersion { baseline: String, candidate: String },
    #[error(
        "cannot diff Agent Stack snapshots with different scopes: baseline={baseline}, candidate={candidate}"
    )]
    IncompatibleScope { baseline: String, candidate: String },
}

pub fn diff_agent_stack_snapshots(
    baseline: &AgentStackSnapshot,
    candidate: &AgentStackSnapshot,
) -> Result<AgentStackSnapshotDiff, AgentStackSnapshotDiffError> {
    if baseline.schema_version != candidate.schema_version {
        return Err(AgentStackSnapshotDiffError::IncompatibleSchemaVersion {
            baseline: baseline.schema_version.clone(),
            candidate: candidate.schema_version.clone(),
        });
    }
    if baseline.scope != candidate.scope {
        return Err(AgentStackSnapshotDiffError::IncompatibleScope {
            baseline: baseline.scope.as_str().to_owned(),
            candidate: candidate.scope.as_str().to_owned(),
        });
    }

    let before = entries_by_component_id(baseline.components());
    let after = entries_by_component_id(candidate.components());
    let mut changes = Vec::new();
    let mut unchanged = 0;

    for (component_id, before_entry) in &before {
        match after.get(component_id) {
            Some(after_entry) => {
                let changed_fields = changed_fields(before_entry, after_entry);
                if changed_fields.is_empty() {
                    unchanged += 1;
                } else {
                    changes.push(AgentStackSnapshotChange::modified(
                        (*before_entry).clone(),
                        (*after_entry).clone(),
                        changed_fields,
                    ));
                }
            }
            None => changes.push(AgentStackSnapshotChange::removed((*before_entry).clone())),
        }
    }
    for (component_id, after_entry) in &after {
        if !before.contains_key(component_id) {
            changes.push(AgentStackSnapshotChange::added((*after_entry).clone()));
        }
    }

    changes.sort_by(|left, right| {
        left.component_id
            .cmp(&right.component_id)
            .then_with(|| left.kind.as_str().cmp(right.kind.as_str()))
    });
    let counts = AgentStackSnapshotDiffCounts {
        added: changes
            .iter()
            .filter(|change| change.kind == AgentStackSnapshotChangeKind::Added)
            .count(),
        removed: changes
            .iter()
            .filter(|change| change.kind == AgentStackSnapshotChangeKind::Removed)
            .count(),
        modified: changes
            .iter()
            .filter(|change| change.kind == AgentStackSnapshotChangeKind::Modified)
            .count(),
        unchanged,
    };
    Ok(AgentStackSnapshotDiff {
        schema_version: AGENT_STACK_DIFF_SCHEMA_VERSION.to_owned(),
        scope: baseline.scope,
        baseline_schema_version: baseline.schema_version.clone(),
        candidate_schema_version: candidate.schema_version.clone(),
        counts,
        changes,
    })
}

fn repository_observation_boundaries() -> Vec<String> {
    REPOSITORY_OBSERVATION_BOUNDARIES
        .iter()
        .map(|value| (*value).to_owned())
        .collect()
}

fn entries_by_component_id(
    entries: &[AgentStackSnapshotEntry],
) -> BTreeMap<&str, &AgentStackSnapshotEntry> {
    entries
        .iter()
        .map(|entry| (entry.component.component_id().as_str(), entry))
        .collect()
}

fn changed_fields(
    before: &AgentStackSnapshotEntry,
    after: &AgentStackSnapshotEntry,
) -> Vec<AgentStackSnapshotChangedField> {
    let mut fields = Vec::new();
    if before.component.kind() != after.component.kind() {
        fields.push(AgentStackSnapshotChangedField::Kind);
    }
    if before.component.source() != after.component.source() {
        fields.push(AgentStackSnapshotChangedField::Source);
    }
    if before.component.observation_class() != after.component.observation_class() {
        fields.push(AgentStackSnapshotChangedField::ObservationClass);
    }
    if before.component.selection_state() != after.component.selection_state() {
        fields.push(AgentStackSnapshotChangedField::SelectionState);
    }
    if before.component.integrity() != after.component.integrity() {
        fields.push(AgentStackSnapshotChangedField::Integrity);
    }
    if before.component.capabilities() != after.component.capabilities() {
        fields.push(AgentStackSnapshotChangedField::Capabilities);
    }
    if before.component.trust_level() != after.component.trust_level() {
        fields.push(AgentStackSnapshotChangedField::TrustLevel);
    }
    if before.component.freshness() != after.component.freshness() {
        fields.push(AgentStackSnapshotChangedField::Freshness);
    }
    if before.entry_class != after.entry_class {
        fields.push(AgentStackSnapshotChangedField::EntryClass);
    }
    fields
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stack::{
        inventory_repository_stack, AgentStackCapability, AgentStackComponentKind,
        AgentStackFreshness, AgentStackInventoryOptions, AgentStackSource, Sha256Digest,
    };
    use std::fs;

    const HASH_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const HASH_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn repository_component(
        locator: &str,
        kind: AgentStackComponentKind,
        integrity: Option<&str>,
    ) -> AgentStackComponent {
        AgentStackComponent::new(
            kind,
            AgentStackSource::new(AgentStackSourceScope::Repository, locator).unwrap(),
            AgentStackObservationClass::RepositoryObserved,
            AgentStackSelectionState::Discovered,
            AgentStackTrustLevel::RepositoryObserved,
            AgentStackFreshness::Fresh,
        )
        .unwrap()
        .with_integrity(integrity.map(|value| Sha256Digest::parse(value).unwrap()))
    }

    fn file_entry(locator: &str, hash: &str) -> AgentStackSnapshotEntry {
        AgentStackSnapshotEntry::new(
            repository_component(locator, AgentStackComponentKind::Instructions, Some(hash)),
            AgentStackEntryClass::RegularFile {
                unix_executable: Some(false),
            },
        )
    }

    #[test]
    fn stack_snapshot_from_inventory_serializes_repository_boundaries() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        fs::write(tempdir.path().join("AGENTS.md"), "instructions").expect("write AGENTS.md");
        let inventory = inventory_repository_stack(&AgentStackInventoryOptions::new(
            tempdir.path().to_path_buf(),
        ))
        .expect("inventory should collect AGENTS.md");

        let snapshot = AgentStackSnapshot::from_inventory(&inventory)
            .expect("inventory snapshot should validate");
        let json = serde_json::to_string(&snapshot).expect("snapshot should serialize");
        let decoded = AgentStackSnapshot::from_json(&json).expect("snapshot should parse");

        assert_eq!(decoded.scope(), AgentStackSnapshotScope::Repository);
        assert!(decoded
            .observation_boundaries()
            .iter()
            .any(|boundary| boundary.contains("does not collect runtime")));
        assert!(decoded.components().iter().any(|entry| entry
            .component()
            .source()
            .locator()
            .as_str()
            == "AGENTS.md"));
    }

    #[test]
    fn stack_snapshot_rejects_incomplete_duplicate_and_cross_scope_entries() {
        let snapshot = AgentStackSnapshot::repository(vec![file_entry("AGENTS.md", HASH_A)])
            .expect("snapshot");
        let mut value = serde_json::to_value(&snapshot).expect("snapshot value");
        value["components"][0]["component"]
            .as_object_mut()
            .unwrap()
            .remove("integrity");

        assert!(matches!(
            AgentStackSnapshot::from_json(&value.to_string()),
            Err(AgentStackSnapshotParseError::Validation(
                AgentStackSnapshotError::MissingRegularFileIntegrity { .. }
            ))
        ));

        let duplicate = AgentStackSnapshot::repository(vec![
            file_entry("AGENTS.md", HASH_A),
            file_entry("AGENTS.md", HASH_A),
        ]);
        assert!(matches!(
            duplicate,
            Err(AgentStackSnapshotError::DuplicateComponentId { .. })
        ));

        let runtime_component = AgentStackComponent::new(
            AgentStackComponentKind::AgentRuntime,
            AgentStackSource::logical(AgentStackSourceScope::Runtime, "codex", "cli").unwrap(),
            AgentStackObservationClass::RuntimeObserved,
            AgentStackSelectionState::Loaded,
            AgentStackTrustLevel::RuntimeObserved,
            AgentStackFreshness::Fresh,
        )
        .unwrap()
        .with_integrity(Some(Sha256Digest::parse(HASH_A).unwrap()));
        let cross_scope = AgentStackSnapshot::repository(vec![AgentStackSnapshotEntry::new(
            runtime_component,
            AgentStackEntryClass::RegularFile {
                unix_executable: None,
            },
        )]);
        assert!(matches!(
            cross_scope,
            Err(AgentStackSnapshotError::ComponentOutsideScope { .. })
        ));
    }

    #[test]
    fn stack_snapshot_diff_reports_changes_in_stable_order() {
        let unchanged = file_entry("AGENTS.md", HASH_A);
        let mut modified = file_entry("WORKFLOW.md", HASH_A);
        modified.component = modified
            .component
            .clone()
            .with_integrity(Some(Sha256Digest::parse(HASH_B).unwrap()))
            .with_capabilities([AgentStackCapability::Shell])
            .unwrap();

        let before = AgentStackSnapshot::repository(vec![
            unchanged.clone(),
            file_entry("old/SKILL.md", HASH_A),
            file_entry("WORKFLOW.md", HASH_A),
        ])
        .expect("before snapshot");
        let after = AgentStackSnapshot::repository(vec![
            unchanged,
            file_entry("new/SKILL.md", HASH_A),
            modified,
        ])
        .expect("after snapshot");

        let diff = diff_agent_stack_snapshots(&before, &after).expect("diff");

        assert_eq!(diff.counts().added(), 1);
        assert_eq!(diff.counts().removed(), 1);
        assert_eq!(diff.counts().modified(), 1);
        assert_eq!(diff.counts().unchanged(), 1);
        assert_eq!(
            diff.changes()
                .iter()
                .map(|change| (change.kind(), change.component_id().to_owned()))
                .collect::<Vec<_>>(),
            vec![
                (
                    AgentStackSnapshotChangeKind::Modified,
                    "repository:instructions:WORKFLOW.md".to_owned()
                ),
                (
                    AgentStackSnapshotChangeKind::Added,
                    "repository:instructions:new/SKILL.md".to_owned()
                ),
                (
                    AgentStackSnapshotChangeKind::Removed,
                    "repository:instructions:old/SKILL.md".to_owned()
                ),
            ]
        );
        assert_eq!(
            diff.changes()[0].changed_fields(),
            &[
                AgentStackSnapshotChangedField::Integrity,
                AgentStackSnapshotChangedField::Capabilities
            ]
        );
    }
}
