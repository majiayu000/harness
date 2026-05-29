//! Stale workflow recovery — periodic tick that resets workflows stuck in
//! non-terminal, non-candidate states back to a state the poller can pick up.
//!
//! The `repo_backlog` workflow type has known dead-end middle states:
//! `scanning`, `planning_batch`, `dispatching`, `reconciling`. The poller's
//! eligibility check is `state IN ('idle','failed')`, so once a workflow lands
//! in one of the middle states with no follow-up command (e.g. claude returned
//! empty output, server restart interrupted the dispatch path, wire-format
//! drift swallowed the next-step decision), it sits there forever and the
//! corresponding repo's GitHub issues stop being processed.
//!
//! This recovery tick is the operational backstop. It scans the runtime store
//! every `interval_secs` (default 600 = 10 min), finds `repo_backlog`
//! instances stuck in any of the dead-end states for longer than
//! `stale_after_secs` (default 1800 = 30 min), and force-resets them to
//! `idle` so the next poller tick will re-enqueue a fresh `poll_repo_backlog`
//! activity.
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
    RuntimeJobStatus, WorkflowCommandRecord, WorkflowRuntimeStore, REPO_BACKLOG_POLL_ACTIVITY,
};

use crate::http::AppState;

/// Workflow definition this tick recovers. Currently only `repo_backlog` —
/// other workflow types (e.g. `github_issue_pr`) have legitimate long-running
/// non-terminal states (`pr_open` waiting for human review, `awaiting_dependencies`
/// waiting on parent issue) and would be incorrectly reset.
const RECOVERED_DEFINITION_ID: &str = "repo_backlog";

/// States that are non-terminal AND non-candidate-for-poller. A workflow in
/// one of these states relies on the next reducer transition firing; if that
/// never happens, the workflow is stuck.
const STUCK_STATES: &[&str] = &["scanning", "planning_batch", "dispatching", "reconciling"];

/// State to reset stuck workflows to. `idle` is in the poller's candidate set
/// (`idle | failed`), so the next poller tick will re-claim it and re-issue a
/// fresh `poll_repo_backlog` activity.
const RECOVERY_TARGET_STATE: &str = "idle";

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
        for mut instance in candidates {
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
            instance.state = RECOVERY_TARGET_STATE.to_string();
            instance.version = instance.version.saturating_add(1);
            // Bump the JSON-baked updated_at so subsequent recovery ticks see a
            // fresh timestamp and do not re-reset this same instance on every
            // tick. The Postgres column updated_at is independently bumped by
            // upsert_instance via CURRENT_TIMESTAMP, but list_instances_by_state
            // returns instances deserialised from the data jsonb and the JSON
            // copy of updated_at is what subsequent ticks compare against.
            //
            // Without this bump the recovery tick thrashes: after the first
            // reset the column moves forward to NOW() but the data jsonb still
            // carries a stale (often pre-reset) updated_at, so 10 minutes
            // later the next tick re-reads the stale JSON timestamp, decides
            // the workflow is still stale, and resets again. Observed in
            // production: 8 workflows reset twice in 10 minutes after server
            // restart, despite having transitioned through scanning legitimately
            // in between.
            instance.updated_at = Utc::now();
            match store.upsert_instance(&instance).await {
                Ok(()) => {
                    tracing::warn!(
                        workflow_id = %instance.id,
                        from_state = %original_state,
                        to_state = %RECOVERY_TARGET_STATE,
                        stuck_secs,
                        "stale_workflow_recovery: reset workflow stuck in non-candidate state"
                    );
                    tick.recovered += 1;
                }
                Err(error) => {
                    tracing::warn!(
                        workflow_id = %instance.id,
                        from_state = %original_state,
                        stuck_secs,
                        "stale_workflow_recovery: upsert failed: {error}"
                    );
                    tick.failed += 1;
                }
            }
        }
    }
    Ok(tick)
}

async fn workflow_has_active_repo_backlog_work(
    store: &WorkflowRuntimeStore,
    workflow_id: &str,
) -> anyhow::Result<bool> {
    let commands = store.commands_for(workflow_id).await?;
    for command in commands.iter().rev().filter(is_repo_backlog_poll_command) {
        match command.status.as_str() {
            "pending" | "dispatching" => return Ok(true),
            "dispatched" => {
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

fn is_repo_backlog_poll_command(command: &&WorkflowCommandRecord) -> bool {
    command.command.activity_name() == Some(REPO_BACKLOG_POLL_ACTIVITY)
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
mod tests {
    use super::*;
    use harness_core::db::resolve_database_url;
    use harness_workflow::runtime::{
        RuntimeKind, WorkflowCommand, WorkflowInstance, WorkflowSubject,
    };
    use serde_json::json;
    use std::sync::Arc;

    async fn open_recovery_test_store() -> Option<Arc<WorkflowRuntimeStore>> {
        let database_url = resolve_database_url(None).ok()?;
        let dir = tempfile::tempdir().ok()?;
        WorkflowRuntimeStore::open_with_database_url(dir.path(), Some(&database_url))
            .await
            .ok()
            .map(Arc::new)
    }

    fn stuck_instance(id: &str, state: &str) -> WorkflowInstance {
        WorkflowInstance::new(
            RECOVERED_DEFINITION_ID,
            1,
            state,
            WorkflowSubject::new("repo", "owner/repo"),
        )
        .with_id(id)
    }

    #[tokio::test]
    async fn recovery_tick_resets_stuck_planning_batch_to_idle() {
        let Some(store) = open_recovery_test_store().await else {
            return;
        };
        let mut instance = stuck_instance("test::stale-recovery::planning::1", "planning_batch");
        // Backdate updated_at to 2h ago by serializing into the store. The
        // store's CURRENT_TIMESTAMP DEFAULT writes a fresh value, so we
        // immediately overwrite it via raw SQL.
        store.upsert_instance(&instance).await.unwrap();
        instance.state = "planning_batch".to_string();
        // Confirm pre-condition: instance.updated_at is now (within seconds).
        let pre = store.get_instance(&instance.id).await.unwrap().unwrap();
        assert_eq!(pre.state, "planning_batch");

        // Stale_after = 0 means "anything not currently being modified counts as
        // stale" — gives the test a deterministic boundary without sleeping.
        let tick = run_stale_workflow_recovery_tick(&store, 0).await.unwrap();
        assert!(
            tick.scanned >= 1,
            "should scan at least the seeded instance"
        );
        assert!(tick.recovered >= 1, "should recover at least one");

        let post = store.get_instance(&instance.id).await.unwrap().unwrap();
        assert_eq!(post.state, "idle", "stuck workflow should be reset to idle");
        assert!(post.version > pre.version, "version should bump");
    }

    #[tokio::test]
    async fn recovery_tick_skips_workflows_in_terminal_or_candidate_state() {
        let Some(store) = open_recovery_test_store().await else {
            return;
        };
        for state in ["idle", "failed", "done", "cancelled"] {
            let id = format!("test::stale-recovery::skip::{state}");
            let instance = stuck_instance(&id, state);
            store.upsert_instance(&instance).await.unwrap();
        }
        let tick = run_stale_workflow_recovery_tick(&store, 0).await.unwrap();
        for state in ["idle", "failed", "done", "cancelled"] {
            let id = format!("test::stale-recovery::skip::{state}");
            let after = store.get_instance(&id).await.unwrap().unwrap();
            assert_eq!(
                after.state, state,
                "instance in {state} should not be touched by recovery"
            );
        }
        // The seeded states are not in STUCK_STATES, so none should have been
        // scanned (the SQL filter excludes them). recovered MUST be 0.
        assert_eq!(tick.recovered, 0);
    }

    #[tokio::test]
    async fn recovery_tick_does_not_re_reset_workflow_on_subsequent_tick() {
        // Regression: previously the tick used the JSON-baked updated_at on
        // each iteration, which never advanced after upsert_instance because
        // upsert_instance bumped the Postgres column via CURRENT_TIMESTAMP
        // but did NOT mutate the deserialized struct's updated_at field.
        // Result: the same workflow was reset on every 10-minute tick.
        //
        // The fix bumps `instance.updated_at = Utc::now()` before each upsert
        // in the recovery path so the JSON timestamp stays in lock-step with
        // the column. Verified here: with a 5-minute threshold, a workflow
        // that was just recovered should not be re-recovered on an
        // immediately-following tick.
        let Some(store) = open_recovery_test_store().await else {
            return;
        };
        let id = "test::stale-recovery::no-thrash";
        let mut seed = stuck_instance(id, "dispatching");
        // Force the seed instance to look stale (JSON updated_at = 1h ago)
        // so the first tick will resolve it.
        seed.updated_at = Utc::now() - chrono::Duration::hours(1);
        store.upsert_instance(&seed).await.unwrap();

        // First tick with 5-min threshold: 1h-old instance is stale, must reset.
        let first = run_stale_workflow_recovery_tick(&store, 300).await.unwrap();
        assert!(
            first.recovered >= 1,
            "first tick should reset stuck workflow"
        );

        let post_first = store.get_instance(id).await.unwrap().unwrap();
        assert_eq!(post_first.state, "idle");

        // Simulate the workflow transitioning back to a stuck state via the
        // reducer path that historically does NOT bump the JSON updated_at.
        // The recovery fix means post_first.updated_at is now ~"first-tick
        // now"; a reducer that copies that value forward writes a JSON
        // timestamp that is well within the 5-minute window.
        let mut transitioned = post_first.clone();
        transitioned.state = "dispatching".to_string();
        transitioned.version = transitioned.version.saturating_add(1);
        // Intentionally do NOT touch transitioned.updated_at — mimics the
        // upstream reducer behavior we observed in production.
        store.upsert_instance(&transitioned).await.unwrap();

        // Second tick with the same 5-min threshold MUST NOT reset, because
        // the workflow's JSON updated_at is now "fresh" relative to the
        // 5-minute cutoff.
        let second = run_stale_workflow_recovery_tick(&store, 300).await.unwrap();
        assert_eq!(
            second.recovered, 0,
            "second tick must not re-reset a workflow whose JSON updated_at \
             was just bumped by the first tick"
        );

        let post_second = store.get_instance(id).await.unwrap().unwrap();
        assert_eq!(
            post_second.state, "dispatching",
            "transitioned-back state must be preserved by second tick"
        );
    }

    #[tokio::test]
    async fn recovery_tick_respects_stale_after_threshold() {
        let Some(store) = open_recovery_test_store().await else {
            return;
        };
        let instance = stuck_instance("test::stale-recovery::not-yet-stale", "dispatching");
        store.upsert_instance(&instance).await.unwrap();
        // Threshold of 3600s with a freshly-upserted row -> not stale yet.
        let tick = run_stale_workflow_recovery_tick(&store, 3600)
            .await
            .unwrap();
        let after = store.get_instance(&instance.id).await.unwrap().unwrap();
        assert_eq!(
            after.state, "dispatching",
            "fresh stuck workflow should not be reset before threshold"
        );
        assert_eq!(tick.recovered, 0);
    }

    #[tokio::test]
    async fn recovery_tick_skips_workflow_with_active_runtime_job() -> anyhow::Result<()> {
        let Some(store) = open_recovery_test_store().await else {
            return Ok(());
        };
        let id = "test::stale-recovery::active-runtime-job";
        let mut instance = stuck_instance(id, "scanning");
        instance.updated_at = Utc::now() - chrono::Duration::hours(1);
        store.upsert_instance(&instance).await?;
        let command =
            WorkflowCommand::enqueue_activity(REPO_BACKLOG_POLL_ACTIVITY, "active-runtime-job");
        let command_id = store.enqueue_command(id, None, &command).await?;
        store
            .enqueue_runtime_job(
                &command_id,
                RuntimeKind::CodexJsonrpc,
                "codex-default",
                json!({ "activity": REPO_BACKLOG_POLL_ACTIVITY }),
            )
            .await?;
        store.mark_command_status(&command_id, "dispatched").await?;

        let tick = run_stale_workflow_recovery_tick(&store, 300).await?;

        let after = store.get_instance(id).await?.ok_or_else(|| {
            anyhow::anyhow!("active runtime workflow missing after recovery tick")
        })?;
        assert_eq!(
            after.state, "scanning",
            "active runtime work must protect the workflow from stale recovery"
        );
        assert_eq!(tick.recovered, 0);
        assert!(
            tick.skipped_active >= 1,
            "active runtime job should be counted as an active skip"
        );
        Ok(())
    }
}
