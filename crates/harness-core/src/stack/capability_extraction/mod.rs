//! Advisory extraction of repository-declared Agent Stack capabilities.

use super::capability_evidence::{
    AgentStackCapabilityEvidence, AgentStackCapabilityEvidenceError, AgentStackCapabilityScope,
};
use super::inventory::{inventory_with_root, AgentStackInventoryError};
use super::{
    AgentStackCapability, AgentStackComponent, AgentStackComponentKind, AgentStackTrustLevel,
    Sha256Digest,
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
use typed::{extract_typed, TypedSource};
const DEFAULT_MAX_FILE_BYTES: u64 = 1024 * 1024;
const DEFAULT_MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
const MAX_REPOSITORY_FINDINGS: usize = 1024;
const REPOSITORY_LIMIT_RULE_ID: &str = "extraction.repository_finding_limit";

#[rustfmt::skip]
#[derive(Debug, Clone)]
pub struct AgentStackCapabilityExtractionOptions { root: PathBuf, inventory_options: super::AgentStackInventoryOptions, max_file_bytes: u64, max_total_bytes: u64 }

#[rustfmt::skip]
impl AgentStackCapabilityExtractionOptions {
    pub fn new(root: PathBuf) -> Self {
        Self { inventory_options: super::AgentStackInventoryOptions::new(root.clone()), root, max_file_bytes: DEFAULT_MAX_FILE_BYTES, max_total_bytes: DEFAULT_MAX_TOTAL_BYTES }
    }
    pub fn with_max_file_bytes(mut self, max_file_bytes: u64) -> Result<Self, AgentStackCapabilityExtractionError> {
        if max_file_bytes == 0 || max_file_bytes == u64::MAX { return Err(AgentStackCapabilityExtractionError::InvalidOptions); }
        self.max_total_bytes = DEFAULT_MAX_TOTAL_BYTES.max(max_file_bytes);
        self.inventory_options = self.inventory_options.clone().with_max_file_bytes(max_file_bytes).and_then(|options| options.with_max_total_bytes(self.max_total_bytes)).map_err(|_| AgentStackCapabilityExtractionError::InvalidOptions)?;
        self.max_file_bytes = max_file_bytes;
        Ok(self)
    }
    pub fn root(&self) -> &Path { &self.root }
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

macro_rules! wire_enum {
    ($name:ident { $($variant:ident => $wire:literal),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name { $($variant),+ }
        impl $name { pub const fn as_str(self) -> &'static str { match self { $(Self::$variant => $wire),+ } } }
    };
}
#[rustfmt::skip]
wire_enum!(AgentStackCapabilityExtractionConfidence { Low => "low", Medium => "medium", High => "high" });
#[rustfmt::skip]
wire_enum!(AgentStackCapabilityExtractionFailureKind { ReadFailed => "read_failed", LimitExceeded => "limit_exceeded", ParseFailed => "parse_failed", InvalidDeclaration => "invalid_declaration", EvidenceValidation => "evidence_validation" });

#[rustfmt::skip]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentStackCapabilityExtractionFailure { component: AgentStackComponent, kind: AgentStackCapabilityExtractionFailureKind, rule_id: Option<String>, reason: String }

#[rustfmt::skip]
impl AgentStackCapabilityExtractionFailure {
    fn new(component: &AgentStackComponent, kind: AgentStackCapabilityExtractionFailureKind, rule_id: Option<&str>, reason: impl Into<String>) -> Self {
        Self { component: component.clone(), kind, rule_id: rule_id.map(str::to_owned), reason: reason.into() }
    }
    pub fn component(&self) -> &AgentStackComponent { &self.component }
    pub const fn kind(&self) -> AgentStackCapabilityExtractionFailureKind { self.kind }
    pub fn rule_id(&self) -> Option<&str> { self.rule_id.as_deref() }
    pub fn reason(&self) -> &str { &self.reason }
}

#[rustfmt::skip]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentStackCapabilityExtractionEvidence { component: AgentStackComponent, evidence: AgentStackCapabilityEvidence, rule_id: String, reason: String, confidence: AgentStackCapabilityExtractionConfidence }

#[rustfmt::skip]
impl AgentStackCapabilityExtractionEvidence {
    fn new(component: &AgentStackComponent, raw: RawCapability) -> Result<Self, AgentStackCapabilityEvidenceError> {
        let evidence = AgentStackCapabilityEvidence::declared(component, raw.capability, component.source().clone(), None, raw.trust_level, raw.scope)?;
        Ok(Self { component: component.clone(), evidence, rule_id: raw.rule_id.to_owned(), reason: raw.reason, confidence: raw.confidence })
    }
    pub fn component(&self) -> &AgentStackComponent { &self.component }
    pub fn evidence(&self) -> &AgentStackCapabilityEvidence { &self.evidence }
    pub const fn capability(&self) -> AgentStackCapability { self.evidence.capability() }
    pub fn rule_id(&self) -> &str { &self.rule_id }
    pub fn reason(&self) -> &str { &self.reason }
    pub const fn confidence(&self) -> AgentStackCapabilityExtractionConfidence { self.confidence }
}

#[rustfmt::skip]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentStackCapabilityExtraction { evidence: Vec<AgentStackCapabilityExtractionEvidence>, failures: Vec<AgentStackCapabilityExtractionFailure> }

#[rustfmt::skip]
impl AgentStackCapabilityExtraction {
    pub fn evidence(&self) -> &[AgentStackCapabilityExtractionEvidence] { &self.evidence }
    pub fn failures(&self) -> &[AgentStackCapabilityExtractionFailure] { &self.failures }
}

#[rustfmt::skip]
#[derive(Debug, Clone)]
pub(super) struct RawCapability { capability: AgentStackCapability, rule_id: &'static str, reason: String, confidence: AgentStackCapabilityExtractionConfidence, trust_level: AgentStackTrustLevel, scope: AgentStackCapabilityScope }

#[rustfmt::skip]
pub fn extract_repository_capability_evidence(options: &AgentStackCapabilityExtractionOptions) -> Result<AgentStackCapabilityExtraction, AgentStackCapabilityExtractionError> {
    let root = Dir::open_ambient_dir(&options.root, cap_std::ambient_authority()).map_err(|_| AgentStackCapabilityExtractionError::RootOpen)?;
    let inventory = inventory_with_root(&root, &options.inventory_options).map_err(AgentStackCapabilityExtractionError::Inventory)?;
    let mut evidence = Vec::new();
    let mut failures = Vec::new();
    let mut remaining_bytes = options.max_total_bytes;
    let config_component = inventory.entries().iter().find(|entry| entry.component().source().locator().as_str() == "harness.toml" && entry.component().kind() == AgentStackComponentKind::Validation).map(|entry| entry.component());
    let mut config_text = config_component.and_then(|component| match read_text(&root, component, "harness.toml", options.max_file_bytes, &mut remaining_bytes) {
        Ok(text) => Some(text), Err((kind, reason)) => { failures.push(AgentStackCapabilityExtractionFailure::new(component, kind, None, reason)); None }
    });
    let config = config_text.as_deref().and_then(|text| toml::from_str::<toml::Value>(text).ok());
    let rules = config.as_ref().and_then(|value| value.get("rules"));
    let normalize = |value: &str| super::inventory::normalize_extraction_source(value).ok().flatten();
    let configured_paths = |name| rules.and_then(|rules| rules.get(name)?.as_array()).into_iter().flatten().filter_map(|value| normalize(value.as_str()?));
    let requirements_path = rules.and_then(|rules| rules.get("requirements_path")?.as_str()).and_then(normalize);
    let exec_policy_paths = configured_paths("exec_policy_paths").collect::<BTreeSet<_>>();
    let markdown_policy_paths = configured_paths("discovery_paths").chain(rules.and_then(|rules| rules.get("builtin_path")?.as_str()).and_then(normalize)).collect::<BTreeSet<_>>();
    let mut exclusions = RepositoryExclusions::default();
    for entry in inventory.entries() {
        let component = entry.component();
        let locator = component.source().locator().as_str();
        if evidence.len() + failures.len() >= MAX_REPOSITORY_FINDINGS - 1 { failures.push(repository_limit_failure(component)); break; }
        if !is_supported_control(component.kind(), locator) || exclusions.excludes(&root, locator) { continue; }
        let text = match if locator == "harness.toml" && component.kind() == AgentStackComponentKind::Validation { config_text.take().ok_or((AgentStackCapabilityExtractionFailureKind::ReadFailed, String::new())) } else { read_text(&root, component, locator, options.max_file_bytes, &mut remaining_bytes) } {
            Ok(text) => text,
            Err((kind, reason)) => { if !reason.is_empty() { failures.push(AgentStackCapabilityExtractionFailure::new(component, kind, None, reason)); } continue; }
        };
        let mut typed = Vec::new();
        let mut component_failures = Vec::new();
        let source_kind = if locator == "requirements.toml" || requirements_path.as_deref() == Some(locator) { TypedSource::Requirements }
            else if locator.ends_with(".star") || exec_policy_paths.contains(locator) { TypedSource::Starlark }
            else if markdown_policy_paths.iter().any(|path| locator == path || locator.strip_prefix(path).is_some_and(|suffix| suffix.starts_with('/'))) { TypedSource::MarkdownPolicy }
            else { TypedSource::Auto };
        extract_typed(component, locator, &text, source_kind, &mut typed, &mut component_failures);
        let typed_capabilities = typed.iter().map(|raw| raw.capability.as_str()).collect::<BTreeSet<_>>();
        let mut raw = typed;
        for static_capability in extract_static(component, locator, &text) {
            if raw.len() + component_failures.len() >= typed::MAX_COMPONENT_FINDINGS - 1 { typed::record_limit(component, &raw, &mut component_failures); break; }
            if !typed_capabilities.contains(static_capability.capability.as_str()) { raw.push(static_capability); }
        }
        let available = MAX_REPOSITORY_FINDINGS - evidence.len() - failures.len() - 1;
        let limited = raw.len() + component_failures.len() > available;
        if limited { component_failures.truncate(available); raw.truncate(available - component_failures.len()); }
        failures.extend(component_failures);
        append_validated(component, raw, &mut evidence, &mut failures);
        if limited { failures.push(repository_limit_failure(component)); break; }
    }

    evidence.sort_by_key(evidence_sort_key);
    failures.sort_by_key(failure_sort_key);
    Ok(AgentStackCapabilityExtraction { evidence, failures })
}

#[rustfmt::skip]
#[derive(Default)]
struct RepositoryExclusions { loaded: BTreeSet<String>, rules: Vec<IgnoreRule> }
#[rustfmt::skip]
struct IgnoreRule { base: String, pattern: String, negated: bool, directory_only: bool, anchored: bool }

#[rustfmt::skip]
impl RepositoryExclusions {
    fn excludes(&mut self, root: &Dir, locator: &str) -> bool {
        if locator.split('/').any(|segment| segment == "generated") { return true; }
        self.load(root, "");
        let mut base = String::new();
        for segment in locator.split('/').take(locator.matches('/').count()) {
            if !base.is_empty() { base.push('/'); } base.push_str(segment);
            if self.ignored(&base, true) { return true; } self.load(root, &base);
        }
        self.ignored(locator, false)
    }
    fn ignored(&self, locator: &str, is_directory: bool) -> bool { self.rules.iter().filter(|rule| rule.matches(locator, is_directory)).fold(false, |_, rule| !rule.negated) }
    fn load(&mut self, root: &Dir, base: &str) {
        if !self.loaded.insert(base.to_owned()) { return; }
        let path = if base.is_empty() { ".gitignore".to_owned() } else { format!("{base}/.gitignore") };
        let Some(text) = read_optional_control(root, &path) else { return; };
        for raw in text.lines() {
            let mut line = raw.trim_end_matches(['\r', ' ']);
            if line.is_empty() || line.starts_with('#') { continue; }
            let escaped_marker = line.starts_with("\\#") || line.starts_with("\\!");
            if escaped_marker { line = &line[1..]; } let negated = !escaped_marker && line.starts_with('!'); if negated { line = &line[1..]; }
            let directory_only = line.ends_with('/');
            line = line.trim_end_matches('/'); let anchored = line.starts_with('/'); line = line.trim_start_matches('/');
            if !line.is_empty() { self.rules.push(IgnoreRule { base: base.to_owned(), pattern: line.to_owned(), negated, directory_only, anchored }); }
        }
    }
}

#[rustfmt::skip]
impl IgnoreRule {
    fn matches(&self, locator: &str, is_directory: bool) -> bool {
        let Some(relative) = self.base.is_empty().then_some(locator).or_else(|| locator.strip_prefix(&format!("{}/", self.base))) else { return false; };
        if self.anchored || self.pattern.contains('/') { return path_or_parent_matches(&self.pattern, relative, self.directory_only, is_directory); }
        let segment_count = relative.matches('/').count() + 1;
        relative.split('/').enumerate().any(|(index, segment)| glob_matches(&self.pattern, segment) && (!self.directory_only || index + 1 < segment_count || is_directory))
    }
}

#[rustfmt::skip]
fn path_or_parent_matches(pattern: &str, path: &str, directory_only: bool, is_directory: bool) -> bool { path.match_indices('/').map(|(index, _)| index).chain(std::iter::once(path.len())).any(|end| glob_matches(pattern, &path[..end]) && (!directory_only || end < path.len() || is_directory)) }

#[rustfmt::skip]
fn glob_matches(pattern: &str, text: &str) -> bool {
    fn visit(pattern: &[u8], text: &[u8], pi: usize, ti: usize, failed: &mut BTreeSet<(usize, usize)>) -> bool {
        if !failed.insert((pi, ti)) { return false; } else if pi == pattern.len() { return ti == text.len(); }
        if pattern[pi] == b'*' {
            let double = pattern.get(pi + 1) == Some(&b'*');
            let next = pi + 1 + usize::from(double);
            if double && pattern.get(next) == Some(&b'/') && visit(pattern, text, next + 1, ti, failed) { return true; }
            return visit(pattern, text, next, ti, failed) || ti < text.len() && (double || text[ti] != b'/') && visit(pattern, text, pi, ti + 1, failed);
        }
        let (expected, next) = if pattern[pi] == b'\\' && pi + 1 < pattern.len() { (pattern[pi + 1], pi + 2) } else { (pattern[pi], pi + 1) };
        ti < text.len() && (expected == b'?' && text[ti] != b'/' || expected == text[ti]) && visit(pattern, text, next, ti + 1, failed)
    }
    visit(pattern.as_bytes(), text.as_bytes(), 0, 0, &mut BTreeSet::new())
}

#[rustfmt::skip]
fn read_optional_control(root: &Dir, locator: &str) -> Option<String> {
    let mut options = OpenOptions::new(); options.read(true);
    #[cfg(unix)] { use cap_std::fs::OpenOptionsExt; options.custom_flags(libc::O_NONBLOCK); }
    let file = root.open_with(locator, &options).ok()?;
    if !file.metadata().ok()?.is_file() { return None; }
    let mut bytes = Vec::new(); file.take(DEFAULT_MAX_FILE_BYTES + 1).read_to_end(&mut bytes).ok()?;
    (bytes.len() as u64 <= DEFAULT_MAX_FILE_BYTES).then(|| String::from_utf8(bytes).ok()).flatten()
}

#[rustfmt::skip]
fn is_supported_control(kind: AgentStackComponentKind, locator: &str) -> bool {
    matches!(kind, AgentStackComponentKind::McpServer | AgentStackComponentKind::Policy | AgentStackComponentKind::Hook) || matches!(locator, "harness.toml" | ".harness/config.toml")
}

#[rustfmt::skip]
fn read_text(root: &Dir, component: &AgentStackComponent, locator: &str, max_file_bytes: u64, remaining_bytes: &mut u64) -> Result<String, (AgentStackCapabilityExtractionFailureKind, String)> {
    if *remaining_bytes == 0 { return Err((AgentStackCapabilityExtractionFailureKind::LimitExceeded, format!("{locator} exceeds the capability extraction total byte limit"))); }
    let mut options = OpenOptions::new(); options.read(true);
    #[cfg(unix)] { use cap_std::fs::OpenOptionsExt; options.custom_flags(libc::O_NONBLOCK); }
    let read_limit = max_file_bytes.min(*remaining_bytes) + 1;
    let mut file = root.open_with(locator, &options).map_err(|_| (
        AgentStackCapabilityExtractionFailureKind::ReadFailed, format!("failed to open {locator} for capability extraction")
    ))?.take(read_limit);
    let mut bytes = Vec::new();
    let read_result = file.read_to_end(&mut bytes);
    let bytes_read = bytes.len() as u64;
    let total_exceeded = bytes_read > *remaining_bytes;
    *remaining_bytes = remaining_bytes.saturating_sub(bytes_read);
    if total_exceeded { return Err((AgentStackCapabilityExtractionFailureKind::LimitExceeded, format!("{locator} exceeds the capability extraction total byte limit"))); }
    read_result.map_err(|_| (AgentStackCapabilityExtractionFailureKind::ReadFailed, format!("failed to read {locator} for capability extraction")))?;
    if bytes.len() as u64 > max_file_bytes { return Err((AgentStackCapabilityExtractionFailureKind::LimitExceeded, format!("{locator} exceeds the capability extraction byte limit"))); }
    if component.integrity().is_some_and(|expected| expected != &Sha256Digest::from_bytes(&bytes)) { return Err((AgentStackCapabilityExtractionFailureKind::ReadFailed, format!("{locator} changed after inventory; capability extraction skipped it"))); }
    String::from_utf8(bytes).map_err(|_| (AgentStackCapabilityExtractionFailureKind::ReadFailed, format!("{locator} is not valid UTF-8; capability extraction skipped it")))
}

#[rustfmt::skip]
fn repository_limit_failure(component: &AgentStackComponent) -> AgentStackCapabilityExtractionFailure {
    AgentStackCapabilityExtractionFailure::new(component, AgentStackCapabilityExtractionFailureKind::LimitExceeded, Some(REPOSITORY_LIMIT_RULE_ID), "repository exceeds the capability extraction finding limit")
}

#[rustfmt::skip]
fn append_validated(component: &AgentStackComponent, raw: Vec<RawCapability>, evidence: &mut Vec<AgentStackCapabilityExtractionEvidence>, failures: &mut Vec<AgentStackCapabilityExtractionFailure>) {
    let mut seen = BTreeSet::new();
    for item in raw {
        if !seen.insert((item.capability.as_str(), item.rule_id, item.reason.clone())) { continue; }
        match AgentStackCapabilityExtractionEvidence::new(component, item) {
            Ok(item) => evidence.push(item),
            Err(error) => failures.push(AgentStackCapabilityExtractionFailure::new(component, AgentStackCapabilityExtractionFailureKind::EvidenceValidation, None, format!("capability evidence validation failed: {error}"))),
        }
    }
}

#[rustfmt::skip]
pub(super) fn parse_capability(value: &str) -> Option<AgentStackCapability> { AgentStackCapability::ALL.iter().copied().find(|capability| capability.as_str() == value) }

#[rustfmt::skip]
pub(super) fn push_unique(capabilities: &mut Vec<AgentStackCapability>, capability: AgentStackCapability) { if !capabilities.contains(&capability) { capabilities.push(capability); } }

#[rustfmt::skip]
pub(super) fn declared_raw(capability: AgentStackCapability, rule_id: &'static str, reason: String) -> RawCapability {
    RawCapability { capability, rule_id, reason, confidence: AgentStackCapabilityExtractionConfidence::High, trust_level: AgentStackTrustLevel::SelfDeclared, scope: AgentStackCapabilityScope::Component }
}

#[rustfmt::skip]
pub(super) fn inferred_raw(capability: AgentStackCapability, rule_id: &'static str, reason: String, confidence: AgentStackCapabilityExtractionConfidence) -> RawCapability {
    RawCapability { capability, rule_id, reason, confidence, trust_level: AgentStackTrustLevel::RepositoryObserved, scope: scope_for_capability(capability) }
}

#[rustfmt::skip]
fn scope_for_capability(capability: AgentStackCapability) -> AgentStackCapabilityScope { match capability { AgentStackCapability::Network => AgentStackCapabilityScope::network(None::<String>).expect("network scope without endpoint is valid"), _ => AgentStackCapabilityScope::Component } }
#[rustfmt::skip]
fn evidence_sort_key(item: &AgentStackCapabilityExtractionEvidence) -> (String, String, String) {
    (item.component().source().locator().as_str().to_owned(), item.evidence().capability().as_str().to_owned(), item.rule_id().to_owned())
}

#[rustfmt::skip]
fn failure_sort_key(item: &AgentStackCapabilityExtractionFailure) -> (String, String, String) {
    (item.component().source().locator().as_str().to_owned(), item.kind().as_str().to_owned(), item.reason().to_owned())
}
