use dashmap::DashMap;
use harness_core::{
    AgentId, Item, Thread, ThreadId, ThreadStatus, Turn, TurnId,
};

pub struct ThreadManager {
    threads: DashMap<String, Thread>,
}

impl ThreadManager {
    pub fn new() -> Self {
        Self {
            threads: DashMap::new(),
        }
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

    pub fn complete_turn(&self, thread_id: &ThreadId, turn_id: &TurnId) -> harness_core::Result<()> {
        let mut thread = self
            .threads
            .get_mut(thread_id.as_str())
            .ok_or_else(|| harness_core::HarnessError::ThreadNotFound(thread_id.to_string()))?;

        if let Some(turn) = thread.turns.iter_mut().find(|t| t.id == *turn_id) {
            turn.complete();
        }
        thread.status = ThreadStatus::Idle;
        thread.updated_at = chrono::Utc::now();
        Ok(())
    }

    pub fn cancel_turn(&self, thread_id: &ThreadId, turn_id: &TurnId) -> harness_core::Result<()> {
        let mut thread = self
            .threads
            .get_mut(thread_id.as_str())
            .ok_or_else(|| harness_core::HarnessError::ThreadNotFound(thread_id.to_string()))?;

        if let Some(turn) = thread.turns.iter_mut().find(|t| t.id == *turn_id) {
            turn.cancel();
        }
        thread.status = ThreadStatus::Idle;
        thread.updated_at = chrono::Utc::now();
        Ok(())
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
        Ok(())
    }
}

impl Default for ThreadManager {
    fn default() -> Self {
        Self::new()
    }
}
