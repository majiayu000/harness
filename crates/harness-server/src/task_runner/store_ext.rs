use super::{store::TaskStore, types::TaskStatus};

impl TaskStore {
    /// Return the earliest `created_at` (RFC-3339) of any task that has started
    /// (active/terminal status or turn > 0). Checks both the DB and the live cache.
    /// Returns `None` when no started task exists yet.
    pub async fn earliest_started_task_created_at(&self) -> Option<String> {
        let cache_min = self
            .cache
            .iter()
            .filter(|entry| {
                let v = entry.value();
                matches!(
                    v.status,
                    TaskStatus::Implementing
                        | TaskStatus::AgentReview
                        | TaskStatus::Waiting
                        | TaskStatus::Reviewing
                        | TaskStatus::Done
                        | TaskStatus::Failed
                ) || v.turn > 0
            })
            .filter_map(|entry| entry.value().created_at.clone())
            .min();
        let db_min = match self.db.earliest_started_created_at().await {
            Ok(min) => min,
            Err(e) => {
                tracing::warn!("failed to query earliest started task: {e}");
                None
            }
        };
        match (cache_min, db_min) {
            (Some(a), Some(b)) => Some(if a <= b { a } else { b }),
            (Some(a), None) | (None, Some(a)) => Some(a),
            (None, None) => None,
        }
    }
}
