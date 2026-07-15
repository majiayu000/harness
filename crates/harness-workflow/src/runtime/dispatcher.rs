use super::model::{RuntimeJob, RuntimeProfile, WorkflowCommandRecord};
use super::status::WorkflowCommandStatus;
use super::store::{RuntimeJobEnqueueOutcome, WorkflowRuntimeStore};
use super::tier_resolution::{
    resolve_isolation_tier, IsolationTaskMetadata, IsolationTierResolution,
};
use anyhow::Context;
use chrono::{DateTime, Duration, Utc};
use harness_core::config::isolation::{
    IsolationAvailability, IsolationConfig, IsolationTrustClass,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use uuid::Uuid;

const COMMAND_STATUS_SKIPPED: WorkflowCommandStatus = WorkflowCommandStatus::Skipped;
const COMMAND_STATUS_CANCELLED: WorkflowCommandStatus = WorkflowCommandStatus::Cancelled;
const COMMAND_STATUS_FAILED: WorkflowCommandStatus = WorkflowCommandStatus::Failed;

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
    isolation_config: IsolationConfig,
    isolation_availability: IsolationAvailability,
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
            isolation_config: IsolationConfig::default(),
            isolation_availability: IsolationAvailability::default(),
            batch_limit: 25,
            dispatcher_id: format!("dispatcher:{}", Uuid::new_v4()),
            lease_duration: Duration::seconds(30),
        }
    }

    pub fn with_isolation_config(mut self, isolation_config: IsolationConfig) -> Self {
        self.isolation_config = isolation_config;
        self
    }

    pub fn with_isolation_availability(
        mut self,
        isolation_availability: IsolationAvailability,
    ) -> Self {
        self.isolation_availability = isolation_availability;
        self
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

        let instance = self.store.get_instance(&command.workflow_id).await?;
        if let Some(instance) = instance.as_ref() {
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
        let mut runtime_profile = self.profile_for_command(&command).await?;
        apply_candidate_runtime_budget(&mut runtime_profile, &command.command.command)?;
        let isolation =
            isolation_resolution_for_instance(instance.as_ref(), &self.isolation_config)
                .with_context(|| {
                    format!(
                        "failed to resolve isolation tier for workflow {}",
                        command.workflow_id
                    )
                })?;
        if let Err(error) = self
            .isolation_availability
            .ensure_tier_available(isolation.tier)
        {
            let reason = error.to_string();
            self.store
                .mark_command_status(&command.id, COMMAND_STATUS_FAILED)
                .await?;
            self.store
                .append_event(
                    &command.workflow_id,
                    "WorkflowRuntimeIsolationUnavailable",
                    "workflow_runtime_command_dispatcher",
                    json!({
                        "command_id": command.id.clone(),
                        "tier": isolation.tier,
                        "trust_class": isolation.trust_class,
                        "reason": reason.clone(),
                    }),
                )
                .await?;
            return Ok(CommandDispatchOutcome::Skipped {
                command_id: command.id,
                reason,
            });
        }
        let not_before = retry_not_before_for_command(&command)?;
        match self
            .store
            .enqueue_runtime_job_for_claimed_command(
                &command.id,
                super::DispatchClaim {
                    owner: &self.dispatcher_id,
                    generation: command.dispatch_claim_generation,
                },
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
                    "isolation": isolation,
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
            RuntimeJobEnqueueOutcome::StaleClaim => Ok(CommandDispatchOutcome::Skipped {
                command_id: command.id,
                reason: "dispatch claim became stale before enqueue".to_string(),
            }),
            RuntimeJobEnqueueOutcome::WorkflowTerminal { status } => {
                Ok(CommandDispatchOutcome::Skipped {
                    command_id: command.id,
                    reason: format!("workflow became terminal; command is `{status}`"),
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

fn apply_candidate_runtime_budget(
    runtime_profile: &mut RuntimeProfile,
    command_payload: &Value,
) -> anyhow::Result<()> {
    let Some(candidate) = command_payload.get("candidate") else {
        return Ok(());
    };
    let candidate_count = required_positive_u32(candidate, "candidate_count")?;
    let max_turns_per_candidate = optional_positive_u32(
        candidate.pointer("/budget/max_turns_per_candidate"),
        "candidate.budget.max_turns_per_candidate",
    )?;
    let max_turns = max_turns_per_candidate.or_else(|| {
        runtime_profile
            .max_turns
            .map(|max_turns| (max_turns / candidate_count).max(1))
    });
    if let Some(max_turns) = max_turns {
        runtime_profile.max_turns = Some(max_turns);
    }
    Ok(())
}

fn required_positive_u32(value: &Value, field: &str) -> anyhow::Result<u32> {
    let raw = value
        .get(field)
        .ok_or_else(|| anyhow::anyhow!("candidate metadata missing {field}"))?;
    positive_u32(raw, field)
}

fn optional_positive_u32(value: Option<&Value>, field: &str) -> anyhow::Result<Option<u32>> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    positive_u32(value, field).map(Some)
}

fn positive_u32(value: &Value, field: &str) -> anyhow::Result<u32> {
    let Some(raw) = value.as_u64() else {
        anyhow::bail!("{field} must be an unsigned integer");
    };
    if raw == 0 || raw > u64::from(u32::MAX) {
        anyhow::bail!("{field} must be between 1 and {}", u32::MAX);
    }
    Ok(raw as u32)
}

fn isolation_resolution_for_instance(
    instance: Option<&super::model::WorkflowInstance>,
    config: &IsolationConfig,
) -> anyhow::Result<IsolationTierResolution> {
    let metadata = match instance {
        Some(instance) => IsolationTaskMetadata {
            author_trust_class: author_trust_class_from_data(&instance.data)?,
        },
        None => IsolationTaskMetadata::default(),
    };
    Ok(resolve_isolation_tier(metadata, config))
}

fn author_trust_class_from_data(
    data: &serde_json::Value,
) -> anyhow::Result<Option<IsolationTrustClass>> {
    let Some(value) = data.get("author_trust_class") else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    serde_json::from_value(value.clone())
        .map(Some)
        .with_context(|| format!("invalid author_trust_class in workflow metadata: {value}"))
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
    use crate::runtime::RuntimeKind;

    #[test]
    fn profile_selector_uses_default_profile_without_activity_override() {
        let mut default_profile = RuntimeProfile::new("codex-default", RuntimeKind::CodexJsonrpc);
        default_profile.model = Some("gpt-5.5".to_string());
        default_profile.reasoning_effort = Some("high".to_string());

        let selector = RuntimeProfileSelector::new(default_profile);
        let profile = selector.select(Some("github_issue_pr"), Some("implement_issue"));

        assert_eq!(profile.kind, RuntimeKind::CodexJsonrpc);
        assert_eq!(profile.name, "codex-default");
        assert_eq!(profile.model.as_deref(), Some("gpt-5.5"));
        assert_eq!(profile.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(profile.timeout_secs, None);
    }

    #[test]
    fn candidate_fanout_budget_splits_runtime_profile_max_turns() -> anyhow::Result<()> {
        let mut profile = RuntimeProfile::new("codex-default", RuntimeKind::CodexJsonrpc);
        profile.max_turns = Some(9);
        let payload = json!({
            "candidate": {
                "candidate_count": 3,
                "budget": {
                    "max_turns_per_candidate": null,
                },
            },
        });

        apply_candidate_runtime_budget(&mut profile, &payload)?;

        assert_eq!(profile.max_turns, Some(3));
        Ok(())
    }

    #[test]
    fn candidate_fanout_budget_override_wins_over_split() -> anyhow::Result<()> {
        let mut profile = RuntimeProfile::new("codex-default", RuntimeKind::CodexJsonrpc);
        profile.max_turns = Some(9);
        let payload = json!({
            "candidate": {
                "candidate_count": 3,
                "budget": {
                    "max_turns_per_candidate": 5,
                },
            },
        });

        apply_candidate_runtime_budget(&mut profile, &payload)?;

        assert_eq!(profile.max_turns, Some(5));
        Ok(())
    }

    #[test]
    fn profile_selector_allows_explicit_activity_override() {
        let default_profile = RuntimeProfile::new("codex-default", RuntimeKind::CodexJsonrpc);
        let mut override_profile = RuntimeProfile::new("custom-feedback", RuntimeKind::ClaudeCode);
        override_profile.timeout_secs = Some(7200);

        let selector = RuntimeProfileSelector::new(default_profile)
            .with_activity_profile("address_pr_feedback", override_profile);
        let profile = selector.select(Some("github_issue_pr"), Some("address_pr_feedback"));

        assert_eq!(profile.kind, RuntimeKind::ClaudeCode);
        assert_eq!(profile.name, "custom-feedback");
        assert_eq!(profile.timeout_secs, Some(7200));
    }
}
