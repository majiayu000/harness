use async_trait::async_trait;
use harness_core::{
    interceptor::{InterceptResult, TurnInterceptor},
    AgentRequest, Severity,
};
use harness_rules::engine::RuleEngine;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Pre-turn interceptor that evaluates loaded rules against the project before
/// each agent execution.
///
/// - **Critical** violations → `Block` (abort task)
/// - **High / Medium / Low** violations → `Warn` (allow execution, log warnings)
/// - No guards registered → `Pass` silently (guards bootstrap asynchronously)
/// - Scan error → `Pass` with a warning log (fail-open to avoid blocking all tasks)
pub struct PolicyEnforcer {
    rules: Arc<RwLock<RuleEngine>>,
}

impl PolicyEnforcer {
    pub fn new(rules: Arc<RwLock<RuleEngine>>) -> Self {
        Self { rules }
    }
}

#[async_trait]
impl TurnInterceptor for PolicyEnforcer {
    fn name(&self) -> &str {
        "policy_enforcer"
    }

    async fn pre_execute(&self, req: &AgentRequest) -> InterceptResult {
        let engine = self.rules.read().await;

        if engine.guards().is_empty() {
            tracing::debug!("policy_enforcer: no guards registered; skipping pre-turn scan");
            return InterceptResult::pass();
        }

        match engine.scan(&req.project_root).await {
            Ok(violations) if violations.is_empty() => InterceptResult::pass(),
            Ok(violations) => {
                let critical: Vec<_> = violations
                    .iter()
                    .filter(|v| v.severity == Severity::Critical)
                    .collect();

                if !critical.is_empty() {
                    let reason = critical
                        .iter()
                        .map(|v| {
                            let loc = v
                                .line
                                .map(|l| format!("{}:{}", v.file.display(), l))
                                .unwrap_or_else(|| v.file.display().to_string());
                            format!("[{}] {} ({})", v.rule_id.as_str(), v.message, loc)
                        })
                        .collect::<Vec<_>>()
                        .join("; ");
                    tracing::warn!(
                        violation_count = critical.len(),
                        project_root = %req.project_root.display(),
                        "policy_enforcer: blocking task — critical violations found"
                    );
                    return InterceptResult::block(format!("Policy violation(s): {reason}"));
                }

                let reason = violations
                    .iter()
                    .map(|v| format!("[{}] {}", v.rule_id.as_str(), v.message))
                    .collect::<Vec<_>>()
                    .join("; ");
                tracing::warn!(
                    violation_count = violations.len(),
                    project_root = %req.project_root.display(),
                    "policy_enforcer: non-critical violations found; allowing execution"
                );
                InterceptResult::warn(format!("Policy warnings: {reason}"))
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    project_root = %req.project_root.display(),
                    "policy_enforcer: rule scan failed; allowing execution"
                );
                InterceptResult::pass()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::{Decision, GuardId, Language};
    use harness_rules::engine::{Guard, RuleEngine};
    use std::path::PathBuf;

    fn make_req(project_root: PathBuf) -> AgentRequest {
        AgentRequest {
            prompt: "fix the bug. Ensure tests pass.".to_string(),
            project_root,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn passes_when_no_guards_registered() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let engine = Arc::new(RwLock::new(RuleEngine::new()));
        let enforcer = PolicyEnforcer::new(engine);
        let result = enforcer
            .pre_execute(&make_req(dir.path().to_path_buf()))
            .await;
        assert_eq!(result.decision, Decision::Pass);
        Ok(())
    }

    #[tokio::test]
    async fn blocks_on_critical_violation() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;

        // Write a guard script that emits a critical-severity violation.
        let guard_dir = dir.path().join("guards");
        std::fs::create_dir_all(&guard_dir)?;
        let script = guard_dir.join("critical-guard.sh");
        std::fs::write(
            &script,
            "#!/usr/bin/env bash\necho 'src/main.rs:1:SEC-01:hardcoded secret detected'\n",
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script)?.permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script, perms)?;
        }

        let mut engine = RuleEngine::new();
        // Load builtin rules so SEC-01 (Critical) is known to the engine.
        engine.load_builtin()?;
        engine.register_guard(Guard {
            id: GuardId::from_str("CRITICAL-TEST-GUARD"),
            script_path: script,
            language: Language::Common,
            rules: vec![],
        });

        let engine = Arc::new(RwLock::new(engine));
        let enforcer = PolicyEnforcer::new(engine);
        let result = enforcer
            .pre_execute(&make_req(dir.path().to_path_buf()))
            .await;

        assert_eq!(
            result.decision,
            Decision::Block,
            "critical violation must block execution"
        );
        assert!(
            result.reason.as_deref().unwrap_or("").contains("SEC-01"),
            "block reason must mention the rule id"
        );
        Ok(())
    }

    #[tokio::test]
    async fn warns_on_non_critical_violation() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;

        let guard_dir = dir.path().join("guards");
        std::fs::create_dir_all(&guard_dir)?;
        let script = guard_dir.join("warn-guard.sh");
        // U-01 is a Style/High rule — not Critical — so should produce Warn, not Block.
        std::fs::write(
            &script,
            "#!/usr/bin/env bash\necho 'src/lib.rs:10:U-01:public API changed without approval'\n",
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script)?.permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script, perms)?;
        }

        let mut engine = RuleEngine::new();
        engine.load_builtin()?;
        engine.register_guard(Guard {
            id: GuardId::from_str("WARN-TEST-GUARD"),
            script_path: script,
            language: Language::Common,
            rules: vec![],
        });

        let engine = Arc::new(RwLock::new(engine));
        let enforcer = PolicyEnforcer::new(engine);
        let result = enforcer
            .pre_execute(&make_req(dir.path().to_path_buf()))
            .await;

        assert_eq!(
            result.decision,
            Decision::Warn,
            "non-critical violation must warn, not block"
        );
        Ok(())
    }

    #[tokio::test]
    async fn passes_when_guard_emits_no_violations() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;

        let guard_dir = dir.path().join("guards");
        std::fs::create_dir_all(&guard_dir)?;
        let script = guard_dir.join("clean-guard.sh");
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
            id: GuardId::from_str("CLEAN-TEST-GUARD"),
            script_path: script,
            language: Language::Common,
            rules: vec![],
        });

        let engine = Arc::new(RwLock::new(engine));
        let enforcer = PolicyEnforcer::new(engine);
        let result = enforcer
            .pre_execute(&make_req(dir.path().to_path_buf()))
            .await;

        assert_eq!(result.decision, Decision::Pass);
        Ok(())
    }

    #[tokio::test]
    async fn passes_on_scan_error() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;

        let mut engine = RuleEngine::new();
        // Register a guard pointing to a nonexistent script — scan will error.
        engine.register_guard(Guard {
            id: GuardId::from_str("MISSING-GUARD"),
            script_path: PathBuf::from("/nonexistent/guard.sh"),
            language: Language::Common,
            rules: vec![],
        });

        let engine = Arc::new(RwLock::new(engine));
        let enforcer = PolicyEnforcer::new(engine);
        let result = enforcer
            .pre_execute(&make_req(dir.path().to_path_buf()))
            .await;

        // Fail-open: scan errors must not block execution.
        assert_eq!(result.decision, Decision::Pass);
        Ok(())
    }

    #[test]
    fn interceptor_name_is_policy_enforcer() {
        let engine = Arc::new(RwLock::new(RuleEngine::new()));
        let enforcer = PolicyEnforcer::new(engine);
        assert_eq!(enforcer.name(), "policy_enforcer");
    }
}
