use super::model::{RuntimeJob, RuntimeProfile, WorkflowCommandRecord};
use super::store::WorkflowRuntimeStore;
use serde_json::json;

const COMMAND_STATUS_DISPATCHED: &str = "dispatched";
const COMMAND_STATUS_SKIPPED: &str = "skipped";

#[derive(Debug, Clone, PartialEq)]
pub enum CommandDispatchOutcome {
    Enqueued {
        command_id: String,
        runtime_job: RuntimeJob,
    },
    AlreadyDispatched {
        command_id: String,
        runtime_job: RuntimeJob,
    },
    Skipped {
        command_id: String,
        reason: String,
    },
}

pub struct RuntimeCommandDispatcher<'a> {
    store: &'a WorkflowRuntimeStore,
    runtime_profile: RuntimeProfile,
    batch_limit: i64,
}

impl<'a> RuntimeCommandDispatcher<'a> {
    pub fn new(store: &'a WorkflowRuntimeStore, runtime_profile: RuntimeProfile) -> Self {
        Self {
            store,
            runtime_profile,
            batch_limit: 25,
        }
    }

    pub fn with_batch_limit(mut self, batch_limit: i64) -> Self {
        self.batch_limit = batch_limit.max(1);
        self
    }

    pub async fn dispatch_once(&self) -> anyhow::Result<Option<CommandDispatchOutcome>> {
        let Some(command) = self.store.pending_commands(1).await?.into_iter().next() else {
            return Ok(None);
        };
        self.dispatch_command(command).await.map(Some)
    }

    pub async fn dispatch_pending(&self) -> anyhow::Result<Vec<CommandDispatchOutcome>> {
        let commands = self.store.pending_commands(self.batch_limit).await?;
        let mut outcomes = Vec::with_capacity(commands.len());
        for command in commands {
            outcomes.push(self.dispatch_command(command).await?);
        }
        Ok(outcomes)
    }

    async fn dispatch_command(
        &self,
        command: WorkflowCommandRecord,
    ) -> anyhow::Result<CommandDispatchOutcome> {
        if !command.command.requires_runtime_job() {
            self.store
                .mark_command_status(&command.id, COMMAND_STATUS_SKIPPED)
                .await?;
            return Ok(CommandDispatchOutcome::Skipped {
                command_id: command.id,
                reason: "command does not require runtime execution".to_string(),
            });
        }

        if let Some(runtime_job) = self
            .store
            .runtime_jobs_for_command(&command.id)
            .await?
            .into_iter()
            .next()
        {
            self.store
                .mark_command_status(&command.id, COMMAND_STATUS_DISPATCHED)
                .await?;
            return Ok(CommandDispatchOutcome::AlreadyDispatched {
                command_id: command.id,
                runtime_job,
            });
        }

        let runtime_job = self
            .store
            .enqueue_runtime_job(
                &command.id,
                self.runtime_profile.kind,
                &self.runtime_profile.name,
                json!({
                    "workflow_id": command.workflow_id.clone(),
                    "command_id": command.id.clone(),
                    "command_type": command.command.command_type,
                    "dedupe_key": command.command.dedupe_key.clone(),
                    "command": command.command.command.clone(),
                    "runtime_profile": self.runtime_profile.clone(),
                }),
            )
            .await?;
        self.store
            .mark_command_status(&command.id, COMMAND_STATUS_DISPATCHED)
            .await?;
        Ok(CommandDispatchOutcome::Enqueued {
            command_id: command.id,
            runtime_job,
        })
    }
}
