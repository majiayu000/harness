use crate::thread_manager::ThreadManager;
use async_trait::async_trait;
use harness_core::agent::{AgentAdapter, AgentEvent, ApprovalDecision, TurnRequest};
use harness_core::types::{AgentId, Item, ThreadId, TokenUsage, TurnId, TurnStatus};
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

// ── Existing ThreadManager tests ─────────────────────────────────────────────

#[test]
fn start_and_get_thread() {
    let tm = ThreadManager::new();
    let id = tm.start_thread(PathBuf::from("/tmp/proj"));
    let thread = tm.get_thread(&id);
    assert!(thread.is_some());
    assert_eq!(
        thread.as_ref().map(|t| &t.project_root),
        Some(&PathBuf::from("/tmp/proj"))
    );
}

#[test]
fn list_threads_returns_all() {
    let tm = ThreadManager::new();
    tm.start_thread(PathBuf::from("/a"));
    tm.start_thread(PathBuf::from("/b"));
    assert_eq!(tm.list_threads().len(), 2);
}

#[test]
fn delete_thread_removes_it() {
    let tm = ThreadManager::new();
    let id = tm.start_thread(PathBuf::from("/tmp"));
    assert!(tm.delete_thread(&id));
    assert!(tm.get_thread(&id).is_none());
    assert!(!tm.delete_thread(&id));
}

#[test]
fn start_turn_creates_turn() -> anyhow::Result<()> {
    let tm = ThreadManager::new();
    let thread_id = tm.start_thread(PathBuf::from("/tmp"));
    tm.start_turn(&thread_id, "do something".to_string(), AgentId::new())?;
    let thread = tm
        .get_thread(&thread_id)
        .ok_or_else(|| anyhow::anyhow!("thread missing"))?;
    assert_eq!(thread.turns.len(), 1);
    Ok(())
}

#[test]
fn complete_turn_updates_status() -> anyhow::Result<()> {
    let tm = ThreadManager::new();
    let thread_id = tm.start_thread(PathBuf::from("/tmp"));
    let turn_id = tm.start_turn(&thread_id, "task".to_string(), AgentId::new())?;
    let usage = tm.complete_turn(&thread_id, &turn_id)?;
    assert!(usage.is_some());
    let thread = tm
        .get_thread(&thread_id)
        .ok_or_else(|| anyhow::anyhow!("thread missing"))?;
    let turn = thread
        .turns
        .iter()
        .find(|t| t.id == turn_id)
        .ok_or_else(|| anyhow::anyhow!("turn missing"))?;
    assert_eq!(turn.status, TurnStatus::Completed);
    Ok(())
}

#[test]
fn fail_turn_updates_status() -> anyhow::Result<()> {
    let tm = ThreadManager::new();
    let thread_id = tm.start_thread(PathBuf::from("/tmp"));
    let turn_id = tm.start_turn(&thread_id, "task".to_string(), AgentId::new())?;
    let usage = tm.fail_turn(&thread_id, &turn_id)?;
    assert!(usage.is_some());

    let thread = tm
        .get_thread(&thread_id)
        .ok_or_else(|| anyhow::anyhow!("thread missing"))?;
    let turn = thread
        .turns
        .iter()
        .find(|t| t.id == turn_id)
        .ok_or_else(|| anyhow::anyhow!("turn missing"))?;
    assert_eq!(turn.status, TurnStatus::Failed);
    Ok(())
}

#[test]
fn set_turn_token_usage_updates_usage() -> anyhow::Result<()> {
    let tm = ThreadManager::new();
    let thread_id = tm.start_thread(PathBuf::from("/tmp"));
    let turn_id = tm.start_turn(&thread_id, "task".to_string(), AgentId::new())?;
    let updated = tm.set_turn_token_usage(
        &thread_id,
        &turn_id,
        TokenUsage {
            input_tokens: 3,
            output_tokens: 5,
            total_tokens: 8,
            cost_usd: 0.1,
        },
    )?;
    assert!(updated);

    let turn = tm
        .get_turn(&thread_id, &turn_id)
        .ok_or_else(|| anyhow::anyhow!("turn missing"))?;
    assert_eq!(turn.token_usage.total_tokens, 8);
    Ok(())
}

#[tokio::test]
async fn cancel_turn_aborts_registered_task() -> anyhow::Result<()> {
    let tm = ThreadManager::new();
    let thread_id = tm.start_thread(PathBuf::from("/tmp"));
    let turn_id = tm.start_turn(&thread_id, "task".to_string(), AgentId::new())?;

    let handle = tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    });
    tm.register_turn_task(&turn_id, handle);
    assert!(tm.has_turn_task(&turn_id));

    let usage = tm.cancel_turn(&thread_id, &turn_id)?;
    assert!(usage.is_some());
    assert!(!tm.has_turn_task(&turn_id));

    let turn = tm
        .get_turn(&thread_id, &turn_id)
        .ok_or_else(|| anyhow::anyhow!("turn missing"))?;
    assert_eq!(turn.status, TurnStatus::Cancelled);
    Ok(())
}

#[test]
fn add_item_appends_to_turn() -> anyhow::Result<()> {
    let tm = ThreadManager::new();
    let thread_id = tm.start_thread(PathBuf::from("/tmp"));
    let turn_id = tm.start_turn(&thread_id, "task".to_string(), AgentId::new())?;
    let item = Item::AgentReasoning {
        content: "thinking...".to_string(),
    };
    tm.add_item(&thread_id, &turn_id, item)?;
    let thread = tm
        .get_thread(&thread_id)
        .ok_or_else(|| anyhow::anyhow!("thread missing"))?;
    let turn = thread
        .turns
        .iter()
        .find(|t| t.id == turn_id)
        .ok_or_else(|| anyhow::anyhow!("turn missing"))?;
    assert_eq!(turn.items.len(), 2); // UserMessage + AgentReasoning
    Ok(())
}

#[test]
fn start_turn_on_missing_thread_returns_error() {
    let tm = ThreadManager::new();
    let bad_id = ThreadId::from_str("nonexistent");
    assert!(tm
        .start_turn(&bad_id, "x".to_string(), AgentId::new())
        .is_err());
}

#[test]
fn find_thread_for_turn_returns_correct_thread() -> anyhow::Result<()> {
    let tm = ThreadManager::new();
    let thread_id = tm.start_thread(PathBuf::from("/tmp"));
    let turn_id = tm.start_turn(&thread_id, "task".to_string(), AgentId::new())?;
    let found = tm.find_thread_for_turn(&turn_id);
    assert_eq!(found, Some(thread_id));
    Ok(())
}

#[test]
fn find_thread_for_turn_returns_none_for_missing_turn() {
    let tm = ThreadManager::new();
    let _thread_id = tm.start_thread(PathBuf::from("/tmp"));
    let fake_turn = TurnId::from_str("no-such-turn");
    assert!(tm.find_thread_for_turn(&fake_turn).is_none());
}

#[test]
fn find_thread_and_turn_returns_thread_and_turn() -> anyhow::Result<()> {
    let tm = ThreadManager::new();
    let thread_id = tm.start_thread(PathBuf::from("/tmp"));
    let turn_id = tm.start_turn(&thread_id, "task".to_string(), AgentId::new())?;
    let (found_thread_id, found_turn) = tm
        .find_thread_and_turn(&turn_id)
        .ok_or_else(|| anyhow::anyhow!("find_thread_and_turn returned None"))?;
    assert_eq!(found_thread_id, thread_id);
    assert_eq!(found_turn.id, turn_id);
    Ok(())
}

/// After forking, inherited turns are rebound with new unique IDs so that
/// `find_thread_for_turn` is deterministic.  The original TurnId must
/// resolve exclusively to the source thread — not to the fork and not to
/// None — because the fork's copy carries a fresh TurnId.
#[test]
fn find_thread_for_turn_after_fork_returns_source_thread() -> anyhow::Result<()> {
    let tm = ThreadManager::new();
    let thread_id = tm.start_thread(PathBuf::from("/tmp"));
    let turn_id = tm.start_turn(&thread_id, "task".to_string(), AgentId::new())?;
    tm.complete_turn(&thread_id, &turn_id)?;
    let _fork_id = tm.fork_thread(&thread_id, None)?;

    let found = tm.find_thread_for_turn(&turn_id);
    assert_eq!(
        found,
        Some(thread_id.clone()),
        "original turn ID must resolve deterministically to the source thread after fork"
    );
    Ok(())
}

/// A turn created exclusively on the forked branch must always resolve to
/// the fork — there is no ambiguity for turns that do not appear in the
/// original thread.
#[test]
fn find_thread_and_turn_fork_only_turn_routes_to_fork() -> anyhow::Result<()> {
    let tm = ThreadManager::new();
    let thread_id = tm.start_thread(PathBuf::from("/tmp"));
    let turn_id = tm.start_turn(&thread_id, "task".to_string(), AgentId::new())?;
    tm.complete_turn(&thread_id, &turn_id)?;
    let fork_id = tm.fork_thread(&thread_id, None)?;

    let fork_turn_id = tm.start_turn(&fork_id, "fork-task".to_string(), AgentId::new())?;
    let (found_thread_id, found_turn) = tm
        .find_thread_and_turn(&fork_turn_id)
        .ok_or_else(|| anyhow::anyhow!("find_thread_and_turn returned None for fork turn"))?;
    assert_eq!(
        found_thread_id, fork_id,
        "fork-only turn must resolve to the fork, not the original"
    );
    assert_eq!(found_turn.id, fork_turn_id);
    Ok(())
}

#[test]
fn steer_turn_appends_instruction() -> anyhow::Result<()> {
    let tm = ThreadManager::new();
    let thread_id = tm.start_thread(PathBuf::from("/tmp"));
    let turn_id = tm.start_turn(&thread_id, "initial".to_string(), AgentId::new())?;
    tm.steer_turn(&thread_id, &turn_id, "steer me".to_string())?;
    let turn = tm
        .get_turn(&thread_id, &turn_id)
        .ok_or_else(|| anyhow::anyhow!("turn missing"))?;
    assert_eq!(turn.items.len(), 2); // UserMessage + steer UserMessage
    Ok(())
}

#[test]
fn resume_thread_sets_status_to_idle() -> anyhow::Result<()> {
    let tm = ThreadManager::new();
    let thread_id = tm.start_thread(PathBuf::from("/tmp"));

    tm.threads_cache()
        .get_mut(thread_id.as_str())
        .ok_or_else(|| anyhow::anyhow!("thread missing before archive"))?
        .status = harness_core::types::ThreadStatus::Archived;

    {
        let pre = tm
            .get_thread(&thread_id)
            .ok_or_else(|| anyhow::anyhow!("thread missing"))?;
        assert_eq!(pre.status, harness_core::types::ThreadStatus::Archived);
    }

    tm.resume_thread(&thread_id)?;

    let thread = tm
        .get_thread(&thread_id)
        .ok_or_else(|| anyhow::anyhow!("thread missing after resume"))?;
    assert_eq!(thread.status, harness_core::types::ThreadStatus::Idle);
    Ok(())
}

#[test]
fn fork_thread_creates_independent_copy() -> anyhow::Result<()> {
    let tm = ThreadManager::new();
    let thread_id = tm.start_thread(PathBuf::from("/tmp"));
    let _turn_id = tm.start_turn(&thread_id, "task".to_string(), AgentId::new())?;
    let fork_id = tm.fork_thread(&thread_id, None)?;
    assert_ne!(fork_id, thread_id);
    let fork = tm
        .get_thread(&fork_id)
        .ok_or_else(|| anyhow::anyhow!("fork missing"))?;
    assert_eq!(fork.turns.len(), 1);
    assert_eq!(fork.status, harness_core::types::ThreadStatus::Idle);
    assert!(
        fork.turns.iter().all(|t| t.thread_id == fork_id),
        "all inherited turns must reference the fork's thread_id"
    );
    Ok(())
}

#[test]
fn fork_thread_truncates_at_turn() -> anyhow::Result<()> {
    let tm = ThreadManager::new();
    let thread_id = tm.start_thread(PathBuf::from("/tmp"));
    let turn1 = tm.start_turn(&thread_id, "first".to_string(), AgentId::new())?;
    tm.complete_turn(&thread_id, &turn1)?;
    let _turn2 = tm.start_turn(&thread_id, "second".to_string(), AgentId::new())?;
    let fork_id = tm.fork_thread(&thread_id, Some(&turn1))?;
    let fork = tm
        .get_thread(&fork_id)
        .ok_or_else(|| anyhow::anyhow!("fork missing"))?;
    assert_eq!(fork.turns.len(), 1);
    assert_ne!(
        fork.turns[0].id, turn1,
        "fork turn must have a new unique ID, not the source turn ID"
    );
    assert!(
        fork.turns.iter().all(|t| t.thread_id == fork_id),
        "all inherited turns must reference the fork's thread_id"
    );
    Ok(())
}

#[test]
fn compact_thread_clears_completed_turn_items() -> anyhow::Result<()> {
    let tm = ThreadManager::new();
    let thread_id = tm.start_thread(PathBuf::from("/tmp"));
    let turn_id = tm.start_turn(&thread_id, "task".to_string(), AgentId::new())?;
    tm.add_item(
        &thread_id,
        &turn_id,
        Item::AgentReasoning {
            content: "thinking".to_string(),
        },
    )?;
    tm.complete_turn(&thread_id, &turn_id)?;
    tm.compact_thread(&thread_id)?;
    let thread = tm
        .get_thread(&thread_id)
        .ok_or_else(|| anyhow::anyhow!("thread missing"))?;
    let turn = thread
        .turns
        .iter()
        .find(|t| t.id == turn_id)
        .ok_or_else(|| anyhow::anyhow!("turn missing after compact"))?;
    assert!(turn.items.is_empty());
    Ok(())
}

#[test]
fn mark_turn_failed_with_error_adds_error_item() -> anyhow::Result<()> {
    let tm = ThreadManager::new();
    let thread_id = tm.start_thread(PathBuf::from("/tmp"));
    let turn_id = tm.start_turn(&thread_id, "task".to_string(), AgentId::new())?;
    tm.mark_turn_failed_with_error(&thread_id, &turn_id, "something broke".to_string())?;
    let turn = tm
        .get_turn(&thread_id, &turn_id)
        .ok_or_else(|| anyhow::anyhow!("turn missing"))?;
    assert_eq!(turn.status, TurnStatus::Failed);
    let has_error = turn
        .items
        .iter()
        .any(|item| matches!(item, Item::Error { message, .. } if message == "something broke"));
    assert!(has_error, "expected an Error item in the turn");
    Ok(())
}

#[tokio::test]
async fn active_turn_task_count_reflects_registered_handles() -> anyhow::Result<()> {
    let tm = ThreadManager::new();
    let thread_id = tm.start_thread(PathBuf::from("/tmp"));
    let turn_id = tm.start_turn(&thread_id, "task".to_string(), AgentId::new())?;
    assert_eq!(tm.active_turn_task_count(), 0);
    let handle =
        tokio::spawn(async { tokio::time::sleep(std::time::Duration::from_secs(5)).await });
    tm.register_turn_task(&turn_id, handle);
    assert_eq!(tm.active_turn_task_count(), 1);
    tm.clear_turn_task(&turn_id);
    assert_eq!(tm.active_turn_task_count(), 0);
    Ok(())
}

#[tokio::test]
async fn abort_turn_task_aborts_handle_directly() -> anyhow::Result<()> {
    let tm = ThreadManager::new();
    let thread_id = tm.start_thread(PathBuf::from("/tmp"));
    let turn_id = tm.start_turn(&thread_id, "task".to_string(), AgentId::new())?;
    let handle =
        tokio::spawn(async { tokio::time::sleep(std::time::Duration::from_secs(5)).await });
    tm.register_turn_task(&turn_id, handle);
    assert!(tm.abort_turn_task(&turn_id));
    assert!(!tm.has_turn_task(&turn_id));
    assert!(!tm.abort_turn_task(&turn_id)); // already gone
    Ok(())
}

#[test]
fn is_turn_running_reflects_turn_status() -> anyhow::Result<()> {
    let tm = ThreadManager::new();
    let thread_id = tm.start_thread(PathBuf::from("/tmp"));
    let turn_id = tm.start_turn(&thread_id, "task".to_string(), AgentId::new())?;
    assert!(tm.is_turn_running(&thread_id, &turn_id));
    tm.complete_turn(&thread_id, &turn_id)?;
    assert!(!tm.is_turn_running(&thread_id, &turn_id));
    Ok(())
}

/// `cancel_turn` on an already-completed turn must be a no-op: it returns
/// `None` (no usage snapshot) and leaves the turn status unchanged.
#[test]
fn cancel_turn_on_completed_turn_is_noop() -> anyhow::Result<()> {
    let tm = ThreadManager::new();
    let thread_id = tm.start_thread(PathBuf::from("/tmp"));
    let turn_id = tm.start_turn(&thread_id, "task".to_string(), AgentId::new())?;
    tm.complete_turn(&thread_id, &turn_id)?;

    let usage = tm.cancel_turn(&thread_id, &turn_id)?;
    assert!(
        usage.is_none(),
        "cancelling a completed turn must return None"
    );

    let turn = tm
        .get_turn(&thread_id, &turn_id)
        .ok_or_else(|| anyhow::anyhow!("turn missing"))?;
    assert_eq!(
        turn.status,
        TurnStatus::Completed,
        "status must remain Completed after noop cancel"
    );
    Ok(())
}

/// `complete_turn` on an already-failed turn must return `None` and leave
/// the status as Failed — `transition_turn` only acts on Running turns.
#[test]
fn complete_turn_on_failed_turn_is_noop() -> anyhow::Result<()> {
    let tm = ThreadManager::new();
    let thread_id = tm.start_thread(PathBuf::from("/tmp"));
    let turn_id = tm.start_turn(&thread_id, "task".to_string(), AgentId::new())?;
    tm.fail_turn(&thread_id, &turn_id)?;

    let usage = tm.complete_turn(&thread_id, &turn_id)?;
    assert!(usage.is_none(), "completing a failed turn must return None");

    let turn = tm
        .get_turn(&thread_id, &turn_id)
        .ok_or_else(|| anyhow::anyhow!("turn missing"))?;
    assert_eq!(
        turn.status,
        TurnStatus::Failed,
        "status must remain Failed after noop complete"
    );
    Ok(())
}

/// `get_turn` returns `None` when the thread itself does not exist.
#[test]
fn get_turn_returns_none_for_missing_thread() {
    let tm = ThreadManager::new();
    let missing_thread = ThreadId::from_str("no-such-thread");
    let missing_turn = TurnId::from_str("no-such-turn");
    assert!(tm.get_turn(&missing_thread, &missing_turn).is_none());
}

/// `steer_turn` propagates `ThreadNotFound` when the thread does not exist.
#[test]
fn steer_turn_on_missing_thread_returns_error() {
    let tm = ThreadManager::new();
    let missing = ThreadId::from_str("ghost");
    let turn = TurnId::from_str("ghost-turn");
    assert!(tm.steer_turn(&missing, &turn, "x".to_string()).is_err());
}

// ── Adapter registry tests ────────────────────────────────────────────────────

struct AlwaysUnsupportedAdapter;

#[async_trait]
impl AgentAdapter for AlwaysUnsupportedAdapter {
    fn name(&self) -> &str {
        "unsupported"
    }
    async fn start_turn(
        &self,
        _req: TurnRequest,
        _tx: tokio::sync::mpsc::Sender<AgentEvent>,
    ) -> harness_core::error::Result<()> {
        Ok(())
    }
    async fn interrupt(&self) -> harness_core::error::Result<()> {
        Ok(())
    }
    async fn steer(&self, _text: String) -> harness_core::error::Result<()> {
        Err(harness_core::error::HarnessError::Unsupported(
            "AlwaysUnsupportedAdapter".into(),
        ))
    }
    async fn respond_approval(
        &self,
        _id: String,
        _decision: ApprovalDecision,
    ) -> harness_core::error::Result<()> {
        Err(harness_core::error::HarnessError::Unsupported(
            "AlwaysUnsupportedAdapter".into(),
        ))
    }
}

struct TrackingAdapter {
    steer_called: Arc<AtomicBool>,
}

#[async_trait]
impl AgentAdapter for TrackingAdapter {
    fn name(&self) -> &str {
        "tracking"
    }
    async fn start_turn(
        &self,
        _req: TurnRequest,
        _tx: tokio::sync::mpsc::Sender<AgentEvent>,
    ) -> harness_core::error::Result<()> {
        Ok(())
    }
    async fn interrupt(&self) -> harness_core::error::Result<()> {
        Ok(())
    }
    async fn steer(&self, _text: String) -> harness_core::error::Result<()> {
        self.steer_called.store(true, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test]
async fn steer_active_turn_delegates_to_registered_adapter() -> anyhow::Result<()> {
    let tm = ThreadManager::new();
    let thread_id = tm.start_thread(PathBuf::from("/tmp"));
    let turn_id = tm.start_turn(&thread_id, "task".to_string(), AgentId::new())?;

    let called = Arc::new(AtomicBool::new(false));
    tm.register_active_adapter(
        &turn_id,
        Arc::new(TrackingAdapter {
            steer_called: called.clone(),
        }),
    );

    tm.steer_active_turn(&turn_id, "redirect".to_string())
        .await?;
    assert!(
        called.load(Ordering::SeqCst),
        "adapter steer must have been called"
    );
    Ok(())
}

#[tokio::test]
async fn steer_active_turn_no_adapter_returns_ok() -> anyhow::Result<()> {
    let tm = ThreadManager::new();
    let thread_id = tm.start_thread(PathBuf::from("/tmp"));
    let turn_id = tm.start_turn(&thread_id, "task".to_string(), AgentId::new())?;
    // No adapter registered.
    let result = tm.steer_active_turn(&turn_id, "steer".to_string()).await;
    assert!(result.is_ok(), "steer with no adapter must return Ok");
    Ok(())
}

#[tokio::test]
async fn respond_approval_on_turn_propagates_unsupported_error() -> anyhow::Result<()> {
    let tm = ThreadManager::new();
    let thread_id = tm.start_thread(PathBuf::from("/tmp"));
    let turn_id = tm.start_turn(&thread_id, "task".to_string(), AgentId::new())?;
    tm.register_active_adapter(&turn_id, Arc::new(AlwaysUnsupportedAdapter));

    let err = tm
        .respond_approval_on_turn(&turn_id, "req-1".to_string(), ApprovalDecision::Accept)
        .await
        .expect_err("should propagate Unsupported");
    assert!(
        err.to_string().contains("unsupported"),
        "error must mention unsupported, got: {err}"
    );
    Ok(())
}

#[tokio::test]
async fn steer_active_turn_after_deregister_is_noop() -> anyhow::Result<()> {
    let tm = ThreadManager::new();
    let thread_id = tm.start_thread(PathBuf::from("/tmp"));
    let turn_id = tm.start_turn(&thread_id, "task".to_string(), AgentId::new())?;

    let called = Arc::new(AtomicBool::new(false));
    tm.register_active_adapter(
        &turn_id,
        Arc::new(TrackingAdapter {
            steer_called: called.clone(),
        }),
    );
    tm.deregister_active_adapter(&turn_id);

    tm.steer_active_turn(&turn_id, "steer".to_string()).await?;
    assert!(
        !called.load(Ordering::SeqCst),
        "adapter must not be called after deregister"
    );
    Ok(())
}
