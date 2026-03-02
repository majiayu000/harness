use harness_core::{SkillId, SkillLocation};
use serde::{Deserialize, Serialize};
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
}

pub struct SkillStore {
    skills: Vec<Skill>,
    discovery_paths: Vec<PathBuf>,
}

impl SkillStore {
    pub fn new() -> Self {
        Self {
            skills: Vec::new(),
            discovery_paths: Vec::new(),
        }
    }

    /// Set up the 4-layer discovery chain.
    pub fn with_discovery(mut self, project_root: &Path) -> Self {
        // Repo level
        self.discovery_paths.push(project_root.join(".harness/skills/"));

        // User level
        if let Ok(home) = std::env::var("HOME") {
            self.discovery_paths.push(PathBuf::from(home).join(".harness/skills/"));
        }

        // Admin level
        self.discovery_paths.push(PathBuf::from("/etc/harness/skills/"));

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
            // Check if it's a project-level path (not under home)
            if let Ok(home) = std::env::var("HOME") {
                if dir.starts_with(&home) && !dir.starts_with(PathBuf::from(&home).join(".harness")) {
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

                    self.skills.push(Skill {
                        id: SkillId::new(),
                        name: name.clone(),
                        description: content.lines().next().unwrap_or("").trim_start_matches('#').trim().to_string(),
                        content,
                        trigger_patterns: Vec::new(),
                        version: "1.0.0".to_string(),
                        author: "system".to_string(),
                        location,
                    });
                }
            }
        }
        Ok(())
    }

    /// Match skills to current context (file patterns, language, etc.).
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

    /// Deduplicate: same-name skills are resolved by location priority (repo > user > admin > system).
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
        let skill = Skill {
            id: SkillId::new(),
            name,
            description: content.lines().next().unwrap_or("").to_string(),
            content,
            trigger_patterns: Vec::new(),
            version: "1.0.0".to_string(),
            author: "user".to_string(),
            location: SkillLocation::User,
        };
        self.skills.push(skill);
        match self.skills.last() {
            Some(skill) => skill,
            None => unreachable!(),
        }
    }

    pub fn get(&self, id: &SkillId) -> Option<&Skill> {
        self.skills.iter().find(|s| s.id == *id)
    }

    pub fn delete(&mut self, id: &SkillId) -> bool {
        let len = self.skills.len();
        self.skills.retain(|s| s.id != *id);
        self.skills.len() < len
    }

    pub fn list(&self) -> &[Skill] {
        &self.skills
    }

    pub fn search(&self, query: &str) -> Vec<&Skill> {
        let q = query.to_lowercase();
        self.skills
            .iter()
            .filter(|s| {
                s.name.to_lowercase().contains(&q)
                    || s.description.to_lowercase().contains(&q)
            })
            .collect()
    }
}

impl Default for SkillStore {
    fn default() -> Self {
        Self::new()
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
        }
    }

    #[test]
    fn deduplicate_keeps_higher_priority() {
        let mut store = SkillStore::new();
        store.skills.push(make_skill("deploy", SkillLocation::System));
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
        store.create("my-skill".to_string(), "# My Skill\nDoes stuff.".to_string());
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
    fn search_finds_by_name() {
        let mut store = SkillStore::new();
        store.create("rust-lint".to_string(), "# Lint tool".to_string());
        store.create("python-format".to_string(), "# Format tool".to_string());
        let results = store.search("rust");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "rust-lint");
    }
}
