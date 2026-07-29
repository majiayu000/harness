use super::{
    AgentStackCapability, AgentStackComponent, AgentStackComponentId, AgentStackSource,
    AgentStackSourceScope, AgentStackTrustLevel,
};
use crate::capability::CapabilityToken;
use crate::config::agents::SandboxMode;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::Path;
use thiserror::Error;

#[cfg(test)]
mod tests;

pub const AGENT_STACK_CAPABILITY_EVIDENCE_SCHEMA_VERSION: &str =
    "agent-stack-capability-evidence/v0.1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentStackCapabilityDefinition {
    pub capability: AgentStackCapability,
    pub summary: &'static str,
}

#[rustfmt::skip]
pub const AGENT_STACK_CAPABILITY_DEFINITIONS: &[AgentStackCapabilityDefinition] = &[
    AgentStackCapabilityDefinition { capability: AgentStackCapability::Destructive, summary: "May delete, overwrite, or irreversibly mutate local or remote state." },
    AgentStackCapabilityDefinition { capability: AgentStackCapability::SecretRead, summary: "May read credentials, tokens, private configuration, or sensitive material." },
    AgentStackCapabilityDefinition { capability: AgentStackCapability::Network, summary: "May initiate outbound network calls or access remote resources." },
    AgentStackCapabilityDefinition { capability: AgentStackCapability::Privileged, summary: "May bypass normal sandbox, permission, or isolation boundaries." },
    AgentStackCapabilityDefinition { capability: AgentStackCapability::ProductionWrite, summary: "May write to production infrastructure, deployments, or customer-affecting systems." },
    AgentStackCapabilityDefinition { capability: AgentStackCapability::Shell, summary: "May execute shell commands or arbitrary local programs." },
    AgentStackCapabilityDefinition { capability: AgentStackCapability::FileWrite, summary: "May create, modify, or delete filesystem content." },
];

impl AgentStackCapability {
    pub const fn definition(self) -> &'static str {
        match self {
            Self::Destructive => "May delete, overwrite, or irreversibly mutate local or remote state.",
            Self::SecretRead => "May read credentials, tokens, private configuration, or sensitive material.",
            Self::Network => "May initiate outbound network calls or access remote resources.",
            Self::Privileged => "May bypass normal sandbox, permission, or isolation boundaries.",
            Self::ProductionWrite => "May write to production infrastructure, deployments, or customer-affecting systems.",
            Self::Shell => "May execute shell commands or arbitrary local programs.",
            Self::FileWrite => "May create, modify, or delete filesystem content.",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStackCapabilityEvidenceClass {
    Declared,
    Granted,
    Observed,
}

impl AgentStackCapabilityEvidenceClass {
    pub const ALL: &'static [Self] = &[Self::Declared, Self::Granted, Self::Observed];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Declared => "declared",
            Self::Granted => "granted",
            Self::Observed => "observed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum AgentStackCapabilityScope {
    Component,
    Repository {
        locator: String,
    },
    Path {
        path: String,
    },
    Network {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        endpoint: Option<String>,
    },
    Host,
}

impl AgentStackCapabilityScope {
    pub fn repository(
        locator: impl Into<String>,
    ) -> Result<Self, AgentStackCapabilityEvidenceError> {
        let scope = Self::Repository {
            locator: locator.into(),
        };
        scope.validate()?;
        Ok(scope)
    }

    pub fn path(path: &Path) -> Result<Self, AgentStackCapabilityEvidenceError> {
        let normalized = super::normalize_absolute_path(path)
            .map_err(|_| AgentStackCapabilityEvidenceError::InvalidScope)?;
        let path = normalized
            .to_str()
            .ok_or(AgentStackCapabilityEvidenceError::NonUtf8Scope)?
            .to_owned();
        let scope = Self::Path { path };
        scope.validate()?;
        Ok(scope)
    }

    pub fn network(
        endpoint: Option<impl Into<String>>,
    ) -> Result<Self, AgentStackCapabilityEvidenceError> {
        let scope = Self::Network {
            endpoint: endpoint.map(Into::into),
        };
        scope.validate()?;
        Ok(scope)
    }

    pub fn validate(&self) -> Result<(), AgentStackCapabilityEvidenceError> {
        match self {
            Self::Component | Self::Host => Ok(()),
            Self::Repository { locator } => super::validate_portable_path(locator)
                .and_then(|_| super::reject_reserved_segments(locator))
                .map_err(|_| AgentStackCapabilityEvidenceError::InvalidScope),
            Self::Path { path } => {
                if path.is_empty() || path.contains('\0') || !Path::new(path).is_absolute() {
                    Err(AgentStackCapabilityEvidenceError::InvalidScope)
                } else {
                    super::normalize_absolute_path(Path::new(path))
                        .map(|_| ())
                        .map_err(|_| AgentStackCapabilityEvidenceError::InvalidScope)
                }
            }
            Self::Network { endpoint } => {
                if endpoint
                    .as_deref()
                    .is_some_and(|value| value.trim().is_empty() || value.contains('\0'))
                {
                    Err(AgentStackCapabilityEvidenceError::InvalidScope)
                } else {
                    Ok(())
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentStackCapabilityEvidence {
    schema_version: &'static str,
    evidence_class: AgentStackCapabilityEvidenceClass,
    capability: AgentStackCapability,
    component_id: AgentStackComponentId,
    source: AgentStackSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    observed_at: Option<DateTime<Utc>>,
    trust_level: AgentStackTrustLevel,
    scope: AgentStackCapabilityScope,
}

impl AgentStackCapabilityEvidence {
    pub fn new(
        evidence_class: AgentStackCapabilityEvidenceClass,
        capability: AgentStackCapability,
        component_id: AgentStackComponentId,
        source: AgentStackSource,
        observed_at: Option<DateTime<Utc>>,
        trust_level: AgentStackTrustLevel,
        scope: AgentStackCapabilityScope,
    ) -> Result<Self, AgentStackCapabilityEvidenceError> {
        let evidence = Self {
            schema_version: AGENT_STACK_CAPABILITY_EVIDENCE_SCHEMA_VERSION,
            evidence_class,
            capability,
            component_id,
            source,
            observed_at,
            trust_level,
            scope,
        };
        evidence.validate()?;
        Ok(evidence)
    }

    pub fn declared(
        component: &AgentStackComponent,
        capability: AgentStackCapability,
        source: AgentStackSource,
        observed_at: Option<DateTime<Utc>>,
        trust_level: AgentStackTrustLevel,
        scope: AgentStackCapabilityScope,
    ) -> Result<Self, AgentStackCapabilityEvidenceError> {
        Self::new(
            AgentStackCapabilityEvidenceClass::Declared,
            capability,
            component.component_id().clone(),
            source,
            observed_at,
            trust_level,
            scope,
        )
    }

    pub fn granted(
        component: &AgentStackComponent,
        capability: AgentStackCapability,
        source: AgentStackSource,
        observed_at: DateTime<Utc>,
        trust_level: AgentStackTrustLevel,
        scope: AgentStackCapabilityScope,
    ) -> Result<Self, AgentStackCapabilityEvidenceError> {
        Self::new(
            AgentStackCapabilityEvidenceClass::Granted,
            capability,
            component.component_id().clone(),
            source,
            Some(observed_at),
            trust_level,
            scope,
        )
    }

    pub fn observed(
        component: &AgentStackComponent,
        capability: AgentStackCapability,
        source: AgentStackSource,
        observed_at: DateTime<Utc>,
        trust_level: AgentStackTrustLevel,
        scope: AgentStackCapabilityScope,
    ) -> Result<Self, AgentStackCapabilityEvidenceError> {
        Self::new(
            AgentStackCapabilityEvidenceClass::Observed,
            capability,
            component.component_id().clone(),
            source,
            Some(observed_at),
            trust_level,
            scope,
        )
    }

    pub fn granted_by_capability_token(
        component: &AgentStackComponent,
        token: &CapabilityToken,
        source: AgentStackSource,
        observed_at: DateTime<Utc>,
    ) -> Result<Vec<Self>, AgentStackCapabilityEvidenceError> {
        let trust_level = runtime_trust_for_source(source.scope())?;
        token
            .allowed_write_paths
            .iter()
            .map(|path| {
                Self::granted(
                    component,
                    AgentStackCapability::FileWrite,
                    source.clone(),
                    observed_at,
                    trust_level,
                    AgentStackCapabilityScope::path(path)?,
                )
            })
            .collect()
    }

    pub fn granted_by_sandbox_mode(
        component: &AgentStackComponent,
        sandbox_mode: SandboxMode,
        project_root: &Path,
        source: AgentStackSource,
        observed_at: DateTime<Utc>,
    ) -> Result<Vec<Self>, AgentStackCapabilityEvidenceError> {
        let trust_level = runtime_trust_for_source(source.scope())?;
        let grants = match sandbox_mode {
            SandboxMode::ReadOnly => Vec::new(),
            SandboxMode::ReadOnlyWithNetwork => vec![(
                AgentStackCapability::Network,
                AgentStackCapabilityScope::network(None::<String>)?,
            )],
            SandboxMode::WorkspaceWrite => vec![
                (
                    AgentStackCapability::Network,
                    AgentStackCapabilityScope::network(None::<String>)?,
                ),
                (
                    AgentStackCapability::FileWrite,
                    AgentStackCapabilityScope::path(project_root)?,
                ),
            ],
            SandboxMode::DangerFullAccess => vec![
                (
                    AgentStackCapability::Destructive,
                    AgentStackCapabilityScope::Host,
                ),
                (
                    AgentStackCapability::SecretRead,
                    AgentStackCapabilityScope::Host,
                ),
                (
                    AgentStackCapability::Network,
                    AgentStackCapabilityScope::Host,
                ),
                (
                    AgentStackCapability::Privileged,
                    AgentStackCapabilityScope::Host,
                ),
                (
                    AgentStackCapability::FileWrite,
                    AgentStackCapabilityScope::Host,
                ),
            ],
        };
        grants
            .into_iter()
            .map(|(capability, scope)| {
                Self::granted(
                    component,
                    capability,
                    source.clone(),
                    observed_at,
                    trust_level,
                    scope,
                )
            })
            .collect()
    }

    pub fn validate(&self) -> Result<(), AgentStackCapabilityEvidenceError> {
        if self.schema_version != AGENT_STACK_CAPABILITY_EVIDENCE_SCHEMA_VERSION {
            return Err(AgentStackCapabilityEvidenceError::UnsupportedSchemaVersion);
        }
        validate_component_id(self.component_id.as_str())?;
        validate_source_for_class(self.evidence_class, self.source.scope())?;
        validate_time_for_class(self.evidence_class, self.observed_at.as_ref())?;
        validate_trust_for_class(self.evidence_class, self.trust_level)?;
        self.scope.validate()
    }

    pub fn from_json(value: &str) -> Result<Self, AgentStackCapabilityEvidenceParseError> {
        let envelope: VersionEnvelope =
            serde_json::from_str(value).map_err(AgentStackCapabilityEvidenceParseError::Syntax)?;
        if envelope.schema_version.as_deref()
            != Some(AGENT_STACK_CAPABILITY_EVIDENCE_SCHEMA_VERSION)
        {
            return Err(AgentStackCapabilityEvidenceError::UnsupportedSchemaVersion.into());
        }
        let wire: V01WireCapabilityEvidence =
            serde_json::from_str(value).map_err(AgentStackCapabilityEvidenceParseError::Syntax)?;
        wire.try_into().map_err(Into::into)
    }

    pub fn schema_version(&self) -> &str {
        self.schema_version
    }

    pub const fn evidence_class(&self) -> AgentStackCapabilityEvidenceClass {
        self.evidence_class
    }

    pub const fn capability(&self) -> AgentStackCapability {
        self.capability
    }

    pub fn component_id(&self) -> &AgentStackComponentId {
        &self.component_id
    }

    pub fn source(&self) -> &AgentStackSource {
        &self.source
    }

    pub fn observed_at(&self) -> Option<&DateTime<Utc>> {
        self.observed_at.as_ref()
    }

    pub const fn trust_level(&self) -> AgentStackTrustLevel {
        self.trust_level
    }

    pub fn scope(&self) -> &AgentStackCapabilityScope {
        &self.scope
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum AgentStackCapabilityEvidenceError {
    #[error("the Agent Stack capability evidence schema version is unsupported")]
    UnsupportedSchemaVersion,
    #[error("the Agent Stack capability evidence component ID is invalid")]
    InvalidComponentId,
    #[error("the Agent Stack capability evidence source is invalid for its evidence class")]
    InvalidEvidenceSource,
    #[error("granted and observed capability evidence require an observation time")]
    MissingEvidenceTime,
    #[error("the Agent Stack capability evidence trust level is invalid for its evidence class")]
    TrustNotSupported,
    #[error("the Agent Stack capability evidence scope is invalid")]
    InvalidScope,
    #[error("the Agent Stack capability evidence scope contains non-UTF-8 data")]
    NonUtf8Scope,
}

#[derive(Debug, Error)]
pub enum AgentStackCapabilityEvidenceParseError {
    #[error("the Agent Stack capability evidence JSON has invalid syntax or shape")]
    Syntax(#[source] serde_json::Error),
    #[error(transparent)]
    Validation(#[from] AgentStackCapabilityEvidenceError),
}

#[derive(Deserialize)]
struct VersionEnvelope {
    schema_version: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct V01WireSource {
    scope: AgentStackSourceScope,
    locator: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct V01WireCapabilityEvidence {
    schema_version: String,
    evidence_class: AgentStackCapabilityEvidenceClass,
    capability: AgentStackCapability,
    component_id: String,
    source: V01WireSource,
    #[serde(default)]
    observed_at: Option<DateTime<Utc>>,
    trust_level: AgentStackTrustLevel,
    scope: AgentStackCapabilityScope,
}

impl TryFrom<V01WireCapabilityEvidence> for AgentStackCapabilityEvidence {
    type Error = AgentStackCapabilityEvidenceError;

    fn try_from(wire: V01WireCapabilityEvidence) -> Result<Self, Self::Error> {
        if wire.schema_version != AGENT_STACK_CAPABILITY_EVIDENCE_SCHEMA_VERSION {
            return Err(AgentStackCapabilityEvidenceError::UnsupportedSchemaVersion);
        }
        let source = AgentStackSource::new(wire.source.scope, &wire.source.locator)
            .map_err(|_| AgentStackCapabilityEvidenceError::InvalidEvidenceSource)?;
        AgentStackCapabilityEvidence::new(
            wire.evidence_class,
            wire.capability,
            AgentStackComponentId(wire.component_id),
            source,
            wire.observed_at,
            wire.trust_level,
            wire.scope,
        )
    }
}

fn runtime_trust_for_source(
    source_scope: AgentStackSourceScope,
) -> Result<AgentStackTrustLevel, AgentStackCapabilityEvidenceError> {
    match source_scope {
        AgentStackSourceScope::Runtime => Ok(AgentStackTrustLevel::RuntimeObserved),
        AgentStackSourceScope::Runner => Ok(AgentStackTrustLevel::RunnerObserved),
        AgentStackSourceScope::Repository
        | AgentStackSourceScope::UserGlobal
        | AgentStackSourceScope::Admin
        | AgentStackSourceScope::System => {
            Err(AgentStackCapabilityEvidenceError::InvalidEvidenceSource)
        }
    }
}

fn validate_component_id(value: &str) -> Result<(), AgentStackCapabilityEvidenceError> {
    let mut segments = value.splitn(3, ':');
    let source_scope = segments.next();
    let component_kind = segments.next();
    let locator = segments.next();
    if source_scope.is_some_and(|value| !value.is_empty())
        && component_kind.is_some_and(|value| !value.is_empty())
        && locator.is_some_and(|value| !value.is_empty() && !value.contains('\0'))
    {
        Ok(())
    } else {
        Err(AgentStackCapabilityEvidenceError::InvalidComponentId)
    }
}

fn validate_source_for_class(
    evidence_class: AgentStackCapabilityEvidenceClass,
    source_scope: AgentStackSourceScope,
) -> Result<(), AgentStackCapabilityEvidenceError> {
    let valid = match evidence_class {
        AgentStackCapabilityEvidenceClass::Declared => !matches!(
            source_scope,
            AgentStackSourceScope::Runtime | AgentStackSourceScope::Runner
        ),
        AgentStackCapabilityEvidenceClass::Granted
        | AgentStackCapabilityEvidenceClass::Observed => {
            matches!(
                source_scope,
                AgentStackSourceScope::Runtime | AgentStackSourceScope::Runner
            )
        }
    };
    valid
        .then_some(())
        .ok_or(AgentStackCapabilityEvidenceError::InvalidEvidenceSource)
}

fn validate_time_for_class(
    evidence_class: AgentStackCapabilityEvidenceClass,
    observed_at: Option<&DateTime<Utc>>,
) -> Result<(), AgentStackCapabilityEvidenceError> {
    match evidence_class {
        AgentStackCapabilityEvidenceClass::Declared => Ok(()),
        AgentStackCapabilityEvidenceClass::Granted
        | AgentStackCapabilityEvidenceClass::Observed => observed_at
            .is_some()
            .then_some(())
            .ok_or(AgentStackCapabilityEvidenceError::MissingEvidenceTime),
    }
}

fn validate_trust_for_class(
    evidence_class: AgentStackCapabilityEvidenceClass,
    trust_level: AgentStackTrustLevel,
) -> Result<(), AgentStackCapabilityEvidenceError> {
    let valid = match evidence_class {
        AgentStackCapabilityEvidenceClass::Declared => matches!(
            trust_level,
            AgentStackTrustLevel::SelfDeclared | AgentStackTrustLevel::RepositoryObserved
        ),
        AgentStackCapabilityEvidenceClass::Granted
        | AgentStackCapabilityEvidenceClass::Observed => {
            matches!(
                trust_level,
                AgentStackTrustLevel::RuntimeObserved | AgentStackTrustLevel::RunnerObserved
            )
        }
    };
    valid
        .then_some(())
        .ok_or(AgentStackCapabilityEvidenceError::TrustNotSupported)
}
