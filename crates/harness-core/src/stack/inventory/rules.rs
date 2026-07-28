//! Private rule model for the repository Agent Stack inventory (GH-1731).

use super::{AgentStackComponentKind, AgentStackInventoryErrorKind};
use serde::Deserialize;
use std::ffi::OsStr;
use std::path::Path;

use AgentStackComponentKind as Kind;
use AgentStackInventoryErrorKind as EK;

#[derive(Debug, Clone, Copy)]
pub(super) struct DirSelector {
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
    pub(in super::super) const SKILL_ROOT: DirSelector = sel(&["md"], NONE, NONE, &["SKILL.md"]);
    pub(in super::super) const HARNESS_SKILLS: DirSelector = sel(&["md"], NONE, NONE, NONE);
    pub(in super::super) const GUARDS: DirSelector = sel(&["sh"], NONE, NONE, NONE);
    pub(in super::super) const WORKFLOWS: DirSelector = sel(&["yml", "yaml"], NONE, NONE, NONE);
    pub(in super::super) const MD_TOML: DirSelector = sel(NONE, NONE, &["md", "toml"], NONE);
    pub(in super::super) const SG: DirSelector = sel(NONE, NONE, &["yml", "yaml"], NONE);
    pub(in super::super) const CURSOR_RULES: DirSelector = sel(NONE, NONE, &["md", "mdc"], NONE);
    pub(in super::super) const VIBEGUARD: DirSelector =
        sel(NONE, NONE, &["md", "toml", "yaml", "yml", "json", "json5"], NONE);
    pub(in super::super) const REMEM: DirSelector = sel(NONE, NONE, &["toml", "yaml", "yml", "json"], NONE);
    pub(in super::super) const GITHOOKS: DirSelector = sel(NONE, super::GIT_LIFECYCLE_HOOKS, NONE, NONE);
}
pub(super) use selectors::*;

#[rustfmt::skip]
const GIT_LIFECYCLE_HOOKS: &[&str] = &[
    "applypatch-msg", "pre-applypatch", "post-applypatch", "pre-commit", "pre-merge-commit",
    "prepare-commit-msg", "commit-msg", "post-commit", "pre-rebase", "post-checkout",
    "post-merge", "pre-push", "pre-receive", "update", "proc-receive", "post-receive",
    "post-update", "reference-transaction", "push-to-checkout", "pre-auto-gc", "post-rewrite",
    "sendemail-validate", "fsmonitor-watchman", "p4-changelist", "p4-prepare-changelist",
    "p4-post-changelist", "p4-pre-submit", "post-index-change",
];

impl DirSelector {
    pub(super) const fn is_recursive(&self) -> bool {
        !self.recursive_extensions.is_empty() || !self.recursive_basenames.is_empty()
    }

    pub(super) fn matches(&self, name: &OsStr, direct_level: bool) -> bool {
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
pub(super) fn has_suffix(name: &OsStr, suffix: &str) -> bool { name.as_encoded_bytes().ends_with(suffix.as_bytes()) }

#[derive(Debug, Clone, Copy)]
pub(super) enum RuleTarget {
    File,
    Directory(DirSelector),
    FileOrDirectory(DirSelector),
    DirectoryPresence,
}

/// Compose the strictest constraint for one normalized (locator, kind).
///
/// `RuleTarget::File` is stricter than any directory-capable target and wins
/// regardless of whether it comes from a static rule, `exec_policy_paths`,
/// `requirements_path`, or their field order. When no exact-file constraint is
/// present, a static directory target keeps its closed selector instead of
/// being replaced by the configured `md`/`toml` selector, and equivalent
/// flexible targets merge without another traversal.
#[rustfmt::skip]
pub(super) fn compose_target(existing: RuleTarget, incoming: RuleTarget) -> RuleTarget {
    use RuleTarget::*;
    match (existing, incoming) {
        (File, _) | (_, File) => File,
        (DirectoryPresence, other) | (other, DirectoryPresence) => other,
        (Directory(selector), _) | (_, Directory(selector)) => Directory(selector),
        (FileOrDirectory(selector), FileOrDirectory(_)) => FileOrDirectory(selector),
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) enum Matcher {
    Exact(&'static str),
    RootSuffix(&'static str),
}

#[derive(Debug, Clone, Copy)]
pub(super) struct StaticRule {
    pub(super) matcher: Matcher,
    pub(super) target: RuleTarget,
    pub(super) kind: AgentStackComponentKind,
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
pub(super) const STATIC_RULES: &[StaticRule] = &[
    fr("AGENTS.md", Kind::Instructions), fr("AGENTS.override.md", Kind::Instructions),
    fr("CLAUDE.md", Kind::Instructions), fr("WORKFLOW.md", Kind::Workflow),
    fr("MEMORY.md", Kind::Memory),
    fr("src/AGENTS.md", Kind::Instructions), fr("src/AGENTS.override.md", Kind::Instructions), fr("src/CLAUDE.md", Kind::Instructions),
    fr("crates/AGENTS.md", Kind::Instructions), fr("crates/AGENTS.override.md", Kind::Instructions), fr("crates/CLAUDE.md", Kind::Instructions),
    fr("lib/AGENTS.md", Kind::Instructions), fr("lib/AGENTS.override.md", Kind::Instructions), fr("lib/CLAUDE.md", Kind::Instructions),
    fr("pkg/AGENTS.md", Kind::Instructions), fr("pkg/AGENTS.override.md", Kind::Instructions), fr("pkg/CLAUDE.md", Kind::Instructions),
    dr(".claude/skills", SKILL_ROOT, Kind::Skill), dr(".codex/skills", SKILL_ROOT, Kind::Skill), dr(".agents/skills", SKILL_ROOT, Kind::Skill),
    dr("skills", SKILL_ROOT, Kind::Skill), dr(".harness/skills", HARNESS_SKILLS, Kind::Skill),
    dr(".harness/guards", GUARDS, Kind::Hook), dr(".githooks", GITHOOKS, Kind::Hook), fr(".mcp.json", Kind::McpServer), fr("mcp.json", Kind::McpServer),
    dr(".vibeguard", VIBEGUARD, Kind::Policy), fr(".vibeguard/run-guards.sh", Kind::Validation),
    dr("rules", MD_TOML, Kind::Policy), fr("requirements.toml", Kind::Policy), dr(".remem", REMEM, Kind::Memory), fr("remem.toml", Kind::Memory),
    fr(".harness/config.toml", Kind::Validation), dr(".harness/rules", MD_TOML, Kind::Policy),
    dr(".harness/sg", SG, Kind::Policy), fr("harness.toml", Kind::Validation),
    dr(".github/workflows", WORKFLOWS, Kind::Workflow), dr(".cursor/rules", CURSOR_RULES, Kind::Policy),
    fr("Cargo.toml", Kind::Validation), fr("go.mod", Kind::Validation), fr("package.json", Kind::Validation),
    fr("pyproject.toml", Kind::Validation), fr("setup.py", Kind::Validation), fr("requirements.txt", Kind::Validation),
    fr("build.gradle", Kind::Validation), fr("build.gradle.kts", Kind::Validation), fr("pom.xml", Kind::Validation),
    fr("Gemfile", Kind::Validation), fr("yarn.lock", Kind::Validation), fr("pnpm-lock.yaml", Kind::Validation),
    fr(".eslintrc", Kind::Validation), fr(".eslintrc.js", Kind::Validation), fr(".eslintrc.cjs", Kind::Validation),
    fr(".eslintrc.json", Kind::Validation), fr(".eslintrc.yaml", Kind::Validation), fr(".eslintrc.yml", Kind::Validation),
    fr("eslint.config.js", Kind::Validation), fr("eslint.config.mjs", Kind::Validation), fr("eslint.config.cjs", Kind::Validation),
    fr("biome.json", Kind::Validation), fr(".rubocop.yml", Kind::Validation),
    sfx(".csproj", Kind::Validation), sfx(".sln", Kind::Validation),
    StaticRule { matcher: Matcher::Exact("spec"), target: RuleTarget::DirectoryPresence, kind: Kind::Validation },
    fr("Makefile", Kind::Validation), fr("justfile", Kind::Validation),
];

#[rustfmt::skip]
#[derive(Debug, Default, Deserialize)]
pub(super) struct ConfigShape {
    #[serde(default)] pub(super) rules: Option<ConfigRules>,
}

#[rustfmt::skip]
#[derive(Debug, Default, Deserialize)]
pub(super) struct ConfigRules {
    #[serde(default)] pub(super) discovery_paths: Vec<String>,
    #[serde(default)] pub(super) builtin_path: Option<String>,
    #[serde(default)] pub(super) exec_policy_paths: Vec<String>,
    #[serde(default)] pub(super) requirements_path: Option<String>,
}

pub(super) fn normalize_configured_source(
    raw: &str,
) -> Result<Option<String>, AgentStackInventoryErrorKind> {
    let bytes = raw.as_bytes();
    if raw.starts_with('/') || raw.starts_with('\\') {
        return Ok(None);
    }
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        // A drive prefix is absolute only with a root separator after the
        // colon (`C:\...` or `C:/...`). A drive-relative form such as
        // `C:policy.toml` has no absolute root and must fail typed on every
        // host so CI can exercise the contract deterministically.
        return if bytes.len() >= 3 && (bytes[2] == b'\\' || bytes[2] == b'/') {
            Ok(None)
        } else {
            Err(EK::ConfiguredSourceInvalid)
        };
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
