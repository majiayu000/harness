use async_trait::async_trait;
use dashmap::DashMap;
use harness_core::agent::AgentRequest;
use harness_core::interceptor::{InterceptResult, TurnInterceptor};
use harness_core::types::Severity;
use harness_rules::engine::RuleEngine;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;

/// Pre-turn interceptor that scans projects with the `RuleEngine` and injects
/// violation summaries into the agent's prompt context.
///
/// ## Design (2026 consensus — advisory, not blocking)
///
/// All violations are **advisory warnings** injected into the agent's context,
/// never hard blocks.  The agent sees the violations and self-corrects during
/// its implementation loop.  This follows the industry pattern established by
/// OpenAI Codex CLI (fail-open hooks), Anthropic Claude Code (PostToolUse
/// feedback), and the broader "quality = post-execution validation" consensus.
///
/// **Rationale**: Pre-execution blocking on code quality rules causes:
/// - False positives blocking entire tasks (SEC-02 on test constants)
/// - Agent deadlocks (can't proceed, can't fix the pre-existing issue)
/// - Wasted retry budget on interceptor-blocked tasks
///
/// Hard blocking is reserved for the OS-level sandbox and command-level policy
/// (outside this interceptor), not for code quality scanning.
pub struct RuleEnforcer {
    rules: Arc<RwLock<RuleEngine>>,
    /// Per-project violation count from the previous scan, used to compute deltas.
    /// Keys are canonical project identifiers (git remote URL for workspace clones,
    /// raw path otherwise) so delta tracking works across ephemeral per-task
    /// workspace directories.
    prev_counts: DashMap<PathBuf, usize>,
}

impl RuleEnforcer {
    pub fn new(rules: Arc<RwLock<RuleEngine>>) -> Self {
        Self {
            rules,
            prev_counts: DashMap::new(),
        }
    }
}

/// Derive a stable project key from a potentially ephemeral workspace path.
///
/// Harness workspace directories (`<data>/workspaces/<task-uuid>`) are per-task
/// clones.  Using them directly in `prev_counts` means every task starts with
/// zero baseline, so `violations_new` always equals `violations_total`.
///
/// Reads the git remote origin URL from `.git/config` (handling both regular
/// clones and worktrees) to produce a key shared across tasks targeting the
/// same repository.
fn canonical_project_key(project_root: &Path) -> PathBuf {
    if !project_root
        .components()
        .any(|c| c.as_os_str() == "workspaces")
    {
        return project_root.to_path_buf();
    }
    resolve_git_remote(project_root).unwrap_or_else(|| project_root.to_path_buf())
}

/// Read the `[remote "origin"]` URL from git config, handling both regular
/// repos (`.git/` directory) and worktrees (`.git` file with `gitdir:` pointer).
fn resolve_git_remote(project_root: &Path) -> Option<PathBuf> {
    let dot_git = project_root.join(".git");
    let config_path = if dot_git.is_dir() {
        dot_git.join("config")
    } else if dot_git.is_file() {
        let content = std::fs::read_to_string(&dot_git).ok()?;
        let gitdir = content.trim().strip_prefix("gitdir: ")?;
        let abs = if Path::new(gitdir).is_absolute() {
            PathBuf::from(gitdir)
        } else {
            project_root.join(gitdir)
        };
        abs.join("../../config").canonicalize().ok()?
    } else {
        return None;
    };
    let content = std::fs::read_to_string(config_path).ok()?;
    let mut in_origin = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed == "[remote \"origin\"]" {
            in_origin = true;
        } else if trimmed.starts_with('[') {
            in_origin = false;
        } else if in_origin {
            if let Some(url) = trimmed.strip_prefix("url = ") {
                return Some(PathBuf::from(url.trim()));
            }
        }
    }
    None
}

#[async_trait]
impl TurnInterceptor for RuleEnforcer {
    fn name(&self) -> &str {
        "rule_enforcer"
    }

    async fn pre_execute(&self, req: &AgentRequest) -> InterceptResult {
        let engine = self.rules.read().await;

        if engine.guards().is_empty() {
            tracing::debug!(
                project_root = %req.project_root.display(),
                "rule_enforcer: no guards registered, skipping"
            );
            return InterceptResult::pass();
        }

        // Fail-open: guard scan errors produce a warning, never block the agent.
        // A broken guard script should not prevent task execution.
        let violations = match engine.scan(&req.project_root).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    project_root = %req.project_root.display(),
                    "rule_enforcer: scan failed (fail-open, agent proceeds)"
                );
                return InterceptResult::pass();
            }
        };

        if violations.is_empty() {
            return InterceptResult::pass();
        }

        let total = violations.len();
        let canonical_key = canonical_project_key(&req.project_root);
        let prev = self
            .prev_counts
            .get(&canonical_key)
            .map(|v| *v)
            .unwrap_or(0);
        let new_violations = total.saturating_sub(prev);
        let fixed_violations = prev.saturating_sub(total);
        self.prev_counts.insert(canonical_key, total);

        let delta_zero = new_violations == 0 && fixed_violations == 0 && prev > 0;
        if delta_zero {
            tracing::debug!(
                violation_count = total,
                violations_total = total,
                violations_new = new_violations,
                violations_fixed = fixed_violations,
                project_root = %req.project_root.display(),
                "rule_enforcer: scan completed"
            );
        } else {
            tracing::info!(
                violation_count = total,
                violations_total = total,
                violations_new = new_violations,
                violations_fixed = fixed_violations,
                project_root = %req.project_root.display(),
                "rule_enforcer: scan completed"
            );
        }

        // All violations are advisory — injected into agent context as warnings.
        // Critical and High get a detailed summary; Medium and below are logged only.
        let actionable: Vec<_> = violations
            .iter()
            .filter(|v| {
                matches!(v.severity, Severity::Critical | Severity::High)
            })
            .collect();

        if !actionable.is_empty() {
            let summary = actionable
                .iter()
                .map(|v| {
                    let loc = v
                        .line
                        .map(|l| format!("{}:{l}", v.file.display()))
                        .unwrap_or_else(|| v.file.display().to_string());
                    format!("[{}] {} ({})", v.rule_id, v.message, loc)
                })
                .collect::<Vec<_>>()
                .join("\n");
            return InterceptResult::warn(format!(
                "rule_enforcer: {} violation(s) detected (advisory — fix if applicable):\n{summary}",
                actionable.len()
            ));
        }

        InterceptResult::pass()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::{agent::AgentRequest, types::Decision, types::GuardId, types::Language};
    use harness_rules::engine::{Guard, RuleEngine};

    fn make_req(project_root: &std::path::Path) -> AgentRequest {
        AgentRequest {
            prompt: "fix the bug. Ensure tests pass.".to_string(),
            project_root: project_root.to_path_buf(),
            ..Default::default()
        }
    }

    fn engine_with_guard(
        guard_dir: &std::path::Path,
        rule_id: &str,
        severity_keyword: &str,
    ) -> anyhow::Result<Arc<RwLock<RuleEngine>>> {
        let script = guard_dir.join("test-guard.sh");
        std::fs::write(
            &script,
            format!(
                "#!/usr/bin/env bash\necho \"src/lib.rs:1:{rule_id}:test violation ({severity_keyword})\"\n"
            ),
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script)?.permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script, perms)?;
        }

        let mut engine = RuleEngine::new();
        engine.register_guard(Guard {
            id: GuardId::from_str("TEST-GUARD"),
            script_path: script,
            language: Language::Common,
            rules: vec![],
        });
        Ok(Arc::new(RwLock::new(engine)))
    }

    #[tokio::test]
    async fn warns_on_critical_violation() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project)?;

        let rules = engine_with_guard(dir.path(), "SEC-01", "critical")?;
        {
            let mut engine = rules.write().await;
            engine.add_rule(harness_rules::engine::Rule {
                id: harness_core::types::RuleId::from_str("SEC-01"),
                title: "SQL injection".to_string(),
                severity: Severity::Critical,
                category: harness_core::types::Category::Security,
                paths: vec![],
                description: String::new(),
                fix_pattern: None,
            });
        }

        let enforcer = RuleEnforcer::new(rules);
        let result = enforcer.pre_execute(&make_req(&project)).await;

        assert_eq!(
            result.decision,
            Decision::Warn,
            "critical violation should warn (advisory), not block"
        );
        assert!(
            result.reason.as_deref().unwrap_or("").contains("advisory"),
            "warn reason should mention advisory: {:?}",
            result.reason
        );
        Ok(())
    }

    #[tokio::test]
    async fn warns_on_high_violation() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project)?;

        let rules = engine_with_guard(dir.path(), "SEC-04", "high")?;
        {
            let mut engine = rules.write().await;
            engine.add_rule(harness_rules::engine::Rule {
                id: harness_core::types::RuleId::from_str("SEC-04"),
                title: "Unprotected API".to_string(),
                severity: Severity::High,
                category: harness_core::types::Category::Security,
                paths: vec![],
                description: String::new(),
                fix_pattern: None,
            });
        }

        let enforcer = RuleEnforcer::new(rules);
        let result = enforcer.pre_execute(&make_req(&project)).await;

        assert_eq!(
            result.decision,
            Decision::Warn,
            "high violation must warn"
        );
        Ok(())
    }

    #[tokio::test]
    async fn passes_when_no_guards_registered() {
        let dir = tempfile::tempdir().unwrap();
        let engine = Arc::new(RwLock::new(RuleEngine::new()));
        let enforcer = RuleEnforcer::new(engine);
        let result = enforcer.pre_execute(&make_req(dir.path())).await;
        assert_eq!(result.decision, Decision::Pass);
    }

    #[tokio::test]
    async fn passes_when_scan_fails_fail_open() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project)?;

        let mut engine = RuleEngine::new();
        engine.register_guard(Guard {
            id: GuardId::from_str("BROKEN-GUARD"),
            script_path: std::path::PathBuf::from("broken\0guard.sh"),
            language: Language::Common,
            rules: vec![],
        });
        let enforcer = RuleEnforcer::new(Arc::new(RwLock::new(engine)));

        let result = enforcer.pre_execute(&make_req(&project)).await;
        assert_eq!(
            result.decision,
            Decision::Pass,
            "scan failure must fail-open (pass), not block"
        );
        Ok(())
    }

    #[tokio::test]
    async fn passes_when_guard_emits_no_violations() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project)?;

        let script = dir.path().join("noop.sh");
        std::fs::write(&script, "#!/usr/bin/env bash\nexit 0\n")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script)?.permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script, perms)?;
        }

        let mut engine = RuleEngine::new();
        engine.register_guard(Guard {
            id: GuardId::from_str("NOOP-GUARD"),
            script_path: script,
            language: Language::Common,
            rules: vec![],
        });
        let rules = Arc::new(RwLock::new(engine));

        let enforcer = RuleEnforcer::new(rules);
        let result = enforcer.pre_execute(&make_req(&project)).await;
        assert_eq!(result.decision, Decision::Pass);
        Ok(())
    }

    #[test]
    fn interceptor_name_is_rule_enforcer() {
        let engine = Arc::new(RwLock::new(RuleEngine::new()));
        let enforcer = RuleEnforcer::new(engine);
        assert_eq!(enforcer.name(), "rule_enforcer");
    }

    #[tokio::test]
    async fn passes_on_medium_violation() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project)?;

        let rules = engine_with_guard(dir.path(), "U-16", "medium")?;
        {
            let mut engine = rules.write().await;
            engine.add_rule(harness_rules::engine::Rule {
                id: harness_core::types::RuleId::from_str("U-16"),
                title: "File size".to_string(),
                severity: Severity::Medium,
                category: harness_core::types::Category::Style,
                paths: vec![],
                description: String::new(),
                fix_pattern: None,
            });
        }

        let enforcer = RuleEnforcer::new(rules);
        let result = enforcer.pre_execute(&make_req(&project)).await;
        assert_eq!(
            result.decision,
            Decision::Pass,
            "medium violation must pass through"
        );
        Ok(())
    }
}
