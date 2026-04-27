use async_trait::async_trait;
use harness_core::agent::AgentRequest;
use harness_core::interceptor::{
    InterceptResult, PostToolUseResult, ToolUseEvent, TurnInterceptor,
};
use harness_core::types::{Decision, Event, SessionId};
use harness_observe::event_store::EventStore;
use harness_rules::engine::RuleEngine;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::circuit_breaker::CircuitBreaker;

/// Post-tool-use hook enforcer.
///
/// When `enabled`, this interceptor fires after each agent turn, scans known
/// modified files with all registered guards, logs a `hook_enforcement` event
/// to the [`EventStore`], and returns any violations as feedback for the
/// agent's next turn prompt.
///
/// ## Infinite-loop protection (circuit breaker)
///
/// The enforcer embeds a [`CircuitBreaker`] that tracks consecutive block
/// counts per session. After 3 consecutive blocks the circuit opens and all
/// subsequent calls auto-pass for 5 minutes, preventing the analysis-paralysis
/// loop described in GitHub issues #3573 and #10205.
///
/// This mirrors the role of Claude Code's `stop_hook_active` input field: once
/// the circuit is open the enforcer stops emitting blocking signals, breaking
/// the loop.
///
/// ## CI guard
///
/// When the `CI` environment variable is set the enforcer skips all
/// enforcement unconditionally. Desktop-specific hooks must not run in CI
/// environments (GitHub issue #3573).
///
/// The enforcer silently passes through when:
/// - `enabled` is `false` (controlled by `rules.hook_enforcement` in config)
/// - `CI` environment variable is set
/// - no guards are registered
/// - no files were modified during the turn
/// - the circuit breaker is open (consecutive-block limit reached)
pub struct HookEnforcer {
    rules: Arc<RwLock<RuleEngine>>,
    events: Arc<EventStore>,
    enabled: bool,
    breaker: CircuitBreaker,
}

impl HookEnforcer {
    pub fn new(rules: Arc<RwLock<RuleEngine>>, events: Arc<EventStore>, enabled: bool) -> Self {
        Self {
            rules,
            events,
            enabled,
            breaker: CircuitBreaker::new(),
        }
    }

    /// Detect files added or modified in `project_root`.
    ///
    /// Host-side git inspection is disabled by project policy, so this returns
    /// an empty list until file changes are supplied by agent telemetry.
    pub async fn detect_modified_files(project_root: &Path) -> Vec<PathBuf> {
        tracing::debug!(
            project_root = %project_root.display(),
            "hook_enforcer: host-side git inspection disabled"
        );
        Vec::new()
    }

    /// Returns the circuit-breaker key for a session.
    ///
    /// Uses the session ID when available so each agent session has its own
    /// failure counter. Falls back to a shared key when the session is unknown.
    fn breaker_key(session_id: Option<&SessionId>) -> String {
        match session_id {
            Some(sid) => format!("session:{sid}"),
            None => "global".to_string(),
        }
    }
}

#[async_trait]
impl TurnInterceptor for HookEnforcer {
    fn name(&self) -> &str {
        "hook_enforcer"
    }

    async fn pre_execute(&self, _req: &AgentRequest) -> InterceptResult {
        InterceptResult::pass()
    }

    /// Scan `event.affected_files` with all registered guards, log a
    /// `hook_enforcement` event to the [`EventStore`], and return violation
    /// feedback when violations are found.
    ///
    /// Auto-passes (returns clean) when the CI environment variable is set or
    /// when the per-session circuit breaker has opened due to repeated blocks.
    async fn post_tool_use(&self, event: &ToolUseEvent, project_root: &Path) -> PostToolUseResult {
        if !self.enabled {
            return PostToolUseResult::clean();
        }

        // CI guard: skip all enforcement in CI environments (#3573).
        if std::env::var("CI").is_ok() {
            return PostToolUseResult::clean();
        }

        if event.affected_files.is_empty() {
            return PostToolUseResult::clean();
        }

        let engine = self.rules.read().await;
        if engine.guards().is_empty() {
            return PostToolUseResult::clean();
        }

        // Circuit breaker: auto-pass when the consecutive-block limit has been
        // reached (stop_hook_active equivalent — prevents infinite loops).
        let key = Self::breaker_key(event.session_id.as_ref());
        if !self.breaker.allow(&key) {
            tracing::warn!(
                session = %key,
                "hook_enforcer: circuit open, auto-passing to break enforcement loop"
            );
            return PostToolUseResult::clean();
        }

        let violations = match engine.scan_files(project_root, &event.affected_files).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    tool = %event.tool_name,
                    "hook_enforcer: scan_files failed"
                );
                return PostToolUseResult::clean();
            }
        };

        let decision = if violations.is_empty() {
            Decision::Pass
        } else {
            Decision::Warn
        };

        let sid = if let Some(id) = event.session_id.clone() {
            id
        } else {
            SessionId::new()
        };
        let mut ev = Event::new(sid, "hook_enforcement", "post_tool_use", decision);
        ev.detail = Some(format!(
            "tool={} files={} violations={}",
            event.tool_name,
            event.affected_files.len(),
            violations.len()
        ));
        if let Err(e) = self.events.log(&ev).await {
            tracing::warn!(error = %e, "hook_enforcer: failed to log event");
        }

        if violations.is_empty() {
            self.breaker.record_pass(&key);
            return PostToolUseResult::clean();
        }

        self.breaker.record_block(&key);

        let feedback = violations
            .iter()
            .map(|v| format!("[{}] {} ({})", v.rule_id, v.message, v.file.display()))
            .collect::<Vec<_>>()
            .join("\n");

        PostToolUseResult::with_violations(format!(
            "Rule violations detected in modified files:\n{feedback}"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::{types::EventFilters, types::GuardId, types::Language};
    use harness_rules::engine::{Guard, RuleEngine};
    use std::ffi::OsString;
    use tempfile::tempdir;
    use tokio::sync::Mutex;

    static CI_ENV_LOCK: Mutex<()> = Mutex::const_new(());

    struct CiEnvGuard {
        previous: Option<OsString>,
    }

    impl Drop for CiEnvGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => {
                    // SAFETY: tests run in a single process; caller serializes with CI_ENV_LOCK.
                    unsafe { std::env::set_var("CI", value) };
                }
                None => {
                    // SAFETY: tests run in a single process; caller serializes with CI_ENV_LOCK.
                    unsafe { std::env::remove_var("CI") };
                }
            }
        }
    }

    fn with_ci_env(value: Option<&str>) -> CiEnvGuard {
        let previous = std::env::var_os("CI");
        match value {
            Some(v) => {
                // SAFETY: tests run in a single process; caller serializes with CI_ENV_LOCK.
                unsafe { std::env::set_var("CI", v) };
            }
            None => {
                // SAFETY: tests run in a single process; caller serializes with CI_ENV_LOCK.
                unsafe { std::env::remove_var("CI") };
            }
        }
        CiEnvGuard { previous }
    }

    async fn make_event_store(dir: &Path) -> Arc<EventStore> {
        Arc::new(EventStore::new(dir).await.unwrap())
    }

    fn make_engine_with_guard(
        guard_dir: &Path,
        rule_id: &str,
        severity_keyword: &str,
    ) -> Arc<RwLock<RuleEngine>> {
        let script = guard_dir.join("hook-test-guard.sh");
        std::fs::write(
            &script,
            format!(
                "#!/usr/bin/env bash\necho \"src/main.rs:1:{rule_id}:hook violation ({severity_keyword})\"\n"
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script, perms).unwrap();
        }
        let mut engine = RuleEngine::new();
        engine.register_guard(Guard {
            id: GuardId::from_str("HOOK-TEST-GUARD"),
            script_path: script,
            language: Language::Common,
            rules: vec![],
        });
        Arc::new(RwLock::new(engine))
    }

    #[tokio::test]
    async fn post_tool_use_returns_violations_when_guard_fires() -> anyhow::Result<()> {
        let _env_lock = CI_ENV_LOCK.lock().await;
        let _ci_guard = with_ci_env(None);
        let _db_guard = crate::test_helpers::acquire_db_state_guard().await;
        let dir = tempdir()?;
        let events = make_event_store(dir.path()).await;
        let rules = make_engine_with_guard(dir.path(), "U-16", "medium");
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project)?;

        let enforcer = HookEnforcer::new(rules, events, true);
        let event = ToolUseEvent {
            tool_name: "write_file".to_string(),
            affected_files: vec![PathBuf::from("src/main.rs")],
            session_id: None,
        };
        let result = enforcer.post_tool_use(&event, &project).await;
        assert!(
            result.violation_feedback.is_some(),
            "expected violation feedback from guard"
        );
        let feedback = result.violation_feedback.as_deref().unwrap_or("");
        assert!(
            feedback.contains("Rule violations detected"),
            "feedback should mention violations: {feedback}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn post_tool_use_disabled_passes_through() -> anyhow::Result<()> {
        let _env_lock = CI_ENV_LOCK.lock().await;
        let _ci_guard = with_ci_env(None);
        let _db_guard = crate::test_helpers::acquire_db_state_guard().await;
        let dir = tempdir()?;
        let events = make_event_store(dir.path()).await;
        let rules = make_engine_with_guard(dir.path(), "U-16", "medium");
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project)?;

        let enforcer = HookEnforcer::new(rules, events, false);
        let event = ToolUseEvent {
            tool_name: "write_file".to_string(),
            affected_files: vec![PathBuf::from("src/main.rs")],
            session_id: None,
        };
        let result = enforcer.post_tool_use(&event, &project).await;
        assert!(
            result.violation_feedback.is_none(),
            "disabled hook must not return feedback"
        );
        Ok(())
    }

    #[tokio::test]
    async fn post_tool_use_empty_files_returns_clean() -> anyhow::Result<()> {
        let _env_lock = CI_ENV_LOCK.lock().await;
        let _ci_guard = with_ci_env(None);
        let _db_guard = crate::test_helpers::acquire_db_state_guard().await;
        let dir = tempdir()?;
        let events = make_event_store(dir.path()).await;
        let rules = make_engine_with_guard(dir.path(), "U-16", "medium");

        let enforcer = HookEnforcer::new(rules, events, true);
        let event = ToolUseEvent {
            tool_name: "write_file".to_string(),
            affected_files: vec![],
            session_id: None,
        };
        let result = enforcer.post_tool_use(&event, dir.path()).await;
        assert!(result.violation_feedback.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn post_tool_use_logs_event_to_store() -> anyhow::Result<()> {
        let _env_lock = CI_ENV_LOCK.lock().await;
        let _ci_guard = with_ci_env(None);
        let _db_guard = crate::test_helpers::acquire_db_state_guard().await;
        let dir = tempdir()?;
        let event_store = Arc::new(EventStore::new(dir.path()).await?);
        let rules = make_engine_with_guard(dir.path(), "U-16", "medium");
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project)?;

        let enforcer = HookEnforcer::new(rules, event_store.clone(), true);
        let event = ToolUseEvent {
            tool_name: "write_file".to_string(),
            affected_files: vec![PathBuf::from("src/main.rs")],
            session_id: None,
        };
        enforcer.post_tool_use(&event, &project).await;

        let filters = EventFilters {
            hook: Some("hook_enforcement".to_string()),
            ..Default::default()
        };
        let logged = event_store.query(&filters).await?;
        assert!(
            !logged.is_empty(),
            "hook_enforcement event must appear in EventStore"
        );
        assert_eq!(logged[0].tool, "post_tool_use");

        Ok(())
    }

    #[tokio::test]
    async fn post_tool_use_no_guards_passes_through() -> anyhow::Result<()> {
        let _env_lock = CI_ENV_LOCK.lock().await;
        let _ci_guard = with_ci_env(None);
        let _db_guard = crate::test_helpers::acquire_db_state_guard().await;
        let dir = tempdir()?;
        let events = make_event_store(dir.path()).await;
        let rules = Arc::new(RwLock::new(RuleEngine::new()));

        let enforcer = HookEnforcer::new(rules, events, true);
        let event = ToolUseEvent {
            tool_name: "write_file".to_string(),
            affected_files: vec![PathBuf::from("src/main.rs")],
            session_id: None,
        };
        let result = enforcer.post_tool_use(&event, dir.path()).await;
        assert!(
            result.violation_feedback.is_none(),
            "no guards means no feedback"
        );
        Ok(())
    }

    #[tokio::test]
    async fn interceptor_name_is_hook_enforcer() -> anyhow::Result<()> {
        let _db_guard = crate::test_helpers::acquire_db_state_guard().await;
        let dir = tempdir()?;
        let events = make_event_store(dir.path()).await;
        let rules = Arc::new(RwLock::new(RuleEngine::new()));
        let enforcer = HookEnforcer::new(rules, events, false);
        assert_eq!(enforcer.name(), "hook_enforcer");
        Ok(())
    }

    #[tokio::test]
    async fn circuit_breaker_opens_after_repeated_blocks() -> anyhow::Result<()> {
        let _env_lock = CI_ENV_LOCK.lock().await;
        let _ci_guard = with_ci_env(None);
        let _db_guard = crate::test_helpers::acquire_db_state_guard().await;
        let dir = tempdir()?;
        let events = make_event_store(dir.path()).await;
        let rules = make_engine_with_guard(dir.path(), "U-16", "medium");
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project)?;

        let sid = SessionId::new();
        let enforcer = HookEnforcer::new(rules, events, true);
        let event = ToolUseEvent {
            tool_name: "write_file".to_string(),
            affected_files: vec![PathBuf::from("src/main.rs")],
            session_id: Some(sid.clone()),
        };

        // First two blocks: violations returned, circuit still closed.
        for _ in 0..2 {
            let result = enforcer.post_tool_use(&event, &project).await;
            assert!(
                result.violation_feedback.is_some(),
                "should return violations before circuit opens"
            );
        }

        // Third block: circuit opens.
        let _ = enforcer.post_tool_use(&event, &project).await;

        // Fourth call: circuit is open — auto-pass (no violation feedback).
        let result = enforcer.post_tool_use(&event, &project).await;
        assert!(
            result.violation_feedback.is_none(),
            "open circuit must auto-pass to break the enforcement loop"
        );
        Ok(())
    }

    #[tokio::test]
    async fn ci_env_skips_enforcement() -> anyhow::Result<()> {
        let _env_lock = CI_ENV_LOCK.lock().await;
        let _ci_guard = with_ci_env(Some("true"));
        let _db_guard = crate::test_helpers::acquire_db_state_guard().await;
        let dir = tempdir()?;
        let events = make_event_store(dir.path()).await;
        let rules = make_engine_with_guard(dir.path(), "U-16", "medium");
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project)?;

        let enforcer = HookEnforcer::new(rules, events, true);
        let event = ToolUseEvent {
            tool_name: "write_file".to_string(),
            affected_files: vec![PathBuf::from("src/main.rs")],
            session_id: None,
        };

        let result = enforcer.post_tool_use(&event, &project).await;

        assert!(
            result.violation_feedback.is_none(),
            "CI environment must bypass hook enforcement"
        );
        Ok(())
    }
}
