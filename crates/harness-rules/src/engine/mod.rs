use crate::exec_policy::{
    ExecPolicy, ExecPolicyCheckOutput, ExecPolicyParser, MatchOptions, RequirementsToml,
};
use anyhow::Context;
use harness_core::{
    AutoFixAttempt, AutoFixReport, Category, GuardId, Language, RuleId, Severity, Violation,
};
use regex::Regex;
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
                include_str!("../../../../rules/golden-principles.md"),
            ),
            (
                "common/coding-style.md",
                include_str!("../../../../rules/common/coding-style.md"),
            ),
            (
                "common/security.md",
                include_str!("../../../../rules/common/security.md"),
            ),
            (
                "go/quality.md",
                include_str!("../../../../rules/go/quality.md"),
            ),
            (
                "python/quality.md",
                include_str!("../../../../rules/python/quality.md"),
            ),
            (
                "rust/quality.md",
                include_str!("../../../../rules/rust/quality.md"),
            ),
            (
                "typescript/quality.md",
                include_str!("../../../../rules/typescript/quality.md"),
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
    /// The value is a sed-style replacement string: `s<DELIM>PATTERN<DELIM>REPLACEMENT<DELIM>`.
    /// The character immediately after `s` is used as the delimiter (e.g. `/`, `|`, `#`).
    /// Returns `None` when the field is absent or the frontmatter is empty.
    fn parse_frontmatter_fix_pattern(frontmatter: &str) -> anyhow::Result<Option<String>> {
        if frontmatter.is_empty() {
            return Ok(None);
        }
        let value = serde_yaml::from_str::<serde_yaml::Value>(frontmatter)?;
        let fix_pattern = match value.get("fix_pattern") {
            Some(serde_yaml::Value::String(s)) => Some(s.clone()),
            _ => None,
        };
        Ok(fix_pattern)
    }

    /// Parse a sed-style fix_pattern string into a compiled `Regex` and replacement string.
    ///
    /// Format: `s<DELIM>PATTERN<DELIM>REPLACEMENT[<DELIM>]`
    /// Returns `None` when the string does not match the expected format or the regex is invalid.
    pub fn parse_fix_pattern(fix_pattern: &str) -> Option<(Regex, String)> {
        let bytes = fix_pattern.as_bytes();
        if bytes.len() < 2 || bytes[0] != b's' {
            return None;
        }
        let delim = bytes[1] as char;
        let rest = &fix_pattern[2..];
        let parts: Vec<&str> = rest.splitn(3, delim).collect();
        if parts.len() < 2 {
            return None;
        }
        let pattern = parts[0];
        let replacement = parts[1].trim_end_matches(delim);
        let re = Regex::new(pattern).ok()?;
        Some((re, replacement.to_string()))
    }

    /// Apply the fix_pattern of the matching rule to the violating file.
    ///
    /// Returns `true` when the file was modified, `false` when no fix was applicable or needed.
    pub fn apply_fix(&self, violation: &Violation, project_root: &Path) -> anyhow::Result<bool> {
        let rule = self.rules.iter().find(|r| r.id == violation.rule_id);
        let Some(rule) = rule else {
            return Ok(false);
        };
        let Some(fix_pattern) = &rule.fix_pattern else {
            return Ok(false);
        };

        let (re, replacement) = Self::parse_fix_pattern(fix_pattern)
            .ok_or_else(|| anyhow::anyhow!("invalid fix_pattern syntax: {}", fix_pattern))?;

        let file_path = if violation.file.is_absolute() {
            violation.file.clone()
        } else {
            project_root.join(&violation.file)
        };

        let content = std::fs::read_to_string(&file_path)
            .with_context(|| format!("failed to read {}", file_path.display()))?;
        let new_content = re.replace_all(&content, replacement.as_str()).to_string();
        if new_content == content {
            return Ok(false);
        }
        std::fs::write(&file_path, &new_content)
            .with_context(|| format!("failed to write {}", file_path.display()))?;
        Ok(true)
    }

    /// Apply fixes for all violations that have a matching rule with a fix_pattern.
    ///
    /// Returns the number of files that were modified.
    pub fn apply_fixes(
        &self,
        violations: &[Violation],
        project_root: &Path,
    ) -> anyhow::Result<usize> {
        let mut fixed = 0usize;
        for violation in violations {
            match self.apply_fix(violation, project_root) {
                Ok(true) => fixed += 1,
                Ok(false) => {}
                Err(e) => tracing::warn!("auto-fix failed for {}: {e}", violation.rule_id),
            }
        }
        Ok(fixed)
    }

    /// Scan for violations and, when `auto_fix` is true, attempt to apply each violation's
    /// `fix_pattern`. Re-scans after all fixes to verify which violations were resolved.
    ///
    /// Returns an [`AutoFixReport`] containing per-attempt outcomes and the residual
    /// violations that remain after the fix loop. When `auto_fix` is false the report
    /// is empty and `residual_violations` holds the initial scan results.
    pub async fn scan_and_fix(
        &self,
        project_root: &Path,
        auto_fix: bool,
    ) -> anyhow::Result<AutoFixReport> {
        let initial_violations = self.scan(project_root).await?;

        if !auto_fix {
            return Ok(AutoFixReport {
                attempts: vec![],
                fixed_count: 0,
                residual_violations: initial_violations,
            });
        }

        let fixable: Vec<&Violation> = initial_violations
            .iter()
            .filter(|v| {
                self.rules
                    .iter()
                    .any(|r| r.id == v.rule_id && r.fix_pattern.is_some())
            })
            .collect();

        if fixable.is_empty() {
            tracing::debug!(
                violations = initial_violations.len(),
                "auto-fix enabled but no violations have a fix_pattern"
            );
            return Ok(AutoFixReport {
                attempts: vec![],
                fixed_count: 0,
                residual_violations: initial_violations,
            });
        }

        let mut attempts: Vec<AutoFixAttempt> = Vec::new();
        let mut fixed_count = 0usize;

        for violation in &fixable {
            let applied = match self.apply_fix(violation, project_root) {
                Ok(true) => {
                    fixed_count += 1;
                    tracing::info!(
                        rule = %violation.rule_id,
                        file = %violation.file.display(),
                        "auto-fix applied"
                    );
                    true
                }
                Ok(false) => {
                    tracing::debug!(
                        rule = %violation.rule_id,
                        file = %violation.file.display(),
                        "auto-fix pattern did not match"
                    );
                    false
                }
                Err(e) => {
                    tracing::warn!(
                        rule = %violation.rule_id,
                        file = %violation.file.display(),
                        "auto-fix failed: {e}"
                    );
                    false
                }
            };
            attempts.push(AutoFixAttempt {
                rule_id: violation.rule_id.clone(),
                file: violation.file.clone(),
                line: violation.line,
                applied,
                resolved: false, // updated after re-scan below
            });
        }

        // Re-scan to verify which violations were actually resolved.
        let residual_violations = self.scan(project_root).await?;

        let residual_keys: HashSet<(&RuleId, &PathBuf)> = residual_violations
            .iter()
            .map(|v| (&v.rule_id, &v.file))
            .collect();

        for attempt in &mut attempts {
            if attempt.applied {
                attempt.resolved = !residual_keys.contains(&(&attempt.rule_id, &attempt.file));
            }
        }

        tracing::info!(
            applied = fixed_count,
            residual = residual_violations.len(),
            project_root = %project_root.display(),
            "auto-fix loop complete"
        );

        Ok(AutoFixReport {
            attempts,
            fixed_count,
            residual_violations,
        })
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

    /// Register guard scripts found in `guards_dir` (all `*.sh` files).
    ///
    /// The guard ID is derived from the filename stem (uppercased, hyphens preserved).
    /// Already-registered guards are skipped. Returns the count of newly registered guards.
    pub fn auto_register_project_guards(&mut self, guards_dir: &Path) -> anyhow::Result<usize> {
        if !guards_dir.is_dir() {
            return Ok(0);
        }

        let mut registered = 0;
        for entry in std::fs::read_dir(guards_dir)
            .with_context(|| format!("failed to read guards directory: {}", guards_dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if !path.extension().map(|e| e == "sh").unwrap_or(false) {
                continue;
            }

            let stem = match path.file_stem().and_then(|s| s.to_str()) {
                Some(s) => s.to_uppercase(),
                None => continue,
            };
            let guard_id = GuardId::from_str(&stem);

            if self.guards.iter().any(|g| g.id == guard_id) {
                continue;
            }

            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(meta) = std::fs::metadata(&path) {
                    let mut perms = meta.permissions();
                    perms.set_mode(0o755);
                    if let Err(e) = std::fs::set_permissions(&path, perms) {
                        tracing::warn!(
                            path = %path.display(),
                            "failed to set executable bit on guard script: {e}"
                        );
                    }
                }
            }

            self.register_guard(Guard {
                id: guard_id,
                script_path: path,
                language: Language::Common,
                rules: Vec::new(),
            });
            registered += 1;
        }
        Ok(registered)
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

    /// Filter out violations suppressed by an inline comment on the preceding line.
    ///
    /// A violation at line `N` is suppressed when line `N-1` contains:
    /// ```text
    /// // vibeguard-disable-next-line <RULE_ID> -- <reason>
    /// ```
    /// The `-- <reason>` part is optional. The check is case-sensitive for the
    /// rule ID and whitespace-insensitive around the directive keyword.
    pub fn filter_suppressed(violations: Vec<Violation>, project_root: &Path) -> Vec<Violation> {
        violations
            .into_iter()
            .filter(|v| {
                let Some(line) = v.line else { return true };
                if line == 0 {
                    return true;
                }
                let file_path = if v.file.is_absolute() {
                    v.file.clone()
                } else {
                    project_root.join(&v.file)
                };
                let Ok(content) = std::fs::read_to_string(&file_path) else {
                    return true;
                };
                let lines: Vec<&str> = content.lines().collect();
                // line is 1-indexed; preceding line is at index line-2
                let prev_idx = match line.checked_sub(2) {
                    Some(i) => i,
                    None => return true,
                };
                let Some(prev) = lines.get(prev_idx) else {
                    return true;
                };
                let needle = format!("vibeguard-disable-next-line {}", v.rule_id);
                // Exact rule-id match: the needle must be followed by whitespace
                // or end-of-line so that e.g. "RS-02" does not suppress "RS-02B".
                let suppressed = prev.find(&needle as &str).map_or(false, |pos| {
                    let after = &prev[pos + needle.len()..];
                    after.is_empty() || after.starts_with(char::is_whitespace)
                });
                !suppressed
            })
            .collect()
    }
}

impl Default for RuleEngine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;
