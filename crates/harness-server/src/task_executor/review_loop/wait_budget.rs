use crate::task_runner::{mutate_and_persist, TaskId, TaskStatus, TaskStore};
use tokio::time::Instant;

pub(super) struct ReviewWaitBudget {
    started_at: Instant,
    budget_secs: u64,
}

impl ReviewWaitBudget {
    pub(super) fn new(started_at: Instant, budget_secs: u64) -> Self {
        Self {
            started_at,
            budget_secs,
        }
    }

    pub(super) async fn fail_if_exceeded(
        &self,
        store: &TaskStore,
        task_id: &TaskId,
        round: u32,
    ) -> anyhow::Result<bool> {
        let elapsed_secs = self.started_at.elapsed().as_secs();
        if self.budget_secs == 0 {
            return Ok(false);
        }
        if elapsed_secs < self.budget_secs {
            return Ok(false);
        }

        let message = format!(
            "review wait budget exceeded: elapsed {elapsed_secs}s exceeded configured budget {}s",
            self.budget_secs
        );
        mutate_and_persist(store, task_id, |s| {
            s.status = TaskStatus::Failed;
            s.turn = round;
            s.error = Some(message.clone());
        })
        .await?;
        Ok(true)
    }
}
