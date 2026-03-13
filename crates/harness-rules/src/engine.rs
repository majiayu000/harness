use crate::exec_policy::{
    ExecPolicy, ExecPolicyCheckOutput, ExecPolicyParser, MatchOptions, RequirementsToml,
};
use anyhow::Context;
use harness_core::{Category, GuardId, Language, RuleId, Severity, Violation};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub const BUILTIN_BASELINE_GUARD_ID: &str = "BUILTIN-BASELINE-SCAN";
pub const WARN_NO_GUARDS_REGISTERED: &str = "rule scan warning: no guards registered";
pub const WARN_EMPTY_SCAN_INPUT: &str = "rule scan warning: empty scan input";

const BUILTIN_BASELINE_GUARD_FILE: &str = "builtin-baseline-scan.sh";
const BUILTIN_BASELINE_GUARD_SCRIPT: &str = r#"#!/usr/bin/env bash
set -euo pipefail
# Baseline built-in guard. It exists to make default scan execution observable
# and intentionally emits no violations.
project_root="${1:-}"
if [[ -z "${project_root}" ]]; then
  exit 0
fi
exit 0
"#;

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
    rule_ids: HashSet<RuleId>,
    guards: Vec<Guard>,
    exec_policy: ExecPolicy,
    discovery_paths: Vec<PathBuf>,
    builtin_path: Option<PathBuf>,
    requirements_path: Option<PathBuf>,
}

impl RuleEngine {
    pub fn new() -> Self {
        Self {
            rules: Vec::new(),
            rule_ids: HashSet::new(),
            guards: Vec::new(),
            exec_policy: ExecPolicy::empty(),
            discovery_paths: Vec::new(),
            builtin_path: None,
            requirements_path: None,
        }
    }

    pub fn configure_sources(
        &mut self,
        discovery_paths: Vec<PathBuf>,
        builtin_path: Option<PathBuf>,
        requirements_path: Option<PathBuf>,
    ) {
        self.discovery_paths = discovery_paths;
        self.builtin_path = builtin_path;
        self.requirements_path = requirements_path;
    }

    /// Load rules using the 4-layer discovery chain: repo → user → admin → system.
    pub fn load(&mut self, project_root: &Path) -> anyhow::Result<()> {
        if self.discovery_paths.is_empty() {
            // Repo level
            let repo_rules = project_root.join(".harness/rules/");
            if repo_rules.is_dir() {
                self.load_from(&repo_rules)?;
            }

            // User level
            if let Ok(home) = std::env::var("HOME") {
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
        } else {
            let discovery_paths = self.discovery_paths.clone();
            for source in discovery_paths {
                if source.is_dir() {
                    self.load_from(&source)?;
                } else if source.is_file() {
                    let content = std::fs::read_to_string(&source)?;
                    self.parse_rule_file(&source, &content)?;
                }
            }
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
            if path
                .extension()
                .map(|e| e == "md" || e == "toml")
                .unwrap_or(false)
            {
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

    pub fn load_builtin(&mut self) -> anyhow::Result<()> {
        if let Some(path) = self.builtin_path.clone() {
            if path.is_dir() {
                self.load_from(&path)?;
                return Ok(());
            }
            if path.is_file() {
                let content = std::fs::read_to_string(&path)?;
                self.parse_rule_file(&path, &content)?;
                return Ok(());
            }
            anyhow::bail!(
                "configured rules.builtin_path does not exist: {}",
                path.display()
            );
        }

        let builtin = [
            (
                "golden-principles.md",
                include_str!("../../../rules/golden-principles.md"),
            ),
            (
                "common/coding-style.md",
                include_str!("../../../rules/common/coding-style.md"),
            ),
            (
                "common/security.md",
                include_str!("../../../rules/common/security.md"),
            ),
            (
                "go/quality.md",
                include_str!("../../../rules/go/quality.md"),
            ),
            (
                "python/quality.md",
                include_str!("../../../rules/python/quality.md"),
            ),
            (
                "rust/quality.md",
                include_str!("../../../rules/rust/quality.md"),
            ),
            (
                "typescript/quality.md",
                include_str!("../../../rules/typescript/quality.md"),
            ),
        ];
        for (name, content) in builtin {
            self.parse_rule_file(Path::new(name), content)?;
        }
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

        let paths = Self::parse_frontmatter_paths(&frontmatter)?;
        let fix_pattern = Self::parse_frontmatter_fix_pattern(&frontmatter)?;

        // Extract rule blocks from markdown body (## ID: Title pattern)
        for section in body.split("\n## ") {
            let first_line = section.lines().next().unwrap_or("");
            if let Some((id_part, title)) = first_line.split_once(':') {
                let id = id_part.trim_start_matches('#').trim().to_string();
                if id.is_empty()
                    || !id
                        .chars()
                        .next()
                        .map(|c| c.is_ascii_uppercase())
                        .unwrap_or(false)
                {
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
                } else if id.starts_with("RS")
                    || id.starts_with("GO")
                    || id.starts_with("TS")
                    || id.starts_with("PY")
                {
                    Category::Stability
                } else {
                    Category::Style
                };

                let rule_id = RuleId::from_str(&id);
                self.rule_ids.insert(rule_id.clone());
                self.rules.push(Rule {
                    id: rule_id,
                    title: title.trim().to_string(),
                    severity,
                    category,
                    paths: paths.clone(),
                    description: section.to_string(),
                    fix_pattern: fix_pattern.clone(),
                });
            }
        }

        Ok(())
    }

    /// Parse `paths:` field from YAML frontmatter using `serde_yaml`.
    ///
    /// Supports any valid YAML representation of `paths:`, including:
    /// - Inline array:  `paths: ["*.rs", "src/**"]`
    /// - Quoted string: `paths: "**/*.go"`
    /// - Block sequence: multi-line YAML list
    fn parse_frontmatter_paths(frontmatter: &str) -> anyhow::Result<Vec<String>> {
        if frontmatter.is_empty() {
            return Ok(Vec::new());
        }
        let value = serde_yaml::from_str::<serde_yaml::Value>(frontmatter)?;
        let paths = match value.get("paths") {
            Some(serde_yaml::Value::Sequence(seq)) => seq
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
            Some(serde_yaml::Value::String(s)) => vec![s.clone()],
            _ => Vec::new(),
        };
        Ok(paths)
    }

    /// Parse `fix_pattern:` field from YAML frontmatter.
    ///
    /// The value is a string in the format `"regex_pattern -> replacement"`.
    /// If no `->` separator is present, the entire string is treated as a
    /// pattern and matches are replaced with an empty string.
    fn parse_frontmatter_fix_pattern(frontmatter: &str) -> anyhow::Result<Option<String>> {
        if frontmatter.is_empty() {
            return Ok(None);
        }
        let value = serde_yaml::from_str::<serde_yaml::Value>(frontmatter)?;
        let pattern = match value.get("fix_pattern") {
            Some(serde_yaml::Value::String(s)) => Some(s.clone()),
            _ => None,
        };
        Ok(pattern)
    }

    /// Apply fix patterns to files for the given violations.
    ///
    /// The `fix_pattern` on a rule is a string of the form `"regex -> replacement"`.
    /// If no `->` separator is present, matches are replaced with an empty string.
    /// Returns the number of files modified.
    pub fn apply_fix_patterns(
        &self,
        violations: &[harness_core::Violation],
        project_root: &Path,
    ) -> anyhow::Result<usize> {
        let mut files_modified = 0;
        for violation in violations {
            let Some(rule) = self.rules.iter().find(|r| r.id == violation.rule_id) else {
                continue;
            };
            let Some(fix_pattern) = &rule.fix_pattern else {
                continue;
            };
            let file_path = if violation.file.is_absolute() {
                violation.file.clone()
            } else {
                project_root.join(&violation.file)
            };
            if !file_path.is_file() {
                continue;
            }
            let content = std::fs::read_to_string(&file_path)?;
            let new_content = Self::apply_fix(fix_pattern, &content)?;
            if new_content != content {
                std::fs::write(&file_path, new_content)?;
                files_modified += 1;
            }
        }
        Ok(files_modified)
    }

    fn apply_fix(fix_pattern: &str, content: &str) -> anyhow::Result<String> {
        let (pattern, replacement) = match fix_pattern.split_once(" -> ") {
            Some((p, r)) => (p, r),
            None => (fix_pattern, ""),
        };
        let re = regex::Regex::new(pattern)
            .with_context(|| format!("invalid fix_pattern regex: {pattern}"))?;
        Ok(re.replace_all(content, replacement).into_owned())
    }

    pub fn register_guard(&mut self, guard: Guard) {
        if self.guards.iter().any(|existing| existing.id == guard.id) {
            return;
        }
        self.guards.push(guard);
    }

    pub fn auto_register_builtin_guards(&mut self, data_dir: &Path) -> anyhow::Result<usize> {
        if self
            .guards
            .iter()
            .any(|guard| guard.id.as_str() == BUILTIN_BASELINE_GUARD_ID)
        {
            return Ok(0);
        }

        let guard_dir = data_dir.join("guards");
        std::fs::create_dir_all(&guard_dir).with_context(|| {
            format!(
                "failed to create builtin guard directory: {}",
                guard_dir.display()
            )
        })?;

        let script_path = guard_dir.join(BUILTIN_BASELINE_GUARD_FILE);
        std::fs::write(&script_path, BUILTIN_BASELINE_GUARD_SCRIPT).with_context(|| {
            format!(
                "failed to write builtin guard script: {}",
                script_path.display()
            )
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script_path)
                .with_context(|| {
                    format!(
                        "failed to read builtin guard metadata: {}",
                        script_path.display()
                    )
                })?
                .permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script_path, perms).with_context(|| {
                format!(
                    "failed to set builtin guard executable bit: {}",
                    script_path.display()
                )
            })?;
        }

        self.register_guard(Guard {
            id: GuardId::from_str(BUILTIN_BASELINE_GUARD_ID),
            script_path,
            language: Language::Common,
            rules: Vec::new(),
        });
        Ok(1)
    }

    pub fn validate_scan_request(&self, files: Option<&[PathBuf]>) -> anyhow::Result<()> {
        if let Some(files) = files {
            if files.is_empty() {
                tracing::warn!("{WARN_EMPTY_SCAN_INPUT}");
                anyhow::bail!(WARN_EMPTY_SCAN_INPUT);
            }
        }

        if self.guards.is_empty() {
            tracing::warn!("{WARN_NO_GUARDS_REGISTERED}");
            anyhow::bail!(WARN_NO_GUARDS_REGISTERED);
        }

        Ok(())
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
    pub async fn scan_files(
        &self,
        project_root: &Path,
        files: &[PathBuf],
    ) -> anyhow::Result<Vec<Violation>> {
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

    /// Add a rule, deduplicating by rule_id (skip if already present).
    pub fn add_rule(&mut self, rule: Rule) {
        if self.rule_ids.contains(&rule.id) {
            return;
        }
        self.rule_ids.insert(rule.id.clone());
        self.rules.push(rule);
    }

    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    pub fn guards(&self) -> &[Guard] {
        &self.guards
    }

    pub fn exec_policy(&self) -> &ExecPolicy {
        &self.exec_policy
    }

    pub fn load_exec_policy_files(&mut self, policy_paths: &[PathBuf]) -> anyhow::Result<()> {
        if policy_paths.is_empty() {
            return Ok(());
        }
        let mut parser = ExecPolicyParser::new();
        for path in policy_paths {
            let content = std::fs::read_to_string(path)
                .with_context(|| format!("failed to read execpolicy file {}", path.display()))?;
            let identifier = path.to_string_lossy().to_string();
            parser
                .parse(&identifier, &content)
                .with_context(|| format!("failed to parse execpolicy file {}", path.display()))?;
        }
        self.exec_policy = self.exec_policy.merge_overlay(&parser.build());
        Ok(())
    }

    pub fn load_requirements_toml(&mut self, path: &Path) -> anyhow::Result<()> {
        let requirements = RequirementsToml::from_path(path)?;
        let requirements_policy = requirements.to_policy()?;
        self.exec_policy = self.exec_policy.merge_overlay(&requirements_policy);
        Ok(())
    }

    pub fn load_configured_requirements(&mut self) -> anyhow::Result<()> {
        let Some(path) = self.requirements_path.clone() else {
            return Ok(());
        };
        if !path.exists() {
            anyhow::bail!(
                "configured rules.requirements_path does not exist: {}",
                path.display()
            );
        }
        self.load_requirements_toml(&path)
    }

    pub fn check_command_policy(
        &self,
        command: &[String],
        options: &MatchOptions,
    ) -> ExecPolicyCheckOutput {
        self.exec_policy.check_command(command, options)
    }
}

impl Default for RuleEngine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_engine_with_content(content: &str) -> anyhow::Result<RuleEngine> {
        let mut engine = RuleEngine::new();
        engine.parse_rule_file(Path::new("test.md"), content)?;
        Ok(engine)
    }

    #[test]
    fn parse_rule_file_extracts_security_rule() -> anyhow::Result<()> {
        // Leading newline required: parser splits on "\n## "
        let md = "\n## SEC-01: SQL injection\n\n严重 — use params.\n";
        let engine = make_engine_with_content(md)?;
        assert_eq!(engine.rules().len(), 1);
        assert_eq!(engine.rules()[0].id, RuleId::from_str("SEC-01"));
        assert_eq!(engine.rules()[0].title, "SQL injection");
        assert_eq!(engine.rules()[0].severity, Severity::Critical);
        assert_eq!(engine.rules()[0].category, Category::Security);
        Ok(())
    }

    #[test]
    fn parse_rule_file_extracts_multiple_rules() -> anyhow::Result<()> {
        let md = "\n## SEC-01: First rule\n\n严重\n\n## SEC-02: Second rule\n\nhigh severity\n";
        let engine = make_engine_with_content(md)?;
        assert_eq!(engine.rules().len(), 2);
        Ok(())
    }

    #[test]
    fn parse_rule_file_skips_lowercase_ids() -> anyhow::Result<()> {
        let md = "\n## lowercase: should be skipped\n\nsome content\n";
        let engine = make_engine_with_content(md)?;
        assert_eq!(engine.rules().len(), 0);
        Ok(())
    }

    #[test]
    fn parse_rule_file_detects_category_from_prefix() -> anyhow::Result<()> {
        let md = "\n## RS-01: Rust stability rule\n\nhigh\n";
        let engine = make_engine_with_content(md)?;
        let rules = engine.rules();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].category, Category::Stability);
        Ok(())
    }

    #[test]
    fn empty_rule_engine_has_no_rules() {
        let engine = RuleEngine::new();
        assert!(engine.rules().is_empty());
        assert!(engine.guards().is_empty());
    }

    #[test]
    fn load_builtin_returns_at_least_40_rules() -> anyhow::Result<()> {
        let mut engine = RuleEngine::new();
        engine.load_builtin()?;
        assert!(
            engine.rules().len() >= 40,
            "expected >= 40 builtin rules, got {}",
            engine.rules().len()
        );
        Ok(())
    }

    #[test]
    fn load_builtin_parses_gp_and_sec_ids() -> anyhow::Result<()> {
        let mut engine = RuleEngine::new();
        engine.load_builtin()?;
        assert!(
            engine.rules().iter().any(|r| r.id.as_str() == "GP-01"),
            "GP-01 rule missing"
        );
        assert!(
            engine.rules().iter().any(|r| r.id.as_str() == "SEC-01"),
            "SEC-01 rule missing"
        );
        Ok(())
    }

    #[test]
    fn parse_frontmatter_paths_inline_array() -> anyhow::Result<()> {
        let frontmatter = "paths: [\"*.rs\", \"src/**\"]\n";
        let paths = RuleEngine::parse_frontmatter_paths(frontmatter)?;
        assert_eq!(paths, vec!["*.rs", "src/**"]);
        Ok(())
    }

    #[test]
    fn parse_frontmatter_paths_empty() -> anyhow::Result<()> {
        let paths = RuleEngine::parse_frontmatter_paths("")?;
        assert!(paths.is_empty());
        Ok(())
    }

    #[test]
    fn parse_frontmatter_paths_quoted_string() -> anyhow::Result<()> {
        let frontmatter = "paths: \"**/*.go\"\n";
        let paths = RuleEngine::parse_frontmatter_paths(frontmatter)?;
        assert_eq!(paths, vec!["**/*.go"]);
        Ok(())
    }

    #[test]
    fn parse_frontmatter_paths_block_sequence() -> anyhow::Result<()> {
        let frontmatter = "paths:\n  - \"*.rs\"\n  - \"src/**\"\n";
        let paths = RuleEngine::parse_frontmatter_paths(frontmatter)?;
        assert_eq!(paths, vec!["*.rs", "src/**"]);
        Ok(())
    }

    #[test]
    fn add_rule_inserts_new_rule() {
        let mut engine = RuleEngine::new();
        let rule = Rule {
            id: RuleId::from_str("LEARN-001"),
            title: "Test rule".to_string(),
            severity: Severity::High,
            category: Category::Style,
            paths: Vec::new(),
            description: "desc".to_string(),
            fix_pattern: None,
        };
        engine.add_rule(rule);
        assert_eq!(engine.rules().len(), 1);
        assert_eq!(engine.rules()[0].id, RuleId::from_str("LEARN-001"));
    }

    #[test]
    fn add_rule_deduplicates_by_id() {
        let mut engine = RuleEngine::new();
        let make = |title: &str| Rule {
            id: RuleId::from_str("LEARN-001"),
            title: title.to_string(),
            severity: Severity::High,
            category: Category::Style,
            paths: Vec::new(),
            description: "desc".to_string(),
            fix_pattern: None,
        };
        engine.add_rule(make("first"));
        engine.add_rule(make("duplicate"));
        assert_eq!(engine.rules().len(), 1, "duplicate rule_id must be skipped");
        assert_eq!(engine.rules()[0].title, "first");
    }

    #[test]
    fn auto_register_builtin_guards_registers_guard_once() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut engine = RuleEngine::new();
        let first = engine.auto_register_builtin_guards(dir.path())?;
        let second = engine.auto_register_builtin_guards(dir.path())?;

        assert_eq!(first, 1);
        assert_eq!(second, 0);
        assert_eq!(engine.guards().len(), 1);
        assert_eq!(engine.guards()[0].id.as_str(), BUILTIN_BASELINE_GUARD_ID);
        assert!(
            engine.guards()[0].script_path.is_file(),
            "builtin guard script should be materialized on disk"
        );
        Ok(())
    }

    #[test]
    fn validate_scan_request_rejects_missing_guards() {
        let engine = RuleEngine::new();
        let err = engine
            .validate_scan_request(None)
            .expect_err("missing guards should be rejected");
        assert!(
            err.to_string().contains(WARN_NO_GUARDS_REGISTERED),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn validate_scan_request_rejects_empty_file_input() {
        let mut engine = RuleEngine::new();
        engine.register_guard(Guard {
            id: GuardId::from_str("TEST-GUARD"),
            script_path: PathBuf::from("unused.sh"),
            language: Language::Common,
            rules: vec![],
        });
        let err = engine
            .validate_scan_request(Some(&[]))
            .expect_err("empty scan input should be rejected");
        assert!(
            err.to_string().contains(WARN_EMPTY_SCAN_INPUT),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn load_exec_policy_files_adds_command_policy() -> anyhow::Result<()> {
        let sandbox = tempfile::tempdir()?;
        let policy_path = sandbox.path().join("policy.star");
        std::fs::write(
            &policy_path,
            r#"
prefix_rule(pattern = ["git", "push"], decision = "prompt")
"#,
        )?;
        let mut engine = RuleEngine::new();
        engine.load_exec_policy_files(&[policy_path])?;

        let result = engine.check_command_policy(
            &["git".to_string(), "push".to_string()],
            &MatchOptions::default(),
        );
        assert_eq!(
            result.decision,
            Some(crate::exec_policy::ExecDecision::Prompt)
        );
        Ok(())
    }

    #[test]
    fn load_exec_policy_files_merges_host_executable_paths_for_same_name() -> anyhow::Result<()> {
        let sandbox = tempfile::tempdir()?;
        let first_policy_path = sandbox.path().join("first.star");
        let second_policy_path = sandbox.path().join("second.star");
        std::fs::write(
            &first_policy_path,
            r#"
prefix_rule(pattern = ["git", "status"], decision = "allow")
host_executable(name = "git", paths = ["/usr/bin/git"])
"#,
        )?;
        std::fs::write(
            &second_policy_path,
            r#"
prefix_rule(pattern = ["git", "status"], decision = "allow")
host_executable(name = "git", paths = ["/opt/homebrew/bin/git"])
"#,
        )?;

        let mut engine = RuleEngine::new();
        engine.load_exec_policy_files(&[first_policy_path, second_policy_path])?;
        let options = MatchOptions {
            resolve_host_executables: true,
        };

        let usr_bin_result = engine.check_command_policy(
            &["/usr/bin/git".to_string(), "status".to_string()],
            &options,
        );
        assert_eq!(
            usr_bin_result.decision,
            Some(crate::exec_policy::ExecDecision::Allow)
        );

        let homebrew_result = engine.check_command_policy(
            &["/opt/homebrew/bin/git".to_string(), "status".to_string()],
            &options,
        );
        assert_eq!(
            homebrew_result.decision,
            Some(crate::exec_policy::ExecDecision::Allow)
        );
        Ok(())
    }

    #[test]
    fn load_configured_requirements_missing_path_errors() {
        let mut engine = RuleEngine::new();
        engine.configure_sources(
            Vec::new(),
            None,
            Some(PathBuf::from("/tmp/does-not-exist-requirements.toml")),
        );
        let error = engine
            .load_configured_requirements()
            .expect_err("missing configured requirements path must error");
        assert!(error.to_string().contains("rules.requirements_path"));
    }

    #[test]
    fn parse_frontmatter_fix_pattern_returns_none_when_absent() -> anyhow::Result<()> {
        let frontmatter = "paths: [\"*.rs\"]\n";
        let pattern = RuleEngine::parse_frontmatter_fix_pattern(frontmatter)?;
        assert!(pattern.is_none());
        Ok(())
    }

    #[test]
    fn parse_frontmatter_fix_pattern_returns_value_when_present() -> anyhow::Result<()> {
        // Use YAML single quotes so backslashes are treated as literals
        let frontmatter = "paths: [\"*.rs\"]\nfix_pattern: 'todo!\\(\\) -> unimplemented!()'\n";
        let pattern = RuleEngine::parse_frontmatter_fix_pattern(frontmatter)?;
        assert_eq!(pattern.as_deref(), Some("todo!\\(\\) -> unimplemented!()"));
        Ok(())
    }

    #[test]
    fn parse_frontmatter_fix_pattern_empty_frontmatter_returns_none() -> anyhow::Result<()> {
        let pattern = RuleEngine::parse_frontmatter_fix_pattern("")?;
        assert!(pattern.is_none());
        Ok(())
    }

    #[test]
    fn parse_rule_file_sets_fix_pattern_from_frontmatter() -> anyhow::Result<()> {
        let md =
            "---\nfix_pattern: \"foo -> bar\"\n---\n\n## FIX-01: Test fix rule\n\nhigh severity\n";
        let engine = make_engine_with_content(md)?;
        assert_eq!(engine.rules().len(), 1);
        assert_eq!(engine.rules()[0].fix_pattern.as_deref(), Some("foo -> bar"));
        Ok(())
    }

    #[test]
    fn parse_rule_file_fix_pattern_none_when_not_in_frontmatter() -> anyhow::Result<()> {
        let md = "---\npaths: [\"*.rs\"]\n---\n\n## FIX-02: No fix rule\n\nhigh severity\n";
        let engine = make_engine_with_content(md)?;
        assert_eq!(engine.rules().len(), 1);
        assert!(engine.rules()[0].fix_pattern.is_none());
        Ok(())
    }

    #[test]
    fn apply_fix_replaces_pattern_with_replacement() -> anyhow::Result<()> {
        let content = "let x = todo!();\nlet y = 42;\n";
        let result = RuleEngine::apply_fix("todo!\\(\\) -> unimplemented!()", content)?;
        assert_eq!(result, "let x = unimplemented!();\nlet y = 42;\n");
        Ok(())
    }

    #[test]
    fn apply_fix_deletes_match_when_no_replacement() -> anyhow::Result<()> {
        let content = "REMOVE_ME some text\n";
        let result = RuleEngine::apply_fix("REMOVE_ME ", content)?;
        assert_eq!(result, "some text\n");
        Ok(())
    }

    #[test]
    fn apply_fix_patterns_modifies_file_on_disk() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let file = dir.path().join("test.rs");
        std::fs::write(&file, "let x = todo!();\n")?;

        let mut engine = RuleEngine::new();
        engine.add_rule(Rule {
            id: RuleId::from_str("FIX-03"),
            title: "Replace todo".to_string(),
            severity: Severity::Medium,
            category: Category::Style,
            paths: vec!["*.rs".to_string()],
            description: "desc".to_string(),
            fix_pattern: Some("todo!\\(\\) -> unimplemented!()".to_string()),
        });

        let violations = vec![harness_core::Violation {
            rule_id: RuleId::from_str("FIX-03"),
            file: file.clone(),
            line: Some(1),
            message: "use unimplemented instead".to_string(),
            severity: Severity::Medium,
        }];

        let count = engine.apply_fix_patterns(&violations, dir.path())?;
        assert_eq!(count, 1);
        let updated = std::fs::read_to_string(&file)?;
        assert_eq!(updated, "let x = unimplemented!();\n");
        Ok(())
    }

    #[test]
    fn apply_fix_patterns_skips_rule_without_fix_pattern() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let file = dir.path().join("test.rs");
        std::fs::write(&file, "original content\n")?;

        let mut engine = RuleEngine::new();
        engine.add_rule(Rule {
            id: RuleId::from_str("FIX-04"),
            title: "No fix".to_string(),
            severity: Severity::Low,
            category: Category::Style,
            paths: vec![],
            description: "desc".to_string(),
            fix_pattern: None,
        });

        let violations = vec![harness_core::Violation {
            rule_id: RuleId::from_str("FIX-04"),
            file: file.clone(),
            line: Some(1),
            message: "some violation".to_string(),
            severity: Severity::Low,
        }];

        let count = engine.apply_fix_patterns(&violations, dir.path())?;
        assert_eq!(count, 0);
        let content = std::fs::read_to_string(&file)?;
        assert_eq!(content, "original content\n");
        Ok(())
    }
}
