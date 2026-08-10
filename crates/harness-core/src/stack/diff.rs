use super::{
    AgentStackCapability, AgentStackComponent, AgentStackComponentError, AgentStackComponentKind,
    AgentStackFreshness, AgentStackObservationClass, AgentStackSelectionState,
    AgentStackSourceScope, AgentStackTrustLevel, AGENT_STACK_COMPONENT_SCHEMA_VERSION,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentStackDiffSnapshot<'a> {
    schema_version: &'a str,
    components: &'a [AgentStackComponent],
}

impl<'a> AgentStackDiffSnapshot<'a> {
    pub fn new(schema_version: &'a str, components: &'a [AgentStackComponent]) -> Self {
        Self {
            schema_version,
            components,
        }
    }

    pub fn from_components(components: &'a [AgentStackComponent]) -> Self {
        Self::new(AGENT_STACK_COMPONENT_SCHEMA_VERSION, components)
    }

    pub fn schema_version(&self) -> &str {
        self.schema_version
    }

    pub fn components(&self) -> &[AgentStackComponent] {
        self.components
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStackDiffSide {
    Before,
    After,
}

impl AgentStackDiffSide {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Before => "before",
            Self::After => "after",
        }
    }
}

impl fmt::Display for AgentStackDiffSide {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum AgentStackDiffError {
    #[error(
        "Agent Stack diff snapshot schema versions are incompatible: before={before_schema_version}, after={after_schema_version}"
    )]
    IncompatibleSchemaVersions {
        before_schema_version: String,
        after_schema_version: String,
    },
    #[error(
        "the {side} Agent Stack diff snapshot schema version is unsupported: {schema_version}"
    )]
    UnsupportedSchemaVersion {
        side: AgentStackDiffSide,
        schema_version: String,
    },
    #[error("the {side} Agent Stack diff snapshot contains an invalid component: {component_id}")]
    InvalidComponent {
        side: AgentStackDiffSide,
        component_id: String,
        #[source]
        source: AgentStackComponentError,
    },
    #[error(
        "the {side} Agent Stack diff snapshot contains duplicate component id: {component_id}"
    )]
    DuplicateComponentId {
        side: AgentStackDiffSide,
        component_id: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStackDiffChangeKind {
    Added,
    Removed,
    Modified,
}

impl AgentStackDiffChangeKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Removed => "removed",
            Self::Modified => "modified",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStackDiffFactKind {
    Runtime,
    Context,
    Capability,
    Trust,
    Freshness,
    Validation,
}

impl AgentStackDiffFactKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Runtime => "runtime",
            Self::Context => "context",
            Self::Capability => "capability",
            Self::Trust => "trust",
            Self::Freshness => "freshness",
            Self::Validation => "validation",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStackDiffField {
    Component,
    ObservationClass,
    SelectionState,
    Integrity,
    Capability,
    TrustLevel,
    Freshness,
}

impl AgentStackDiffField {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Component => "component",
            Self::ObservationClass => "observation_class",
            Self::SelectionState => "selection_state",
            Self::Integrity => "integrity",
            Self::Capability => "capability",
            Self::TrustLevel => "trust_level",
            Self::Freshness => "freshness",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentStackDiffComponentEvidence {
    component_id: String,
    kind: AgentStackComponentKind,
    source_scope: AgentStackSourceScope,
    source_locator: String,
    observation_class: AgentStackObservationClass,
    selection_state: AgentStackSelectionState,
    integrity: Option<String>,
    capabilities: Vec<AgentStackCapability>,
    trust_level: AgentStackTrustLevel,
    freshness: AgentStackFreshness,
}

#[rustfmt::skip]
impl AgentStackDiffComponentEvidence {
    fn from_component(component: &AgentStackComponent) -> Self {
        Self {
            component_id: component.component_id().as_str().to_owned(),
            kind: component.kind(),
            source_scope: component.source().scope(),
            source_locator: component.source().locator().as_str().to_owned(),
            observation_class: component.observation_class(),
            selection_state: component.selection_state(),
            integrity: component.integrity().map(|digest| digest.as_str().to_owned()),
            capabilities: component.capabilities().to_vec(),
            trust_level: component.trust_level(),
            freshness: component.freshness(),
        }
    }
    pub fn component_id(&self) -> &str { &self.component_id }
    pub fn kind(&self) -> AgentStackComponentKind { self.kind }
    pub fn source_scope(&self) -> AgentStackSourceScope { self.source_scope }
    pub fn source_locator(&self) -> &str { &self.source_locator }
    pub fn observation_class(&self) -> AgentStackObservationClass { self.observation_class }
    pub fn selection_state(&self) -> AgentStackSelectionState { self.selection_state }
    pub fn integrity(&self) -> Option<&str> { self.integrity.as_deref() }
    pub fn capabilities(&self) -> &[AgentStackCapability] { &self.capabilities }
    pub fn trust_level(&self) -> AgentStackTrustLevel { self.trust_level }
    pub fn freshness(&self) -> AgentStackFreshness { self.freshness }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "value_type", content = "value")]
pub enum AgentStackDiffValue {
    Component(AgentStackDiffComponentEvidence),
    ObservationClass(AgentStackObservationClass),
    SelectionState(AgentStackSelectionState),
    Integrity(String),
    Capability(AgentStackCapability),
    TrustLevel(AgentStackTrustLevel),
    Freshness(AgentStackFreshness),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentStackDiffFact {
    change_kind: AgentStackDiffChangeKind,
    fact_kind: AgentStackDiffFactKind,
    component_id: String,
    component_kind: AgentStackComponentKind,
    source_scope: AgentStackSourceScope,
    source_locator: String,
    field: AgentStackDiffField,
    #[serde(skip_serializing_if = "Option::is_none")]
    capability: Option<AgentStackCapability>,
    #[serde(skip_serializing_if = "Option::is_none")]
    before: Option<AgentStackDiffValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    after: Option<AgentStackDiffValue>,
}

#[rustfmt::skip]
impl AgentStackDiffFact {
    fn new(
        change_kind: AgentStackDiffChangeKind,
        fact_kind: AgentStackDiffFactKind,
        component: &AgentStackComponent,
        field: AgentStackDiffField,
        capability: Option<AgentStackCapability>,
        before: Option<AgentStackDiffValue>,
        after: Option<AgentStackDiffValue>,
    ) -> Self {
        Self {
            change_kind,
            fact_kind,
            component_id: component.component_id().as_str().to_owned(),
            component_kind: component.kind(),
            source_scope: component.source().scope(),
            source_locator: component.source().locator().as_str().to_owned(),
            field,
            capability,
            before,
            after,
        }
    }
    pub fn change_kind(&self) -> AgentStackDiffChangeKind { self.change_kind }
    pub fn fact_kind(&self) -> AgentStackDiffFactKind { self.fact_kind }
    pub fn component_id(&self) -> &str { &self.component_id }
    pub fn component_kind(&self) -> AgentStackComponentKind { self.component_kind }
    pub fn source_scope(&self) -> AgentStackSourceScope { self.source_scope }
    pub fn source_locator(&self) -> &str { &self.source_locator }
    pub fn field(&self) -> AgentStackDiffField { self.field }
    pub fn capability(&self) -> Option<AgentStackCapability> { self.capability }
    pub fn before(&self) -> Option<&AgentStackDiffValue> { self.before.as_ref() }
    pub fn after(&self) -> Option<&AgentStackDiffValue> { self.after.as_ref() }
}

pub fn stack_diff(
    before: AgentStackDiffSnapshot<'_>,
    after: AgentStackDiffSnapshot<'_>,
) -> Result<Vec<AgentStackDiffFact>, AgentStackDiffError> {
    validate_compatible_versions(before, after)?;
    let before_by_id = by_component_id(AgentStackDiffSide::Before, before.components())?;
    let after_by_id = by_component_id(AgentStackDiffSide::After, after.components())?;
    let mut facts = Vec::new();

    for (component_id, before_component) in &before_by_id {
        match after_by_id.get(component_id).copied() {
            Some(after_component) => {
                compare_existing(before_component, after_component, &mut facts)
            }
            None => facts.push(component_fact(
                AgentStackDiffChangeKind::Removed,
                before_component,
                None,
            )),
        }
    }
    for (component_id, after_component) in &after_by_id {
        if !before_by_id.contains_key(component_id) {
            facts.push(component_fact(
                AgentStackDiffChangeKind::Added,
                after_component,
                Some(after_component),
            ));
        }
    }

    facts.sort_by(|left, right| fact_sort_key(left).cmp(&fact_sort_key(right)));
    Ok(facts)
}

fn validate_compatible_versions(
    before: AgentStackDiffSnapshot<'_>,
    after: AgentStackDiffSnapshot<'_>,
) -> Result<(), AgentStackDiffError> {
    if before.schema_version() != after.schema_version() {
        return Err(AgentStackDiffError::IncompatibleSchemaVersions {
            before_schema_version: before.schema_version().to_owned(),
            after_schema_version: after.schema_version().to_owned(),
        });
    }
    if before.schema_version() != AGENT_STACK_COMPONENT_SCHEMA_VERSION {
        return Err(AgentStackDiffError::UnsupportedSchemaVersion {
            side: AgentStackDiffSide::Before,
            schema_version: before.schema_version().to_owned(),
        });
    }
    Ok(())
}

fn by_component_id(
    side: AgentStackDiffSide,
    components: &[AgentStackComponent],
) -> Result<BTreeMap<&str, &AgentStackComponent>, AgentStackDiffError> {
    let mut by_id = BTreeMap::new();
    for component in components {
        component
            .validate()
            .map_err(|source| AgentStackDiffError::InvalidComponent {
                side,
                component_id: component.component_id().as_str().to_owned(),
                source,
            })?;
        let component_id = component.component_id().as_str();
        if by_id.insert(component_id, component).is_some() {
            return Err(AgentStackDiffError::DuplicateComponentId {
                side,
                component_id: component_id.to_owned(),
            });
        }
    }
    Ok(by_id)
}

fn compare_existing(
    before: &AgentStackComponent,
    after: &AgentStackComponent,
    facts: &mut Vec<AgentStackDiffFact>,
) {
    if before.observation_class() != after.observation_class() {
        facts.push(modified_fact(
            before,
            AgentStackDiffField::ObservationClass,
            None,
            Some(AgentStackDiffValue::ObservationClass(
                before.observation_class(),
            )),
            Some(AgentStackDiffValue::ObservationClass(
                after.observation_class(),
            )),
        ));
    }
    if before.selection_state() != after.selection_state() {
        facts.push(modified_fact(
            before,
            AgentStackDiffField::SelectionState,
            None,
            Some(AgentStackDiffValue::SelectionState(
                before.selection_state(),
            )),
            Some(AgentStackDiffValue::SelectionState(after.selection_state())),
        ));
    }
    if before.integrity() != after.integrity() {
        facts.push(modified_fact(
            before,
            AgentStackDiffField::Integrity,
            None,
            before
                .integrity()
                .map(|digest| AgentStackDiffValue::Integrity(digest.as_str().to_owned())),
            after
                .integrity()
                .map(|digest| AgentStackDiffValue::Integrity(digest.as_str().to_owned())),
        ));
    }
    compare_capabilities(before, after, facts);
    if before.trust_level() != after.trust_level() {
        facts.push(modified_fact(
            before,
            AgentStackDiffField::TrustLevel,
            None,
            Some(AgentStackDiffValue::TrustLevel(before.trust_level())),
            Some(AgentStackDiffValue::TrustLevel(after.trust_level())),
        ));
    }
    if before.freshness() != after.freshness() {
        facts.push(modified_fact(
            before,
            AgentStackDiffField::Freshness,
            None,
            Some(AgentStackDiffValue::Freshness(before.freshness())),
            Some(AgentStackDiffValue::Freshness(after.freshness())),
        ));
    }
}

fn compare_capabilities(
    before: &AgentStackComponent,
    after: &AgentStackComponent,
    facts: &mut Vec<AgentStackDiffFact>,
) {
    for capability in before.capabilities() {
        if !after.capabilities().contains(capability) {
            facts.push(AgentStackDiffFact::new(
                AgentStackDiffChangeKind::Removed,
                AgentStackDiffFactKind::Capability,
                before,
                AgentStackDiffField::Capability,
                Some(*capability),
                Some(AgentStackDiffValue::Capability(*capability)),
                None,
            ));
        }
    }
    for capability in after.capabilities() {
        if !before.capabilities().contains(capability) {
            facts.push(AgentStackDiffFact::new(
                AgentStackDiffChangeKind::Added,
                AgentStackDiffFactKind::Capability,
                after,
                AgentStackDiffField::Capability,
                Some(*capability),
                None,
                Some(AgentStackDiffValue::Capability(*capability)),
            ));
        }
    }
}

fn component_fact(
    change_kind: AgentStackDiffChangeKind,
    before: &AgentStackComponent,
    after: Option<&AgentStackComponent>,
) -> AgentStackDiffFact {
    let component = after.unwrap_or(before);
    let before_value = (change_kind == AgentStackDiffChangeKind::Removed).then(|| {
        AgentStackDiffValue::Component(AgentStackDiffComponentEvidence::from_component(before))
    });
    let after_value = (change_kind == AgentStackDiffChangeKind::Added).then(|| {
        AgentStackDiffValue::Component(AgentStackDiffComponentEvidence::from_component(component))
    });
    AgentStackDiffFact::new(
        change_kind,
        component_fact_kind(component),
        component,
        AgentStackDiffField::Component,
        None,
        before_value,
        after_value,
    )
}

fn modified_fact(
    component: &AgentStackComponent,
    field: AgentStackDiffField,
    capability: Option<AgentStackCapability>,
    before: Option<AgentStackDiffValue>,
    after: Option<AgentStackDiffValue>,
) -> AgentStackDiffFact {
    AgentStackDiffFact::new(
        AgentStackDiffChangeKind::Modified,
        modified_fact_kind(component, field),
        component,
        field,
        capability,
        before,
        after,
    )
}

fn modified_fact_kind(
    component: &AgentStackComponent,
    field: AgentStackDiffField,
) -> AgentStackDiffFactKind {
    match field {
        AgentStackDiffField::ObservationClass | AgentStackDiffField::SelectionState => {
            AgentStackDiffFactKind::Runtime
        }
        AgentStackDiffField::Capability => AgentStackDiffFactKind::Capability,
        AgentStackDiffField::TrustLevel => AgentStackDiffFactKind::Trust,
        AgentStackDiffField::Freshness => AgentStackDiffFactKind::Freshness,
        AgentStackDiffField::Integrity
            if component.kind() == AgentStackComponentKind::Validation =>
        {
            AgentStackDiffFactKind::Validation
        }
        AgentStackDiffField::Component | AgentStackDiffField::Integrity => {
            AgentStackDiffFactKind::Context
        }
    }
}

fn component_fact_kind(component: &AgentStackComponent) -> AgentStackDiffFactKind {
    if component.kind() == AgentStackComponentKind::Validation {
        AgentStackDiffFactKind::Validation
    } else if component.kind() == AgentStackComponentKind::AgentRuntime
        || matches!(
            component.source().scope(),
            AgentStackSourceScope::Runtime | AgentStackSourceScope::Runner
        )
        || matches!(
            component.observation_class(),
            AgentStackObservationClass::RuntimeObserved
                | AgentStackObservationClass::RunnerObserved
        )
    {
        AgentStackDiffFactKind::Runtime
    } else {
        AgentStackDiffFactKind::Context
    }
}

fn fact_sort_key(
    fact: &AgentStackDiffFact,
) -> (&str, &'static str, &'static str, &'static str, &'static str) {
    (
        fact.component_id(),
        fact.change_kind().as_str(),
        fact.fact_kind().as_str(),
        fact.field().as_str(),
        fact.capability()
            .map_or("", |capability| capability.as_str()),
    )
}
