use super::model::{RuntimeJob, RuntimeProfile, WorkflowCommandRecord};
use super::store::WorkflowRuntimeStore;
use serde_json::json;
use std::collections::BTreeMap;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeProfileSelector {
    default_profile: RuntimeProfile,
    workflow_profiles: BTreeMap<String, RuntimeProfile>,
    activity_profiles: BTreeMap<String, RuntimeProfile>,
}

impl RuntimeProfileSelector {
    pub fn new(default_profile: RuntimeProfile) -> Self {
        Self {
            default_profile,
            workflow_profiles: BTreeMap::new(),
            activity_profiles: BTreeMap::new(),
        }
    }

    pub fn with_workflow_profile(
        mut self,
        definition_id: impl Into<String>,
        profile: RuntimeProfile,
    ) -> Self {
        self.workflow_profiles.insert(definition_id.into(), profile);
        self
    }

    pub fn with_activity_profile(
        mut self,
        activity: impl Into<String>,
        profile: RuntimeProfile,
    ) -> Self {
        self.activity_profiles.insert(activity.into(), profile);
        self
    }

    pub fn select(&self, definition_id: Option<&str>, activity: Option<&str>) -> &RuntimeProfile {
        activity
            .and_then(|name| self.activity_profiles.get(name))
            .or_else(|| definition_id.and_then(|id| self.workflow_profiles.get(id)))
            .unwrap_or(&self.default_profile)
    }

    pub fn select_for_workflow(&self, definition_id: Option<&str>) -> &RuntimeProfile {
        definition_id
            .and_then(|id| self.workflow_profiles.get(id))
            .unwrap_or(&self.default_profile)
    }
}

impl From<RuntimeProfile> for RuntimeProfileSelector {
    fn from(default_profile: RuntimeProfile) -> Self {
        Self::new(default_profile)
    }
}

pub struct RuntimeCommandDispatcher<'a> {
    store: &'a WorkflowRuntimeStore,
    profile_selector: RuntimeProfileSelector,
    batch_limit: i64,
}

impl<'a> RuntimeCommandDispatcher<'a> {
    pub fn new(store: &'a WorkflowRuntimeStore, runtime_profile: RuntimeProfile) -> Self {
        Self::with_profile_selector(store, RuntimeProfileSelector::new(runtime_profile))
    }

    pub fn with_profile_selector(
        store: &'a WorkflowRuntimeStore,
        profile_selector: RuntimeProfileSelector,
    ) -> Self {
        Self {
            store,
            profile_selector,
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

        let runtime_profile = self.profile_for_command(&command).await?;
        let runtime_job = self
            .store
            .enqueue_runtime_job(
                &command.id,
                runtime_profile.kind,
                &runtime_profile.name,
                json!({
                    "workflow_id": command.workflow_id.clone(),
                    "command_id": command.id.clone(),
                    "command_type": command.command.command_type,
                    "dedupe_key": command.command.dedupe_key.clone(),
                    "command": command.command.command.clone(),
                    "runtime_profile": runtime_profile.clone(),
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

    async fn profile_for_command(
        &self,
        command: &WorkflowCommandRecord,
    ) -> anyhow::Result<RuntimeProfile> {
        let instance = self.store.get_instance(&command.workflow_id).await?;
        Ok(self
            .profile_selector
            .select(
                instance
                    .as_ref()
                    .map(|workflow| workflow.definition_id.as_str()),
                command.command.activity_name(),
            )
            .clone())
    }
}
