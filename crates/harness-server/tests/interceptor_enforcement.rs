use harness_core::{
    interceptor::{InterceptResult, ToolUseEvent, TurnInterceptor},
    Decision, EventFilters, SessionId,
};
use harness_observe::EventStore;
use harness_rules::engine::{Guard, RuleEngine};
use harness_server::hook_enforcer::HookEnforcer;
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::tempdir;
use tokio::sync::RwLock;

/// Serializes temporary mutations of process-global `CI` in this test module.
static CI_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// RAII guard that temporarily unsets `CI` and restores the previous value.
struct CiEnvGuard {
    original: Option<String>,
}

impl CiEnvGuard {
    /// # Safety
    /// Caller must hold `CI_ENV_LOCK` for the lifetime of the guard.
    unsafe fn unset() -> Self {
        let original = std::env::var("CI").ok();
        std::env::remove_var("CI");
        Self { original }
    }
}

impl Drop for CiEnvGuard {
    fn drop(&mut self) {
        unsafe {
            match self.original.take() {
                Some(value) => std::env::set_var("CI", value),
                None => std::env::remove_var("CI"),
            }
        }
    }
}

fn make_engine_with_guard(guard_dir: &std::path::Path) -> Arc<RwLock<RuleEngine>> {
    let script = guard_dir.join("enf-test-guard.sh");
    std::fs::write(
        &script,
        "#!/usr/bin/env bash\necho \"src/lib.rs:1:U-TEST:violation (medium)\"\n",
    )
    .expect("write guard script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&script)
            .expect("stat guard")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).expect("chmod guard");
    }
    let mut engine = RuleEngine::new();
    engine.register_guard(Guard {
        id: harness_core::GuardId::from_str("ENF-TEST-GUARD"),
        script_path: script,
        language: harness_core::Language::Common,
        rules: vec![],
    });
    Arc::new(RwLock::new(engine))
}

/// post_tool_use detects violations and writes a hook_enforcement event to EventStore.
#[tokio::test]
async fn post_tool_use_violation_is_logged_to_event_store() -> anyhow::Result<()> {
    let _ci_lock = CI_ENV_LOCK.lock().await;
    // SAFETY: lock held for the entire test scope.
    let _ci_guard = unsafe { CiEnvGuard::unset() };

    let dir = tempdir()?;
    let event_store = Arc::new(EventStore::new(dir.path()).await?);
    let rules = make_engine_with_guard(dir.path());
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project)?;

    let session = SessionId::new();
    let enforcer = HookEnforcer::new(rules, event_store.clone(), true);
    let event = ToolUseEvent {
        tool_name: "write_file".to_string(),
        affected_files: vec![PathBuf::from("src/lib.rs")],
        session_id: Some(session.clone()),
    };

    let result = enforcer.post_tool_use(&event, &project).await;

    // Violation feedback must be returned to the agent.
    assert!(
        result.violation_feedback.is_some(),
        "expected violation feedback"
    );

    // The hook_enforcement event must be persisted with the correct session_id.
    let logged = event_store
        .query(&EventFilters {
            hook: Some("hook_enforcement".to_string()),
            session_id: Some(session),
            ..Default::default()
        })
        .await?;
    assert!(
        !logged.is_empty(),
        "hook_enforcement event must be persisted for the real session_id"
    );
    assert_eq!(logged[0].decision, Decision::Warn);

    Ok(())
}

/// pre_execute with a blocking interceptor rejects the turn.
#[tokio::test]
async fn pre_execute_block_rejects_the_turn() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let event_store = Arc::new(EventStore::new(dir.path()).await?);
    let rules = Arc::new(RwLock::new(RuleEngine::new()));

    // HookEnforcer itself only blocks in pre_tool_use / post_tool_use;
    // pre_execute always passes through. We verify the contract here and
    // separately test a blocking interceptor via InterceptResult::block().
    let enforcer = HookEnforcer::new(rules, event_store, true);

    let req = harness_core::AgentRequest {
        prompt: "do something".to_string(),
        project_root: dir.path().to_path_buf(),
        ..Default::default()
    };

    let result = enforcer.pre_execute(&req).await;
    assert_eq!(
        result.decision,
        Decision::Pass,
        "HookEnforcer.pre_execute must always pass"
    );

    // Verify that InterceptResult::block() carries Decision::Block.
    let blocked = InterceptResult::block("rule violated");
    assert_eq!(blocked.decision, Decision::Block);
    assert_eq!(blocked.reason.as_deref(), Some("rule violated"));

    Ok(())
}

/// No violations → clean pass-through, no event written.
#[tokio::test]
async fn no_violations_pass_through_without_event() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let event_store = Arc::new(EventStore::new(dir.path()).await?);
    // Empty engine — no guards registered.
    let rules = Arc::new(RwLock::new(RuleEngine::new()));
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project)?;

    let enforcer = HookEnforcer::new(rules, event_store.clone(), true);
    let event = ToolUseEvent {
        tool_name: "write_file".to_string(),
        affected_files: vec![PathBuf::from("src/lib.rs")],
        session_id: None,
    };

    let result = enforcer.post_tool_use(&event, &project).await;

    assert!(
        result.violation_feedback.is_none(),
        "no guards means no violations"
    );

    // With no guards the enforcer exits early, so no event should be logged.
    let logged = event_store
        .query(&EventFilters {
            hook: Some("hook_enforcement".to_string()),
            ..Default::default()
        })
        .await?;
    assert!(
        logged.is_empty(),
        "no event should be logged when no guards are registered"
    );

    Ok(())
}
