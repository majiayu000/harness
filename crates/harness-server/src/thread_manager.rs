use crate::thread_db::ThreadDb;
use dashmap::DashMap;
use harness_core::{
    AgentId, Item, Thread, ThreadId, ThreadStatus, Turn, TurnId,
};

pub struct ThreadManager {
    threads: DashMap<String, Thread>,
    db: Option<ThreadDb>,
}

impl ThreadManager {
    pub fn new() -> Self {
        Self {
            threads: DashMap::new(),
            db: None,
        }
    }

    /// Open with SQLite persistence. Loads existing threads into cache.
    pub async fn open(db_path: &std::path::Path) -> anyhow::Result<Self> {
        let db = ThreadDb::open(db_path).await?;
        let threads = DashMap::new();
        for thread in db.list().await? {
            threads.insert(thread.id.as_str().to_string(), thread);
        }
        Ok(Self { threads, db: Some(db) })
    }

    /// Persist a single thread to DB (no-op if no DB configured).
    pub async fn persist(&self, id: &ThreadId) {
        if let Some(db) = &self.db {
            if let Some(thread) = self.threads.get(id.as_str()) {
                if let Err(e) = db.update(thread.value()).await {
                    tracing::error!("thread_db update failed: {e}");
                }
            }
        }
    }

    /// Access the underlying threads cache (for loading persisted threads).
    pub fn threads_cache(&self) -> &DashMap<String, Thread> {
        &self.threads
    }

    /// Persist a newly inserted thread to DB.
    pub async fn persist_insert(&self, id: &ThreadId) {
        if let Some(db) = &self.db {
            if let Some(thread) = self.threads.get(id.as_str()) {
                if let Err(e) = db.insert(thread.value()).await {
                    tracing::error!("thread_db insert failed: {e}");
                }
            }
        }
    }

    /// Persist a thread deletion to DB.
    pub async fn persist_delete(&self, id: &str) {
        if let Some(db) = &self.db {
            if let Err(e) = db.delete(id).await {
                tracing::error!("thread_db delete failed: {e}");
            }
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
            turn.items.push(Item::UserMessage { content: instruction });
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
    use harness_core::{AgentId, Item, TurnStatus};
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
        let thread = tm.get_thread(&thread_id)
            .ok_or_else(|| anyhow::anyhow!("thread missing"))?;
        assert_eq!(thread.turns.len(), 1);
        Ok(())
    }

    #[test]
    fn complete_turn_updates_status() -> anyhow::Result<()> {
        let tm = ThreadManager::new();
        let thread_id = tm.start_thread(PathBuf::from("/tmp"));
        let turn_id = tm.start_turn(&thread_id, "task".to_string(), AgentId::new())?;
        tm.complete_turn(&thread_id, &turn_id)?;
        let thread = tm.get_thread(&thread_id)
            .ok_or_else(|| anyhow::anyhow!("thread missing"))?;
        let turn = thread.turns.iter().find(|t| t.id == turn_id)
            .ok_or_else(|| anyhow::anyhow!("turn missing"))?;
        assert_eq!(turn.status, TurnStatus::Completed);
        Ok(())
    }

    #[test]
    fn add_item_appends_to_turn() -> anyhow::Result<()> {
        let tm = ThreadManager::new();
        let thread_id = tm.start_thread(PathBuf::from("/tmp"));
        let turn_id = tm.start_turn(&thread_id, "task".to_string(), AgentId::new())?;
        let item = Item::AgentReasoning { content: "thinking...".to_string() };
        tm.add_item(&thread_id, &turn_id, item)?;
        let thread = tm.get_thread(&thread_id)
            .ok_or_else(|| anyhow::anyhow!("thread missing"))?;
        let turn = thread.turns.iter().find(|t| t.id == turn_id)
            .ok_or_else(|| anyhow::anyhow!("turn missing"))?;
        assert_eq!(turn.items.len(), 2); // UserMessage + AgentReasoning
        Ok(())
    }

    #[test]
    fn start_turn_on_missing_thread_returns_error() {
        let tm = ThreadManager::new();
        let bad_id = ThreadId::from_str("nonexistent");
        assert!(tm.start_turn(&bad_id, "x".to_string(), AgentId::new()).is_err());
    }
}
