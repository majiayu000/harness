use super::dispatcher_throttle::{daily_throttle_breach, ThrottleBandRequest};
use super::model::{
    RuntimeJob, RuntimeKind, RuntimeProfile, WorkflowCommandRecord, WorkflowInstance,
};
use super::store::{
    cost_usd_from_micros, cost_usd_to_micros, ClaimedCommandTerminalOutcome,
    RuntimeJobEnqueueOutcome, WorkflowRuntimeStore,
};
use super::tier_resolution::{
    resolve_isolation_tier, IsolationTaskMetadata, IsolationTierResolution,
};
use super::{
    DeferClaimedCommandOutcome, DispatchBackoffPolicy, DispatchBarrier, DispatchBarrierInput,
    DispatchBarrierReasonCode,
};
use anyhow::Context;
use chrono::{DateTime, Duration, Utc};
use harness_core::config::isolation::{
    IsolationAvailability, IsolationConfig, IsolationTier, IsolationTrustClass,
};
use harness_core::config::workflow::{RuntimeBudgetEnforcement, RuntimeBudgetPolicy};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use uuid::Uuid;

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
    Deferred {
        command_id: String,
        barrier: DispatchBarrier,
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
    defer_backoff: DispatchBackoffPolicy,
    budget_policy: RuntimeBudgetPolicy,
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
            defer_backoff: DispatchBackoffPolicy::default(),
            budget_policy: RuntimeBudgetPolicy::default(),
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

    pub fn with_defer_backoff(mut self, defer_backoff: DispatchBackoffPolicy) -> Self {
        self.defer_backoff = defer_backoff;
        self
    }

    pub fn with_budget_policy(mut self, budget_policy: RuntimeBudgetPolicy) -> Self {
        self.budget_policy = budget_policy;
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
            let reason = if self
                .store
                .skip_claimed_command_if_owned(
                    &command.id,
                    super::DispatchClaim {
                        owner: &self.dispatcher_id,
                        generation: command.dispatch_claim_generation,
                    },
                )
                .await?
            {
                "command does not require runtime execution"
            } else {
                "dispatch claim became stale before non-runtime skip"
            };
            return Ok(CommandDispatchOutcome::Skipped {
                command_id: command.id,
                reason: reason.to_string(),
            });
        }

        match self
            .store
            .finish_claimed_command_for_terminal_workflow(
                &command.id,
                super::DispatchClaim {
                    owner: &self.dispatcher_id,
                    generation: command.dispatch_claim_generation,
                },
            )
            .await?
        {
            ClaimedCommandTerminalOutcome::NotTerminal => {}
            ClaimedCommandTerminalOutcome::StaleClaim => {
                return Ok(CommandDispatchOutcome::Skipped {
                    command_id: command.id,
                    reason: "dispatch claim became stale before terminal check".to_string(),
                });
            }
            ClaimedCommandTerminalOutcome::WorkflowTerminal {
                status,
                workflow_state,
            } => {
                return Ok(CommandDispatchOutcome::Skipped {
                    command_id: command.id,
                    reason: format!(
                        "workflow {} is terminal ({workflow_state}) before dispatch; command is `{status}`",
                        command.workflow_id
                    ),
                });
            }
        }
        let instance = self.store.get_instance(&command.workflow_id).await?;

        let activity = command.command.runtime_activity_key().to_string();
        let mut runtime_profile = self.profile_for_command(&command).await?;
        apply_eval_runtime_profile_policy(&mut runtime_profile, &command)?;
        apply_candidate_runtime_budget(&mut runtime_profile, &command.command.command)?;
        let isolation =
            isolation_resolution_for_command(instance.as_ref(), &command, &self.isolation_config)
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
            let project_id = command_project_id(instance.as_ref(), &command)?;
            let barrier = DispatchBarrierInput::new(
                DispatchBarrierReasonCode::IsolationTierUnavailable,
                reason,
                project_id,
            )
            .with_isolation(
                isolation.tier.as_str(),
                match isolation.trust_class {
                    IsolationTrustClass::Trusted => "trusted",
                    IsolationTrustClass::NonCollaborator => "non_collaborator",
                },
            );
            return dispatch_deferral_outcome(
                &command,
                self.store
                    .defer_claimed_command_if_owned(
                        &command.id,
                        &self.dispatcher_id,
                        command.dispatch_claim_generation,
                        barrier,
                        Utc::now(),
                        self.defer_backoff,
                    )
                    .await?,
            );
        }
        if let Some(outcome) = self
            .budget_gate_outcome(instance.as_ref(), &command, &runtime_profile.name)
            .await?
        {
            return Ok(outcome);
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

    /// Pre-dispatch budget gate (GH-1770). Returns `Some(outcome)` when the
    /// command was deferred; `None` when dispatch may proceed.
    ///
    /// Shadow enforcement records the would-block decision as a
    /// `BudgetShadowDecision` runtime event and lets the command through;
    /// enforce defers it with the `workflow_budget_exhausted` barrier.
    async fn budget_gate_outcome(
        &self,
        instance: Option<&WorkflowInstance>,
        command: &WorkflowCommandRecord,
        runtime_profile_name: &str,
    ) -> anyhow::Result<Option<CommandDispatchOutcome>> {
        if self.budget_policy.unlimited {
            return Ok(None);
        }
        // Compare in integer micro-dollars: spend is stored in micros, and a
        // float comparison could flip the gate at the boundary.
        let budget_usd = self.budget_policy.default_workflow_budget_usd;
        let budget_usd_micros = cost_usd_to_micros(budget_usd)?;
        let spent_usd_micros = self
            .store
            .runtime_usage_for_workflow(&command.workflow_id)
            .await?
            .map(|usage| usage.cost_usd_micros)
            .unwrap_or(0);
        if spent_usd_micros >= budget_usd_micros {
            let spent_usd = cost_usd_from_micros(spent_usd_micros);
            return self
                .budget_breach_outcome(
                    instance,
                    command,
                    DispatchBarrierReasonCode::WorkflowBudgetExhausted,
                    format!(
                        "workflow {} spent {spent_usd:.2} USD, reaching its {budget_usd:.2} USD budget",
                        command.workflow_id
                    ),
                    json!({
                        "spent_usd": spent_usd,
                        "budget_usd": budget_usd,
                    }),
                )
                .await;
        }
        if let Some(cap_usd) = self.budget_policy.daily_profile_cap_usd {
            let cap_usd_micros = cost_usd_to_micros(cap_usd)?;
            let utc_day_start = Utc::now()
                .date_naive()
                .and_hms_opt(0, 0, 0)
                .map(|day_start| day_start.and_utc())
                .context("failed to compute UTC day start for the daily profile cap")?;
            let profile_spent_micros = self
                .store
                .runtime_usage_cost_for_profile_since(runtime_profile_name, utc_day_start)
                .await?;
            if profile_spent_micros >= cap_usd_micros {
                let profile_spent_usd = cost_usd_from_micros(profile_spent_micros);
                return self
                    .budget_breach_outcome(
                        instance,
                        command,
                        DispatchBarrierReasonCode::ProfileDailyCapReached,
                        format!(
                            "runtime profile {runtime_profile_name} spent {profile_spent_usd:.2} USD \
                             today, reaching its {cap_usd:.2} USD daily cap",
                        ),
                        json!({
                            "runtime_profile": runtime_profile_name,
                            "profile_spent_usd_today": profile_spent_usd,
                            "daily_profile_cap_usd": cap_usd,
                        }),
                    )
                    .await;
            }
            // Throttle band (GH-1770 §4.1): deprioritize, do not block.
            if let Some(breach) = daily_throttle_breach(ThrottleBandRequest {
                store: self.store,
                profile_selector: &self.profile_selector,
                budget_policy: &self.budget_policy,
                runtime_profile_name,
                profile_spent_usd_micros: profile_spent_micros,
                cap_usd,
                utc_day_start,
                peek_limit: self.batch_limit,
            })
            .await?
            {
                return self
                    .budget_breach_outcome(
                        instance,
                        command,
                        DispatchBarrierReasonCode::ProfileDailyThrottled,
                        breach.reason,
                        breach.evidence,
                    )
                    .await;
            }
        }
        Ok(None)
    }

    /// Shared shadow/enforce disposition for a breached budget ceiling.
    /// Shadow records a `BudgetShadowDecision` runtime event and dispatches;
    /// enforce defers the command with the given barrier reason.
    async fn budget_breach_outcome(
        &self,
        instance: Option<&WorkflowInstance>,
        command: &WorkflowCommandRecord,
        reason_code: DispatchBarrierReasonCode,
        reason: String,
        mut evidence: Value,
    ) -> anyhow::Result<Option<CommandDispatchOutcome>> {
        match self.budget_policy.enforcement {
            RuntimeBudgetEnforcement::Shadow => {
                if let Some(evidence) = evidence.as_object_mut() {
                    evidence.insert("command_id".to_string(), json!(command.id));
                    evidence.insert("decision".to_string(), json!("would_defer"));
                    evidence.insert(
                        "barrier_reason_code".to_string(),
                        json!(reason_code.as_str()),
                    );
                }
                self.store
                    .append_event(
                        &command.workflow_id,
                        "BudgetShadowDecision",
                        "workflow_runtime_command_dispatcher",
                        evidence,
                    )
                    .await?;
                Ok(None)
            }
            RuntimeBudgetEnforcement::Enforce => {
                let project_id = command_project_id(instance, command)?;
                let barrier = DispatchBarrierInput::new(reason_code, reason, project_id);
                dispatch_deferral_outcome(
                    command,
                    self.store
                        .defer_claimed_command_if_owned(
                            &command.id,
                            &self.dispatcher_id,
                            command.dispatch_claim_generation,
                            barrier,
                            Utc::now(),
                            self.defer_backoff,
                        )
                        .await?,
                )
                .map(Some)
            }
        }
    }
}

fn command_project_id(
    instance: Option<&super::model::WorkflowInstance>,
    command: &WorkflowCommandRecord,
) -> anyhow::Result<String> {
    instance
        .and_then(|instance| instance.data.get("project_id"))
        .or_else(|| command.command.command.get("project_id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| anyhow::anyhow!("runtime dispatch barrier requires a project identity"))
}

fn dispatch_deferral_outcome(
    command: &WorkflowCommandRecord,
    outcome: DeferClaimedCommandOutcome,
) -> anyhow::Result<CommandDispatchOutcome> {
    Ok(match outcome {
        DeferClaimedCommandOutcome::Deferred(barrier)
        | DeferClaimedCommandOutcome::AlreadyDeferred(barrier) => {
            CommandDispatchOutcome::Deferred {
                command_id: command.id.clone(),
                barrier,
            }
        }
        DeferClaimedCommandOutcome::StaleClaim => CommandDispatchOutcome::Skipped {
            command_id: command.id.clone(),
            reason: "dispatch claim became stale before deferral".to_string(),
        },
        DeferClaimedCommandOutcome::WorkflowTerminal { status } => {
            CommandDispatchOutcome::Skipped {
                command_id: command.id.clone(),
                reason: format!("workflow became terminal; command is `{status}`"),
            }
        }
    })
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

fn isolation_resolution_for_command(
    instance: Option<&super::model::WorkflowInstance>,
    command: &WorkflowCommandRecord,
    config: &IsolationConfig,
) -> anyhow::Result<IsolationTierResolution> {
    if let Some(resolution) = eval_required_isolation_resolution(command)? {
        return Ok(resolution);
    }
    let metadata = match instance {
        Some(instance) => IsolationTaskMetadata {
            author_trust_class: author_trust_class_from_data(&instance.data)?,
        },
        None => IsolationTaskMetadata::default(),
    };
    Ok(resolve_isolation_tier(metadata, config))
}

fn eval_required_isolation_resolution(
    command: &WorkflowCommandRecord,
) -> anyhow::Result<Option<IsolationTierResolution>> {
    let Some(isolation) = command.command.command.pointer("/eval/isolation") else {
        return Ok(None);
    };
    let Some(raw_tier) = isolation.get("tier") else {
        anyhow::bail!("eval command {} is missing eval.isolation.tier", command.id);
    };
    let tier: IsolationTier = serde_json::from_value(raw_tier.clone()).with_context(|| {
        format!(
            "eval command {} has invalid eval.isolation.tier: {raw_tier}",
            command.id
        )
    })?;
    if tier == IsolationTier::Host {
        anyhow::bail!(
            "eval command {} requested host isolation, which is not valid for untrusted eval cases",
            command.id
        );
    }
    if tier == IsolationTier::Microvm {
        anyhow::bail!(
            "eval command {} requested microvm isolation, which is reserved but not implemented",
            command.id
        );
    }
    let network_allowlist = isolation
        .get("network_allowlist")
        .map(|value| {
            serde_json::from_value::<Vec<String>>(value.clone()).with_context(|| {
                format!(
                    "eval command {} has invalid eval.isolation.network_allowlist: {value}",
                    command.id
                )
            })
        })
        .transpose()?
        .unwrap_or_default();
    let network_policy = harness_sandbox::EvalNetworkPolicy::for_allowlist(&network_allowlist)
        .with_context(|| {
            format!(
                "eval command {} has invalid eval.isolation.network_allowlist",
                command.id
            )
        })?;

    Ok(Some(IsolationTierResolution {
        tier,
        reason: "eval command required container isolation tier from policy".to_string(),
        trust_class: IsolationTrustClass::NonCollaborator,
        network_allowlist: network_policy.network_allowlist,
    }))
}

fn apply_eval_runtime_profile_policy(
    profile: &mut RuntimeProfile,
    command: &WorkflowCommandRecord,
) -> anyhow::Result<()> {
    let Some(isolation) = command.command.command.pointer("/eval/isolation") else {
        return Ok(());
    };
    let runtime_kind = required_runtime_kind(isolation, "runtime_kind").with_context(|| {
        format!(
            "eval command {} has invalid eval isolation runtime profile policy",
            command.id
        )
    })?;
    if runtime_kind != RuntimeKind::RemoteHost {
        anyhow::bail!(
            "eval command {} requires runtime_kind remote_host, got `{}`",
            command.id,
            runtime_kind.as_str()
        );
    }
    let runtime_profile =
        required_string_field(isolation, "runtime_profile").with_context(|| {
            format!(
                "eval command {} has invalid eval isolation runtime profile policy",
                command.id
            )
        })?;
    let sandbox = required_string_field(isolation, "sandbox").with_context(|| {
        format!(
            "eval command {} has invalid eval isolation runtime profile policy",
            command.id
        )
    })?;
    if sandbox != "workspace-write" {
        anyhow::bail!(
            "eval command {} requires sandbox workspace-write, got `{sandbox}`",
            command.id
        );
    }
    profile.kind = runtime_kind;
    profile.name = runtime_profile.to_string();
    profile.model = None;
    profile.reasoning_effort = None;
    profile.sandbox = Some(sandbox.to_string());
    profile.approval_policy = None;
    profile.timeout_secs = eval_timeout_secs(command).or(profile.timeout_secs);
    Ok(())
}

fn required_runtime_kind(value: &Value, field: &str) -> anyhow::Result<RuntimeKind> {
    let Some(raw) = value.get(field) else {
        anyhow::bail!("{field} is required");
    };
    serde_json::from_value(raw.clone()).with_context(|| format!("{field} is invalid: {raw}"))
}

fn required_string_field<'a>(value: &'a Value, field: &str) -> anyhow::Result<&'a str> {
    let Some(raw) = value.get(field).and_then(Value::as_str).map(str::trim) else {
        anyhow::bail!("{field} is required");
    };
    if raw.is_empty() {
        anyhow::bail!("{field} must not be empty");
    }
    Ok(raw)
}

fn eval_timeout_secs(command: &WorkflowCommandRecord) -> Option<u64> {
    command
        .command
        .command
        .pointer("/eval/timeout_secs")
        .and_then(Value::as_u64)
        .filter(|timeout| *timeout > 0)
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
    use crate::runtime::{
        RuntimeKind, WorkflowCommand, WorkflowCommandStatus, WorkflowCommandType,
    };

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

    #[test]
    fn eval_isolation_command_policy_overrides_host_defaults() -> anyhow::Result<()> {
        let command = command_record(WorkflowCommand::new(
            WorkflowCommandType::EnqueueActivity,
            "eval-implement",
            json!({
                "activity": "implement_issue",
                "eval": {
                    "timeout_secs": 1800,
                    "isolation": {
                        "tier": "container",
                        "runtime_kind": "remote_host",
                        "runtime_profile": "eval-isolated-runtime-host",
                        "sandbox": "workspace-write",
                        "backend": "container_runtime_host",
                        "image": "harness-eval-runner:local",
                        "lifecycle": "ephemeral",
                        "cleanup_required": true
                    }
                }
            }),
        ));

        let resolution =
            isolation_resolution_for_command(None, &command, &IsolationConfig::default())?;

        assert_eq!(resolution.tier, IsolationTier::Container);
        assert_eq!(resolution.trust_class, IsolationTrustClass::NonCollaborator);
        assert!(resolution.reason.contains("eval command required"));
        Ok(())
    }

    #[test]
    fn eval_isolation_command_policy_normalizes_network_allowlist() -> anyhow::Result<()> {
        let command = command_record(WorkflowCommand::new(
            WorkflowCommandType::EnqueueActivity,
            "eval-implement",
            json!({
                "activity": "implement_issue",
                "eval": {
                    "timeout_secs": 1800,
                    "isolation": {
                        "tier": "container",
                        "runtime_kind": "remote_host",
                        "runtime_profile": "eval-isolated-runtime-host",
                        "sandbox": "workspace-write",
                        "backend": "container_runtime_host",
                        "image": "harness-eval-runner:local",
                        "lifecycle": "ephemeral",
                        "cleanup_required": true,
                        "network_allowlist": [" GitHub.COM. ", "api.github.com"]
                    }
                }
            }),
        ));

        let resolution =
            isolation_resolution_for_command(None, &command, &IsolationConfig::default())?;

        assert_eq!(
            resolution.network_allowlist,
            vec!["github.com".to_string(), "api.github.com".to_string()]
        );
        Ok(())
    }

    #[test]
    fn eval_isolation_command_policy_selects_remote_host_profile() -> anyhow::Result<()> {
        let command = command_record(WorkflowCommand::new(
            WorkflowCommandType::EnqueueActivity,
            "eval-implement",
            json!({
                "activity": "implement_issue",
                "eval": {
                    "timeout_secs": 1800,
                    "isolation": {
                        "tier": "container",
                        "runtime_kind": "remote_host",
                        "runtime_profile": "eval-isolated-runtime-host",
                        "sandbox": "workspace-write",
                        "backend": "container_runtime_host",
                        "image": "harness-eval-runner:local",
                        "lifecycle": "ephemeral",
                        "cleanup_required": true
                    }
                }
            }),
        ));
        let mut profile = RuntimeProfile::new("codex-default", RuntimeKind::CodexJsonrpc);

        apply_eval_runtime_profile_policy(&mut profile, &command)?;

        assert_eq!(profile.kind, RuntimeKind::RemoteHost);
        assert_eq!(profile.name, "eval-isolated-runtime-host");
        assert_eq!(profile.sandbox.as_deref(), Some("workspace-write"));
        assert_eq!(profile.timeout_secs, Some(1800));
        assert_eq!(profile.model, None);
        assert_eq!(profile.reasoning_effort, None);
        Ok(())
    }

    #[test]
    fn eval_isolation_command_policy_rejects_host_tier() {
        let command = command_record(WorkflowCommand::new(
            WorkflowCommandType::EnqueueActivity,
            "eval-implement",
            json!({
                "activity": "implement_issue",
                "eval": {
                    "isolation": {
                        "tier": "host"
                    }
                }
            }),
        ));

        let error = isolation_resolution_for_command(None, &command, &IsolationConfig::default())
            .expect_err("host eval isolation must fail");

        assert!(error.to_string().contains("requested host isolation"));
    }

    fn command_record(command: WorkflowCommand) -> WorkflowCommandRecord {
        WorkflowCommandRecord {
            id: "command-1".to_string(),
            workflow_id: "workflow-1".to_string(),
            decision_id: None,
            status: WorkflowCommandStatus::Pending,
            dispatch_owner: None,
            dispatch_lease_expires_at: None,
            dispatch_not_before: None,
            dispatch_attempt_count: 0,
            dispatch_claim_generation: 0,
            dispatch_barrier: None,
            command,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            attempt_generation: 1,
            superseded_by_command_id: None,
        }
    }
}
