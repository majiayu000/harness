//! Advisory extraction of repository-declared Agent Stack capabilities.

use super::capability_evidence::{
    AgentStackCapabilityEvidence, AgentStackCapabilityEvidenceError, AgentStackCapabilityScope,
};
use super::inventory::{inventory_with_root, AgentStackInventoryError};
use super::{
    AgentStackCapability, AgentStackComponent, AgentStackComponentKind, AgentStackTrustLevel,
};
use cap_std::fs::{Dir, OpenOptions};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use thiserror::Error;

mod static_patterns;
#[cfg(test)]
mod tests;
mod typed;

use static_patterns::extract_static;
use typed::extract_typed;

const DEFAULT_MAX_FILE_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone)]
pub struct AgentStackCapabilityExtractionOptions {
    root: PathBuf,
    inventory_options: super::AgentStackInventoryOptions,
    max_file_bytes: u64,
}

impl AgentStackCapabilityExtractionOptions {
    pub fn new(root: PathBuf) -> Self {
        Self {
            inventory_options: super::AgentStackInventoryOptions::new(root.clone()),
            root,
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
        }
    }

    pub fn with_max_file_bytes(
        mut self,
        max_file_bytes: u64,
    ) -> Result<Self, AgentStackCapabilityExtractionError> {
        if max_file_bytes == 0 || max_file_bytes == u64::MAX {
            return Err(AgentStackCapabilityExtractionError::InvalidOptions);
        }
        self.max_file_bytes = max_file_bytes;
        Ok(self)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum AgentStackCapabilityExtractionError {
    #[error("agent stack capability extraction options are invalid")]
    InvalidOptions,
    #[error("agent stack capability extraction failed to open the repository root")]
    RootOpen,
    #[error("agent stack inventory failed before capability extraction")]
    Inventory(#[source] AgentStackInventoryError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStackCapabilityExtractionConfidence {
    Low,
    Medium,
    High,
}

impl AgentStackCapabilityExtractionConfidence {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStackCapabilityExtractionFailureKind {
    ReadFailed,
    LimitExceeded,
    ParseFailed,
    InvalidDeclaration,
    EvidenceValidation,
}

impl AgentStackCapabilityExtractionFailureKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReadFailed => "read_failed",
            Self::LimitExceeded => "limit_exceeded",
            Self::ParseFailed => "parse_failed",
            Self::InvalidDeclaration => "invalid_declaration",
            Self::EvidenceValidation => "evidence_validation",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentStackCapabilityExtractionFailure {
    component: AgentStackComponent,
    kind: AgentStackCapabilityExtractionFailureKind,
    rule_id: Option<String>,
    reason: String,
}

impl AgentStackCapabilityExtractionFailure {
    fn new(
        component: &AgentStackComponent,
        kind: AgentStackCapabilityExtractionFailureKind,
        rule_id: Option<&str>,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            component: component.clone(),
            kind,
            rule_id: rule_id.map(str::to_owned),
            reason: reason.into(),
        }
    }

    pub fn component(&self) -> &AgentStackComponent {
        &self.component
    }

    pub const fn kind(&self) -> AgentStackCapabilityExtractionFailureKind {
        self.kind
    }

    pub fn rule_id(&self) -> Option<&str> {
        self.rule_id.as_deref()
    }

    pub fn reason(&self) -> &str {
        &self.reason
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentStackCapabilityExtractionEvidence {
    component: AgentStackComponent,
    evidence: AgentStackCapabilityEvidence,
    rule_id: String,
    reason: String,
    confidence: AgentStackCapabilityExtractionConfidence,
}

impl AgentStackCapabilityExtractionEvidence {
    fn new(
        component: &AgentStackComponent,
        raw: RawCapability,
    ) -> Result<Self, AgentStackCapabilityEvidenceError> {
        let evidence = AgentStackCapabilityEvidence::declared(
            component,
            raw.capability,
            component.source().clone(),
            None,
            raw.trust_level,
            raw.scope,
        )?;
        Ok(Self {
            component: component.clone(),
            evidence,
            rule_id: raw.rule_id.to_owned(),
            reason: raw.reason,
            confidence: raw.confidence,
        })
    }

    pub fn component(&self) -> &AgentStackComponent {
        &self.component
    }

    pub fn evidence(&self) -> &AgentStackCapabilityEvidence {
        &self.evidence
    }

    pub const fn capability(&self) -> AgentStackCapability {
        self.evidence.capability()
    }

    pub fn rule_id(&self) -> &str {
        &self.rule_id
    }

    pub fn reason(&self) -> &str {
        &self.reason
    }

    pub const fn confidence(&self) -> AgentStackCapabilityExtractionConfidence {
        self.confidence
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentStackCapabilityExtraction {
    evidence: Vec<AgentStackCapabilityExtractionEvidence>,
    failures: Vec<AgentStackCapabilityExtractionFailure>,
}

impl AgentStackCapabilityExtraction {
    pub fn evidence(&self) -> &[AgentStackCapabilityExtractionEvidence] {
        &self.evidence
    }

    pub fn failures(&self) -> &[AgentStackCapabilityExtractionFailure] {
        &self.failures
    }
}

#[derive(Debug, Clone)]
pub(super) struct RawCapability {
    capability: AgentStackCapability,
    rule_id: &'static str,
    reason: String,
    confidence: AgentStackCapabilityExtractionConfidence,
    trust_level: AgentStackTrustLevel,
    scope: AgentStackCapabilityScope,
}

pub fn extract_repository_capability_evidence(
    options: &AgentStackCapabilityExtractionOptions,
) -> Result<AgentStackCapabilityExtraction, AgentStackCapabilityExtractionError> {
    let root = Dir::open_ambient_dir(&options.root, cap_std::ambient_authority())
        .map_err(|_| AgentStackCapabilityExtractionError::RootOpen)?;
    let inventory = inventory_with_root(&root, &options.inventory_options)
        .map_err(AgentStackCapabilityExtractionError::Inventory)?;

    let mut evidence = Vec::new();
    let mut failures = Vec::new();
    for entry in inventory.entries() {
        let component = entry.component();
        let locator = component.source().locator().as_str();
        if !is_supported_control(component.kind(), locator) {
            continue;
        }
        let Some(text) = read_text(
            &root,
            component,
            locator,
            options.max_file_bytes,
            &mut failures,
        ) else {
            continue;
        };
        let mut typed = Vec::new();
        extract_typed(component, locator, &text, &mut typed, &mut failures);
        let typed_capabilities = typed
            .iter()
            .map(|raw| raw.capability.as_str())
            .collect::<BTreeSet<_>>();
        let mut raw = typed;
        for static_capability in extract_static(component, locator, &text) {
            if !typed_capabilities.contains(static_capability.capability.as_str()) {
                raw.push(static_capability);
            }
        }
        append_validated(component, raw, &mut evidence, &mut failures);
    }

    evidence.sort_by_key(evidence_sort_key);
    failures.sort_by_key(failure_sort_key);
    Ok(AgentStackCapabilityExtraction { evidence, failures })
}

fn is_supported_control(kind: AgentStackComponentKind, locator: &str) -> bool {
    matches!(
        kind,
        AgentStackComponentKind::McpServer
            | AgentStackComponentKind::Policy
            | AgentStackComponentKind::Hook
    ) || matches!(locator, "harness.toml" | ".harness/config.toml")
}

fn read_text(
    root: &Dir,
    component: &AgentStackComponent,
    locator: &str,
    max_file_bytes: u64,
    failures: &mut Vec<AgentStackCapabilityExtractionFailure>,
) -> Option<String> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let mut file = match root.open_with(locator, &options) {
        Ok(file) => file.take(max_file_bytes + 1),
        Err(_) => {
            failures.push(AgentStackCapabilityExtractionFailure::new(
                component,
                AgentStackCapabilityExtractionFailureKind::ReadFailed,
                None,
                format!("failed to open {locator} for capability extraction"),
            ));
            return None;
        }
    };
    let mut bytes = Vec::new();
    if file.read_to_end(&mut bytes).is_err() {
        failures.push(AgentStackCapabilityExtractionFailure::new(
            component,
            AgentStackCapabilityExtractionFailureKind::ReadFailed,
            None,
            format!("failed to read {locator} for capability extraction"),
        ));
        return None;
    }
    if bytes.len() as u64 > max_file_bytes {
        failures.push(AgentStackCapabilityExtractionFailure::new(
            component,
            AgentStackCapabilityExtractionFailureKind::LimitExceeded,
            None,
            format!("{locator} exceeds the capability extraction byte limit"),
        ));
        return None;
    }
    String::from_utf8(bytes).ok()
}

fn append_validated(
    component: &AgentStackComponent,
    raw: Vec<RawCapability>,
    evidence: &mut Vec<AgentStackCapabilityExtractionEvidence>,
    failures: &mut Vec<AgentStackCapabilityExtractionFailure>,
) {
    let mut seen = BTreeSet::new();
    for item in raw {
        let key = (item.capability.as_str(), item.rule_id, item.reason.clone());
        if !seen.insert(key) {
            continue;
        }
        match AgentStackCapabilityExtractionEvidence::new(component, item) {
            Ok(item) => evidence.push(item),
            Err(error) => failures.push(AgentStackCapabilityExtractionFailure::new(
                component,
                AgentStackCapabilityExtractionFailureKind::EvidenceValidation,
                None,
                format!("capability evidence validation failed: {error}"),
            )),
        }
    }
}

pub(super) fn parse_capability(value: &str) -> Option<AgentStackCapability> {
    AgentStackCapability::ALL
        .iter()
        .copied()
        .find(|capability| capability.as_str() == value)
}

pub(super) fn push_unique(
    capabilities: &mut Vec<AgentStackCapability>,
    capability: AgentStackCapability,
) {
    if !capabilities.contains(&capability) {
        capabilities.push(capability);
    }
}

pub(super) fn declared_raw(
    capability: AgentStackCapability,
    rule_id: &'static str,
    reason: String,
) -> RawCapability {
    RawCapability {
        capability,
        rule_id,
        reason,
        confidence: AgentStackCapabilityExtractionConfidence::High,
        trust_level: AgentStackTrustLevel::SelfDeclared,
        scope: AgentStackCapabilityScope::Component,
    }
}

pub(super) fn inferred_raw(
    capability: AgentStackCapability,
    rule_id: &'static str,
    reason: String,
    confidence: AgentStackCapabilityExtractionConfidence,
) -> RawCapability {
    RawCapability {
        capability,
        rule_id,
        reason,
        confidence,
        trust_level: AgentStackTrustLevel::RepositoryObserved,
        scope: scope_for_capability(capability),
    }
}

fn scope_for_capability(capability: AgentStackCapability) -> AgentStackCapabilityScope {
    match capability {
        AgentStackCapability::Network => AgentStackCapabilityScope::network(None::<String>)
            .expect("network scope without endpoint is valid"),
        _ => AgentStackCapabilityScope::Component,
    }
}

fn evidence_sort_key(item: &AgentStackCapabilityExtractionEvidence) -> (String, String, String) {
    (
        item.component().source().locator().as_str().to_owned(),
        item.evidence().capability().as_str().to_owned(),
        item.rule_id().to_owned(),
    )
}

fn failure_sort_key(item: &AgentStackCapabilityExtractionFailure) -> (String, String, String) {
    (
        item.component().source().locator().as_str().to_owned(),
        item.kind().as_str().to_owned(),
        item.reason().to_owned(),
    )
}
