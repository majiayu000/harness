use super::{
    AgentStackComponent, AgentStackComponentKind, AgentStackFreshnessEvidence,
    AgentStackObservationClass, AgentStackSelectionState, AgentStackSource, AgentStackSourceScope,
    AgentStackTrustLevel, Sha256Digest,
};
use cap_std::fs::{Dir, FileType, OpenOptions};
use serde::Deserialize;
use std::collections::{BTreeMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct AgentStackInventoryOptions {
    root: PathBuf,
    max_file_bytes: u64,
    max_total_bytes: u64,
    max_files: usize,
    max_directories: usize,
    max_total_entries: usize,
    max_depth: usize,
    max_entries_per_directory: usize,
    #[cfg(test)]
    injected_read_failure: Option<String>,
}

macro_rules! limit_setters {
    ($($setter:ident => $field:ident: $ty:ty),+ $(,)?) => {
        $(pub fn $setter(mut self, value: $ty) -> Result<Self, AgentStackInventoryError> {
            self.$field = value;
            self.validated()
        })+
    };
}

impl AgentStackInventoryOptions {
    #[rustfmt::skip]
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            max_file_bytes: 1024 * 1024, max_total_bytes: 64 * 1024 * 1024,
            max_files: 10_000, max_directories: 1_000, max_total_entries: 50_000,
            max_depth: 32, max_entries_per_directory: 10_000,
            #[cfg(test)] injected_read_failure: None,
        }
    }

    limit_setters!(
        with_max_file_bytes => max_file_bytes: u64, with_max_total_bytes => max_total_bytes: u64,
        with_max_files => max_files: usize, with_max_directories => max_directories: usize,
        with_max_total_entries => max_total_entries: usize, with_max_depth => max_depth: usize,
        with_max_entries_per_directory => max_entries_per_directory: usize,
    );

    #[cfg(test)]
    #[rustfmt::skip]
    pub(super) fn with_injected_read_failure(mut self, locator: &str) -> Self { self.injected_read_failure = Some(locator.to_owned()); self }

    fn validated(self) -> Result<Self, AgentStackInventoryError> {
        let bytes_ok = |v: u64| v > 0 && v < u64::MAX;
        let count_ok = |v: usize| v > 0 && v < usize::MAX;
        if bytes_ok(self.max_file_bytes)
            && bytes_ok(self.max_total_bytes)
            && count_ok(self.max_files)
            && count_ok(self.max_directories)
            && count_ok(self.max_total_entries)
            && count_ok(self.max_depth)
            && count_ok(self.max_entries_per_directory)
        {
            Ok(self)
        } else {
            Err(AgentStackInventoryError::new(EK::InvalidOptions, None))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentStackEntryClass {
    RegularFile { unix_executable: Option<bool> },
    DirectoryPresence,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentStackInventoryEntry {
    component: AgentStackComponent,
    entry_class: AgentStackEntryClass,
}

#[rustfmt::skip]
impl AgentStackInventoryEntry {
    pub fn component(&self) -> &AgentStackComponent { &self.component }
    pub fn entry_class(&self) -> &AgentStackEntryClass { &self.entry_class }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentStackInventory {
    entries: Vec<AgentStackInventoryEntry>,
}

#[rustfmt::skip]
impl AgentStackInventory {
    pub fn entries(&self) -> &[AgentStackInventoryEntry] { &self.entries }
}

macro_rules! error_kinds {
    ($($variant:ident => $wire:literal),+ $(,)?) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum AgentStackInventoryErrorKind { $($variant),+ }
        impl AgentStackInventoryErrorKind {
            pub const fn as_str(&self) -> &'static str {
                match self { $(Self::$variant => $wire),+ }
            }
        }
    };
}
#[rustfmt::skip]
error_kinds!(
    InvalidOptions => "invalid_options", RootOpen => "root_open",
    EntryMetadata => "entry_metadata", BrokenSymlink => "broken_symlink",
    RootEscape => "root_escape", NonRegularEntry => "non_regular_entry",
    NonUtf8Locator => "non_utf8_locator", ReadFailed => "read_failed",
    EntryRaced => "entry_raced", CycleDetected => "cycle_detected",
    ConfigParse => "config_parse", ConfiguredSourceInvalid => "configured_source_invalid",
    ConfiguredSourceMissing => "configured_source_missing", LimitExceeded => "limit_exceeded",
    ComponentValidation => "component_validation",
);
use AgentStackComponentKind as Kind;
use AgentStackInventoryErrorKind as EK;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentStackInventoryError {
    kind: AgentStackInventoryErrorKind,
    locator: Option<String>,
}

#[rustfmt::skip]
impl AgentStackInventoryError {
    fn new(kind: AgentStackInventoryErrorKind, locator: Option<String>) -> Self {
        Self { kind, locator }
    }
    pub const fn kind(&self) -> AgentStackInventoryErrorKind { self.kind }
    pub fn locator(&self) -> Option<&str> { self.locator.as_deref() }
}

impl fmt::Display for AgentStackInventoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "agent stack inventory {}", self.kind.as_str())?;
        if let Some(locator) = &self.locator {
            write!(f, ": {locator}")?;
        }
        Ok(())
    }
}

impl std::error::Error for AgentStackInventoryError {}

type IErr = AgentStackInventoryError;

fn err(kind: AgentStackInventoryErrorKind, locator: &str) -> IErr {
    IErr::new(kind, (!locator.is_empty()).then(|| locator.to_owned()))
}

#[derive(Debug, Clone, Copy)]
struct DirSelector {
    direct_extensions: &'static [&'static str],
    direct_basenames: &'static [&'static str],
    recursive_extensions: &'static [&'static str],
    recursive_basenames: &'static [&'static str],
}

const NONE: &[&str] = &[];

#[rustfmt::skip]
const fn sel(
    direct_extensions: &'static [&'static str],
    direct_basenames: &'static [&'static str],
    recursive_extensions: &'static [&'static str],
    recursive_basenames: &'static [&'static str],
) -> DirSelector {
    DirSelector { direct_extensions, direct_basenames, recursive_extensions, recursive_basenames }
}

#[rustfmt::skip]
mod selectors {
    use super::{sel, DirSelector, NONE};
    pub(super) const SKILL_ROOT: DirSelector = sel(&["md"], NONE, NONE, &["SKILL.md"]); pub(super) const HARNESS_SKILLS: DirSelector = sel(&["md"], NONE, NONE, NONE);
    pub(super) const GUARDS: DirSelector = sel(&["sh"], NONE, NONE, NONE); pub(super) const WORKFLOWS: DirSelector = sel(&["yml", "yaml"], NONE, NONE, NONE);
    pub(super) const MD_TOML: DirSelector = sel(NONE, NONE, &["md", "toml"], NONE); pub(super) const SG: DirSelector = sel(NONE, NONE, &["yml", "yaml"], NONE);
    pub(super) const CURSOR_RULES: DirSelector = sel(NONE, NONE, &["md", "mdc"], NONE); pub(super) const VIBEGUARD: DirSelector = sel(NONE, NONE, &["md", "toml", "yaml", "yml", "json", "json5"], NONE);
    pub(super) const REMEM: DirSelector = sel(NONE, NONE, &["toml", "yaml", "yml", "json"], NONE); pub(super) const GITHOOKS: DirSelector = sel(NONE, super::GIT_LIFECYCLE_HOOKS, NONE, NONE);
}
use selectors::*;

#[rustfmt::skip]
const GIT_LIFECYCLE_HOOKS: &[&str] = &[
    "applypatch-msg", "pre-applypatch", "post-applypatch", "pre-commit", "pre-merge-commit", "prepare-commit-msg", "commit-msg", "post-commit",
    "pre-rebase", "post-checkout", "post-merge", "pre-push", "pre-receive", "update", "proc-receive", "post-receive", "post-update",
    "reference-transaction", "push-to-checkout", "pre-auto-gc", "post-rewrite", "sendemail-validate", "fsmonitor-watchman", "p4-changelist",
    "p4-prepare-changelist", "p4-post-changelist", "p4-pre-submit", "post-index-change",
];

impl DirSelector {
    const fn is_recursive(&self) -> bool {
        !self.recursive_extensions.is_empty() || !self.recursive_basenames.is_empty()
    }

    fn matches(&self, name: &OsStr, direct_level: bool) -> bool {
        let ext_match = |ext: &&str| has_extension(name, ext);
        let base_match = |base: &&str| name == OsStr::new(base);
        self.recursive_extensions.iter().any(ext_match)
            || self.recursive_basenames.iter().any(base_match)
            || (direct_level
                && (self.direct_extensions.iter().any(ext_match)
                    || self.direct_basenames.iter().any(base_match)))
    }
}

#[rustfmt::skip]
fn has_extension(name: &OsStr, ext: &str) -> bool { Path::new(name).extension() == Some(OsStr::new(ext)) }
#[rustfmt::skip]
fn has_suffix(name: &OsStr, suffix: &str) -> bool { name.as_encoded_bytes().ends_with(suffix.as_bytes()) }

#[derive(Debug, Clone, Copy)]
enum RuleTarget {
    File,
    Directory(DirSelector),
    FileOrDirectory(DirSelector),
    DirectoryPresence,
}

#[derive(Debug, Clone, Copy)]
enum Matcher {
    Exact(&'static str),
    RootSuffix(&'static str),
}

#[derive(Debug, Clone, Copy)]
struct StaticRule {
    matcher: Matcher,
    target: RuleTarget,
    kind: AgentStackComponentKind,
}

#[rustfmt::skip]
const fn fr(path: &'static str, kind: Kind) -> StaticRule {
    StaticRule { matcher: Matcher::Exact(path), target: RuleTarget::File, kind }
}
#[rustfmt::skip]
const fn dr(path: &'static str, sel: DirSelector, kind: Kind) -> StaticRule {
    StaticRule { matcher: Matcher::Exact(path), target: RuleTarget::Directory(sel), kind }
}
#[rustfmt::skip]
const fn sfx(suffix: &'static str, kind: Kind) -> StaticRule {
    StaticRule { matcher: Matcher::RootSuffix(suffix), target: RuleTarget::File, kind }
}

#[rustfmt::skip]
const STATIC_RULES: &[StaticRule] = &[
    fr("AGENTS.md", Kind::Instructions), fr("AGENTS.override.md", Kind::Instructions), fr("CLAUDE.md", Kind::Instructions), fr("WORKFLOW.md", Kind::Workflow), fr("MEMORY.md", Kind::Memory),
    fr("src/AGENTS.md", Kind::Instructions), fr("src/AGENTS.override.md", Kind::Instructions), fr("src/CLAUDE.md", Kind::Instructions), fr("crates/AGENTS.md", Kind::Instructions), fr("crates/AGENTS.override.md", Kind::Instructions), fr("crates/CLAUDE.md", Kind::Instructions),
    fr("lib/AGENTS.md", Kind::Instructions), fr("lib/AGENTS.override.md", Kind::Instructions), fr("lib/CLAUDE.md", Kind::Instructions), fr("pkg/AGENTS.md", Kind::Instructions), fr("pkg/AGENTS.override.md", Kind::Instructions), fr("pkg/CLAUDE.md", Kind::Instructions),
    dr(".claude/skills", SKILL_ROOT, Kind::Skill), dr(".codex/skills", SKILL_ROOT, Kind::Skill), dr(".agents/skills", SKILL_ROOT, Kind::Skill), dr("skills", SKILL_ROOT, Kind::Skill), dr(".harness/skills", HARNESS_SKILLS, Kind::Skill),
    dr(".harness/guards", GUARDS, Kind::Hook), dr(".githooks", GITHOOKS, Kind::Hook), fr(".mcp.json", Kind::McpServer), fr("mcp.json", Kind::McpServer), dr(".vibeguard", VIBEGUARD, Kind::Policy), fr(".vibeguard/run-guards.sh", Kind::Validation),
    dr("rules", MD_TOML, Kind::Policy), fr("requirements.toml", Kind::Policy), dr(".remem", REMEM, Kind::Memory), fr("remem.toml", Kind::Memory), fr(".harness/config.toml", Kind::Validation), dr(".harness/rules", MD_TOML, Kind::Policy),
    dr(".harness/sg", SG, Kind::Policy), fr("harness.toml", Kind::Validation), dr(".github/workflows", WORKFLOWS, Kind::Workflow), dr(".cursor/rules", CURSOR_RULES, Kind::Policy),
    fr("Cargo.toml", Kind::Validation), fr("go.mod", Kind::Validation), fr("package.json", Kind::Validation), fr("pyproject.toml", Kind::Validation), fr("setup.py", Kind::Validation), fr("requirements.txt", Kind::Validation),
    fr("build.gradle", Kind::Validation), fr("build.gradle.kts", Kind::Validation), fr("pom.xml", Kind::Validation), fr("Gemfile", Kind::Validation), fr("yarn.lock", Kind::Validation), fr("pnpm-lock.yaml", Kind::Validation),
    fr(".eslintrc", Kind::Validation), fr(".eslintrc.js", Kind::Validation), fr(".eslintrc.cjs", Kind::Validation), fr(".eslintrc.json", Kind::Validation), fr(".eslintrc.yaml", Kind::Validation), fr(".eslintrc.yml", Kind::Validation),
    fr("eslint.config.js", Kind::Validation), fr("eslint.config.mjs", Kind::Validation), fr("eslint.config.cjs", Kind::Validation), fr("biome.json", Kind::Validation), fr(".rubocop.yml", Kind::Validation),
    sfx(".csproj", Kind::Validation), sfx(".sln", Kind::Validation), StaticRule { matcher: Matcher::Exact("spec"), target: RuleTarget::DirectoryPresence, kind: Kind::Validation },
    fr("Makefile", Kind::Validation), fr("justfile", Kind::Validation),
];

#[rustfmt::skip]
#[derive(Debug, Default, Deserialize)]
struct ConfigShape {
    #[serde(default)] rules: Option<ConfigRules>,
}

#[rustfmt::skip]
#[derive(Debug, Default, Deserialize)]
struct ConfigRules {
    #[serde(default)] discovery_paths: Vec<String>,
    #[serde(default)] builtin_path: Option<String>,
    #[serde(default)] exec_policy_paths: Vec<String>,
    #[serde(default)] requirements_path: Option<String>,
}

#[rustfmt::skip]
fn normalize_configured_source(raw: &str) -> Result<Option<String>, AgentStackInventoryErrorKind> {
    let bytes = raw.as_bytes();
    if raw.starts_with('/')
        || raw.starts_with('\\')
        || (bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
            && matches!(bytes[2], b'/' | b'\\'))
    {
        return Ok(None);
    }
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return Err(EK::ConfiguredSourceInvalid);
    }
    if raw.is_empty() || raw.contains('\0') || raw.contains('\\') {
        return Err(EK::ConfiguredSourceInvalid);
    }
    let mut segments = Vec::new();
    for segment in raw.split('/') {
        match segment {
            "" | "." => {}
            ".." => return Err(EK::ConfiguredSourceInvalid),
            other => segments.push(other),
        }
    }
    if segments.is_empty() {
        Err(EK::ConfiguredSourceInvalid)
    } else {
        Ok(Some(segments.join("/")))
    }
}

#[rustfmt::skip]
pub(super) fn classify_resolution_failure(
    root: &Dir, path: impl AsRef<Path>, kind: std::io::ErrorKind,
) -> AgentStackInventoryErrorKind {
    if kind != std::io::ErrorKind::NotFound { return EK::RootEscape; }
    match root.symlink_metadata(path) {
        Ok(meta) if meta.is_symlink() => EK::BrokenSymlink,
        Ok(_) => EK::EntryRaced,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => EK::EntryRaced,
        Err(_) => EK::EntryMetadata,
    }
}

#[rustfmt::skip]
pub(super) fn classify_open_failure(kind: std::io::ErrorKind) -> AgentStackInventoryErrorKind {
    if kind == std::io::ErrorKind::NotFound { EK::EntryRaced } else { EK::ReadFailed }
}

#[rustfmt::skip]
fn observed_non_regular(root: &Dir, path: &str) -> bool {
    match root.symlink_metadata(path) {
        Ok(meta) if meta.is_symlink() => root.metadata(path).ok().is_some_and(|target| !target.is_file()),
        Ok(meta) => !meta.is_file(), Err(_) => false,
    }
}

#[rustfmt::skip]
pub(super) fn classify_selected_open_failure(
    root: &Dir, path: &str, kind: std::io::ErrorKind,
) -> AgentStackInventoryErrorKind {
    if kind == std::io::ErrorKind::NotFound { classify_resolution_failure(root, path, kind) }
    else if observed_non_regular(root, path) { EK::NonRegularEntry } else { classify_open_failure(kind) }
}

#[rustfmt::skip]
pub(super) fn classify_entry_metadata_failure(kind: std::io::ErrorKind) -> AgentStackInventoryErrorKind {
    if kind == std::io::ErrorKind::NotFound { EK::EntryRaced } else { EK::EntryMetadata }
}

#[rustfmt::skip]
pub(super) fn classify_directory_open_failure(kind: std::io::ErrorKind) -> AgentStackInventoryErrorKind {
    match kind { std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory => EK::EntryRaced, std::io::ErrorKind::PermissionDenied => EK::RootEscape, _ => EK::EntryMetadata }
}

#[rustfmt::skip]
pub(super) fn classify_traversal_open_failure(
    root: &Dir, path: &str, kind: std::io::ErrorKind,
) -> AgentStackInventoryErrorKind {
    if kind == std::io::ErrorKind::NotFound { classify_resolution_failure(root, path, kind) }
    else { classify_directory_open_failure(kind) }
}

type DerivedRule = (String, RuleTarget, AgentStackComponentKind);
type ExactRule = (String, RuleTarget, AgentStackComponentKind, bool, bool);
type Listing = Vec<(OsString, FileType)>;

#[derive(Clone, PartialEq, Eq)]
pub(super) struct DirectoryIdentity(Arc<same_file::Handle>);
#[derive(Clone)]
#[rustfmt::skip]
struct FileObservation { digest: Sha256Digest, class: AgentStackEntryClass }

#[rustfmt::skip]
struct Scan<'a> {
    opts: &'a AgentStackInventoryOptions,
    files_used: usize, dirs_opened: usize, entries_seen: usize, bytes_used: u64,
    entries: BTreeMap<(String, &'static str), AgentStackInventoryEntry>,
    file_observations: BTreeMap<String, FileObservation>,
    listings: BTreeMap<String, Listing>,
    ancestors: Vec<DirectoryIdentity>,
}

fn charge(counter: &mut usize, max: usize, locator: &str) -> Result<(), IErr> {
    match counter.checked_add(1) {
        Some(next) if next <= max => {
            *counter = next;
            Ok(())
        }
        _ => Err(err(EK::LimitExceeded, locator)),
    }
}

pub fn inventory_repository_stack(
    options: &AgentStackInventoryOptions,
) -> Result<AgentStackInventory, AgentStackInventoryError> {
    let root = Dir::open_ambient_dir(&options.root, cap_std::ambient_authority())
        .map_err(|_| IErr::new(EK::RootOpen, None))?;
    inventory_with_root(&root, options)
}

#[rustfmt::skip]
pub(crate) fn inventory_with_root(
    root: &Dir,
    options: &AgentStackInventoryOptions,
) -> Result<AgentStackInventory, AgentStackInventoryError> {
    let mut scan = Scan {
        opts: options,
        files_used: 0,
        dirs_opened: 1, // The opened repository root counts as directory 1.
        entries_seen: 0,
        bytes_used: 0,
        entries: BTreeMap::new(),
        file_observations: BTreeMap::new(),
        listings: BTreeMap::new(),
        ancestors: vec![directory_identity(root, "")?],
    };
    let derived = scan.load_config(root)?;
    let mut exact_rules: Vec<ExactRule> = STATIC_RULES.iter().filter_map(|rule| match rule.matcher {
        Matcher::Exact(path) => Some((path.to_owned(), rule.target, rule.kind, false, true)),
        Matcher::RootSuffix(_) => None,
    }).collect();
    for (locator, target, kind) in derived {
        if let Some(rule) = exact_rules.iter_mut()
            .find(|rule| rule.0 == locator && rule.2.as_str() == kind.as_str())
        {
            if !rule.4 && matches!(target, RuleTarget::File) { rule.1 = target; }
            rule.3 = true;
        } else {
            exact_rules.push((locator, target, kind, true, false));
        }
    }
    for (locator, target, kind, derived, _) in exact_rules {
        scan.apply_exact(root, &locator, target, kind, derived)?;
    }
    for static_rule in STATIC_RULES {
        if let Matcher::RootSuffix(suffix) = static_rule.matcher {
            scan.apply_suffix(root, suffix, static_rule.kind)?;
        }
    }
    Ok(AgentStackInventory {
        entries: scan.entries.into_values().collect(),
    })
}

impl Scan<'_> {
    fn load_config(&mut self, root: &Dir) -> Result<Vec<DerivedRule>, IErr> {
        const CONFIG: &str = "harness.toml";
        match self.lookup_exact(root, CONFIG)? {
            None => return Ok(Vec::new()),
            Some(true) => return Err(err(EK::NonRegularEntry, CONFIG)),
            Some(false) => {}
        }
        let bytes = self
            .read_selected(root, CONFIG.to_owned(), Kind::Validation)?
            .unwrap_or_default();
        let text = std::str::from_utf8(&bytes).map_err(|_| err(EK::ConfigParse, CONFIG))?;
        let shape: ConfigShape = toml::from_str(text).map_err(|_| err(EK::ConfigParse, CONFIG))?;
        let Some(rules) = shape.rules else {
            return Ok(Vec::new());
        };
        let mut derived: Vec<DerivedRule> = Vec::new();
        let mut seen: HashSet<(String, bool)> = HashSet::new();
        let sources = rules
            .discovery_paths
            .iter()
            .map(|raw| (raw, false))
            .chain(rules.builtin_path.iter().map(|raw| (raw, false)))
            .chain(rules.exec_policy_paths.iter().map(|raw| (raw, true)))
            .chain(rules.requirements_path.iter().map(|raw| (raw, true)));
        for (raw, exact_file) in sources {
            let Some(locator) =
                normalize_configured_source(raw).map_err(|kind| err(kind, CONFIG))?
            else {
                continue; // Absolute sources are outside the repository scope.
            };
            let key = (locator.clone(), exact_file);
            if !seen.insert(key) {
                continue;
            }
            let target = if exact_file {
                RuleTarget::File
            } else {
                RuleTarget::FileOrDirectory(MD_TOML)
            };
            derived.push((locator, target, Kind::Policy));
        }
        Ok(derived)
    }

    #[rustfmt::skip]
    fn lookup_exact(&mut self, root: &Dir, path: &str) -> Result<Option<bool>, IErr> {
        if !self.has_exact_case(root, path)? { return Ok(None); }
        let meta = match root.symlink_metadata(path) {
            Ok(meta) => meta,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Err(err(EK::EntryRaced, path)),
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                return Err(err(EK::RootEscape, path))
            }
            Err(_) => return Err(err(EK::EntryMetadata, path)),
        };
        let is_dir = if meta.is_symlink() {
            root.metadata(path)
                .map_err(|error| err(classify_resolution_failure(root, path, error.kind()), path))?
                .is_dir()
        } else {
            meta.is_dir()
        };
        Ok(Some(is_dir))
    }

    #[rustfmt::skip]
    fn has_exact_case(&mut self, root: &Dir, path: &str) -> Result<bool, IErr> {
        let mut parent = String::new();
        let components = path.split('/').count();
        for (depth, segment) in path.split('/').enumerate() {
            if !self.listings.contains_key(&parent) {
                if parent.is_empty() { self.enumerate(root, "")?; } else {
                    if root.symlink_metadata(&parent).ok().is_some_and(|meta| meta.is_symlink()) && root.metadata(&parent).ok().is_some_and(|meta| !meta.is_dir()) { return Ok(false); }
                    if depth > self.opts.max_depth { return Err(err(EK::LimitExceeded, &parent)); }
                    charge(&mut self.dirs_opened, self.opts.max_directories, &parent)?;
                    let dir = root.open_dir(&parent).map_err(|error| {
                        let kind = if error.kind() == std::io::ErrorKind::NotFound {
                            classify_resolution_failure(root, &parent, error.kind())
                        } else { classify_directory_open_failure(error.kind()) };
                        err(kind, &parent)
                    })?;
                    self.enumerate(&dir, &parent)?;
                }
            }
            let exact = self.listings.get(&parent).and_then(|listing| listing.iter()
                .find(|(name, _)| name == OsStr::new(segment)).map(|(_, kind)| (kind.is_dir(), kind.is_symlink())));
            let Some((is_dir, is_symlink)) = exact else { return Ok(false); };
            if depth + 1 < components && !is_dir && !is_symlink { return Ok(false); }
            parent = if parent.is_empty() { segment.to_owned() } else { format!("{parent}/{segment}") };
        }
        Ok(true)
    }

    #[rustfmt::skip]
    fn apply_exact(
        &mut self, root: &Dir, path: &str, target: RuleTarget, kind: Kind, derived: bool,
    ) -> Result<(), IErr> {
        let is_dir = match self.lookup_exact(root, path)? {
            None if derived => return Err(err(EK::ConfiguredSourceMissing, path)),
            None => return Ok(()),
            Some(is_dir) => is_dir,
        };
        match target {
            RuleTarget::DirectoryPresence => {
                if is_dir {
                    let class = AgentStackEntryClass::DirectoryPresence;
                    self.emit(kind, path.to_owned(), None, class)?;
                }
                Ok(())
            }
            RuleTarget::Directory(selector) | RuleTarget::FileOrDirectory(selector) if is_dir => {
                self.traverse(root, path.to_owned(), &selector, kind, path.split('/').count(), true)
            }
            RuleTarget::File if is_dir => Err(err(EK::NonRegularEntry, path)),
            RuleTarget::Directory(_) => Err(err(EK::NonRegularEntry, path)),
            RuleTarget::File | RuleTarget::FileOrDirectory(_) => {
                self.read_selected(root, path.to_owned(), kind).map(drop)
            }
        }
    }

    fn apply_suffix(&mut self, root: &Dir, suffix: &str, kind: Kind) -> Result<(), IErr> {
        let listing = self.enumerate(root, "")?;
        let mut selected = Vec::new();
        for (name, file_type) in listing {
            if !has_suffix(&name, suffix) {
                continue;
            }
            let locator = name
                .to_str()
                .map(str::to_owned)
                .ok_or_else(|| IErr::new(EK::NonUtf8Locator, None))?;
            if file_type.is_dir() {
                return Err(err(EK::NonRegularEntry, &locator));
            }
            if file_type.is_symlink() {
                let resolved = root.metadata(&name).map_err(|error| {
                    err(
                        classify_resolution_failure(root, &name, error.kind()),
                        &locator,
                    )
                })?;
                if resolved.is_dir() {
                    return Err(err(EK::NonRegularEntry, &locator));
                }
            }
            selected.push(locator);
        }
        selected.sort();
        for locator in selected {
            self.read_selected(root, locator, kind)?;
        }
        Ok(())
    }

    #[rustfmt::skip]
    fn enumerate(&mut self, dir: &Dir, prefix: &str) -> Result<Listing, IErr> {
        if let Some(listing) = self.listings.get(prefix) { return Ok(listing.clone()); }
        let iter = dir.entries().map_err(|_| err(EK::EntryMetadata, prefix))?;
        let mut entries = Vec::new();
        for entry in iter {
            let entry = entry.map_err(|_| err(EK::EntryMetadata, prefix))?;
            if entries.len() + 1 > self.opts.max_entries_per_directory {
                return Err(err(EK::LimitExceeded, prefix));
            }
            charge(&mut self.entries_seen, self.opts.max_total_entries, prefix)?;
            entries.push((entry.file_name(), entry));
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let mut listing = Vec::new();
        for (name, entry) in entries {
            let file_type = entry
                .file_type()
                .map_err(|error| err(classify_entry_metadata_failure(error.kind()), prefix))?;
            listing.push((name, file_type));
        }
        self.listings.insert(prefix.to_owned(), listing.clone());
        Ok(listing)
    }

    #[rustfmt::skip]
    fn traverse(
        &mut self, root: &Dir, prefix: String,
        selector: &DirSelector, kind: Kind, depth: usize, direct_level: bool,
    ) -> Result<(), IErr> {
        if depth > self.opts.max_depth {
            return Err(err(EK::LimitExceeded, &prefix));
        }
        charge(&mut self.dirs_opened, self.opts.max_directories, &prefix)?;
        let dir = root.open_dir(&prefix).map_err(|error| {
            let kind = classify_traversal_open_failure(root, &prefix, error.kind());
            err(kind, &prefix)
        })?;
        let identity = directory_identity(&dir, &prefix)?;
        if self.ancestors.contains(&identity) {
            return Err(err(EK::CycleDetected, &prefix));
        }
        self.ancestors.push(identity);
        let result = self.traverse_entries(root, &dir, &prefix, selector, kind, (depth, direct_level));
        self.ancestors.pop();
        result
    }

    #[rustfmt::skip]
    fn traverse_entries(
        &mut self, root: &Dir, dir: &Dir, prefix: &str,
        selector: &DirSelector, kind: Kind, position: (usize, bool),
    ) -> Result<(), IErr> {
        let (depth, direct_level) = position;
        let listing = self.enumerate(dir, prefix)?;
        let mut candidates: Vec<(bool, String)> = Vec::new();
        for (name, file_type) in &listing {
            let is_dir = if file_type.is_symlink() {
                root.metadata(Path::new(prefix).join(name))
                    .map_err(|error| {
                        let hint = name
                            .to_str()
                            .map_or_else(|| prefix.to_owned(), |n| format!("{prefix}/{n}"));
                        let path = Path::new(prefix).join(name);
                        err(classify_resolution_failure(root, path, error.kind()), &hint)
                    })?
                    .is_dir()
            } else {
                file_type.is_dir()
            };
            let selected = if is_dir {
                selector.is_recursive()
            } else {
                selector.matches(name, direct_level)
            };
            if selected {
                let name = name.to_str().ok_or_else(|| err(EK::NonUtf8Locator, prefix))?;
                candidates.push((is_dir, name.to_owned()));
            }
        }
        candidates.sort_by(|a, b| a.1.cmp(&b.1));
        for (is_dir, name) in candidates {
            let locator = format!("{prefix}/{name}");
            if is_dir {
                self.traverse(root, locator, selector, kind, depth + 1, false)?;
            } else {
                self.read_selected(root, locator, kind)?;
            }
        }
        Ok(())
    }

    #[rustfmt::skip]
    fn read_selected(
        &mut self, root: &Dir, locator: String, kind: Kind,
    ) -> Result<Option<Vec<u8>>, IErr> {
        if self.entries.contains_key(&(locator.clone(), kind.as_str())) {
            return Ok(None);
        }
        if let Some(observation) = self.file_observations.get(&locator).cloned() {
            self.emit(kind, locator, Some(observation.digest), observation.class)?;
            return Ok(None);
        }
        let mut open_options = OpenOptions::new();
        open_options.read(true);
        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt;
            open_options.custom_flags(libc::O_NONBLOCK);
        }
        let file = root
            .open_with(&locator, &open_options)
            .map_err(|error| {
                err(
                    classify_selected_open_failure(root, &locator, error.kind()),
                    &locator,
                )
            })?;
        let meta = file.metadata().map_err(|_| err(EK::EntryMetadata, &locator))?;
        if !meta.is_file() {
            return Err(err(EK::NonRegularEntry, &locator));
        }
        charge(&mut self.files_used, self.opts.max_files, &locator)?;
        #[cfg(unix)]
        let unix_executable = {
            use cap_std::fs::MetadataExt;
            Some(meta.mode() & 0o111 != 0)
        };
        #[cfg(not(unix))]
        let unix_executable: Option<bool> = None;
        let remaining = self.opts.max_total_bytes - self.bytes_used;
        let limit = (self.opts.max_file_bytes + 1).min(remaining + 1);
        let mut bytes = Vec::new();
        #[cfg(test)]
        if self.opts.injected_read_failure.as_deref() == Some(&locator) { return Err(err(EK::ReadFailed, &locator)); }
        file.take(limit)
            .read_to_end(&mut bytes)
            .map_err(|_| err(EK::ReadFailed, &locator))?;
        let read = bytes.len() as u64;
        if read > self.opts.max_file_bytes || read > remaining {
            return Err(err(EK::LimitExceeded, &locator));
        }
        self.bytes_used += read;
        let digest = Sha256Digest::from_bytes(&bytes);
        let class = AgentStackEntryClass::RegularFile { unix_executable };
        self.file_observations.insert(locator.clone(), FileObservation {
            digest: digest.clone(), class: class.clone(),
        });
        self.emit(kind, locator, Some(digest), class)?;
        Ok(Some(bytes))
    }

    #[rustfmt::skip]
    fn emit(
        &mut self, kind: Kind, locator: String,
        integrity: Option<Sha256Digest>, entry_class: AgentStackEntryClass,
    ) -> Result<(), IErr> {
        let key = (locator.clone(), kind.as_str());
        if self.entries.contains_key(&key) {
            return Ok(());
        }
        let validation = |_| err(EK::ComponentValidation, &locator);
        let source = AgentStackSource::new(AgentStackSourceScope::Repository, &locator)
            .map_err(validation)?;
        let freshness = AgentStackFreshnessEvidence::new(false, None, None, true, false).classify();
        let component = AgentStackComponent::new(
            kind,
            source,
            AgentStackObservationClass::RepositoryObserved,
            AgentStackSelectionState::Discovered,
            AgentStackTrustLevel::RepositoryObserved,
            freshness,
        )
        .map_err(validation)?
        .with_integrity(integrity);
        component.validate().map_err(validation)?;
        self.entries.insert(key, AgentStackInventoryEntry { component, entry_class });
        Ok(())
    }
}

#[rustfmt::skip]
pub(super) fn directory_identity(directory: &Dir, locator: &str) -> Result<DirectoryIdentity, IErr> {
    let file = directory.try_clone().map(Dir::into_std_file)
        .map_err(|_| err(EK::EntryMetadata, locator))?;
    same_file::Handle::from_file(file).map(|handle| DirectoryIdentity(Arc::new(handle)))
        .map_err(|_| err(EK::EntryMetadata, locator))
}
