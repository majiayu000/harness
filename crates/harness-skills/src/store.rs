use chrono::{DateTime, Utc};
use harness_core::{SkillId, SkillLocation};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

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
}

/// Sidecar data persisted alongside skill files.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct SkillUsage {
    usage_count: u64,
    last_used: Option<DateTime<Utc>>,
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
        let (name, count, last_used) = {
            let Some(skill) = self.skills.iter_mut().find(|s| s.id == *id) else {
                return;
            };
            skill.usage_count += 1;
            skill.last_used = Some(Utc::now());
            (skill.name.clone(), skill.usage_count, skill.last_used)
        };
        if let Some(dir) = self.skill_dirs.get(&name) {
            let path = dir.join(format!("{}.usage.json", name));
            let usage = SkillUsage {
                usage_count: count,
                last_used,
            };
            match serde_json::to_string(&usage) {
                Ok(json) => {
                    if let Err(e) = std::fs::write(&path, json) {
                        tracing::warn!("failed to persist usage for skill \'{}\': {e}", name);
                    }
                }
                Err(e) => tracing::warn!("failed to serialize usage for skill \'{}\': {e}", name),
            }
        }
    }

    /// Enable disk persistence: created skills are written to `{dir}/{name}.md`,
    /// deleted skills are removed, and the dir is added to discovery_paths so
    /// skills survive restarts.
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
            })
            .collect()
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
        let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        let mut to_remove = Vec::new();

        for (idx, skill) in self.skills.iter().enumerate() {
            if let Some(&existing_idx) = seen.get(&skill.name) {
                let existing_priority = location_priority(self.skills[existing_idx].location);
                let new_priority = location_priority(skill.location);
                if new_priority > existing_priority {
                    to_remove.push(existing_idx);
                    seen.insert(skill.name.clone(), idx);
                } else {
                    to_remove.push(idx);
                }
            } else {
                seen.insert(skill.name.clone(), idx);
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
                    usage_count: 0,
                    last_used: None,
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
    }

    #[test]
    fn load_usage_sidecar_returns_defaults_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let usage = load_usage_sidecar(tmp.path(), "nonexistent");
        assert_eq!(usage.usage_count, 0);
        assert!(usage.last_used.is_none());
    }

    #[test]
    fn load_usage_sidecar_restores_persisted_values() {
        let tmp = tempfile::tempdir().unwrap();
        let sidecar = tmp.path().join("myskill.usage.json");
        let stored = SkillUsage {
            usage_count: 42,
            last_used: Some(chrono::Utc::now()),
        };
        std::fs::write(&sidecar, serde_json::to_string(&stored).unwrap()).unwrap();
        let loaded = load_usage_sidecar(tmp.path(), "myskill");
        assert_eq!(loaded.usage_count, 42);
        assert!(loaded.last_used.is_some());
    }
}
