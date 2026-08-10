use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;
use std::path::Path;
use thiserror::Error;
pub mod capability_evidence;
mod diff;
#[cfg(test)]
mod diff_tests;
pub mod inventory;
#[cfg(test)]
mod inventory_tests;
mod protective_control_diff;
#[cfg(test)]
mod protective_control_diff_tests;
mod source_locator;
#[cfg(test)]
mod tests;
pub use diff::{
    stack_diff, AgentStackDiffChangeKind, AgentStackDiffComponentEvidence, AgentStackDiffError,
    AgentStackDiffFact, AgentStackDiffFactKind, AgentStackDiffField, AgentStackDiffSide,
    AgentStackDiffSnapshot, AgentStackDiffValue,
};
pub use inventory::{
    inventory_repository_stack, AgentStackEntryClass, AgentStackInventory,
    AgentStackInventoryEntry, AgentStackInventoryError, AgentStackInventoryErrorKind,
    AgentStackInventoryOptions,
};
pub use protective_control_diff::{
    protective_control_diff, AgentStackProtectionConfidence, AgentStackProtectionControl,
    AgentStackProtectionControlDiff, AgentStackProtectionControlError,
    AgentStackProtectionControlEvidence, AgentStackProtectionControlReason,
    AgentStackProtectionDiffKind, AgentStackProtectionFailureMode, AgentStackProtectionRole,
    AgentStackProtectionScope,
};
#[cfg(test)]
use source_locator::root_keys_match_for_test;
use source_locator::{
    is_reserved, is_snake_case, is_uuid_shaped, relative_portable_path, valid_logical_segments,
    validate_source_locator,
};
pub use source_locator::{resolve_xdg_config_harness_root, select_user_global_root};
pub const AGENT_STACK_COMPONENT_SCHEMA_VERSION: &str = "agent-stack-component/v0.1";

macro_rules! closed_enum {
    ($name:ident { $($variant:ident => $wire:literal),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name { $($variant),+ }
        impl $name {
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];
            pub const fn as_str(&self) -> &'static str {
                match self { $(Self::$variant => $wire),+ }
            }
        }
    };
}
#[rustfmt::skip]
closed_enum!(AgentStackComponentKind { Instructions => "instructions", Skill => "skill", McpServer => "mcp_server", McpTool => "mcp_tool", Hook => "hook", Memory => "memory", Policy => "policy", Workflow => "workflow", Validation => "validation", AgentRuntime => "agent_runtime" });
#[rustfmt::skip]
closed_enum!(AgentStackSourceScope { Repository => "repository", UserGlobal => "user_global", Admin => "admin", System => "system", Runtime => "runtime", Runner => "runner" });
#[rustfmt::skip]
closed_enum!(AgentStackUserGlobalRoot { HomeHarness => "home_harness", XdgConfigHarness => "xdg_config_harness", PlatformConfigHarness => "platform_config_harness", ConfiguredUser => "configured_user" });
#[rustfmt::skip]
closed_enum!(AgentStackObservationClass { RepositoryObserved => "repository_observed", RuntimeObserved => "runtime_observed", RunnerObserved => "runner_observed" });
#[rustfmt::skip]
closed_enum!(AgentStackSelectionState { Discovered => "discovered", Eligible => "eligible", Selected => "selected", Loaded => "loaded", Observed => "observed" });
#[rustfmt::skip]
closed_enum!(AgentStackCapability { Destructive => "destructive", SecretRead => "secret_read", Network => "network", Privileged => "privileged", ProductionWrite => "production_write", Shell => "shell", FileWrite => "file_write" });
#[rustfmt::skip]
closed_enum!(AgentStackTrustLevel { SelfDeclared => "self_declared", RepositoryObserved => "repository_observed", RuntimeObserved => "runtime_observed", RunnerObserved => "runner_observed" });
#[rustfmt::skip]
closed_enum!(AgentStackFreshness { Unknown => "unknown", Fresh => "fresh", Stale => "stale", Expired => "expired" });
macro_rules! string_newtype {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);
        impl $name {
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
    };
}
macro_rules! copy_getters {
    ($($name:ident: $ty:ty),+ $(,)?) => { $(pub const fn $name(&self) -> $ty { self.$name })+ };
}
macro_rules! borrow_getters {
    ($($name:ident: $ty:ty),+ $(,)?) => { $(pub fn $name(&self) -> &$ty { &self.$name })+ };
}
macro_rules! option_getters {
    ($($name:ident: $ty:ty),+ $(,)?) => {
        $(pub fn $name(&self) -> Option<&$ty> { self.$name.as_ref() })+
    };
}

string_newtype!(AgentStackComponentId);
string_newtype!(AgentStackSourceLocator);
string_newtype!(Sha256Digest);
impl AgentStackComponentId {
    pub fn from_source(kind: AgentStackComponentKind, source: &AgentStackSource) -> Self {
        Self(format!(
            "{}:{}:{}",
            source.scope.as_str(),
            kind.as_str(),
            source.locator.as_str()
        ))
    }
}
impl Sha256Digest {
    pub fn parse(value: &str) -> Result<Self, AgentStackComponentError> {
        let valid = value.len() == 64
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            && value.bytes().any(|b| b != b'0');
        valid
            .then(|| Self(value.to_owned()))
            .ok_or(AgentStackComponentError::InvalidSha256Digest)
    }

    pub fn from_bytes(bytes: &[u8]) -> Self {
        let mut value = String::with_capacity(64);
        for byte in Sha256::digest(bytes) {
            use fmt::Write as _;
            write!(&mut value, "{byte:02x}").expect("writing to a String cannot fail");
        }
        Self(value)
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct AgentStackSource {
    scope: AgentStackSourceScope,
    locator: AgentStackSourceLocator,
}

impl AgentStackSource {
    pub fn new(
        scope: AgentStackSourceScope,
        locator: &str,
    ) -> Result<Self, AgentStackComponentError> {
        validate_source_locator(scope, locator)?;
        Ok(Self {
            scope,
            locator: AgentStackSourceLocator(locator.to_owned()),
        })
    }

    pub fn repository_from_path(
        root: &Path,
        source: &Path,
    ) -> Result<Self, AgentStackComponentError> {
        Self::new(
            AgentStackSourceScope::Repository,
            &relative_portable_path(root, source)?,
        )
    }

    pub fn admin_from_path(source: &Path) -> Result<Self, AgentStackComponentError> {
        let root = Path::new("/etc/harness");
        Self::new(
            AgentStackSourceScope::Admin,
            &relative_portable_path(root, source)?,
        )
    }

    pub fn user_global_from_path(
        source: &Path,
        home_harness: Option<&Path>,
        xdg_config_harness: Option<&Path>,
        platform_config_harness: Option<&Path>,
        configured_user_roots: &[(&str, &Path)],
    ) -> Result<Self, AgentStackComponentError> {
        let (_, locator) = select_user_global_root(
            source,
            home_harness,
            xdg_config_harness,
            platform_config_harness,
            configured_user_roots,
        )?;
        Self::new(AgentStackSourceScope::UserGlobal, locator.as_str())
    }

    pub fn logical(
        scope: AgentStackSourceScope,
        namespace: &str,
        stable_path: &str,
    ) -> Result<Self, AgentStackComponentError> {
        if !matches!(
            scope,
            AgentStackSourceScope::System
                | AgentStackSourceScope::Runtime
                | AgentStackSourceScope::Runner
        ) || !is_snake_case(namespace)
            || is_uuid_shaped(namespace)
            || is_reserved(namespace)
            || !valid_logical_segments(stable_path)
        {
            return Err(AgentStackComponentError::InvalidSourceLocator);
        }
        let locator = if scope == AgentStackSourceScope::System {
            format!("builtin/{namespace}/{stable_path}")
        } else {
            format!("{namespace}/{stable_path}")
        };
        Self::new(scope, &locator)
    }

    pub const fn scope(&self) -> AgentStackSourceScope {
        self.scope
    }
    pub fn locator(&self) -> &AgentStackSourceLocator {
        &self.locator
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentStackFreshnessEvidence {
    authoritatively_invalidated: bool,
    observation_time: Option<DateTime<Utc>>,
    valid_until: Option<DateTime<Utc>>,
    current_source_observed: bool,
    cached_prior_observation: bool,
}

impl AgentStackFreshnessEvidence {
    pub fn new(
        authoritatively_invalidated: bool,
        observation_time: Option<DateTime<Utc>>,
        valid_until: Option<DateTime<Utc>>,
        current_source_observed: bool,
        cached_prior_observation: bool,
    ) -> Self {
        Self {
            authoritatively_invalidated,
            observation_time,
            valid_until,
            current_source_observed,
            cached_prior_observation,
        }
    }

    pub fn classify(&self) -> AgentStackFreshness {
        if self.authoritatively_invalidated
            || matches!(
                (&self.observation_time, &self.valid_until),
                (Some(observed), Some(valid_until)) if observed >= valid_until
            )
        {
            AgentStackFreshness::Expired
        } else if self.current_source_observed {
            AgentStackFreshness::Fresh
        } else if self.cached_prior_observation {
            AgentStackFreshness::Stale
        } else {
            AgentStackFreshness::Unknown
        }
    }

    copy_getters!(
        authoritatively_invalidated: bool,
        current_source_observed: bool,
        cached_prior_observation: bool,
    );
    option_getters!(observation_time: DateTime<Utc>, valid_until: DateTime<Utc>);
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentStackComponent {
    schema_version: &'static str,
    component_id: AgentStackComponentId,
    kind: AgentStackComponentKind,
    source: AgentStackSource,
    observation_class: AgentStackObservationClass,
    selection_state: AgentStackSelectionState,
    #[serde(skip_serializing_if = "Option::is_none")]
    integrity: Option<Sha256Digest>,
    capabilities: Vec<AgentStackCapability>,
    trust_level: AgentStackTrustLevel,
    freshness: AgentStackFreshness,
}

impl AgentStackComponent {
    pub fn new(
        kind: AgentStackComponentKind,
        source: AgentStackSource,
        observation_class: AgentStackObservationClass,
        selection_state: AgentStackSelectionState,
        trust_level: AgentStackTrustLevel,
        freshness: AgentStackFreshness,
    ) -> Result<Self, AgentStackComponentError> {
        validate_selection(observation_class, selection_state)?;
        validate_trust(observation_class, trust_level)?;
        let component_id = AgentStackComponentId::from_source(kind, &source);
        Ok(Self {
            schema_version: AGENT_STACK_COMPONENT_SCHEMA_VERSION,
            component_id,
            kind,
            source,
            observation_class,
            selection_state,
            integrity: None,
            capabilities: Vec::new(),
            trust_level,
            freshness,
        })
    }

    pub fn with_integrity(mut self, integrity: Option<Sha256Digest>) -> Self {
        self.integrity = integrity;
        self
    }

    pub fn with_capabilities(
        mut self,
        capabilities: impl IntoIterator<Item = AgentStackCapability>,
    ) -> Result<Self, AgentStackComponentError> {
        self.capabilities = capabilities.into_iter().collect();
        if has_duplicate_capabilities(&self.capabilities) {
            return Err(AgentStackComponentError::DuplicateCapability);
        }
        self.capabilities.sort_by_key(AgentStackCapability::as_str);
        Ok(self)
    }

    pub fn validate(&self) -> Result<(), AgentStackComponentError> {
        if self.schema_version != AGENT_STACK_COMPONENT_SCHEMA_VERSION {
            return Err(AgentStackComponentError::UnsupportedSchemaVersion);
        }
        validate_source_locator(self.source.scope, self.source.locator.as_str())?;
        if self.component_id != AgentStackComponentId::from_source(self.kind, &self.source) {
            return Err(AgentStackComponentError::NonCanonicalComponentId);
        }
        self.integrity
            .as_ref()
            .map(|digest| Sha256Digest::parse(digest.as_str()))
            .transpose()?;
        validate_selection(self.observation_class, self.selection_state)?;
        validate_trust(self.observation_class, self.trust_level)?;
        if has_duplicate_capabilities(&self.capabilities) {
            Err(AgentStackComponentError::DuplicateCapability)
        } else {
            Ok(())
        }
    }

    pub fn from_json(value: &str) -> Result<Self, AgentStackComponentParseError> {
        let envelope: VersionEnvelope =
            serde_json::from_str(value).map_err(AgentStackComponentParseError::Syntax)?;
        if envelope.schema_version.as_deref() != Some(AGENT_STACK_COMPONENT_SCHEMA_VERSION) {
            return Err(AgentStackComponentError::UnsupportedSchemaVersion.into());
        }
        let wire: V01WireComponent =
            serde_json::from_str(value).map_err(AgentStackComponentParseError::Syntax)?;
        wire.try_into().map_err(Into::into)
    }

    pub fn schema_version(&self) -> &str {
        self.schema_version
    }
    copy_getters!(
        kind: AgentStackComponentKind,
        observation_class: AgentStackObservationClass,
        selection_state: AgentStackSelectionState,
        trust_level: AgentStackTrustLevel,
        freshness: AgentStackFreshness,
    );
    borrow_getters!(component_id: AgentStackComponentId, source: AgentStackSource);
    option_getters!(integrity: Sha256Digest);
    pub fn capabilities(&self) -> &[AgentStackCapability] {
        &self.capabilities
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum AgentStackComponentError {
    #[error("the Agent Stack component schema version is unsupported")]
    UnsupportedSchemaVersion,
    #[error("the Agent Stack source locator is invalid")]
    InvalidSourceLocator,
    #[error("the Agent Stack source locator contains non-UTF-8 data")]
    NonUtf8SourceLocator,
    #[error("the Agent Stack source is outside its declared root")]
    SourceOutsideRoot,
    #[error("multiple configured user roots are ambiguous")]
    AmbiguousConfiguredUserRoot,
    #[error("an XDG Harness configuration root cannot be resolved")]
    XdgConfigRootUnavailable,
    #[error("the discovery source has no typed ownership")]
    UntypedDiscoverySource,
    #[error("the Agent Stack component ID is not canonical")]
    NonCanonicalComponentId,
    #[error("the SHA-256 digest is invalid")]
    InvalidSha256Digest,
    #[error("the selection state is unsupported by the observation class")]
    SelectionNotSupported,
    #[error("the trust level exceeds the observation class")]
    TrustExceedsObservation,
    #[error("the capability list contains a duplicate")]
    DuplicateCapability,
}

#[derive(Debug, Error)]
pub enum AgentStackComponentParseError {
    #[error("the Agent Stack component JSON has invalid syntax or shape")]
    Syntax(#[source] serde_json::Error),
    #[error(transparent)]
    Validation(#[from] AgentStackComponentError),
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
struct V01WireComponent {
    schema_version: String,
    component_id: String,
    kind: AgentStackComponentKind,
    source: V01WireSource,
    observation_class: AgentStackObservationClass,
    selection_state: AgentStackSelectionState,
    #[serde(default, deserialize_with = "deserialize_present_integrity")]
    integrity: Option<String>,
    capabilities: Vec<AgentStackCapability>,
    trust_level: AgentStackTrustLevel,
    freshness: AgentStackFreshness,
}

impl TryFrom<V01WireComponent> for AgentStackComponent {
    type Error = AgentStackComponentError;

    fn try_from(wire: V01WireComponent) -> Result<Self, Self::Error> {
        if wire.schema_version != AGENT_STACK_COMPONENT_SCHEMA_VERSION {
            return Err(AgentStackComponentError::UnsupportedSchemaVersion);
        }
        let source = AgentStackSource::new(wire.source.scope, &wire.source.locator)?;
        let expected = AgentStackComponentId::from_source(wire.kind, &source);
        if wire.component_id != expected.as_str() {
            return Err(AgentStackComponentError::NonCanonicalComponentId);
        }
        let integrity = wire
            .integrity
            .as_deref()
            .map(Sha256Digest::parse)
            .transpose()?;
        AgentStackComponent::new(
            wire.kind,
            source,
            wire.observation_class,
            wire.selection_state,
            wire.trust_level,
            wire.freshness,
        )?
        .with_integrity(integrity)
        .with_capabilities(wire.capabilities)
    }
}

fn deserialize_present_integrity<'de, D>(d: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    String::deserialize(d).map(Some)
}

fn validate_selection(
    observation: AgentStackObservationClass,
    selection: AgentStackSelectionState,
) -> Result<(), AgentStackComponentError> {
    if observation == AgentStackObservationClass::RepositoryObserved
        && matches!(
            selection,
            AgentStackSelectionState::Loaded | AgentStackSelectionState::Observed
        )
    {
        Err(AgentStackComponentError::SelectionNotSupported)
    } else {
        Ok(())
    }
}

fn validate_trust(
    observation: AgentStackObservationClass,
    trust: AgentStackTrustLevel,
) -> Result<(), AgentStackComponentError> {
    let allowed = match observation {
        AgentStackObservationClass::RepositoryObserved => matches!(
            trust,
            AgentStackTrustLevel::SelfDeclared | AgentStackTrustLevel::RepositoryObserved
        ),
        AgentStackObservationClass::RuntimeObserved => {
            trust != AgentStackTrustLevel::RunnerObserved
        }
        AgentStackObservationClass::RunnerObserved => true,
    };
    allowed
        .then_some(())
        .ok_or(AgentStackComponentError::TrustExceedsObservation)
}

fn has_duplicate_capabilities(capabilities: &[AgentStackCapability]) -> bool {
    (0..capabilities.len()).any(|index| capabilities[..index].contains(&capabilities[index]))
}
