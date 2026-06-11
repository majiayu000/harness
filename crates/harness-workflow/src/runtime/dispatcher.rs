use super::model::{RuntimeJob, RuntimeKind, RuntimeProfile, WorkflowCommandRecord};
use super::repo_backlog::REPO_BACKLOG_POLL_ACTIVITY;
use super::status::WorkflowCommandStatus;
use super::store::{RuntimeJobEnqueueOutcome, WorkflowRuntimeStore};
use anyhow::Context;
use chrono::{DateTime, Duration, Utc};
use serde_json::json;
use std::collections::BTreeMap;
use uuid::Uuid;

const COMMAND_STATUS_SKIPPED: WorkflowCommandStatus = WorkflowCommandStatus::Skipped;
const COMMAND_STATUS_CANCELLED: WorkflowCommandStatus = WorkflowCommandStatus::Cancelled;
const DEFAULT_REPO_BACKLOG_POLL_RUNTIME_PROFILE: &str = "codex-backlog-exec";
const DEFAULT_REPO_BACKLOG_POLL_TIMEOUT_SECS: u64 = 3600;

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
        let activity_profiles = Self::default_activity_profiles(&default_profile);
        Self {
            default_profile,
            workflow_profiles: BTreeMap::new(),
            activity_profiles,
            workflow_activity_profiles: BTreeMap::new(),
        }
    }

    fn default_activity_profiles(
        default_profile: &RuntimeProfile,
    ) -> BTreeMap<String, RuntimeProfile> {
        let mut activity_profiles = BTreeMap::new();
        activity_profiles.insert(
            REPO_BACKLOG_POLL_ACTIVITY.to_string(),
            default_repo_backlog_poll_runtime_profile(default_profile),
        );
        activity_profiles
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

fn default_repo_backlog_poll_runtime_profile(default_profile: &RuntimeProfile) -> RuntimeProfile {
    let mut profile = RuntimeProfile::new(
        DEFAULT_REPO_BACKLOG_POLL_RUNTIME_PROFILE,
        RuntimeKind::CodexExec,
    );
    if matches!(
        default_profile.kind,
        RuntimeKind::CodexExec | RuntimeKind::CodexJsonrpc
    ) {
        profile.model = default_profile.model.clone();
        profile.reasoning_effort = default_profile.reasoning_effort.clone();
        profile.approval_policy = default_profile.approval_policy.clone();
    }
    profile.sandbox = default_profile.sandbox.clone();
    profile.max_turns = default_profile.max_turns;
    profile.timeout_secs = Some(DEFAULT_REPO_BACKLOG_POLL_TIMEOUT_SECS);
    profile
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
    dispatcher_id: String,
    lease_duration: Duration,
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
            dispatcher_id: format!("dispatcher:{}", Uuid::new_v4()),
            lease_duration: Duration::seconds(30),
        }
    }

    pub fn with_batch_limit(mut self, batch_limit: i64) -> Self {
        self.batch_limit = batch_limit.max(1);
        self
    }

    pub fn with_dispatcher_id(mut self, dispatcher_id: impl Into<String>) -> Self {
        self.dispatcher_id = dispatcher_id.into();
        self
    }

    pub fn with_lease_duration(mut self, lease_duration: Duration) -> Self {
        self.lease_duration = lease_duration.max(Duration::seconds(1));
        self
    }

    pub async fn dispatch_once(&self) -> anyhow::Result<Option<CommandDispatchOutcome>> {
        let Some(command) = self
            .store
            .claim_pending_commands(&self.dispatcher_id, Utc::now() + self.lease_duration, 1)
            .await?
            .into_iter()
            .next()
        else {
            return Ok(None);
        };
        self.dispatch_command(command).await.map(Some)
    }

    pub async fn dispatch_pending(&self) -> anyhow::Result<Vec<CommandDispatchOutcome>> {
        let commands = self
            .store
            .claim_pending_commands(
                &self.dispatcher_id,
                Utc::now() + self.lease_duration,
                self.batch_limit,
            )
            .await?;
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

        if let Some(instance) = self.store.get_instance(&command.workflow_id).await? {
            if instance.is_terminal() {
                let command_status = if instance.state == "cancelled" {
                    COMMAND_STATUS_CANCELLED
                } else {
                    COMMAND_STATUS_SKIPPED
                };
                self.store
                    .mark_command_status(&command.id, command_status)
                    .await?;
                return Ok(CommandDispatchOutcome::Skipped {
                    command_id: command.id,
                    reason: format!(
                        "workflow {} is terminal ({}) before dispatch",
                        instance.id, instance.state
                    ),
                });
            }
        }

        let activity = command.command.runtime_activity_key().to_string();
        let runtime_profile = self.profile_for_command(&command).await?;
        let not_before = retry_not_before_for_command(&command)?;
        match self
            .store
            .enqueue_runtime_job_for_claimed_command(
                &command.id,
                &self.dispatcher_id,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_selector_defaults_repo_backlog_poll_to_codex_exec() {
        let mut default_profile = RuntimeProfile::new("codex-default", RuntimeKind::CodexJsonrpc);
        default_profile.model = Some("gpt-5.5".to_string());
        default_profile.reasoning_effort = Some("high".to_string());

        let selector = RuntimeProfileSelector::new(default_profile);
        let profile = selector.select(Some("repo_backlog"), Some(REPO_BACKLOG_POLL_ACTIVITY));

        assert_eq!(profile.kind, RuntimeKind::CodexExec);
        assert_eq!(profile.name, DEFAULT_REPO_BACKLOG_POLL_RUNTIME_PROFILE);
        assert_eq!(profile.model.as_deref(), Some("gpt-5.5"));
        assert_eq!(profile.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(
            profile.timeout_secs,
            Some(DEFAULT_REPO_BACKLOG_POLL_TIMEOUT_SECS)
        );
    }

    #[test]
    fn profile_selector_allows_explicit_repo_backlog_poll_override() {
        let default_profile = RuntimeProfile::new("codex-default", RuntimeKind::CodexJsonrpc);
        let mut override_profile = RuntimeProfile::new("custom-backlog", RuntimeKind::ClaudeCode);
        override_profile.timeout_secs = Some(7200);

        let selector = RuntimeProfileSelector::new(default_profile)
            .with_activity_profile(REPO_BACKLOG_POLL_ACTIVITY, override_profile);
        let profile = selector.select(Some("repo_backlog"), Some(REPO_BACKLOG_POLL_ACTIVITY));

        assert_eq!(profile.kind, RuntimeKind::ClaudeCode);
        assert_eq!(profile.name, "custom-backlog");
        assert_eq!(profile.timeout_secs, Some(7200));
    }
}
