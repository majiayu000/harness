use dashmap::DashMap;
use harness_core::{
    AgentId, Item, Thread, ThreadId, ThreadStatus, TokenUsage, Turn, TurnId, TurnStatus,
};
use tokio::task::JoinHandle;

/// In-memory thread registry and turn lifecycle manager.
///
/// `ThreadManager` is a pure in-memory cache. It owns no persistence layer.
/// Thread persistence is handled exclusively by `CoreServices.thread_db`
/// (`AppState.core.thread_db`).
///
/// Lifecycle ownership:
/// - **Startup hydration**: performed in `http.rs` during app initialisation,
///   which loads persisted threads from `thread_db` into this cache via
///   `threads_cache()`.
/// - **Mutation persistence**: every mutation to this cache is followed by a
///   write-through to `thread_db`. There are two call sites responsible:
///   1. `handlers/thread.rs` — `persist_thread` / `persist_thread_insert`
///      are called after each direct handler mutation: thread create/fork
///      (`thread_start`, `thread_fork`), turn start (`turn_start`), turn
///      cancel (`turn_cancel`), turn steer (`turn_steer`), thread resume
///      (`thread_resume`), and thread compact (`thread_compact`).
///   2. `task_executor` — `persist_runtime_thread` is called (a) after each
///      stream item is applied to the cache (`task_executor/helpers.rs`
///      `process_stream_item`), and (b) at turn completion or failure
///      (`task_executor.rs` and `task_executor/helpers.rs`
///      `mark_turn_failed`).
pub struct ThreadManager {
    threads: DashMap<String, Thread>,
    running_turn_tasks: DashMap<String, JoinHandle<()>>,
}

impl ThreadManager {
    pub fn new() -> Self {
        Self {
            threads: DashMap::new(),
            running_turn_tasks: DashMap::new(),
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
    ) -> harness_core::Result<TurnId> {
        let mut thread = self
            .threads
            .get_mut(thread_id.as_str())
            .ok_or_else(|| harness_core::HarnessError::ThreadNotFound(thread_id.to_string()))?;

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
    ) -> harness_core::Result<Option<TokenUsage>> {
        let mut thread = self
            .threads
            .get_mut(thread_id.as_str())
            .ok_or_else(|| harness_core::HarnessError::ThreadNotFound(thread_id.to_string()))?;

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
    ) -> harness_core::Result<Option<TokenUsage>> {
        self.transition_turn(thread_id, turn_id, TurnStatus::Completed)
    }

    pub fn fail_turn(
        &self,
        thread_id: &ThreadId,
        turn_id: &TurnId,
    ) -> harness_core::Result<Option<TokenUsage>> {
        self.transition_turn(thread_id, turn_id, TurnStatus::Failed)
    }

    pub fn cancel_turn(
        &self,
        thread_id: &ThreadId,
        turn_id: &TurnId,
    ) -> harness_core::Result<Option<TokenUsage>> {
        self.abort_turn_task(turn_id);
        self.transition_turn(thread_id, turn_id, TurnStatus::Cancelled)
    }

    pub fn add_item(
        &self,
        thread_id: &ThreadId,
        turn_id: &TurnId,
        item: Item,
    ) -> harness_core::Result<()> {
        let mut thread = self
            .threads
            .get_mut(thread_id.as_str())
            .ok_or_else(|| harness_core::HarnessError::ThreadNotFound(thread_id.to_string()))?;

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
    ) -> harness_core::Result<bool> {
        let mut thread = self
            .threads
            .get_mut(thread_id.as_str())
            .ok_or_else(|| harness_core::HarnessError::ThreadNotFound(thread_id.to_string()))?;

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
    ) -> harness_core::Result<Option<TokenUsage>> {
        let _ = self.add_item(thread_id, turn_id, Item::Error { code: -1, message });
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
    ) -> harness_core::Result<()> {
        let mut thread = self
            .threads
            .get_mut(thread_id.as_str())
            .ok_or_else(|| harness_core::HarnessError::ThreadNotFound(thread_id.to_string()))?;

        if let Some(turn) = thread.turns.iter_mut().find(|t| t.id == *turn_id) {
            turn.items.push(Item::UserMessage {
                content: instruction,
            });
        }
        thread.updated_at = chrono::Utc::now();
        Ok(())
    }

    /// Resume an archived thread back to Idle.
    pub fn resume_thread(&self, id: &ThreadId) -> harness_core::Result<()> {
        let mut thread = self
            .threads
            .get_mut(id.as_str())
            .ok_or_else(|| harness_core::HarnessError::ThreadNotFound(id.to_string()))?;

        thread.status = ThreadStatus::Idle;
        thread.updated_at = chrono::Utc::now();
        Ok(())
    }

    /// Fork a thread, optionally truncating turns at a given position.
    pub fn fork_thread(
        &self,
        id: &ThreadId,
        from_turn: Option<&TurnId>,
    ) -> harness_core::Result<ThreadId> {
        let source = self
            .threads
            .get(id.as_str())
            .ok_or_else(|| harness_core::HarnessError::ThreadNotFound(id.to_string()))?;

        let mut new_thread = source.clone();
        new_thread.id = ThreadId::new();
        new_thread.status = ThreadStatus::Idle;
        new_thread.updated_at = chrono::Utc::now();

        if let Some(turn_id) = from_turn {
            if let Some(pos) = new_thread.turns.iter().position(|t| t.id == *turn_id) {
                new_thread.turns.truncate(pos + 1);
            }
        }

        let new_id = new_thread.id.clone();
        self.threads.insert(new_id.as_str().to_string(), new_thread);
        Ok(new_id)
    }

    /// Compact a thread by clearing items from all completed turns.
    pub fn compact_thread(&self, id: &ThreadId) -> harness_core::Result<()> {
        let mut thread = self
            .threads
            .get_mut(id.as_str())
            .ok_or_else(|| harness_core::HarnessError::ThreadNotFound(id.to_string()))?;

        for turn in &mut thread.turns {
            if matches!(turn.status, harness_core::TurnStatus::Completed) {
                turn.items.clear();
            }
        }
        thread.updated_at = chrono::Utc::now();
        Ok(())
    }
}

impl Default for ThreadManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::{AgentId, Item, TokenUsage, TurnStatus};
    use std::path::PathBuf;

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
}
