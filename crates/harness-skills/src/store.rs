use chrono::{DateTime, Utc};
use harness_core::{types::SkillId, types::SkillLocation};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub use crate::freshness::FreshnessClass;

#[cfg(test)]
mod tests;

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
    pub fn list_stale(&self, now: DateTime<Utc>) -> Vec<(&Skill, FreshnessClass)> {
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
        for &(name, content) in crate::builtin::BUILTIN_SKILLS {
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
