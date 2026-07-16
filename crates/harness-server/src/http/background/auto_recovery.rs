//! Opt-in auto-recovery recheck scheduler for stopped workflow-runtime
//! instances (GH-1584).
//!
//! Every tick scans `blocked` / `failed` GitHub issue PR workflow instances
//! for repos that opted into `[intake.github.auto_recovery]`, classifies the
//! active stop reason through the single classifier (B-001), and re-runs the
//! exact manual recovery path (`recover_stopped_instance`) for transient
//! stops within bounded, backoff-scheduled attempts. Terminal stops are never
//! touched (B-012); with the global flag off the task is not even spawned and
//! runtime behavior is identical to GH-1567 semantics (B-003).

use super::AppState;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use harness_core::config::intake::{GitHubAutoRecoveryConfig, GitHubIntakeConfig};
use harness_workflow::runtime::{
    classify_stop, ActivityErrorKind, StopReasonClass, WorkflowInstance,
    WorkflowRuntimeRecoveryAction, WorkflowRuntimeRecoveryOutcome, WorkflowRuntimeRecoveryRequest,
    WorkflowRuntimeStore, GITHUB_ISSUE_PR_DEFINITION_ID,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;

/// Actor recorded on audit events and recovery decisions for automated
/// recoveries; manual operator actions keep recording `operator`.
pub(crate) const AUTO_RECOVERY_ACTOR: &str = "auto_recovery";
/// Appended BEFORE the recovery action is taken (B-008).
pub(crate) const AUTO_RECOVERY_ATTEMPT_EVENT: &str = "AutoRecoveryAttempt";
/// Appended once the attempt outcome is known: `succeeded`, `superseded`,
/// `recheck_failed`, or `interrupted`.
pub(crate) const AUTO_RECOVERY_OUTCOME_EVENT: &str = "AutoRecoveryOutcome";
/// Emitted exactly once per stop episode when attempts are exhausted (B-010).
/// Keep the event type name stable: GH-1582 alerting consumes it.
pub(crate) const AUTO_RECOVERY_EXHAUSTED_EVENT: &str = "AutoRecoveryExhausted";
const AUTO_RECOVERY_EVENT_SOURCE: &str = "workflow_runtime_auto_recovery";
const AUTO_RECOVERY_SCAN_LIMIT: i64 = 500;
const STOPPED_STATES: &[&str] = &["blocked", "failed"];

/// Attempt state persisted under `auto_recovery` in workflow instance data.
/// Additive JSON; survives restart (B-006, B-007). `episode_event_id` ties
/// the counter to the current stop episode (`last_stop.event_id`); a fresh
/// episode resets it (B-011) and successful recovery clears the whole object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct AutoRecoveryState {
    pub(crate) episode_event_id: String,
    #[serde(default)]
    pub(crate) attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) next_attempt_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) last_outcome: Option<String>,
    /// Audit event id of an attempt whose outcome was never recorded (crash
    /// between audit and outcome). Reconciled as `interrupted` next tick.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) pending_attempt_event_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) pending_attempt_number: Option<u32>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) exhausted: bool,
}

impl AutoRecoveryState {
    fn new_episode(episode_event_id: &str) -> Self {
        Self {
            episode_event_id: episode_event_id.to_string(),
            attempts: 0,
            next_attempt_at: None,
            last_outcome: None,
            pending_attempt_event_id: None,
            pending_attempt_number: None,
            exhausted: false,
        }
    }

    fn to_value(&self) -> anyhow::Result<Value> {
        Ok(serde_json::to_value(self)?)
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AutoRecoveryTick {
    pub(crate) selected: usize,
    pub(crate) recovered: usize,
    pub(crate) superseded: usize,
    pub(crate) recheck_failed: usize,
    pub(crate) interrupted: usize,
    pub(crate) exhausted_emitted: usize,
    pub(crate) errored: usize,
}

impl AutoRecoveryTick {
    fn touched_anything(&self) -> bool {
        self.selected > 0 || self.exhausted_emitted > 0 || self.errored > 0
    }
}

/// Spawn the recheck scheduler. With the global policy disabled (the
/// default) the background task is not spawned at all (B-003).
pub(in crate::http) fn spawn_auto_recovery(state: &Arc<AppState>) {
    let Some(github) = state.core.server.config.intake.github.clone() else {
        return;
    };
    if !github.auto_recovery.enabled {
        tracing::debug!(
            "workflow runtime auto-recovery globally disabled; recheck scheduler not spawned"
        );
        return;
    }
    let tick_interval = std::time::Duration::from_secs(github.auto_recovery.tick_interval_secs);
    let state = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tick_interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            let Some(store) = state.core.workflow_runtime_store.as_ref() else {
                continue;
            };
            let alerts = &state.observability.alerts;
            match run_auto_recovery_tick(store, &github, alerts, Utc::now()).await {
                Ok(tick) if tick.touched_anything() => {
                    tracing::info!(?tick, "workflow runtime auto-recovery tick");
                }
                Ok(_) => {}
                Err(error) => {
                    // Store unavailable or scan failure: nothing was consumed;
                    // the next tick retries the scan.
                    tracing::error!("workflow runtime auto-recovery tick failed: {error}");
                }
            }
        }
    });
}

/// One scheduler pass. Separated from the spawn loop so tests can drive
/// ticks against a store and config directly with a controlled clock.
pub(crate) async fn run_auto_recovery_tick(
    store: &WorkflowRuntimeStore,
    github: &GitHubIntakeConfig,
    alerts: &crate::alerting::AlertHandle,
    now: DateTime<Utc>,
) -> anyhow::Result<AutoRecoveryTick> {
    let mut tick = AutoRecoveryTick::default();
    // Defense in depth for B-003: the spawn gate already keeps the task from
    // starting when globally disabled; a tick with a disabled policy is inert.
    if !github.auto_recovery.enabled {
        return Ok(tick);
    }
    // Eligibility is pushed into the store query (opted-in repos, transient
    // classification, non-exhausted episode) so a large backlog of ineligible
    // stopped rows cannot occupy the bounded scan window and starve newer
    // eligible instances. `process_instance` re-checks everything fail-closed.
    let opted_in_repos: Vec<String> = github
        .effective_repos()
        .into_iter()
        .map(|repo| repo.repo)
        .filter(|repo| github.auto_recovery_enabled_for_repo(repo))
        .collect();
    if opted_in_repos.is_empty() {
        return Ok(tick);
    }
    for state in STOPPED_STATES {
        let instances = store
            .list_transient_stopped_candidates(
                GITHUB_ISSUE_PR_DEFINITION_ID,
                state,
                &opted_in_repos,
                AUTO_RECOVERY_SCAN_LIMIT,
            )
            .await?;
        for instance in instances {
            match process_instance(store, github, alerts, &instance, now).await {
                Ok(outcome) => outcome.record(&mut tick),
                Err(error) => {
                    tick.errored += 1;
                    tracing::error!(
                        workflow_id = %instance.id,
                        "workflow runtime auto-recovery attempt failed: {error}"
                    );
                }
            }
        }
    }
    Ok(tick)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstanceOutcome {
    Skipped,
    Recovered,
    Superseded,
    RecheckFailed,
    Interrupted,
    ExhaustedEmitted,
}

impl InstanceOutcome {
    fn record(self, tick: &mut AutoRecoveryTick) {
        match self {
            Self::Skipped => return,
            Self::Recovered => tick.recovered += 1,
            Self::Superseded => tick.superseded += 1,
            Self::RecheckFailed => tick.recheck_failed += 1,
            Self::Interrupted => tick.interrupted += 1,
            Self::ExhaustedEmitted => {
                tick.exhausted_emitted += 1;
                return;
            }
        }
        tick.selected += 1;
    }
}

async fn process_instance(
    store: &WorkflowRuntimeStore,
    github: &GitHubIntakeConfig,
    alerts: &crate::alerting::AlertHandle,
    instance: &WorkflowInstance,
    now: DateTime<Utc>,
) -> anyhow::Result<InstanceOutcome> {
    if !supports_auto_recovery(instance) {
        return Ok(InstanceOutcome::Skipped);
    }
    let policy = &github.auto_recovery;
    let Some(repo) = data_string(&instance.data, "repo") else {
        return Ok(InstanceOutcome::Skipped);
    };
    if !github.auto_recovery_enabled_for_repo(&repo) {
        return Ok(InstanceOutcome::Skipped);
    }
    let stop_reason_code = stop_reason_code(&instance.data);
    let error_kind = stop_error_kind(&instance.data);
    // B-002 / B-004 / B-012 / B-014: only transient-classified stops are ever
    // scheduled; unknown or legacy metadata fails closed to Terminal.
    if classify_stop(stop_reason_code.as_deref(), error_kind) != StopReasonClass::Transient {
        return Ok(InstanceOutcome::Skipped);
    }
    // The episode id anchors the attempt counter (B-011). Without structured
    // stop metadata there is no episode to recover: fail closed.
    let Some(episode_event_id) = data_string_at(&instance.data, "/last_stop/event_id") else {
        return Ok(InstanceOutcome::Skipped);
    };

    let mut attempt_state = instance
        .data
        .get("auto_recovery")
        .and_then(|value| serde_json::from_value::<AutoRecoveryState>(value.clone()).ok())
        .filter(|state| state.episode_event_id == episode_event_id)
        .unwrap_or_else(|| AutoRecoveryState::new_episode(&episode_event_id));

    if attempt_state.pending_attempt_event_id.is_some() {
        return reconcile_interrupted_attempt(store, policy, alerts, instance, attempt_state, now)
            .await;
    }
    if attempt_state.exhausted {
        return Ok(InstanceOutcome::Skipped);
    }
    if attempt_state.attempts >= policy.max_attempts {
        // Restart safety: attempts were exhausted but the exhaustion event
        // was never emitted (e.g. crash). Emit it exactly once (B-010).
        emit_exhausted_once(store, alerts, instance, &mut attempt_state, None).await?;
        return Ok(InstanceOutcome::ExhaustedEmitted);
    }
    if attempt_state
        .next_attempt_at
        .is_some_and(|next_attempt_at| next_attempt_at > now)
    {
        return Ok(InstanceOutcome::Skipped);
    }

    run_recovery_attempt(
        store,
        policy,
        alerts,
        instance,
        attempt_state,
        stop_reason_code.as_deref(),
    )
    .await
}

fn supports_auto_recovery(instance: &WorkflowInstance) -> bool {
    instance.definition_id == GITHUB_ISSUE_PR_DEFINITION_ID
}

/// A prior attempt crashed between its audit event and its outcome event.
/// Reconcile it as `interrupted` and consume the attempt so a crash can
/// neither orphan the audit trail nor grant free retries (B-006, B-008).
async fn reconcile_interrupted_attempt(
    store: &WorkflowRuntimeStore,
    policy: &GitHubAutoRecoveryConfig,
    alerts: &crate::alerting::AlertHandle,
    instance: &WorkflowInstance,
    mut attempt_state: AutoRecoveryState,
    now: DateTime<Utc>,
) -> anyhow::Result<InstanceOutcome> {
    let attempt_number = attempt_state
        .pending_attempt_number
        .unwrap_or_else(|| attempt_state.attempts.saturating_add(1));
    let attempt_event_id = attempt_state.pending_attempt_event_id.take();
    attempt_state.pending_attempt_number = None;
    attempt_state.attempts = attempt_state.attempts.max(attempt_number);
    attempt_state.last_outcome = Some("interrupted".to_string());
    attempt_state.next_attempt_at = Some(next_attempt_at(policy, &attempt_state, instance, now));
    store
        .append_event(
            &instance.id,
            AUTO_RECOVERY_OUTCOME_EVENT,
            AUTO_RECOVERY_EVENT_SOURCE,
            json!({
                "outcome": "interrupted",
                "attempt": attempt_number,
                "max_attempts": policy.max_attempts,
                "episode_event_id": attempt_state.episode_event_id,
                "attempt_event_id": attempt_event_id,
                "actor": AUTO_RECOVERY_ACTOR,
            }),
        )
        .await?;
    if attempt_state.attempts >= policy.max_attempts {
        // `emit_exhausted_once` persists the final attempt state itself; a
        // second write here would be redundant.
        emit_exhausted_once(store, alerts, instance, &mut attempt_state, None).await?;
    } else {
        store
            .set_auto_recovery_state_if_state(
                &instance.id,
                &instance.state,
                Some(&attempt_state.to_value()?),
            )
            .await?;
    }
    Ok(InstanceOutcome::Interrupted)
}

async fn run_recovery_attempt(
    store: &WorkflowRuntimeStore,
    policy: &GitHubAutoRecoveryConfig,
    alerts: &crate::alerting::AlertHandle,
    instance: &WorkflowInstance,
    mut attempt_state: AutoRecoveryState,
    stop_reason_code: Option<&str>,
) -> anyhow::Result<InstanceOutcome> {
    let action = match instance.state.as_str() {
        "blocked" => WorkflowRuntimeRecoveryAction::Unblock,
        "failed" => WorkflowRuntimeRecoveryAction::Retry,
        _ => return Ok(InstanceOutcome::Skipped),
    };
    let attempt_number = attempt_state.attempts.saturating_add(1);

    // B-008: the audit event is appended BEFORE the recovery action.
    let attempt_event = store
        .append_event(
            &instance.id,
            AUTO_RECOVERY_ATTEMPT_EVENT,
            AUTO_RECOVERY_EVENT_SOURCE,
            json!({
                "action": action.as_str(),
                "attempt": attempt_number,
                "max_attempts": policy.max_attempts,
                "reason_class": StopReasonClass::Transient.as_str(),
                "stop_reason_code": stop_reason_code,
                "episode_event_id": attempt_state.episode_event_id,
                "actor": AUTO_RECOVERY_ACTOR,
            }),
        )
        .await?;
    attempt_state.pending_attempt_event_id = Some(attempt_event.id.clone());
    attempt_state.pending_attempt_number = Some(attempt_number);
    let marked = store
        .set_auto_recovery_state_if_state(
            &instance.id,
            &instance.state,
            Some(&attempt_state.to_value()?),
        )
        .await?;
    if !marked {
        // The instance changed state between listing and the attempt (e.g. a
        // concurrent manual unblock): record `superseded`, consume nothing,
        // and stop scheduling this episode (B-009).
        append_outcome_event(
            store,
            instance,
            policy,
            &attempt_state,
            attempt_number,
            "superseded",
            Some("instance state changed before the recheck started"),
        )
        .await?;
        return Ok(InstanceOutcome::Superseded);
    }

    let reason = format!(
        "auto-recovery attempt {attempt_number}/{}: transient stop ({}) recheck",
        policy.max_attempts,
        stop_reason_code.unwrap_or("error_kind"),
    );
    let outcome = store
        .recover_stopped_instance(WorkflowRuntimeRecoveryRequest {
            workflow_id: &instance.id,
            action,
            reason: &reason,
            actor: AUTO_RECOVERY_ACTOR,
            target_state: None,
            evidence: &[],
        })
        .await;

    // Store errors after the pending marker was written intentionally leave
    // the marker in place: the next tick reconciles it as `interrupted` and
    // consumes the attempt, so crashes never grant free retries.
    let outcome = outcome?;
    match outcome {
        WorkflowRuntimeRecoveryOutcome::Recovered { .. } => {
            // `recover_stopped_instance` cleared the `auto_recovery` object
            // together with the state transition (same path as manual
            // recovery, B-005).
            append_outcome_event(
                store,
                instance,
                policy,
                &attempt_state,
                attempt_number,
                "succeeded",
                None,
            )
            .await?;
            Ok(InstanceOutcome::Recovered)
        }
        WorkflowRuntimeRecoveryOutcome::WrongState { .. }
        | WorkflowRuntimeRecoveryOutcome::NotFound => {
            // Lost the race with a concurrent transition (B-009): record
            // `superseded`, do not increment, stop scheduling. Clearing the
            // stale marker is best-effort — the guard fails when the state
            // moved on, and the leftover object is inert for this episode.
            append_outcome_event(
                store,
                instance,
                policy,
                &attempt_state,
                attempt_number,
                "superseded",
                Some("recovery observed a concurrent state change"),
            )
            .await?;
            attempt_state.pending_attempt_event_id = None;
            attempt_state.pending_attempt_number = None;
            store
                .set_auto_recovery_state_if_state(
                    &instance.id,
                    &instance.state,
                    Some(&attempt_state.to_value()?),
                )
                .await?;
            Ok(InstanceOutcome::Superseded)
        }
        WorkflowRuntimeRecoveryOutcome::NonRetryableFailure { error_kind, .. } => {
            record_terminal_recheck(
                store,
                policy,
                alerts,
                instance,
                attempt_state,
                attempt_number,
                &format!("failure is not retryable: {error_kind:?}"),
            )
            .await
        }
        WorkflowRuntimeRecoveryOutcome::UnsupportedStoppedActivity { activity, .. } => {
            record_terminal_recheck(
                store,
                policy,
                alerts,
                instance,
                attempt_state,
                attempt_number,
                &format!("no supported stopped activity: {activity:?}"),
            )
            .await
        }
        WorkflowRuntimeRecoveryOutcome::UnsupportedDefinition { .. } => {
            record_terminal_recheck(
                store,
                policy,
                alerts,
                instance,
                attempt_state,
                attempt_number,
                "workflow definition does not support runtime recovery",
            )
            .await
        }
        WorkflowRuntimeRecoveryOutcome::InvalidDefinitionPin { error, .. } => {
            record_terminal_recheck(
                store,
                policy,
                alerts,
                instance,
                attempt_state,
                attempt_number,
                &format!("declarative definition pin is invalid: {error:?}"),
            )
            .await
        }
        WorkflowRuntimeRecoveryOutcome::OperatorRequired { .. }
        | WorkflowRuntimeRecoveryOutcome::TargetRequired { .. }
        | WorkflowRuntimeRecoveryOutcome::TargetNotAllowed { .. }
        | WorkflowRuntimeRecoveryOutcome::MissingRequiredEvidence { .. } => {
            record_terminal_recheck(
                store,
                policy,
                alerts,
                instance,
                attempt_state,
                attempt_number,
                "declarative recovery requires an explicit operator-selected target",
            )
            .await
        }
    }
}

/// The recovery path rejected the recheck for a reason that cannot heal
/// within this stop episode (non-retryable error kind, unsupported stopped
/// activity or definition). Retrying would be futile: record the failed
/// attempt evidence and immediately mark the episode exhausted so the
/// scheduler stops touching the instance; only a manual operator action or a
/// fresh stop episode re-enables it. `emit_exhausted_once` persists the
/// final attempt state, so no additional write is needed.
async fn record_terminal_recheck(
    store: &WorkflowRuntimeStore,
    policy: &GitHubAutoRecoveryConfig,
    alerts: &crate::alerting::AlertHandle,
    instance: &WorkflowInstance,
    mut attempt_state: AutoRecoveryState,
    attempt_number: u32,
    detail: &str,
) -> anyhow::Result<InstanceOutcome> {
    attempt_state.pending_attempt_event_id = None;
    attempt_state.pending_attempt_number = None;
    attempt_state.attempts = attempt_number;
    attempt_state.last_outcome = Some("recheck_failed".to_string());
    attempt_state.next_attempt_at = None;
    append_outcome_event(
        store,
        instance,
        policy,
        &attempt_state,
        attempt_number,
        "recheck_failed",
        Some(detail),
    )
    .await?;
    emit_exhausted_once(store, alerts, instance, &mut attempt_state, Some(detail)).await?;
    Ok(InstanceOutcome::RecheckFailed)
}

async fn append_outcome_event(
    store: &WorkflowRuntimeStore,
    instance: &WorkflowInstance,
    policy: &GitHubAutoRecoveryConfig,
    attempt_state: &AutoRecoveryState,
    attempt_number: u32,
    outcome: &str,
    detail: Option<&str>,
) -> anyhow::Result<()> {
    store
        .append_event(
            &instance.id,
            AUTO_RECOVERY_OUTCOME_EVENT,
            AUTO_RECOVERY_EVENT_SOURCE,
            json!({
                "outcome": outcome,
                "detail": detail,
                "attempt": attempt_number,
                "max_attempts": policy.max_attempts,
                "episode_event_id": attempt_state.episode_event_id,
                "attempt_event_id": attempt_state.pending_attempt_event_id,
                "actor": AUTO_RECOVERY_ACTOR,
            }),
        )
        .await?;
    Ok(())
}

/// Append the `AutoRecoveryExhausted` escalation event exactly once per stop
/// episode (B-010). The instance stays stopped and keeps surfacing through
/// the operator monitor exactly like an unrecovered GH-1567 instance.
async fn emit_exhausted_once(
    store: &WorkflowRuntimeStore,
    alerts: &crate::alerting::AlertHandle,
    instance: &WorkflowInstance,
    attempt_state: &mut AutoRecoveryState,
    detail: Option<&str>,
) -> anyhow::Result<()> {
    if attempt_state.exhausted {
        return Ok(());
    }
    attempt_state.exhausted = true;
    store
        .append_event(
            &instance.id,
            AUTO_RECOVERY_EXHAUSTED_EVENT,
            AUTO_RECOVERY_EVENT_SOURCE,
            json!({
                "attempts": attempt_state.attempts,
                "episode_event_id": attempt_state.episode_event_id,
                "state": instance.state,
                "detail": detail,
                "actor": AUTO_RECOVERY_ACTOR,
            }),
        )
        .await?;
    // GH-1582 escalation: raise the external alert exactly once per episode,
    // gated on the same idempotency flag as the exhaustion event.
    let class = if instance.state == "failed" {
        harness_core::alert::AlertClass::WorkflowFailed
    } else {
        harness_core::alert::AlertClass::WorkflowBlocked
    };
    let stop_code = stop_reason_code(&instance.data);
    let reason = format!(
        "auto-recovery exhausted after {} attempts: {}",
        attempt_state.attempts,
        detail.or(stop_code.as_deref()).unwrap_or("transient stop"),
    );
    alerts.raise(crate::alerting::producers::workflow_stopped(
        class,
        &instance.id,
        instance
            .data
            .get("issue_number")
            .and_then(Value::as_u64)
            .map(|issue| issue.to_string())
            .as_deref(),
        data_string(&instance.data, "repo").as_deref(),
        &reason,
    ));
    store
        .set_auto_recovery_state_if_state(
            &instance.id,
            &instance.state,
            Some(&attempt_state.to_value()?),
        )
        .await?;
    Ok(())
}

/// Exponential backoff with deterministic jitter (B-007):
/// `min(max_backoff, initial * 2^(attempts - 1)) * (1 +/- jitter_ratio)`.
/// `attempts` is bounded by `max_attempts <= 16` at config load and the shift
/// saturates, so the computation cannot overflow.
fn next_attempt_at(
    policy: &GitHubAutoRecoveryConfig,
    attempt_state: &AutoRecoveryState,
    instance: &WorkflowInstance,
    now: DateTime<Utc>,
) -> DateTime<Utc> {
    let seed = format!(
        "{}:{}:{}",
        instance.id, attempt_state.episode_event_id, attempt_state.attempts
    );
    now + ChronoDuration::seconds(backoff_delay_secs(
        policy,
        attempt_state.attempts,
        jitter_factor(&seed, policy.jitter_ratio),
    ) as i64)
}

pub(crate) fn backoff_delay_secs(
    policy: &GitHubAutoRecoveryConfig,
    attempts: u32,
    jitter_factor: f64,
) -> u64 {
    let shift = attempts
        .saturating_sub(1)
        .min(harness_core::config::intake::AUTO_RECOVERY_MAX_ATTEMPTS_CEILING);
    let base = policy
        .initial_backoff_secs
        .saturating_mul(1u64 << shift)
        .min(policy.max_backoff_secs);
    (base as f64 * jitter_factor).round() as u64
}

/// Deterministic jitter in `[1 - ratio, 1 + ratio]` derived from an FNV-1a
/// hash of the seed: reproducible in tests, no extra dependency, and stable
/// across restarts for the same (workflow, episode, attempt) triple.
pub(crate) fn jitter_factor(seed: &str, ratio: f64) -> f64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in seed.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let unit = (hash >> 11) as f64 / (1u64 << 53) as f64;
    1.0 - ratio + 2.0 * ratio * unit
}

fn data_string(data: &Value, field: &str) -> Option<String> {
    data.get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn data_string_at(data: &Value, pointer: &str) -> Option<String> {
    data.pointer(pointer)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

/// The structured stop reason code of the active stop, read from instance
/// data (top level first, then `last_stop`). Never inferred (B-014).
fn stop_reason_code(data: &Value) -> Option<String> {
    data_string(data, "stop_reason_code")
        .or_else(|| data_string_at(data, "/last_stop/stop_reason_code"))
}

/// The `error_kind` of the active stop. A present-but-unparseable value is
/// mapped to `Unknown` so classification fails closed (B-002).
fn stop_error_kind(data: &Value) -> Option<ActivityErrorKind> {
    let raw = data
        .get("error_kind")
        .filter(|value| !value.is_null())
        .or_else(|| {
            data.pointer("/last_stop/error_kind")
                .filter(|value| !value.is_null())
        })?;
    Some(
        serde_json::from_value::<ActivityErrorKind>(raw.clone())
            .unwrap_or(ActivityErrorKind::Unknown),
    )
}

#[cfg(test)]
#[path = "auto_recovery_tests.rs"]
mod tests;
