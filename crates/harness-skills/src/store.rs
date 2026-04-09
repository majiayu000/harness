use chrono::{DateTime, Utc};
use harness_core::{types::SkillId, types::SkillLocation};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub use crate::freshness::FreshnessClass;

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SkillGovernanceStatus {
    #[default]
    Active,
    Watch,
    Quarantine,
    Retired,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    pub id: SkillId,
    pub name: String,
    pub description: String,
    pub content: String,
    pub trigger_patterns: Vec<String>,
    pub version: String,
    pub author: String,
    pub location: SkillLocation,
    /// SHA-style hex digest of `content` used for change detection.
    pub content_hash: String,
    pub usage_count: u64,
    pub last_used: Option<DateTime<Utc>>,
    /// EMA score in [0,1], where higher means better observed outcomes.
    pub quality_score: f64,
    /// Number of scored outcome samples accumulated for this skill.
    pub scored_samples: u64,
    /// Governance state that controls automatic injection.
    pub governance_status: SkillGovernanceStatus,
    /// Fraction of prompts allowed when in quarantine (0.0-1.0).
    pub canary_ratio: f64,
    /// Last time the governance score was refreshed.
    pub last_scored: Option<DateTime<Utc>>,
}

/// Sidecar data persisted alongside skill files.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SkillUsage {
    usage_count: u64,
    last_used: Option<DateTime<Utc>>,
    #[serde(default = "default_quality_score")]
    quality_score: f64,
    #[serde(default)]
    scored_samples: u64,
    #[serde(default)]
    governance_status: SkillGovernanceStatus,
    #[serde(default = "default_canary_ratio")]
    canary_ratio: f64,
    #[serde(default)]
    last_scored: Option<DateTime<Utc>>,
}

impl Default for SkillUsage {
    fn default() -> Self {
        Self {
            usage_count: 0,
            last_used: None,
            quality_score: default_quality_score(),
            scored_samples: 0,
            governance_status: SkillGovernanceStatus::Active,
            canary_ratio: default_canary_ratio(),
            last_scored: None,
        }
    }
}

fn default_quality_score() -> f64 {
    0.5
}

fn default_canary_ratio() -> f64 {
    1.0
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SkillGovernanceInput {
    pub success: u64,
    pub fail: u64,
    pub unknown: u64,
}

impl SkillGovernanceInput {
    pub fn total(self) -> u64 {
        self.success + self.fail + self.unknown
    }

    /// Samples with known outcomes that are safe to score.
    pub fn scored_total(self) -> u64 {
        self.success + self.fail
    }
}

#[derive(Debug, Clone)]
pub struct SkillGovernanceUpdate {
    pub name: String,
    pub previous_status: SkillGovernanceStatus,
    pub current_status: SkillGovernanceStatus,
    pub quality_score: f64,
    pub scored_samples: u64,
    pub canary_ratio: f64,
}

pub struct SkillStore {
    skills: Vec<Skill>,
    discovery_paths: Vec<PathBuf>,
    persist_dir: Option<PathBuf>,
    /// Maps skill name to the directory where its source file lives,
    /// used to write usage sidecar files alongside the skill.
    skill_dirs: HashMap<String, PathBuf>,
}

impl SkillStore {
    pub fn new() -> Self {
        Self {
            skills: Vec::new(),
            discovery_paths: Vec::new(),
            persist_dir: None,
            skill_dirs: HashMap::new(),
        }
    }

    /// Record that a skill was used: increments its counter, updates `last_used`,
    /// and persists a sidecar `.usage.json` alongside the skill file when possible.
    pub fn record_use(&mut self, id: &SkillId) {
        let (name, sidecar) = {
            let Some(skill) = self.skills.iter_mut().find(|s| s.id == *id) else {
                return;
            };
            skill.usage_count += 1;
            skill.last_used = Some(Utc::now());
            (
                skill.name.clone(),
                SkillUsage {
                    usage_count: skill.usage_count,
                    last_used: skill.last_used,
                    quality_score: skill.quality_score,
                    scored_samples: skill.scored_samples,
                    governance_status: skill.governance_status,
                    canary_ratio: skill.canary_ratio,
                    last_scored: skill.last_scored,
                },
            )
        };
        if let Some(dir) = self.skill_dirs.get(&name) {
            persist_usage_sidecar(dir, &name, &sidecar);
        }
    }

    /// Enable disk persistence: created skills are written to `{dir}/{name}.md`,
    /// deleted skills are removed, and the dir is added to discovery_paths so
    /// skills survive restarts.
    ///
    /// # Startup sequence
    ///
    /// After calling `with_persist_dir`, call `load_builtin()` then `discover()`
    /// so previously persisted skills are reloaded from disk.  User-tier skills
    /// loaded from `persist_dir` take priority over same-named builtin (System)
    /// skills during the `deduplicate()` step inside `discover()`.
    pub fn with_persist_dir(mut self, dir: PathBuf) -> Self {
        self.discovery_paths.push(dir.clone());
        self.persist_dir = Some(dir);
        self
    }

    /// Set up the 4-layer discovery chain.
    pub fn with_discovery(mut self, project_root: &Path) -> Self {
        // Repo level
        self.discovery_paths
            .push(project_root.join(".harness/skills/"));

        // User level
        if let Ok(home) = std::env::var("HOME") {
            self.discovery_paths
                .push(PathBuf::from(home).join(".harness/skills/"));
        }

        // Admin level
        self.discovery_paths
            .push(PathBuf::from("/etc/harness/skills/"));

        self
    }

    /// Discover skills from all configured paths.
    pub fn discover(&mut self) -> anyhow::Result<()> {
        for path in self.discovery_paths.clone() {
            if path.is_dir() {
                self.load_from_dir(&path)?;
            }
        }
        self.deduplicate();
        Ok(())
    }

    fn load_from_dir(&mut self, dir: &Path) -> anyhow::Result<()> {
        let location = if dir.to_string_lossy().contains("/etc/") {
            SkillLocation::Admin
        } else if dir.to_string_lossy().contains("/.harness/skills") {
            if let Ok(home) = std::env::var("HOME") {
                if dir.starts_with(&home) && !dir.starts_with(PathBuf::from(&home).join(".harness"))
                {
                    SkillLocation::Repo
                } else {
                    SkillLocation::User
                }
            } else {
                SkillLocation::Repo
            }
        } else if self.persist_dir.as_deref() == Some(dir) {
            // Skills loaded from the persist directory are user-created and
            // must shadow same-named builtins (System tier). Use User tier so
            // the dedup priority (User=3 > System=1) keeps user skills.
            SkillLocation::User
        } else {
            SkillLocation::System
        };

        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().map(|e| e == "md").unwrap_or(false) {
                if let Ok(content) = std::fs::read_to_string(&path) {
                    let name = path
                        .file_stem()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default();

                    let version = parse_version_from_frontmatter(&content);
                    let hash = compute_content_hash(&content);
                    let usage = load_usage_sidecar(dir, &name);
                    self.skill_dirs.insert(name.clone(), dir.to_path_buf());
                    self.skills.push(Skill {
                        id: SkillId::new(),
                        name: name.clone(),
                        description: content
                            .lines()
                            .next()
                            .unwrap_or("")
                            .trim_start_matches('#')
                            .trim()
                            .to_string(),
                        trigger_patterns: parse_trigger_patterns(&content),
                        content,
                        version,
                        author: "system".to_string(),
                        location,
                        content_hash: hash,
                        usage_count: usage.usage_count,
                        last_used: usage.last_used,
                        quality_score: usage.quality_score,
                        scored_samples: usage.scored_samples,
                        governance_status: usage.governance_status,
                        canary_ratio: usage.canary_ratio,
                        last_scored: usage.last_scored,
                    });
                }
            }
        }
        Ok(())
    }

    pub fn match_prompt(&self, prompt: &str) -> Vec<&Skill> {
        let prompt_lower = prompt.to_lowercase();
        self.skills
            .iter()
            .filter(|skill| {
                !skill.trigger_patterns.is_empty()
                    && skill
                        .trigger_patterns
                        .iter()
                        .any(|p| prompt_lower.contains(&p.to_lowercase()))
                    && allows_auto_injection(skill, prompt)
            })
            .collect()
    }

    /// Apply an outcome summary to a skill and update governance state.
    ///
    /// Score is updated with an EMA to avoid sharp oscillation.
    pub fn apply_governance_outcome(
        &mut self,
        id: &SkillId,
        input: SkillGovernanceInput,
    ) -> Option<SkillGovernanceUpdate> {
        let scored_total = input.scored_total();
        if scored_total == 0 {
            return None;
        }
        let idx = self.skills.iter().position(|s| s.id == *id)?;
        let skill = &mut self.skills[idx];
        let previous_status = skill.governance_status;

        // Unknown outcomes are ignored for scoring to avoid penalizing in-flight tasks.
        let total = scored_total as f64;
        let raw = (input.success as f64 - 0.7 * input.fail as f64) / total;
        let target = ((raw + 1.0) / 2.0).clamp(0.0, 1.0);
        skill.quality_score = (skill.quality_score * 0.8 + target * 0.2).clamp(0.0, 1.0);
        skill.scored_samples = skill.scored_samples.saturating_add(scored_total);
        skill.last_scored = Some(Utc::now());

        skill.governance_status =
            decide_governance_status(skill.quality_score, skill.scored_samples);
        skill.canary_ratio = match skill.governance_status {
            SkillGovernanceStatus::Active | SkillGovernanceStatus::Watch => 1.0,
            SkillGovernanceStatus::Quarantine => 0.1,
            SkillGovernanceStatus::Retired => 0.0,
        };

        if let Some(dir) = self.skill_dirs.get(&skill.name) {
            let sidecar = SkillUsage {
                usage_count: skill.usage_count,
                last_used: skill.last_used,
                quality_score: skill.quality_score,
                scored_samples: skill.scored_samples,
                governance_status: skill.governance_status,
                canary_ratio: skill.canary_ratio,
                last_scored: skill.last_scored,
            };
            persist_usage_sidecar(dir, &skill.name, &sidecar);
        }

        Some(SkillGovernanceUpdate {
            name: skill.name.clone(),
            previous_status,
            current_status: skill.governance_status,
            quality_score: skill.quality_score,
            scored_samples: skill.scored_samples,
            canary_ratio: skill.canary_ratio,
        })
    }

    pub fn match_context(&self, file_path: Option<&Path>, language: Option<&str>) -> Vec<&Skill> {
        self.skills
            .iter()
            .filter(|skill| {
                if skill.trigger_patterns.is_empty() {
                    return false;
                }
                if let Some(path) = file_path {
                    let path_str = path.to_string_lossy();
                    return skill.trigger_patterns.iter().any(|p| {
                        glob::Pattern::new(p)
                            .map(|g| g.matches(&path_str))
                            .unwrap_or(false)
                    });
                }
                if let Some(lang) = language {
                    return skill.trigger_patterns.iter().any(|p| p.contains(lang));
                }
                false
            })
            .collect()
    }

    pub fn deduplicate(&mut self) {
        let mut seen: HashMap<String, usize> = HashMap::new();
        let mut to_remove = Vec::new();

        for (idx, skill) in self.skills.iter().enumerate() {
            match seen.entry(skill.name.clone()) {
                std::collections::hash_map::Entry::Vacant(slot) => {
                    slot.insert(idx);
                }
                std::collections::hash_map::Entry::Occupied(mut slot) => {
                    let existing_idx = *slot.get();
                    let existing_priority = location_priority(self.skills[existing_idx].location);
                    let new_priority = location_priority(skill.location);
                    if new_priority > existing_priority {
                        to_remove.push(existing_idx);
                        *slot.get_mut() = idx;
                    } else {
                        to_remove.push(idx);
                    }
                }
            }
        }

        to_remove.sort_unstable();
        for idx in to_remove.into_iter().rev() {
            self.skills.remove(idx);
        }
    }

    pub fn create(&mut self, name: String, content: String) -> &Skill {
        let trigger_patterns = parse_trigger_patterns(&content);
        let version = parse_version_from_frontmatter(&content);
        let content_hash = compute_content_hash(&content);
        let skill = Skill {
            id: SkillId::new(),
            name: name.clone(),
            description: content.lines().next().unwrap_or("").to_string(),
            content,
            trigger_patterns,
            version,
            author: "user".to_string(),
            location: SkillLocation::User,
            content_hash,
            usage_count: 0,
            last_used: None,
            quality_score: default_quality_score(),
            scored_samples: 0,
            governance_status: SkillGovernanceStatus::Active,
            canary_ratio: default_canary_ratio(),
            last_scored: None,
        };
        self.skills.push(skill);
        let skill_ref = match self.skills.last() {
            Some(s) => s,
            None => unreachable!("skill was just pushed, so it must exist"),
        };
        if let Some(dir) = &self.persist_dir.clone() {
            if let Err(e) = std::fs::create_dir_all(dir) {
                tracing::warn!("failed to create skills dir {}: {e}", dir.display());
            } else {
                let path = dir.join(format!("{}.md", skill_ref.name));
                if let Err(e) = std::fs::write(&path, &skill_ref.content) {
                    tracing::warn!("failed to persist skill {}: {e}", path.display());
                }
                self.skill_dirs.insert(name, dir.clone());
            }
        }
        skill_ref
    }

    pub fn get(&self, id: &SkillId) -> Option<&Skill> {
        self.skills.iter().find(|s| s.id == *id)
    }

    pub fn delete(&mut self, id: &SkillId) -> bool {
        let name = self
            .skills
            .iter()
            .find(|s| s.id == *id)
            .map(|s| s.name.clone());
        let len = self.skills.len();
        self.skills.retain(|s| s.id != *id);
        let deleted = self.skills.len() < len;
        if deleted {
            if let (Some(dir), Some(name)) = (&self.persist_dir, name) {
                let path = dir.join(format!("{}.md", name));
                if path.exists() {
                    if let Err(e) = std::fs::remove_file(&path) {
                        tracing::warn!("failed to remove skill file {}: {e}", path.display());
                    }
                }
            }
        }
        deleted
    }

    pub fn list(&self) -> &[Skill] {
        &self.skills
    }

    pub fn search(&self, query: &str) -> Vec<&Skill> {
        let q = query.to_lowercase();
        self.skills
            .iter()
            .filter(|s| {
                s.name.to_lowercase().contains(&q) || s.description.to_lowercase().contains(&q)
            })
            .collect()
    }

    pub fn update(&mut self, id: &SkillId, new_content: String) -> Option<&Skill> {
        let idx = self.skills.iter().position(|s| s.id == *id)?;
        let new_hash = compute_content_hash(&new_content);
        let version = if new_hash != self.skills[idx].content_hash {
            increment_patch(&self.skills[idx].version)
        } else {
            self.skills[idx].version.clone()
        };
        let trigger_patterns = parse_trigger_patterns(&new_content);
        let description = new_content
            .lines()
            .next()
            .unwrap_or("")
            .trim_start_matches('#')
            .trim()
            .to_string();
        let name = self.skills[idx].name.clone();
        self.skills[idx].content = new_content.clone();
        self.skills[idx].trigger_patterns = trigger_patterns;
        self.skills[idx].description = description;
        self.skills[idx].version = version;
        self.skills[idx].content_hash = new_hash;
        if let Some(dir) = &self.persist_dir {
            let path = dir.join(format!("{}.md", name));
            if let Err(e) = std::fs::write(&path, &new_content) {
                tracing::warn!("failed to persist skill {}: {e}", path.display());
            }
        }
        Some(&self.skills[idx])
    }

    pub fn get_by_name(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|s| s.name == name)
    }

    /// Mutable access to the skills vector — used internally and in tests.
    #[cfg(test)]
    pub fn skills_mut(&mut self) -> &mut Vec<Skill> {
        &mut self.skills
    }

    /// Return all skills classified as [`FreshnessClass::Dormant`] or
    /// [`FreshnessClass::Stale`], sorted by staleness: Stale before Dormant,
    /// then by `last_used` ascending (oldest first), with `None` (never used)
    /// last.
    pub fn list_stale(&self) -> Vec<(&Skill, FreshnessClass)> {
        let now = Utc::now();
        let mut entries: Vec<(&Skill, FreshnessClass)> = self
            .skills
            .iter()
            .filter_map(|s| {
                let class = s.classify_freshness(now);
                match class {
                    FreshnessClass::Dormant | FreshnessClass::Stale => Some((s, class)),
                    _ => None,
                }
            })
            .collect();

        entries.sort_by(|(a, ac), (b, bc)| {
            // Stale sorts before Dormant
            let class_ord = stale_class_order(*ac).cmp(&stale_class_order(*bc));
            if class_ord != std::cmp::Ordering::Equal {
                return class_ord;
            }
            // Within same class: oldest last_used first; None (never used) last
            match (a.last_used, b.last_used) {
                (None, None) => std::cmp::Ordering::Equal,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (Some(_), None) => std::cmp::Ordering::Less,
                (Some(ta), Some(tb)) => ta.cmp(&tb),
            }
        });

        entries
    }

    pub fn load_builtin(&mut self) {
        let builtins = [
            ("interview", include_str!("../../../skills/interview.md")),
            ("exec-plan", include_str!("../../../skills/exec-plan.md")),
            ("preflight", include_str!("../../../skills/preflight.md")),
            ("check", include_str!("../../../skills/check.md")),
            ("build-fix", include_str!("../../../skills/build-fix.md")),
            ("review", include_str!("../../../skills/review.md")),
            (
                "cross-review",
                include_str!("../../../skills/cross-review.md"),
            ),
            ("learn", include_str!("../../../skills/learn.md")),
            ("gc", include_str!("../../../skills/gc.md")),
            ("stats", include_str!("../../../skills/stats.md")),
        ];
        for (name, content) in builtins {
            if self.get_by_name(name).is_none() {
                let usage = if let Some(dir) = &self.persist_dir {
                    self.skill_dirs.insert(name.to_string(), dir.clone());
                    load_usage_sidecar(dir, name)
                } else {
                    SkillUsage::default()
                };
                self.skills.push(Skill {
                    id: SkillId::from_str(name),
                    name: name.to_string(),
                    description: content
                        .lines()
                        .next()
                        .unwrap_or("")
                        .trim_start_matches('#')
                        .trim()
                        .to_string(),
                    content: content.to_string(),
                    trigger_patterns: parse_trigger_patterns(content),
                    version: parse_version_from_frontmatter(content),
                    author: "system".to_string(),
                    location: SkillLocation::System,
                    content_hash: compute_content_hash(content),
                    usage_count: usage.usage_count,
                    last_used: usage.last_used,
                    quality_score: usage.quality_score,
                    scored_samples: usage.scored_samples,
                    governance_status: usage.governance_status,
                    canary_ratio: usage.canary_ratio,
                    last_scored: usage.last_scored,
                });
            }
        }
    }
}

impl Default for SkillStore {
    fn default() -> Self {
        Self::new()
    }
}

fn parse_version_from_frontmatter(content: &str) -> String {
    let mut lines = content.lines();
    if lines.next().map(|l| l.trim()) != Some("---") {
        return "1.0.0".to_string();
    }
    for line in lines {
        let trimmed = line.trim();
        if trimmed == "---" {
            break;
        }
        if let Some(rest) = trimmed.strip_prefix("version:") {
            let ver = rest.trim().to_string();
            if !ver.is_empty() {
                return ver;
            }
        }
    }
    "1.0.0".to_string()
}

fn compute_content_hash(content: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    content.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

pub fn increment_patch(version: &str) -> String {
    let parts: Vec<&str> = version.splitn(3, '.').collect();
    if parts.len() == 3 {
        if let (Ok(major), Ok(minor), Ok(patch)) = (
            parts[0].parse::<u32>(),
            parts[1].parse::<u32>(),
            parts[2].parse::<u32>(),
        ) {
            return format!("{}.{}.{}", major, minor, patch + 1);
        }
    }
    format!("{}.1", version)
}

fn parse_trigger_patterns(content: &str) -> Vec<String> {
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(inner) = trimmed
            .strip_prefix("<!-- trigger-patterns:")
            .and_then(|s| s.strip_suffix("-->"))
        {
            return inner
                .split(',')
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty())
                .collect();
        }
    }
    Vec::new()
}

fn decide_governance_status(score: f64, samples: u64) -> SkillGovernanceStatus {
    if samples >= 30 && score < 0.30 {
        SkillGovernanceStatus::Retired
    } else if samples >= 10 && score < 0.45 {
        SkillGovernanceStatus::Quarantine
    } else if score < 0.60 {
        SkillGovernanceStatus::Watch
    } else {
        SkillGovernanceStatus::Active
    }
}

fn allows_auto_injection(skill: &Skill, prompt: &str) -> bool {
    match skill.governance_status {
        SkillGovernanceStatus::Active | SkillGovernanceStatus::Watch => true,
        SkillGovernanceStatus::Quarantine => {
            skill.canary_ratio > 0.0 && in_canary_bucket(&skill.id, prompt, skill.canary_ratio)
        }
        SkillGovernanceStatus::Retired => false,
    }
}

fn in_canary_bucket(skill_id: &SkillId, prompt: &str, ratio: f64) -> bool {
    if ratio <= 0.0 {
        return false;
    }
    if ratio >= 1.0 {
        return true;
    }
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    skill_id.as_str().hash(&mut hasher);
    prompt.hash(&mut hasher);
    let bucket = hasher.finish() % 10_000;
    let threshold = (ratio.clamp(0.0, 1.0) * 10_000.0).round() as u64;
    bucket < threshold
}

fn persist_usage_sidecar(dir: &Path, skill_name: &str, usage: &SkillUsage) {
    if let Err(e) = std::fs::create_dir_all(dir) {
        tracing::warn!("failed to create usage dir {}: {e}", dir.display());
        return;
    }
    let path = dir.join(format!("{}.usage.json", skill_name));
    match serde_json::to_string(usage) {
        Ok(json) => {
            if let Err(e) = std::fs::write(&path, json) {
                tracing::warn!("failed to persist usage for skill \'{}\': {e}", skill_name);
            }
        }
        Err(e) => tracing::warn!(
            "failed to serialize usage for skill \'{}\': {e}",
            skill_name
        ),
    }
}

fn load_usage_sidecar(dir: &Path, skill_name: &str) -> SkillUsage {
    let path = dir.join(format!("{}.usage.json", skill_name));
    if !path.exists() {
        return SkillUsage::default();
    }
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Sort key for staleness: lower = sorted first. Stale (0) before Dormant (1).
fn stale_class_order(class: FreshnessClass) -> u8 {
    match class {
        FreshnessClass::Stale => 0,
        FreshnessClass::Dormant => 1,
        _ => 2,
    }
}

fn location_priority(loc: SkillLocation) -> u8 {
    match loc {
        SkillLocation::Repo => 4,
        SkillLocation::User => 3,
        SkillLocation::Admin => 2,
        SkillLocation::System => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_skill(name: &str, location: SkillLocation) -> Skill {
        Skill {
            id: SkillId::new(),
            name: name.to_string(),
            description: "desc".to_string(),
            content: "content".to_string(),
            trigger_patterns: Vec::new(),
            version: "1.0.0".to_string(),
            author: "test".to_string(),
            location,
            content_hash: compute_content_hash("content"),
            usage_count: 0,
            last_used: None,
            quality_score: default_quality_score(),
            scored_samples: 0,
            governance_status: SkillGovernanceStatus::Active,
            canary_ratio: default_canary_ratio(),
            last_scored: None,
        }
    }

    #[test]
    fn deduplicate_keeps_higher_priority() {
        let mut store = SkillStore::new();
        store
            .skills
            .push(make_skill("deploy", SkillLocation::System));
        store.skills.push(make_skill("deploy", SkillLocation::Repo));
        store.deduplicate();
        assert_eq!(store.list().len(), 1);
        assert_eq!(store.list()[0].location, SkillLocation::Repo);
    }

    #[test]
    fn deduplicate_removes_lower_priority_duplicate() {
        let mut store = SkillStore::new();
        store.skills.push(make_skill("lint", SkillLocation::User));
        store.skills.push(make_skill("lint", SkillLocation::Admin));
        store.deduplicate();
        assert_eq!(store.list().len(), 1);
        assert_eq!(store.list()[0].location, SkillLocation::User);
    }

    #[test]
    fn deduplicate_keeps_unique_skills() {
        let mut store = SkillStore::new();
        store.skills.push(make_skill("alpha", SkillLocation::Repo));
        store.skills.push(make_skill("beta", SkillLocation::User));
        store.deduplicate();
        assert_eq!(store.list().len(), 2);
    }

    #[test]
    fn create_adds_skill_to_store() {
        let mut store = SkillStore::new();
        store.create(
            "my-skill".to_string(),
            "# My Skill\nDoes stuff.".to_string(),
        );
        assert_eq!(store.list().len(), 1);
        assert_eq!(store.list()[0].name, "my-skill");
    }

    #[test]
    fn delete_removes_skill() {
        let mut store = SkillStore::new();
        store.create("removable".to_string(), "content".to_string());
        let id = store.list()[0].id.clone();
        assert!(store.delete(&id));
        assert!(store.list().is_empty());
    }

    #[test]
    fn create_persists_file_to_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let persist_path = dir.path().to_path_buf();
        let mut store = SkillStore::new().with_persist_dir(persist_path.clone());
        store.create(
            "my-skill".to_string(),
            "# My Skill\nDoes stuff.".to_string(),
        );
        let file = persist_path.join("my-skill.md");
        assert!(file.exists(), "skill file should be written to disk");
        let contents = std::fs::read_to_string(&file).expect("read file");
        assert!(contents.contains("Does stuff."));
    }

    #[test]
    fn delete_removes_file_from_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let persist_path = dir.path().to_path_buf();
        let mut store = SkillStore::new().with_persist_dir(persist_path.clone());
        store.create("removable".to_string(), "content".to_string());
        let file = persist_path.join("removable.md");
        assert!(file.exists(), "file should exist after create");
        let id = store.list()[0].id.clone();
        assert!(store.delete(&id));
        assert!(!file.exists(), "file should be removed after delete");
    }

    #[test]
    fn search_finds_by_name() {
        let mut store = SkillStore::new();
        store.create("rust-lint".to_string(), "# Lint tool".to_string());
        store.create("python-format".to_string(), "# Format tool".to_string());
        let results = store.search("rust");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "rust-lint");
    }

    #[test]
    fn load_builtin_adds_10_skills() {
        let mut store = SkillStore::new();
        store.load_builtin();
        assert_eq!(store.list().len(), 10);
    }

    #[test]
    fn load_builtin_all_system_location() {
        let mut store = SkillStore::new();
        store.load_builtin();
        for skill in store.list() {
            assert_eq!(
                skill.location,
                SkillLocation::System,
                "builtin skill \'{}\' should have System location",
                skill.name
            );
        }
    }

    #[test]
    fn load_builtin_user_skill_overrides_builtin() {
        let mut store = SkillStore::new();
        store.skills.push(make_skill("review", SkillLocation::User));
        store.load_builtin();
        assert_eq!(store.list().len(), 10);
        let Some(review) = store.get_by_name("review") else {
            panic!("review skill must exist");
        };
        assert_eq!(review.location, SkillLocation::User);
    }

    #[test]
    fn load_builtin_idempotent() {
        let mut store = SkillStore::new();
        store.load_builtin();
        store.load_builtin();
        assert_eq!(
            store.list().len(),
            10,
            "calling load_builtin twice must not duplicate skills"
        );
    }

    #[test]
    fn load_builtin_restores_sidecar_governance_from_persist_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let persist_path = dir.path().to_path_buf();
        let sidecar_path = persist_path.join("review.usage.json");
        let sidecar = SkillUsage {
            usage_count: 7,
            last_used: Some(Utc::now()),
            quality_score: 0.2,
            scored_samples: 40,
            governance_status: SkillGovernanceStatus::Retired,
            canary_ratio: 0.0,
            last_scored: Some(Utc::now()),
        };
        std::fs::write(
            &sidecar_path,
            serde_json::to_string(&sidecar).expect("serialize sidecar"),
        )
        .expect("write sidecar");

        let mut store = SkillStore::new().with_persist_dir(persist_path);
        store.load_builtin();
        let review = store
            .get_by_name("review")
            .expect("builtin review skill should exist");
        assert_eq!(review.usage_count, 7);
        assert_eq!(review.governance_status, SkillGovernanceStatus::Retired);
        assert_eq!(review.scored_samples, 40);
    }

    #[test]
    fn get_by_name_returns_correct_skill() {
        let mut store = SkillStore::new();
        store.load_builtin();
        let Some(skill) = store.get_by_name("interview") else {
            panic!("interview skill must exist");
        };
        assert_eq!(skill.name, "interview");
        assert_eq!(skill.location, SkillLocation::System);
    }

    #[test]
    fn get_by_name_returns_none_for_missing() {
        let store = SkillStore::new();
        assert!(store.get_by_name("nonexistent").is_none());
    }

    #[test]
    fn load_builtin_skills_have_trigger_patterns() {
        let mut store = SkillStore::new();
        store.load_builtin();
        for skill in store.list() {
            assert!(
                !skill.trigger_patterns.is_empty(),
                "builtin skill \'{}\' should have trigger patterns parsed from content",
                skill.name
            );
        }
    }

    #[test]
    fn create_parses_trigger_patterns_from_content() {
        let mut store = SkillStore::new();
        store.create(
            "my-skill".to_string(),
            "# My Skill\n<!-- trigger-patterns: my keyword, another pattern -->\nContent."
                .to_string(),
        );
        assert_eq!(
            store.list()[0].trigger_patterns,
            vec!["my keyword", "another pattern"]
        );
    }

    #[test]
    fn match_prompt_returns_matching_skills() {
        let mut store = SkillStore::new();
        store.skills.push(Skill {
            id: SkillId::new(),
            name: "review".to_string(),
            description: "review code".to_string(),
            content: String::new(),
            trigger_patterns: vec!["code review".to_string(), "review pr".to_string()],
            version: "1.0.0".to_string(),
            author: "system".to_string(),
            location: SkillLocation::System,
            content_hash: compute_content_hash(""),
            usage_count: 0,
            last_used: None,
            quality_score: default_quality_score(),
            scored_samples: 0,
            governance_status: SkillGovernanceStatus::Active,
            canary_ratio: default_canary_ratio(),
            last_scored: None,
        });
        let matches = store.match_prompt("please do a code review of this PR");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].name, "review");
    }

    #[test]
    fn match_prompt_is_case_insensitive() {
        let mut store = SkillStore::new();
        store.skills.push(Skill {
            id: SkillId::new(),
            name: "build-fix".to_string(),
            description: "fix builds".to_string(),
            content: String::new(),
            trigger_patterns: vec!["build error".to_string()],
            version: "1.0.0".to_string(),
            author: "system".to_string(),
            location: SkillLocation::System,
            content_hash: compute_content_hash(""),
            usage_count: 0,
            last_used: None,
            quality_score: default_quality_score(),
            scored_samples: 0,
            governance_status: SkillGovernanceStatus::Active,
            canary_ratio: default_canary_ratio(),
            last_scored: None,
        });
        let matches = store.match_prompt("I have a BUILD ERROR in my project");
        assert_eq!(matches.len(), 1);
    }

    #[test]
    fn match_prompt_skips_skills_without_patterns() {
        let mut store = SkillStore::new();
        store
            .skills
            .push(make_skill("no-patterns", SkillLocation::System));
        let matches = store.match_prompt("any prompt with no-patterns keywords");
        assert!(
            matches.is_empty(),
            "skills without patterns should not be returned"
        );
    }

    #[test]
    fn match_prompt_returns_empty_when_no_match() {
        let mut store = SkillStore::new();
        store.skills.push(Skill {
            id: SkillId::new(),
            name: "review".to_string(),
            description: "review code".to_string(),
            content: String::new(),
            trigger_patterns: vec!["code review".to_string()],
            version: "1.0.0".to_string(),
            author: "system".to_string(),
            location: SkillLocation::System,
            content_hash: compute_content_hash(""),
            usage_count: 0,
            last_used: None,
            quality_score: default_quality_score(),
            scored_samples: 0,
            governance_status: SkillGovernanceStatus::Active,
            canary_ratio: default_canary_ratio(),
            last_scored: None,
        });
        let matches = store.match_prompt("implement feature X");
        assert!(matches.is_empty());
    }

    #[test]
    fn parse_version_from_frontmatter_returns_version_field() {
        let content = "---\nversion: 2.3.4\n---\n# Title\n";
        assert_eq!(parse_version_from_frontmatter(content), "2.3.4");
    }

    #[test]
    fn parse_version_from_frontmatter_defaults_when_absent() {
        let content = "# Title\nNo frontmatter here.";
        assert_eq!(parse_version_from_frontmatter(content), "1.0.0");
    }

    #[test]
    fn parse_version_from_frontmatter_defaults_when_field_missing() {
        let content = "---\nauthor: alice\n---\n# Title\n";
        assert_eq!(parse_version_from_frontmatter(content), "1.0.0");
    }

    #[test]
    fn parse_version_from_frontmatter_ignores_trailing_whitespace() {
        let content = "---\nversion:  1.2.3  \n---\n";
        assert_eq!(parse_version_from_frontmatter(content), "1.2.3");
    }

    #[test]
    fn increment_patch_bumps_last_component() {
        assert_eq!(increment_patch("1.0.0"), "1.0.1");
        assert_eq!(increment_patch("2.5.9"), "2.5.10");
    }

    #[test]
    fn increment_patch_does_not_touch_major_or_minor() {
        assert_eq!(increment_patch("3.7.2"), "3.7.3");
    }

    #[test]
    fn update_increments_patch_when_content_changes() {
        let mut store = SkillStore::new();
        store.create("skill-a".to_string(), "original content".to_string());
        let id = store.list()[0].id.clone();
        assert_eq!(store.list()[0].version, "1.0.0");
        store.update(&id, "changed content".to_string());
        assert_eq!(store.list()[0].version, "1.0.1");
    }

    #[test]
    fn update_does_not_increment_when_content_unchanged() {
        let mut store = SkillStore::new();
        store.create("skill-b".to_string(), "same content".to_string());
        let id = store.list()[0].id.clone();
        store.update(&id, "same content".to_string());
        assert_eq!(store.list()[0].version, "1.0.0");
    }

    #[test]
    fn update_returns_none_for_unknown_id() {
        let mut store = SkillStore::new();
        let unknown = SkillId::new();
        assert!(store.update(&unknown, "content".to_string()).is_none());
    }

    #[test]
    fn create_parses_version_from_frontmatter() {
        let mut store = SkillStore::new();
        store.create(
            "versioned".to_string(),
            "---\nversion: 3.1.4\n---\n# Title\n".to_string(),
        );
        assert_eq!(store.list()[0].version, "3.1.4");
    }

    #[test]
    fn create_defaults_version_when_no_frontmatter() {
        let mut store = SkillStore::new();
        store.create(
            "plain".to_string(),
            "# Plain skill\nNo frontmatter.".to_string(),
        );
        assert_eq!(store.list()[0].version, "1.0.0");
    }

    #[test]
    fn record_use_increments_counter() {
        let mut store = SkillStore::new();
        let skill = make_skill("deploy", SkillLocation::System);
        let id = skill.id.clone();
        store.skills.push(skill);
        store.record_use(&id);
        store.record_use(&id);
        let s = store.get(&id).unwrap();
        assert_eq!(s.usage_count, 2);
        assert!(s.last_used.is_some());
    }

    #[test]
    fn record_use_unknown_id_is_noop() {
        let mut store = SkillStore::new();
        store
            .skills
            .push(make_skill("deploy", SkillLocation::System));
        let unknown = SkillId::new();
        store.record_use(&unknown);
        assert_eq!(store.list()[0].usage_count, 0);
    }

    #[test]
    fn record_use_persists_sidecar() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = SkillStore::new();
        let skill = make_skill("deploy", SkillLocation::User);
        let id = skill.id.clone();
        store.skills.push(skill);
        store
            .skill_dirs
            .insert("deploy".to_string(), tmp.path().to_path_buf());
        store.record_use(&id);
        let sidecar = tmp.path().join("deploy.usage.json");
        assert!(sidecar.exists(), "sidecar file should be created");
        let data: SkillUsage =
            serde_json::from_str(&std::fs::read_to_string(&sidecar).unwrap()).unwrap();
        assert_eq!(data.usage_count, 1);
        assert!(data.last_used.is_some());
        assert_eq!(data.governance_status, SkillGovernanceStatus::Active);
        assert!(data.quality_score > 0.0);
    }

    #[test]
    fn load_usage_sidecar_returns_defaults_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let usage = load_usage_sidecar(tmp.path(), "nonexistent");
        assert_eq!(usage.usage_count, 0);
        assert!(usage.last_used.is_none());
        assert_eq!(usage.governance_status, SkillGovernanceStatus::Active);
        assert_eq!(usage.canary_ratio, 1.0);
    }

    #[test]
    fn load_usage_sidecar_restores_persisted_values() {
        let tmp = tempfile::tempdir().unwrap();
        let sidecar = tmp.path().join("myskill.usage.json");
        let stored = SkillUsage {
            usage_count: 42,
            last_used: Some(chrono::Utc::now()),
            ..Default::default()
        };
        std::fs::write(&sidecar, serde_json::to_string(&stored).unwrap()).unwrap();
        let loaded = load_usage_sidecar(tmp.path(), "myskill");
        assert_eq!(loaded.usage_count, 42);
        assert!(loaded.last_used.is_some());
    }

    #[test]
    fn governance_update_can_quarantine_skill() {
        let mut store = SkillStore::new();
        store.create(
            "review-skill".to_string(),
            "# review\n<!-- trigger-patterns: review -->".to_string(),
        );
        let id = store.list()[0].id.clone();

        let update = store
            .apply_governance_outcome(
                &id,
                SkillGovernanceInput {
                    success: 0,
                    fail: 20,
                    unknown: 0,
                },
            )
            .expect("governance update should exist");
        assert_eq!(update.current_status, SkillGovernanceStatus::Quarantine);
        assert_eq!(update.canary_ratio, 0.1);
        assert!(update.quality_score < 0.45);
    }

    #[test]
    fn governance_update_ignores_unknown_only_samples() {
        let mut store = SkillStore::new();
        store.create(
            "unknown-skill".to_string(),
            "# unknown\n<!-- trigger-patterns: unknown -->".to_string(),
        );
        let id = store.list()[0].id.clone();

        let update = store.apply_governance_outcome(
            &id,
            SkillGovernanceInput {
                success: 0,
                fail: 0,
                unknown: 12,
            },
        );
        assert!(
            update.is_none(),
            "unknown-only samples should not change score"
        );

        let skill = store.get(&id).expect("skill should exist");
        assert_eq!(skill.scored_samples, 0);
        assert_eq!(skill.quality_score, default_quality_score());
    }

    #[test]
    fn retired_skill_is_not_auto_injected() {
        let mut store = SkillStore::new();
        store.skills.push(Skill {
            id: SkillId::new(),
            name: "retired-review".to_string(),
            description: "review code".to_string(),
            content: String::new(),
            trigger_patterns: vec!["code review".to_string()],
            version: "1.0.0".to_string(),
            author: "system".to_string(),
            location: SkillLocation::System,
            content_hash: compute_content_hash(""),
            usage_count: 0,
            last_used: None,
            quality_score: 0.2,
            scored_samples: 100,
            governance_status: SkillGovernanceStatus::Retired,
            canary_ratio: 0.0,
            last_scored: Some(Utc::now()),
        });
        let matches = store.match_prompt("please do a code review of this PR");
        assert!(matches.is_empty(), "retired skill should not auto-inject");
    }

    #[test]
    fn discover_reloads_persisted_skills_across_restart() {
        let dir = tempfile::tempdir().expect("tempdir");
        let persist_path = dir.path().to_path_buf();

        // First "session": create and persist a skill to disk.
        let mut store = SkillStore::new().with_persist_dir(persist_path.clone());
        store.create(
            "custom-workflow".to_string(),
            "# Custom Workflow\nDoes custom things.".to_string(),
        );
        assert!(
            persist_path.join("custom-workflow.md").exists(),
            "skill file must be written to disk before restart"
        );

        // Simulate server restart: new store, same persist_dir.
        let mut restarted = SkillStore::new().with_persist_dir(persist_path.clone());
        restarted.load_builtin();
        restarted
            .discover()
            .expect("discover must not fail on restart");

        assert!(
            restarted.get_by_name("custom-workflow").is_some(),
            "persisted skill must survive simulated server restart"
        );
    }

    #[test]
    fn persisted_skill_shadowing_builtin_survives_restart() {
        let dir = tempfile::tempdir().expect("tempdir");
        let persist_path = dir.path().to_path_buf();

        // First "session": create a user skill whose name matches a builtin.
        let user_content = "# review\nMy custom review skill.".to_string();
        let mut store = SkillStore::new().with_persist_dir(persist_path.clone());
        store.create("review".to_string(), user_content.clone());
        assert!(
            persist_path.join("review.md").exists(),
            "skill file must be written to disk before restart"
        );

        // Simulate server restart: load builtins first (as http.rs does),
        // then discover persisted skills.
        let mut restarted = SkillStore::new().with_persist_dir(persist_path.clone());
        restarted.load_builtin();
        restarted
            .discover()
            .expect("discover must not fail on restart");

        // The user's override must win over the same-named builtin.
        let skill = restarted
            .get_by_name("review")
            .expect("shadowed builtin must survive restart");
        assert_eq!(
            skill.location,
            SkillLocation::User,
            "persisted skill must have User location, not System"
        );
        assert_eq!(
            skill.content, user_content,
            "persisted skill content must be the user version, not the builtin"
        );
    }
}
