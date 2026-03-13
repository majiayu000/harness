use async_trait::async_trait;
use harness_core::{
    interceptor::{InterceptResult, PostToolUseResult, ToolUseEvent, TurnInterceptor},
    AgentRequest, Decision, Event, SessionId,
};
use harness_observe::EventStore;
use harness_rules::engine::RuleEngine;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;

/// Post-tool-use hook enforcer.
///
/// When `enabled`, this interceptor fires after each agent turn, detects files
/// that were modified (via `git status --porcelain`), scans them with all
/// registered guards, logs a `hook_enforcement` event to the [`EventStore`],
/// and returns any violations as feedback for the agent's next turn prompt.
///
/// The enforcer silently passes through when:
/// - `enabled` is `false` (controlled by `rules.hook_enforcement` in config)
/// - no guards are registered
/// - no files were modified during the turn
pub struct HookEnforcer {
    rules: Arc<RwLock<RuleEngine>>,
    events: Arc<EventStore>,
    enabled: bool,
}

impl HookEnforcer {
    pub fn new(rules: Arc<RwLock<RuleEngine>>, events: Arc<EventStore>, enabled: bool) -> Self {
        Self {
            rules,
            events,
            enabled,
        }
    }

    /// Detect files added or modified in `project_root` using `git status --porcelain`.
    ///
    /// Deleted files are excluded. Returns an empty list when git is unavailable
    /// or `project_root` is not a git repository.
    pub async fn detect_modified_files(project_root: &Path) -> Vec<PathBuf> {
        let output = tokio::process::Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(project_root)
            .output()
            .await;
        match output {
            Ok(out) => String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter_map(|line| {
                    // git status --porcelain format: "XY filename" (at least 4 chars)
                    let line = line.trim();
                    if line.len() < 4 {
                        return None;
                    }
                    // Skip deleted entries
                    if line[..2].contains('D') {
                        return None;
                    }
                    Some(PathBuf::from(line[3..].trim()))
                })
                .collect(),
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    project_root = %project_root.display(),
                    "hook_enforcer: git status unavailable"
                );
                Vec::new()
            }
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
    async fn post_tool_use(&self, event: &ToolUseEvent, project_root: &Path) -> PostToolUseResult {
        if !self.enabled {
            return PostToolUseResult::clean();
        }
        if event.affected_files.is_empty() {
            return PostToolUseResult::clean();
        }

        let engine = self.rules.read().await;
        if engine.guards().is_empty() {
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

        let mut ev = Event::new(
            SessionId::new(),
            "hook_enforcement",
            "post_tool_use",
            decision,
        );
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
            return PostToolUseResult::clean();
        }

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
    use harness_core::{EventFilters, GuardId, Language};
    use harness_rules::engine::{Guard, RuleEngine};
    use tempfile::tempdir;

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
        let dir = tempdir()?;
        let events = make_event_store(dir.path()).await;
        let rules = make_engine_with_guard(dir.path(), "U-16", "medium");
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project)?;

        let enforcer = HookEnforcer::new(rules, events, true);
        let event = ToolUseEvent {
            tool_name: "write_file".to_string(),
            affected_files: vec![PathBuf::from("src/main.rs")],
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
        let dir = tempdir()?;
        let events = make_event_store(dir.path()).await;
        let rules = make_engine_with_guard(dir.path(), "U-16", "medium");
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project)?;

        let enforcer = HookEnforcer::new(rules, events, false);
        let event = ToolUseEvent {
            tool_name: "write_file".to_string(),
            affected_files: vec![PathBuf::from("src/main.rs")],
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
        let dir = tempdir()?;
        let events = make_event_store(dir.path()).await;
        let rules = make_engine_with_guard(dir.path(), "U-16", "medium");

        let enforcer = HookEnforcer::new(rules, events, true);
        let event = ToolUseEvent {
            tool_name: "write_file".to_string(),
            affected_files: vec![],
        };
        let result = enforcer.post_tool_use(&event, dir.path()).await;
        assert!(result.violation_feedback.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn post_tool_use_logs_event_to_store() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let event_store = Arc::new(EventStore::new(dir.path()).await?);
        let rules = make_engine_with_guard(dir.path(), "U-16", "medium");
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project)?;

        let enforcer = HookEnforcer::new(rules, event_store.clone(), true);
        let event = ToolUseEvent {
            tool_name: "write_file".to_string(),
            affected_files: vec![PathBuf::from("src/main.rs")],
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
        let dir = tempdir()?;
        let events = make_event_store(dir.path()).await;
        let rules = Arc::new(RwLock::new(RuleEngine::new()));

        let enforcer = HookEnforcer::new(rules, events, true);
        let event = ToolUseEvent {
            tool_name: "write_file".to_string(),
            affected_files: vec![PathBuf::from("src/main.rs")],
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
        let dir = tempdir()?;
        let events = make_event_store(dir.path()).await;
        let rules = Arc::new(RwLock::new(RuleEngine::new()));
        let enforcer = HookEnforcer::new(rules, events, false);
        assert_eq!(enforcer.name(), "hook_enforcer");
        Ok(())
    }
}
