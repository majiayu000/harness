use super::model::{RuntimeJob, RuntimeProfile, WorkflowCommandRecord};
use super::store::{RuntimeJobEnqueueOutcome, WorkflowRuntimeStore};
use anyhow::Context;
use chrono::{DateTime, Utc};
use serde_json::json;
use std::collections::BTreeMap;

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
    workflow_activity_profiles: BTreeMap<String, BTreeMap<String, RuntimeProfile>>,
}

impl RuntimeProfileSelector {
    pub fn new(default_profile: RuntimeProfile) -> Self {
        Self {
            default_profile,
            workflow_profiles: BTreeMap::new(),
            activity_profiles: BTreeMap::new(),
            workflow_activity_profiles: BTreeMap::new(),
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

    pub fn with_workflow_activity_profile(
        mut self,
        definition_id: impl Into<String>,
        activity: impl Into<String>,
        profile: RuntimeProfile,
    ) -> Self {
        self.workflow_activity_profiles
            .entry(definition_id.into())
            .or_default()
            .insert(activity.into(), profile);
        self
    }

    pub fn select(&self, definition_id: Option<&str>, activity: Option<&str>) -> &RuntimeProfile {
        definition_id
            .and_then(|id| {
                activity.and_then(|name| {
                    self.workflow_activity_profiles
                        .get(id)
                        .and_then(|profiles| profiles.get(name))
                })
            })
            .or_else(|| activity.and_then(|name| self.activity_profiles.get(name)))
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

    pub async fn dispatch_command(
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

        let activity = command.command.runtime_activity_key().to_string();
        let runtime_profile = self.profile_for_command(&command).await?;
        let not_before = retry_not_before_for_command(&command)?;
        match self
            .store
            .enqueue_runtime_job_for_pending_command(
                &command.id,
                runtime_profile.kind,
                &runtime_profile.name,
                json!({
                    "workflow_id": command.workflow_id.clone(),
                    "command_id": command.id.clone(),
                    "command_type": command.command.command_type,
                    "dedupe_key": command.command.dedupe_key.clone(),
                    "activity": activity,
                    "command": command.command.command.clone(),
                    "runtime_profile": runtime_profile.clone(),
                }),
                not_before,
            )
            .await?
        {
            RuntimeJobEnqueueOutcome::Enqueued(runtime_job) => {
                Ok(CommandDispatchOutcome::Enqueued {
                    command_id: command.id,
                    runtime_job,
                })
            }
            RuntimeJobEnqueueOutcome::AlreadyExists(runtime_job) => {
                Ok(CommandDispatchOutcome::AlreadyDispatched {
                    command_id: command.id,
                    runtime_job,
                })
            }
            RuntimeJobEnqueueOutcome::CommandNotPending { status } => {
                Ok(CommandDispatchOutcome::Skipped {
                    command_id: command.id,
                    reason: format!("command status changed to `{status}` before dispatch"),
                })
            }
        }
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
                Some(command.command.runtime_activity_key()),
            )
            .clone())
    }
}

fn retry_not_before_for_command(
    command: &WorkflowCommandRecord,
) -> anyhow::Result<Option<DateTime<Utc>>> {
    let Some(raw) = command
        .command
        .command
        .get("retry_not_before")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
    else {
        return Ok(None);
    };
    DateTime::parse_from_rfc3339(raw)
        .map(|value| Some(value.with_timezone(&Utc)))
        .with_context(|| {
            format!(
                "workflow command {} has invalid retry_not_before",
                command.id
            )
        })
}
