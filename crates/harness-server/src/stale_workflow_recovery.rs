//! Stale workflow recovery — periodic tick that routes workflows stuck in
//! non-terminal, non-candidate states through an explicit recovery decision.
//!
//! The `repo_backlog` workflow type has known dead-end states:
//! `scanning`, `planning_batch`, `dispatching`, `reconciling`, `blocked`. The poller's
//! eligibility check is `state IN ('idle','failed')`, so once a workflow lands
//! in one of those states with no follow-up command (e.g. claude returned empty
//! output, server restart interrupted the dispatch path, wire-format drift
//! swallowed the next-step decision), it sits there forever and the corresponding
//! repo's GitHub issues stop being processed.
//!
//! This recovery tick is the operational backstop. It scans the runtime store
//! every `interval_secs` (default 600 = 10 min), finds `repo_backlog`
//! instances stuck in any of the dead-end states for longer than
//! `stale_after_secs` (default 1800 = 30 min), records recovery evidence, and
//! commits an accepted recovery decision to `idle` so the next poller tick will
//! re-enqueue a fresh `poll_repo_backlog` activity.
//!
//! This is a defensive recovery layer, not a correctness fix. The right long-
//! term answer is removing the `repo_backlog` state machine entirely (see
//! `docs/stateless-repo-backlog-poll-spec.md`). Until that lands, this tick
//! prevents single-incident state drift from snowballing into hours of zero
//! intake throughput.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use harness_workflow::runtime::{
    DecisionValidator, RuntimeJobStatus, ValidationContext, WorkflowCommand, WorkflowCommandRecord,
    WorkflowCommandStatus, WorkflowCommandType, WorkflowDecision, WorkflowDecisionRecord,
    WorkflowEvent, WorkflowEvidence, WorkflowInstance, WorkflowRuntimeStore,
    REPO_BACKLOG_POLL_ACTIVITY, REPO_BACKLOG_SPRINT_PLAN_ACTIVITY,
};
use serde::Serialize;
use serde_json::{json, Value};

use crate::http::AppState;

/// Workflow definition this tick recovers. Currently only `repo_backlog` —
/// other workflow types (e.g. `github_issue_pr`) have legitimate long-running
/// non-terminal states (`pr_open` waiting for human review, `awaiting_dependencies`
/// waiting on parent issue) and would be incorrectly reset.
const RECOVERED_DEFINITION_ID: &str = "repo_backlog";

/// States that are non-terminal AND non-candidate-for-poller. A workflow in
/// one of these states relies on the next reducer transition firing; if that
/// never happens, the workflow is stuck.
const STUCK_STATES: &[&str] = &[
    "scanning",
    "planning_batch",
    "dispatching",
    "reconciling",
    "blocked",
];

/// State to reset stuck workflows to. `idle` is in the poller's candidate set
/// (`idle | failed`), so the next poller tick will re-claim it and re-issue a
/// fresh `poll_repo_backlog` activity.
const RECOVERY_TARGET_STATE: &str = "idle";
const RECOVERY_EVENT_TYPE: &str = "RecoveryDetected";
const RECOVERY_EVENT_SOURCE: &str = "stale_workflow_recovery";
const RECOVERY_DECISION_NAME: &str = "recover_stale_repo_backlog_workflow";
const RECOVERY_REASON: &str = "stale repo_backlog recovery returned workflow to poller eligibility";
const LEGACY_REPO_BACKLOG_SPRINT_PLAN_ACTIVITY: &str = "plan_repo_sprint_from_scan";
const REPO_BACKLOG_MARK_BOUND_ISSUE_DONE_ACTIVITY: &str = "mark_bound_issue_done";
const REPO_BACKLOG_RECOVER_ISSUE_WORKFLOW_ACTIVITY: &str = "recover_issue_workflow";

/// Default seconds a workflow must sit in a stuck state before recovery kicks
/// in. 30 minutes is well above the longest legitimate `poll_repo_backlog`
/// turn (~5 min p99) but tight enough that operators don't wait hours for
/// throughput to recover.
const DEFAULT_STALE_AFTER_SECS: u64 = 1800;

/// Default tick cadence. Slow enough not to thrash if the recovery itself
/// takes a moment, fast enough that one stuck workflow blocks throughput
/// for at most one tick interval beyond the stale threshold.
const DEFAULT_INTERVAL_SECS: u64 = 600;

/// Per-tick scan limit. Caps DB load and warning-log spam on a still-broken
/// system. 50 is well above the realistic 8-repo deployment worst case.
const SCAN_LIMIT: i64 = 50;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct StaleRecoveryTick {
    pub scanned: usize,
    pub recovered: usize,
    pub failed: usize,
    pub skipped_active: usize,
}

impl StaleRecoveryTick {
    fn has_recovery_activity(&self) -> bool {
        self.scanned > 0 || self.skipped_active > 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecoveryCommitOutcome {
    Recovered { stuck_secs: i64 },
    SkippedChanged,
    SkippedActive { stuck_secs: i64 },
}

/// Run one recovery pass. Public for unit-testing.
pub async fn run_stale_workflow_recovery_tick(
    store: &WorkflowRuntimeStore,
    stale_after_secs: u64,
) -> anyhow::Result<StaleRecoveryTick> {
    let mut tick = StaleRecoveryTick::default();
    let cutoff = Utc::now() - chrono::Duration::seconds(stale_after_secs as i64);

    for state in STUCK_STATES {
        let candidates = store
            .list_instances_by_state(RECOVERED_DEFINITION_ID, state, SCAN_LIMIT)
            .await?;
        for instance in candidates {
            if instance.updated_at >= cutoff {
                continue;
            }
            tick.scanned += 1;
            let original_state = instance.state.clone();
            let stuck_secs = (Utc::now() - instance.updated_at).num_seconds();
            if workflow_has_active_repo_backlog_work(store, &instance.id).await? {
                tick.skipped_active += 1;
                tracing::debug!(
                    workflow_id = %instance.id,
                    state = %original_state,
                    stuck_secs,
                    "stale_workflow_recovery: skipped workflow with active repo backlog runtime work"
                );
                continue;
            }
            match commit_stale_recovery_decision(
                store,
                &instance.id,
                &original_state,
                stale_after_secs,
                cutoff,
            )
            .await
            {
                Ok(RecoveryCommitOutcome::Recovered { stuck_secs }) => {
                    tracing::warn!(
                        workflow_id = %instance.id,
                        from_state = %original_state,
                        to_state = %RECOVERY_TARGET_STATE,
                        stuck_secs,
                        "stale_workflow_recovery: reset workflow stuck in non-candidate state"
                    );
                    tick.recovered += 1;
                }
                Ok(RecoveryCommitOutcome::SkippedActive { stuck_secs }) => {
                    tick.skipped_active += 1;
                    tracing::debug!(
                        workflow_id = %instance.id,
                        state = %original_state,
                        stuck_secs,
                        "stale_workflow_recovery: skipped workflow with active repo backlog runtime work after lock"
                    );
                }
                Ok(RecoveryCommitOutcome::SkippedChanged) => {
                    tracing::debug!(
                        workflow_id = %instance.id,
                        from_state = %original_state,
                        "stale_workflow_recovery: skipped workflow changed before recovery commit"
                    );
                }
                Err(error) => {
                    tracing::warn!(
                        workflow_id = %instance.id,
                        from_state = %original_state,
                        stuck_secs,
                        "stale_workflow_recovery: atomic recovery write failed: {error}"
                    );
                    tick.failed += 1;
                }
            }
        }
    }
    Ok(tick)
}

async fn commit_stale_recovery_decision(
    store: &WorkflowRuntimeStore,
    workflow_id: &str,
    original_state: &str,
    stale_after_secs: u64,
    cutoff: chrono::DateTime<Utc>,
) -> anyhow::Result<RecoveryCommitOutcome> {
    commit_stale_recovery_decision_inner(
        store,
        workflow_id,
        original_state,
        stale_after_secs,
        cutoff,
        false,
        false,
    )
    .await
}

async fn commit_stale_recovery_decision_inner(
    store: &WorkflowRuntimeStore,
    workflow_id: &str,
    original_state: &str,
    stale_after_secs: u64,
    cutoff: chrono::DateTime<Utc>,
    force_rollback_after_event_insert: bool,
    force_rollback_after_decision_insert: bool,
) -> anyhow::Result<RecoveryCommitOutcome> {
    let mut tx = store.pool().begin().await?;
    let Some((current_data,)) = sqlx::query_as::<_, (String,)>(
        "SELECT data::text FROM workflow_instances WHERE id = $1 FOR UPDATE",
    )
    .bind(workflow_id)
    .fetch_optional(&mut *tx)
    .await?
    else {
        tx.commit().await?;
        return Ok(RecoveryCommitOutcome::SkippedChanged);
    };
    let mut instance: WorkflowInstance = serde_json::from_str(&current_data)?;
    if instance.definition_id != RECOVERED_DEFINITION_ID
        || instance.state != original_state
        || !STUCK_STATES.contains(&instance.state.as_str())
        || instance.updated_at >= cutoff
    {
        tx.commit().await?;
        return Ok(RecoveryCommitOutcome::SkippedChanged);
    }
    let stuck_secs = (Utc::now() - instance.updated_at).num_seconds();
    if workflow_has_active_repo_backlog_work_tx(&mut tx, workflow_id).await? {
        tx.commit().await?;
        return Ok(RecoveryCommitOutcome::SkippedActive { stuck_secs });
    }

    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("workflow_events:{workflow_id}"))
        .execute(&mut *tx)
        .await?;
    let (next_sequence,): (i64,) = sqlx::query_as(
        "SELECT COALESCE(MAX(sequence), 0) + 1 FROM workflow_events WHERE workflow_id = $1",
    )
    .bind(workflow_id)
    .fetch_one(&mut *tx)
    .await?;
    let payload = json!({
        "definition_id": RECOVERED_DEFINITION_ID,
        "from_state": original_state,
        "to_state": RECOVERY_TARGET_STATE,
        "decision": RECOVERY_DECISION_NAME,
        "stuck_secs": stuck_secs,
        "stale_after_secs": stale_after_secs,
        "reason": "repo_backlog workflow exceeded stale recovery threshold without active repo backlog work",
    });
    let event = WorkflowEvent::new(
        &instance.id,
        next_sequence as u64,
        RECOVERY_EVENT_TYPE,
        RECOVERY_EVENT_SOURCE,
    )
    .with_payload(payload);
    let decision = stale_repo_backlog_recovery_decision(&instance, stuck_secs, stale_after_secs);
    DecisionValidator::repo_backlog().validate(
        &instance,
        &decision,
        &ValidationContext::new(RECOVERY_EVENT_SOURCE, event.created_at),
    )?;
    let event_data = to_jsonb_string(&event)?;
    sqlx::query(
        "INSERT INTO workflow_events
            (id, workflow_id, sequence, event_type, source, data)
         VALUES ($1, $2, $3, $4, $5, $6::jsonb)",
    )
    .bind(&event.id)
    .bind(&event.workflow_id)
    .bind(event.sequence as i64)
    .bind(&event.event_type)
    .bind(&event.source)
    .bind(&event_data)
    .execute(&mut *tx)
    .await?;
    if force_rollback_after_event_insert {
        anyhow::bail!("forced stale recovery rollback after event insert");
    }

    let record = WorkflowDecisionRecord::accepted(decision, Some(event.id.clone()));
    insert_recovery_decision_tx(&mut tx, &record).await?;
    if force_rollback_after_decision_insert {
        anyhow::bail!("forced stale recovery rollback after decision insert");
    }

    merge_recovery_decision_data(
        &mut instance,
        &record,
        &event,
        original_state,
        stuck_secs,
        stale_after_secs,
    );
    instance.state = record.decision.next_state.clone();
    instance.version = instance.version.saturating_add(1);
    // Keep the JSON timestamp fresh for replay/projection paths.
    instance.updated_at = Utc::now();

    let instance_data = to_jsonb_string(&instance)?;
    let rows_affected = sqlx::query(
        "UPDATE workflow_instances
         SET definition_id = $2,
             state = $3,
             subject_type = $4,
             subject_key = $5,
             parent_workflow_id = $6,
             data = $7::jsonb,
             version = $8,
             updated_at = CURRENT_TIMESTAMP
         WHERE id = $1",
    )
    .bind(&instance.id)
    .bind(&instance.definition_id)
    .bind(&instance.state)
    .bind(&instance.subject.subject_type)
    .bind(&instance.subject.subject_key)
    .bind(&instance.parent_workflow_id)
    .bind(&instance_data)
    .bind(instance.version as i64)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if rows_affected != 1 {
        anyhow::bail!(
            "stale recovery update affected {rows_affected} rows for workflow `{workflow_id}`"
        );
    }
    tx.commit().await?;
    Ok(RecoveryCommitOutcome::Recovered { stuck_secs })
}

fn stale_repo_backlog_recovery_decision(
    instance: &WorkflowInstance,
    stuck_secs: i64,
    stale_after_secs: u64,
) -> WorkflowDecision {
    WorkflowDecision::new(
        &instance.id,
        &instance.state,
        RECOVERY_DECISION_NAME,
        RECOVERY_TARGET_STATE,
        RECOVERY_REASON,
    )
    .with_evidence(WorkflowEvidence::new(
        RECOVERY_EVENT_TYPE,
        format!(
            "repo_backlog workflow was stuck in `{}` for {stuck_secs}s with stale_after_secs={stale_after_secs}",
            instance.state
        ),
    ))
    .high_confidence()
}

async fn insert_recovery_decision_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    record: &WorkflowDecisionRecord,
) -> anyhow::Result<()> {
    let data = to_jsonb_string(record)?;
    sqlx::query(
        "INSERT INTO workflow_decisions
            (id, workflow_id, event_id, accepted, data, rejection_reason)
         VALUES ($1, $2, $3, $4, $5::jsonb, $6)
         ON CONFLICT (id) DO UPDATE SET
            accepted = EXCLUDED.accepted,
            data = EXCLUDED.data,
            rejection_reason = EXCLUDED.rejection_reason",
    )
    .bind(&record.id)
    .bind(&record.workflow_id)
    .bind(&record.event_id)
    .bind(record.accepted)
    .bind(&data)
    .bind(&record.rejection_reason)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

fn merge_recovery_decision_data(
    instance: &mut WorkflowInstance,
    record: &WorkflowDecisionRecord,
    event: &WorkflowEvent,
    original_state: &str,
    stuck_secs: i64,
    stale_after_secs: u64,
) {
    let metadata = json!({
        "event_id": event.id,
        "decision_id": record.id,
        "decision": record.decision.decision,
        "from_state": original_state,
        "to_state": record.decision.next_state,
        "stuck_secs": stuck_secs,
        "stale_after_secs": stale_after_secs,
        "recovered_at": event.created_at,
    });

    match &mut instance.data {
        Value::Object(map) => {
            map.insert(
                "last_decision".to_string(),
                Value::String(record.decision.decision.clone()),
            );
            map.insert("last_recovery".to_string(), metadata);
        }
        data => {
            let previous_data = std::mem::take(data);
            *data = json!({
                "previous_data": previous_data,
                "last_decision": record.decision.decision,
                "last_recovery": metadata,
            });
        }
    }
}

async fn workflow_has_active_repo_backlog_work_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workflow_id: &str,
) -> anyhow::Result<bool> {
    let command_rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT id, status, data::text FROM workflow_commands
         WHERE workflow_id = $1
         ORDER BY created_at ASC",
    )
    .bind(workflow_id)
    .fetch_all(&mut **tx)
    .await?;
    for (command_id, status, data) in command_rows.into_iter().rev() {
        let command: WorkflowCommand = serde_json::from_str(&data)?;
        if !is_repo_backlog_runtime_command(&command) {
            continue;
        }
        match WorkflowCommandStatus::try_from(status.as_str())? {
            WorkflowCommandStatus::Pending | WorkflowCommandStatus::Dispatching => return Ok(true),
            WorkflowCommandStatus::Dispatched => {
                let (has_active_job,): (bool,) = sqlx::query_as(
                    "SELECT EXISTS(
                        SELECT 1 FROM runtime_jobs
                        WHERE command_id = $1 AND status IN ('pending', 'running')
                    )",
                )
                .bind(&command_id)
                .fetch_one(&mut **tx)
                .await?;
                if has_active_job {
                    return Ok(true);
                }
            }
            _ => {}
        }
    }
    Ok(false)
}

fn to_jsonb_string(value: &impl Serialize) -> anyhow::Result<String> {
    Ok(serde_json::to_string(value)?.replace("\\u0000", ""))
}

async fn workflow_has_active_repo_backlog_work(
    store: &WorkflowRuntimeStore,
    workflow_id: &str,
) -> anyhow::Result<bool> {
    let commands = store.commands_for(workflow_id).await?;
    for command in commands
        .iter()
        .rev()
        .filter(is_repo_backlog_runtime_command_record)
    {
        match command.status {
            WorkflowCommandStatus::Pending | WorkflowCommandStatus::Dispatching => return Ok(true),
            WorkflowCommandStatus::Dispatched => {
                let jobs = store.runtime_jobs_for_command(&command.id).await?;
                if jobs.iter().any(|job| {
                    matches!(
                        job.status,
                        RuntimeJobStatus::Pending | RuntimeJobStatus::Running
                    )
                }) {
                    return Ok(true);
                }
            }
            _ => {}
        }
    }
    Ok(false)
}

fn is_repo_backlog_runtime_command_record(command: &&WorkflowCommandRecord) -> bool {
    is_repo_backlog_runtime_command(&command.command)
}

fn is_repo_backlog_runtime_command(command: &WorkflowCommand) -> bool {
    if command.command_type == WorkflowCommandType::StartChildWorkflow {
        return true;
    }
    command.command_type == WorkflowCommandType::EnqueueActivity
        && matches!(
            command.activity_name(),
            Some(
                REPO_BACKLOG_POLL_ACTIVITY
                    | REPO_BACKLOG_SPRINT_PLAN_ACTIVITY
                    | LEGACY_REPO_BACKLOG_SPRINT_PLAN_ACTIVITY
                    | REPO_BACKLOG_MARK_BOUND_ISSUE_DONE_ACTIVITY
                    | REPO_BACKLOG_RECOVER_ISSUE_WORKFLOW_ACTIVITY
            )
        )
}

/// Spawn the periodic recovery loop. Idempotent across restarts because each
/// tick is itself idempotent: workflows that have moved out of the stuck
/// state set since the last tick are simply not returned by the query.
pub fn spawn_stale_workflow_recovery(state: &Arc<AppState>) {
    if state.core.workflow_runtime_store.is_none() {
        tracing::debug!("stale_workflow_recovery: store unavailable; recovery loop not spawned");
        return;
    }
    let weak_state = Arc::downgrade(state);
    tokio::spawn(async move {
        let interval = Duration::from_secs(DEFAULT_INTERVAL_SECS);
        loop {
            let state = match weak_state.upgrade() {
                Some(state) => state,
                None => break,
            };
            let Some(store) = state.core.workflow_runtime_store.as_ref() else {
                break;
            };
            match run_stale_workflow_recovery_tick(store, DEFAULT_STALE_AFTER_SECS).await {
                Ok(tick) if tick.has_recovery_activity() => {
                    tracing::info!(
                        scanned = tick.scanned,
                        recovered = tick.recovered,
                        failed = tick.failed,
                        skipped_active = tick.skipped_active,
                        "stale_workflow_recovery: tick complete"
                    );
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!("stale_workflow_recovery: tick failed: {error}");
                }
            }
            drop(state);
            tokio::time::sleep(interval).await;
        }
    });
}

#[cfg(test)]
#[path = "stale_workflow_recovery_tests.rs"]
mod tests;
