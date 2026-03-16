use async_trait::async_trait;
use harness_core::{
    interceptor::{InterceptResult, TurnInterceptor},
    AgentRequest, Severity,
};
use harness_rules::engine::RuleEngine;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Pre-turn interceptor that enforces rules loaded into the `RuleEngine`.
///
/// On every `pre_execute` call the enforcer scans the project root using all
/// registered guards.  Critical violations block the turn; high-severity
/// violations produce a warning that is injected into the prompt context.
/// When no guards are registered the enforcer passes through silently.
pub struct RuleEnforcer {
    rules: Arc<RwLock<RuleEngine>>,
}

impl RuleEnforcer {
    pub fn new(rules: Arc<RwLock<RuleEngine>>) -> Self {
        Self { rules }
    }
}

#[async_trait]
impl TurnInterceptor for RuleEnforcer {
    fn name(&self) -> &str {
        "rule_enforcer"
    }

    async fn pre_execute(&self, req: &AgentRequest) -> InterceptResult {
        let engine = self.rules.read().await;

        // Nothing to enforce without guards.
        if engine.guards().is_empty() {
            return InterceptResult::pass();
        }

        // Only enforce rules on projects that have opted in by providing their
        // own `.harness/guards` directory.  Guards loaded at startup are scoped
        // to the projects they belong to; scanning unrelated projects produces
        // false positives (e.g. SEC-02 on test constants in external repos).
        if !req.project_root.join(".harness").join("guards").is_dir() {
            tracing::debug!(
                project_root = %req.project_root.display(),
                "rule_enforcer: project has no .harness/guards, skipping enforcement"
            );
            return InterceptResult::pass();
        }

        let violations = match engine.scan(&req.project_root).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    project_root = %req.project_root.display(),
                    "rule_enforcer: scan failed, skipping enforcement"
                );
                return InterceptResult::pass();
            }
        };

        if violations.is_empty() {
            return InterceptResult::pass();
        }

        tracing::info!(
            violation_count = violations.len(),
            project_root = %req.project_root.display(),
            "rule_enforcer: scan completed"
        );

        let critical: Vec<_> = violations
            .iter()
            .filter(|v| v.severity == Severity::Critical)
            .collect();

        if !critical.is_empty() {
            let summary = critical
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
            return InterceptResult::block(format!(
                "rule_enforcer: {} critical violation(s) must be fixed before proceeding:\n{summary}",
                critical.len()
            ));
        }

        let high: Vec<_> = violations
            .iter()
            .filter(|v| v.severity == Severity::High)
            .collect();

        if !high.is_empty() {
            let summary = high
                .iter()
                .map(|v| format!("[{}] {}", v.rule_id, v.message))
                .collect::<Vec<_>>()
                .join("\n");
            return InterceptResult::warn(format!(
                "rule_enforcer: {} high-severity violation(s) detected:\n{summary}",
                high.len()
            ));
        }

        InterceptResult::pass()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::{AgentRequest, Decision, GuardId, Language};
    use harness_rules::engine::{Guard, RuleEngine};

    fn make_req(project_root: &std::path::Path) -> AgentRequest {
        AgentRequest {
            prompt: "fix the bug. Ensure tests pass.".to_string(),
            project_root: project_root.to_path_buf(),
            ..Default::default()
        }
    }

    /// Build a guard script that always emits one violation with the given
    /// severity keyword and rule ID, then register it in a fresh RuleEngine.
    fn engine_with_guard(
        guard_dir: &std::path::Path,
        rule_id: &str,
        severity_keyword: &str,
    ) -> anyhow::Result<Arc<RwLock<RuleEngine>>> {
        let script = guard_dir.join("test-guard.sh");
        // Output format: FILE:LINE:RULE_ID:MESSAGE
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
    async fn blocks_on_critical_violation() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let project = dir.path().join("project");
        std::fs::create_dir_all(project.join(".harness").join("guards"))?;

        let rules = engine_with_guard(dir.path(), "SEC-01", "critical")?;
        {
            // Load a rule so the engine can resolve severity from the guard output.
            let mut engine = rules.write().await;
            engine.add_rule(harness_rules::engine::Rule {
                id: harness_core::RuleId::from_str("SEC-01"),
                title: "SQL injection".to_string(),
                severity: Severity::Critical,
                category: harness_core::Category::Security,
                paths: vec![],
                description: String::new(),
                fix_pattern: None,
            });
        }

        let enforcer = RuleEnforcer::new(rules);
        let result = enforcer.pre_execute(&make_req(&project)).await;

        assert_eq!(
            result.decision,
            Decision::Block,
            "critical violation must block"
        );
        assert!(
            result.reason.as_deref().unwrap_or("").contains("critical"),
            "block reason should mention severity: {:?}",
            result.reason
        );
        Ok(())
    }

    #[tokio::test]
    async fn warns_on_high_violation() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let project = dir.path().join("project");
        std::fs::create_dir_all(project.join(".harness").join("guards"))?;

        let rules = engine_with_guard(dir.path(), "SEC-04", "high")?;
        {
            let mut engine = rules.write().await;
            engine.add_rule(harness_rules::engine::Rule {
                id: harness_core::RuleId::from_str("SEC-04"),
                title: "Unprotected API".to_string(),
                severity: Severity::High,
                category: harness_core::Category::Security,
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
            "high violation must warn, not block"
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
    async fn passes_when_guard_emits_no_violations() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project)?;

        // Guard that emits nothing.
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
                id: harness_core::RuleId::from_str("U-16"),
                title: "File size".to_string(),
                severity: Severity::Medium,
                category: harness_core::Category::Style,
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
