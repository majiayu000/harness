use dashmap::DashMap;
use harness_core::{
    agent::{AgentAdapter, ApprovalDecision},
    types::AgentId,
    types::Item,
    types::Thread,
    types::ThreadId,
    types::ThreadStatus,
    types::TokenUsage,
    types::Turn,
    types::TurnId,
    types::TurnStatus,
};
use std::sync::Arc;
use tokio::task::JoinHandle;
use tracing;

/// In-memory thread registry and turn lifecycle manager.
///
/// `ThreadManager` is a pure in-memory cache with no persistence layer.
/// It owns only two DashMaps: one for thread state and one for running-turn
/// task handles.  It intentionally has no `db` field and no async persistence
/// methods (`open`, `persist`, `persist_insert`, `persist_delete`); those were
/// removed in issue #97 because they duplicated the write-through logic that
/// handlers and the task executor perform via `AppState.core.thread_db`.
///
/// Thread persistence is handled exclusively by `CoreServices.thread_db`
/// (`AppState.core.thread_db`).
///
/// Lifecycle ownership:
/// - **Startup hydration**: performed in `http.rs` during app initialisation,
///   which loads persisted threads from `thread_db` into this cache via
///   `threads_cache()`.
/// - **Mutation persistence**: when `thread_db` is configured, mutations to
///   this cache are followed by a write-through. All persistence helpers
///   short-circuit when `thread_db` is `None`, so durability is optional.
///   There are three call sites responsible:
///   1. `handlers/thread.rs` — `persist_thread` / `persist_thread_insert`
///      are called after each direct handler mutation: thread create/fork
///      (`thread_start`, `thread_fork`), turn start (`turn_start`), turn
///      cancel (`turn_cancel`), turn steer (`turn_steer`), thread resume
///      (`thread_resume`), and thread compact (`thread_compact`).
///   2. `handlers/thread.rs` — `thread_delete` removes the entry from this
///      cache and, when `thread_db` is present, calls `db.delete(...)` to
///      remove it from the backing store as well.
///   3. `task_executor` — `persist_runtime_thread` is called (a) after each
///      stream item is applied to the cache (`task_executor/helpers.rs`
///      `process_stream_item`), and (b) at turn completion or failure
///      (`task_executor.rs` and `task_executor/helpers.rs`
///      `mark_turn_failed`).
pub struct ThreadManager {
    threads: DashMap<String, Thread>,
    running_turn_tasks: DashMap<String, JoinHandle<()>>,
    running_adapters: DashMap<String, Arc<dyn AgentAdapter>>,
}

impl ThreadManager {
    pub fn new() -> Self {
        Self {
            threads: DashMap::new(),
            running_turn_tasks: DashMap::new(),
            running_adapters: DashMap::new(),
        }
    }

    /// Access the underlying threads cache (for loading persisted threads).
    pub fn threads_cache(&self) -> &DashMap<String, Thread> {
        &self.threads
    }

    pub fn start_thread(&self, cwd: std::path::PathBuf) -> ThreadId {
        let thread = Thread::new(cwd);
        let id = thread.id.clone();
        self.threads.insert(id.as_str().to_string(), thread);
        id
    }

    pub fn get_thread(&self, id: &ThreadId) -> Option<Thread> {
        self.threads.get(id.as_str()).map(|t| t.clone())
    }

    pub fn list_threads(&self) -> Vec<Thread> {
        self.threads.iter().map(|e| e.value().clone()).collect()
    }

    pub fn delete_thread(&self, id: &ThreadId) -> bool {
        self.threads.remove(id.as_str()).is_some()
    }

    pub fn register_turn_task(&self, turn_id: &TurnId, handle: JoinHandle<()>) {
        if let Some((_, previous)) = self.running_turn_tasks.remove(turn_id.as_str()) {
            previous.abort();
        }
        self.running_turn_tasks
            .insert(turn_id.as_str().to_string(), handle);
    }

    pub fn clear_turn_task(&self, turn_id: &TurnId) {
        self.running_turn_tasks.remove(turn_id.as_str());
    }

    pub fn abort_turn_task(&self, turn_id: &TurnId) -> bool {
        if let Some((_, handle)) = self.running_turn_tasks.remove(turn_id.as_str()) {
            handle.abort();
            true
        } else {
            false
        }
    }

    pub fn start_turn(
        &self,
        thread_id: &ThreadId,
        input: String,
        agent_id: AgentId,
    ) -> harness_core::error::Result<TurnId> {
        let mut thread = self.threads.get_mut(thread_id.as_str()).ok_or_else(|| {
            harness_core::error::HarnessError::ThreadNotFound(thread_id.to_string())
        })?;

        thread.status = ThreadStatus::Active;

        let mut turn = Turn::new(thread_id.clone(), agent_id);
        turn.items.push(Item::UserMessage { content: input });
        let turn_id = turn.id.clone();
        thread.turns.push(turn);
        thread.updated_at = chrono::Utc::now();

        Ok(turn_id)
    }

    fn sync_thread_status(thread: &mut Thread) {
        let has_running_turn = thread
            .turns
            .iter()
            .any(|turn| matches!(turn.status, TurnStatus::Running));
        thread.status = if has_running_turn {
            ThreadStatus::Active
        } else {
            ThreadStatus::Idle
        };
    }

    fn transition_turn(
        &self,
        thread_id: &ThreadId,
        turn_id: &TurnId,
        target_status: TurnStatus,
    ) -> harness_core::error::Result<Option<TokenUsage>> {
        let mut thread = self.threads.get_mut(thread_id.as_str()).ok_or_else(|| {
            harness_core::error::HarnessError::ThreadNotFound(thread_id.to_string())
        })?;

        let transitioned_usage =
            if let Some(turn) = thread.turns.iter_mut().find(|t| t.id == *turn_id) {
                if matches!(turn.status, TurnStatus::Running) {
                    match target_status {
                        TurnStatus::Completed => turn.complete(),
                        TurnStatus::Cancelled => turn.cancel(),
                        TurnStatus::Failed => turn.fail(),
                        TurnStatus::Running => {}
                    }
                    Some(turn.token_usage.clone())
                } else {
                    None
                }
            } else {
                None
            };

        Self::sync_thread_status(&mut thread);
        thread.updated_at = chrono::Utc::now();
        Ok(transitioned_usage)
    }

    pub fn complete_turn(
        &self,
        thread_id: &ThreadId,
        turn_id: &TurnId,
    ) -> harness_core::error::Result<Option<TokenUsage>> {
        self.transition_turn(thread_id, turn_id, TurnStatus::Completed)
    }

    pub fn fail_turn(
        &self,
        thread_id: &ThreadId,
        turn_id: &TurnId,
    ) -> harness_core::error::Result<Option<TokenUsage>> {
        self.transition_turn(thread_id, turn_id, TurnStatus::Failed)
    }

    pub fn cancel_turn(
        &self,
        thread_id: &ThreadId,
        turn_id: &TurnId,
    ) -> harness_core::error::Result<Option<TokenUsage>> {
        self.abort_turn_task(turn_id);
        self.transition_turn(thread_id, turn_id, TurnStatus::Cancelled)
    }

    pub fn add_item(
        &self,
        thread_id: &ThreadId,
        turn_id: &TurnId,
        item: Item,
    ) -> harness_core::error::Result<()> {
        let mut thread = self.threads.get_mut(thread_id.as_str()).ok_or_else(|| {
            harness_core::error::HarnessError::ThreadNotFound(thread_id.to_string())
        })?;

        if let Some(turn) = thread.turns.iter_mut().find(|t| t.id == *turn_id) {
            turn.items.push(item);
        }
        thread.updated_at = chrono::Utc::now();
        Ok(())
    }

    pub fn set_turn_token_usage(
        &self,
        thread_id: &ThreadId,
        turn_id: &TurnId,
        usage: TokenUsage,
    ) -> harness_core::error::Result<bool> {
        let mut thread = self.threads.get_mut(thread_id.as_str()).ok_or_else(|| {
            harness_core::error::HarnessError::ThreadNotFound(thread_id.to_string())
        })?;

        let mut updated = false;
        if let Some(turn) = thread.turns.iter_mut().find(|t| t.id == *turn_id) {
            turn.token_usage = usage;
            updated = true;
        }
        if updated {
            thread.updated_at = chrono::Utc::now();
        }
        Ok(updated)
    }

    pub fn get_turn(&self, thread_id: &ThreadId, turn_id: &TurnId) -> Option<Turn> {
        self.threads
            .get(thread_id.as_str())
            .and_then(|thread| thread.turns.iter().find(|t| t.id == *turn_id).cloned())
    }

    pub fn is_turn_running(&self, thread_id: &ThreadId, turn_id: &TurnId) -> bool {
        self.get_turn(thread_id, turn_id)
            .is_some_and(|turn| matches!(turn.status, TurnStatus::Running))
    }

    pub fn mark_turn_failed_with_error(
        &self,
        thread_id: &ThreadId,
        turn_id: &TurnId,
        message: String,
    ) -> harness_core::error::Result<Option<TokenUsage>> {
        if let Err(e) = self.add_item(thread_id, turn_id, Item::Error { code: -1, message }) {
            tracing::warn!("thread manager: failed to add error item for turn {turn_id}: {e}");
        }
        self.fail_turn(thread_id, turn_id)
    }

    pub fn active_turn_task_count(&self) -> usize {
        self.running_turn_tasks.len()
    }

    pub fn has_turn_task(&self, turn_id: &TurnId) -> bool {
        self.running_turn_tasks.contains_key(turn_id.as_str())
    }

    pub fn find_thread_and_turn(&self, turn_id: &TurnId) -> Option<(ThreadId, Turn)> {
        for entry in self.threads.iter() {
            if let Some(turn) = entry.value().turns.iter().find(|turn| turn.id == *turn_id) {
                return Some((entry.value().id.clone(), turn.clone()));
            }
        }
        None
    }

    /// Find which thread contains a given turn.
    pub fn find_thread_for_turn(&self, turn_id: &TurnId) -> Option<ThreadId> {
        for entry in self.threads.iter() {
            if entry.value().turns.iter().any(|t| t.id == *turn_id) {
                return Some(entry.value().id.clone());
            }
        }
        None
    }

    /// Append a steering instruction to a running turn.
    pub fn steer_turn(
        &self,
        thread_id: &ThreadId,
        turn_id: &TurnId,
        instruction: String,
    ) -> harness_core::error::Result<()> {
        let mut thread = self.threads.get_mut(thread_id.as_str()).ok_or_else(|| {
            harness_core::error::HarnessError::ThreadNotFound(thread_id.to_string())
        })?;

        if let Some(turn) = thread.turns.iter_mut().find(|t| t.id == *turn_id) {
            turn.items.push(Item::UserMessage {
                content: instruction,
            });
        }
        thread.updated_at = chrono::Utc::now();
        Ok(())
    }

    /// Resume an archived thread back to Idle.
    pub fn resume_thread(&self, id: &ThreadId) -> harness_core::error::Result<()> {
        let mut thread = self
            .threads
            .get_mut(id.as_str())
            .ok_or_else(|| harness_core::error::HarnessError::ThreadNotFound(id.to_string()))?;

        thread.status = ThreadStatus::Idle;
        thread.updated_at = chrono::Utc::now();
        Ok(())
    }

    /// Fork a thread, optionally truncating turns at a given position.
    pub fn fork_thread(
        &self,
        id: &ThreadId,
        from_turn: Option<&TurnId>,
    ) -> harness_core::error::Result<ThreadId> {
        // Clone eagerly and drop the read guard before any subsequent write to
        // `self.threads`.  DashMap shards reads with a RwLock: holding a read
        // Ref while calling `insert` on a key that hashes to the same shard
        // will deadlock because `insert` requests an exclusive write lock.
        let mut new_thread = self
            .threads
            .get(id.as_str())
            .ok_or_else(|| harness_core::error::HarnessError::ThreadNotFound(id.to_string()))?
            .clone();
        new_thread.id = ThreadId::new();
        new_thread.status = ThreadStatus::Idle;
        new_thread.updated_at = chrono::Utc::now();

        if let Some(turn_id) = from_turn {
            if let Some(pos) = new_thread.turns.iter().position(|t| t.id == *turn_id) {
                new_thread.turns.truncate(pos + 1);
            }
        }

        // Rebind every inherited turn: assign a fresh unique TurnId and update
        // thread_id to point at the fork.  Without this, the same TurnId would
        // exist in both the source and the fork, making find_thread_for_turn()
        // nondeterministic (DashMap iteration order) and causing turn-control
        // RPCs (cancel / status / steer) to silently route to the wrong branch.
        let fork_id = new_thread.id.clone();
        for turn in &mut new_thread.turns {
            turn.id = TurnId::new();
            turn.thread_id = fork_id.clone();
        }

        self.threads
            .insert(fork_id.as_str().to_string(), new_thread);
        Ok(fork_id)
    }

    /// Compact a thread by clearing items from all completed turns.
    pub fn compact_thread(&self, id: &ThreadId) -> harness_core::error::Result<()> {
        let mut thread = self
            .threads
            .get_mut(id.as_str())
            .ok_or_else(|| harness_core::error::HarnessError::ThreadNotFound(id.to_string()))?;

        for turn in &mut thread.turns {
            if matches!(turn.status, harness_core::types::TurnStatus::Completed) {
                turn.items.clear();
            }
        }
        thread.updated_at = chrono::Utc::now();
        Ok(())
    }

    /// Register an active `AgentAdapter` for a running turn.
    pub fn register_active_adapter(&self, turn_id: &TurnId, adapter: Arc<dyn AgentAdapter>) {
        self.running_adapters
            .insert(turn_id.as_str().to_string(), adapter);
    }

    /// Remove the adapter registration for a completed or cancelled turn.
    pub fn deregister_active_adapter(&self, turn_id: &TurnId) {
        self.running_adapters.remove(turn_id.as_str());
    }

    /// Append a steering instruction to the turn's item list, then forward the
    /// instruction to the live adapter if one is registered for this turn.
    ///
    /// Adapter errors are logged and swallowed — state mutation is authoritative.
    /// Returns `Ok(())` when no adapter is registered (turn may have just completed).
    pub async fn steer_active_turn(
        &self,
        turn_id: &TurnId,
        instruction: String,
    ) -> harness_core::error::Result<()> {
        // Find thread for state mutation.
        let thread_id = match self.find_thread_for_turn(turn_id) {
            Some(id) => id,
            None => return Ok(()), // Turn not found — may have completed.
        };

        // Persist instruction in thread state regardless of adapter status.
        self.steer_turn(&thread_id, turn_id, instruction.clone())?;

        // Clone Arc out of DashMap before async call to avoid holding the ref.
        let adapter = self
            .running_adapters
            .get(turn_id.as_str())
            .map(|r| r.value().clone());
        if let Some(adapter) = adapter {
            if let Err(e) = adapter.steer(instruction).await {
                tracing::warn!(turn_id = %turn_id, "adapter steer failed: {e}");
            }
        }

        Ok(())
    }

    /// Forward an approval response to the live adapter for the given turn.
    ///
    /// Returns `Err` when no adapter is registered or when the adapter rejects
    /// the call (e.g. `Unsupported` for Claude-backed turns).
    pub async fn respond_approval_on_turn(
        &self,
        turn_id: &TurnId,
        request_id: String,
        decision: ApprovalDecision,
    ) -> harness_core::error::Result<()> {
        let adapter = self
            .running_adapters
            .get(turn_id.as_str())
            .map(|r| r.value().clone());
        match adapter {
            Some(adapter) => adapter.respond_approval(request_id, decision).await,
            None => Err(harness_core::error::HarnessError::Unsupported(format!(
                "no active adapter registered for turn {turn_id}"
            ))),
        }
    }
}

impl Default for ThreadManager {
    fn default() -> Self {
        Self::new()
    }
}
