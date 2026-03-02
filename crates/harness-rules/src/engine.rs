use harness_core::{
    Category, GuardId, Language, RuleId, Severity, Violation,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    pub id: RuleId,
    pub title: String,
    pub severity: Severity,
    pub category: Category,
    pub paths: Vec<String>,
    pub description: String,
    pub fix_pattern: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Guard {
    pub id: GuardId,
    pub script_path: PathBuf,
    pub language: Language,
    pub rules: Vec<RuleId>,
}

pub struct RuleEngine {
    rules: Vec<Rule>,
    guards: Vec<Guard>,
}

impl RuleEngine {
    pub fn new() -> Self {
        Self {
            rules: Vec::new(),
            guards: Vec::new(),
        }
    }

    /// Load rules using the 4-layer discovery chain: repo → user → admin → system.
    pub fn load(&mut self, project_root: &Path) -> anyhow::Result<()> {
        // Repo level
        let repo_rules = project_root.join(".harness/rules/");
        if repo_rules.is_dir() {
            self.load_from(&repo_rules)?;
        }

        // User level
        if let Some(home) = std::env::var("HOME").ok() {
            let user_rules = PathBuf::from(home).join(".harness/rules/");
            if user_rules.is_dir() {
                self.load_from(&user_rules)?;
            }
        }

        // Admin level
        let admin_rules = Path::new("/etc/harness/rules/");
        if admin_rules.is_dir() {
            self.load_from(admin_rules)?;
        }

        // System (builtin) level
        self.load_builtin()?;

        Ok(())
    }

    fn load_from(&mut self, dir: &Path) -> anyhow::Result<()> {
        if !dir.is_dir() {
            return Ok(());
        }

        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().map(|e| e == "md" || e == "toml").unwrap_or(false) {
                if let Ok(content) = std::fs::read_to_string(&path) {
                    self.parse_rule_file(&path, &content)?;
                }
            }

            // Recurse into subdirectories
            if path.is_dir() {
                self.load_from(&path)?;
            }
        }
        Ok(())
    }

    fn load_builtin(&mut self) -> anyhow::Result<()> {
        // Built-in golden principles are loaded from compiled-in rules
        Ok(())
    }

    fn parse_rule_file(&mut self, _path: &Path, content: &str) -> anyhow::Result<()> {
        // Parse markdown rule files with optional YAML frontmatter
        let mut in_frontmatter = false;
        let mut frontmatter = String::new();
        let mut body = String::new();

        for line in content.lines() {
            if line.trim() == "---" {
                if in_frontmatter {
                    in_frontmatter = false;
                    continue;
                }
                if frontmatter.is_empty() && body.is_empty() {
                    in_frontmatter = true;
                    continue;
                }
            }
            if in_frontmatter {
                frontmatter.push_str(line);
                frontmatter.push('\n');
            } else {
                body.push_str(line);
                body.push('\n');
            }
        }

        // Extract rule blocks from markdown body (## ID: Title pattern)
        for section in body.split("\n## ") {
            let first_line = section.lines().next().unwrap_or("");
            if let Some((id_part, title)) = first_line.split_once(':') {
                let id = id_part.trim().to_string();
                if id.is_empty() || !id.chars().next().map(|c| c.is_ascii_uppercase()).unwrap_or(false) {
                    continue;
                }
                let severity = if section.contains("严重") || section.contains("critical") {
                    Severity::Critical
                } else if section.contains("严格") || section.contains("high") {
                    Severity::High
                } else if section.contains("中") || section.contains("medium") {
                    Severity::Medium
                } else {
                    Severity::Low
                };

                let category = if id.starts_with("SEC") {
                    Category::Security
                } else if id.starts_with("RS") || id.starts_with("GO") || id.starts_with("TS") || id.starts_with("PY") {
                    Category::Stability
                } else {
                    Category::Style
                };

                self.rules.push(Rule {
                    id: RuleId::from_str(&id),
                    title: title.trim().to_string(),
                    severity,
                    category,
                    paths: Vec::new(),
                    description: section.to_string(),
                    fix_pattern: None,
                });
            }
        }

        Ok(())
    }

    pub fn register_guard(&mut self, guard: Guard) {
        self.guards.push(guard);
    }

    /// Scan a project for violations using registered guards.
    pub async fn scan(&self, project_root: &Path) -> anyhow::Result<Vec<Violation>> {
        let mut violations = Vec::new();
        for guard in &self.guards {
            let output = tokio::process::Command::new("bash")
                .arg(&guard.script_path)
                .arg(project_root)
                .output()
                .await?;
            violations.extend(self.parse_guard_output(&output, guard)?);
        }
        Ok(violations)
    }

    /// Scan specific files.
    pub async fn scan_files(&self, project_root: &Path, files: &[PathBuf]) -> anyhow::Result<Vec<Violation>> {
        let mut violations = Vec::new();
        for guard in &self.guards {
            for file in files {
                let output = tokio::process::Command::new("bash")
                    .arg(&guard.script_path)
                    .arg(project_root)
                    .arg(file)
                    .output()
                    .await?;
                violations.extend(self.parse_guard_output(&output, guard)?);
            }
        }
        Ok(violations)
    }

    fn parse_guard_output(
        &self,
        output: &std::process::Output,
        _guard: &Guard,
    ) -> anyhow::Result<Vec<Violation>> {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut violations = Vec::new();

        for line in stdout.lines() {
            // Expected format: FILE:LINE:RULE_ID:MESSAGE
            let parts: Vec<&str> = line.splitn(4, ':').collect();
            if parts.len() >= 4 {
                let rule_id = RuleId::from_str(parts[2].trim());
                let severity = self
                    .rules
                    .iter()
                    .find(|r| r.id == rule_id)
                    .map(|r| r.severity)
                    .unwrap_or(Severity::Medium);

                violations.push(Violation {
                    rule_id,
                    file: PathBuf::from(parts[0]),
                    line: parts[1].parse().ok(),
                    message: parts[3].to_string(),
                    severity,
                });
            }
        }

        Ok(violations)
    }

    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    pub fn guards(&self) -> &[Guard] {
        &self.guards
    }
}

impl Default for RuleEngine {
    fn default() -> Self {
        Self::new()
    }
}
