use harness_core::compress::NapStatus;
use harness_core::run_id::RunId;
use harness_core::types::{ProjectId, ThreadId};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use thiserror::Error;

pub type Tokens = u32;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ProviderId(pub String);

impl ProviderId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ProviderId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ItemId(pub String);

impl ItemId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ItemId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComposeRequest {
    pub thread_id: ThreadId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<RunId>,
    pub project: ProjectId,
    pub task_profile: TaskProfile,
    pub budget_hint: Tokens,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TaskProfile {
    #[serde(default)]
    pub task_kind: Option<String>,
    #[serde(default)]
    pub target_paths: Vec<PathBuf>,
    #[serde(default)]
    pub agent_kind: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub contract: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemClass {
    Rule,
    Skill,
    Contract,
    Brief,
    Draft,
}

impl ItemClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rule => "rule",
            Self::Skill => "skill",
            Self::Contract => "contract",
            Self::Brief => "brief",
            Self::Draft => "draft",
        }
    }

    pub(crate) fn section_title(self) -> &'static str {
        match self {
            Self::Contract => "Contract",
            Self::Rule => "Rules",
            Self::Skill => "Skills",
            Self::Brief => "Brief",
            Self::Draft => "Drafts",
        }
    }

    pub(crate) fn render_order(self) -> u8 {
        match self {
            Self::Contract => 0,
            Self::Rule => 1,
            Self::Skill => 2,
            Self::Brief => 3,
            Self::Draft => 4,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    P0,
    P1,
    P2,
}

impl Priority {
    pub(crate) fn rank(self) -> u8 {
        match self {
            Self::P0 => 0,
            Self::P1 => 1,
            Self::P2 => 2,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "level", content = "content", rename_all = "snake_case")]
pub enum Degraded {
    Summary(String),
    Pointer(String),
    /// LLM-compressed rendition with a NAP verification status
    /// (GH1574). Produced asynchronously by the caller before compose;
    /// the composer only selects, it never calls a model.
    Summarized {
        text: String,
        nap: NapStatus,
    },
}

impl Degraded {
    pub(crate) fn level_name(&self) -> &'static str {
        match self {
            Self::Summary(_) => "summary",
            Self::Pointer(_) => "pointer",
            Self::Summarized { .. } => "summarized",
        }
    }

    pub(crate) fn content(&self) -> &str {
        match self {
            Self::Summary(content) | Self::Pointer(content) => content,
            Self::Summarized { text, .. } => text,
        }
    }

    pub(crate) fn nap(&self) -> Option<NapStatus> {
        match self {
            Self::Summarized { nap, .. } => Some(*nap),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextItem {
    pub id: ItemId,
    pub class: ItemClass,
    pub content: String,
    pub est_tokens: Tokens,
    pub priority: Priority,
    pub relevance: f32,
    #[serde(default)]
    pub degrade: Vec<Degraded>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dedupe_key: Option<String>,
    pub instruction_bearing: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderError {
    pub provider_id: ProviderId,
    pub message: String,
}

impl ProviderError {
    pub fn new(provider_id: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            provider_id: ProviderId::new(provider_id),
            message: message.into(),
        }
    }
}

pub trait ContextProvider: Send + Sync {
    fn id(&self) -> ProviderId;
    fn propose(&self, req: &ComposeRequest) -> Result<Vec<ContextItem>, ProviderError>;
}

pub trait Estimator: Send + Sync {
    fn estimate(&self, content: &str) -> Tokens;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct BytesDivFourEstimator;

impl Estimator for BytesDivFourEstimator {
    fn estimate(&self, content: &str) -> Tokens {
        if content.is_empty() {
            0
        } else {
            ((content.len() as Tokens) / 4).max(1)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComposeMode {
    Shadow,
    Enforce,
    Preview,
}

impl From<harness_core::config::misc::ContextMode> for ComposeMode {
    fn from(value: harness_core::config::misc::ContextMode) -> Self {
        match value {
            harness_core::config::misc::ContextMode::Shadow => Self::Shadow,
            harness_core::config::misc::ContextMode::Enforce => Self::Enforce,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextQuotas {
    pub rule: f32,
    pub skill: f32,
    pub contract: f32,
    pub brief: f32,
    pub draft: f32,
}

impl Default for ContextQuotas {
    fn default() -> Self {
        Self {
            rule: 0.30,
            skill: 0.25,
            contract: 0.25,
            brief: 0.15,
            draft: 0.05,
        }
    }
}

impl ContextQuotas {
    pub(crate) fn for_class(&self, class: ItemClass) -> f32 {
        match class {
            ItemClass::Rule => self.rule,
            ItemClass::Skill => self.skill,
            ItemClass::Contract => self.contract,
            ItemClass::Brief => self.brief,
            ItemClass::Draft => self.draft,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComposeConfig {
    pub mode: ComposeMode,
    pub budget_tokens: Tokens,
    pub reserved_headroom: f32,
    pub provider_timeout_ms: u64,
    pub quotas: ContextQuotas,
}

impl Default for ComposeConfig {
    fn default() -> Self {
        Self {
            mode: ComposeMode::Shadow,
            budget_tokens: 24_000,
            reserved_headroom: 0.20,
            provider_timeout_ms: 2_000,
            quotas: ContextQuotas::default(),
        }
    }
}

impl From<&harness_core::config::misc::ContextConfig> for ComposeConfig {
    fn from(value: &harness_core::config::misc::ContextConfig) -> Self {
        Self {
            mode: value.mode.into(),
            budget_tokens: value.budget_tokens,
            reserved_headroom: value.reserved_headroom,
            provider_timeout_ms: value.provider_timeout_ms,
            quotas: ContextQuotas {
                rule: value.quotas.rule,
                skill: value.quotas.skill,
                contract: value.quotas.contract,
                brief: value.quotas.brief,
                draft: value.quotas.draft,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetManifest {
    pub total: Tokens,
    pub effective: Tokens,
    pub used: Tokens,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManifestDecision {
    Included,
    Degraded,
    Excluded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestItem {
    pub id: ItemId,
    pub provider_id: ProviderId,
    pub class: ItemClass,
    pub decision: ManifestDecision,
    pub tokens: Tokens,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_tokens: Option<Tokens>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compressed_tokens: Option<Tokens>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nap: Option<NapStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComposeManifest {
    pub v: u8,
    pub thread_id: ThreadId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<RunId>,
    pub mode: ComposeMode,
    pub budget: BudgetManifest,
    pub items: Vec<ManifestItem>,
    pub provider_errors: Vec<ProviderError>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Composition {
    pub rendered: String,
    pub manifest: ComposeManifest,
}

#[derive(Debug, Error)]
pub enum ComposeError {
    #[error(
        "mandatory context exceeds effective budget: mandatory={mandatory}, effective={effective}"
    )]
    MandatoryOverflow {
        mandatory: Tokens,
        effective: Tokens,
        manifest: Box<ComposeManifest>,
    },
}

impl ComposeError {
    pub fn manifest(&self) -> &ComposeManifest {
        match self {
            Self::MandatoryOverflow { manifest, .. } => manifest,
        }
    }
}
