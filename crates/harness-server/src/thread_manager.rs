use dashmap::DashMap;
use harness_core::{
    AgentId, Item, Thread, ThreadId, ThreadStatus, TokenUsage, Turn, TurnId, TurnStatus,
};
use tokio::task::JoinHandle;

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
        let fake_turn = harness_core::TurnId::from_str("no-such-turn");
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

        // This turn exists only in the fork — lookup must be unambiguous.
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

        // Manually archive the thread so we can validate the Archived → Idle
        // transition. Without this the thread is already Idle after start_thread
        // and resume_thread() would be a no-op with respect to status.
        tm.threads_cache()
            .get_mut(thread_id.as_str())
            .ok_or_else(|| anyhow::anyhow!("thread missing before archive"))?
            .status = harness_core::ThreadStatus::Archived;

        {
            let pre = tm
                .get_thread(&thread_id)
                .ok_or_else(|| anyhow::anyhow!("thread missing"))?;
            assert_eq!(pre.status, harness_core::ThreadStatus::Archived);
        }

        tm.resume_thread(&thread_id)?;

        let thread = tm
            .get_thread(&thread_id)
            .ok_or_else(|| anyhow::anyhow!("thread missing after resume"))?;
        assert_eq!(thread.status, harness_core::ThreadStatus::Idle);
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
        assert_eq!(fork.status, harness_core::ThreadStatus::Idle);
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
        // Inherited turns are rebound with fresh IDs — the fork's turn must not
        // share the source's TurnId (which would cause nondeterministic routing).
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
        let has_error = turn.items.iter().any(
            |item| matches!(item, Item::Error { message, .. } if message == "something broke"),
        );
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
}
