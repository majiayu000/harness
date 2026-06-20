use super::*;
use harness_core::db::resolve_database_url;
use harness_workflow::runtime::{
    RuntimeKind, WorkflowCommand, WorkflowCommandStatus, WorkflowEvent, WorkflowInstance,
    WorkflowSubject,
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

async fn assert_recovery_decision_committed(
    store: &WorkflowRuntimeStore,
    workflow_id: &str,
    recovery_event: &WorkflowEvent,
    observed_state: &str,
) -> anyhow::Result<()> {
    let decisions = store.decisions_for(workflow_id).await?;
    let Some(record) = decisions
        .iter()
        .find(|record| record.event_id.as_deref() == Some(recovery_event.id.as_str()))
    else {
        anyhow::bail!("recovery event should have an accepted decision record");
    };
    assert!(record.accepted, "recovery decision should be accepted");
    assert_eq!(record.decision.decision, RECOVERY_DECISION_NAME);
    assert_eq!(record.decision.observed_state, observed_state);
    assert_eq!(record.decision.next_state, RECOVERY_TARGET_STATE);
    assert!(
        record.decision.commands.is_empty(),
        "recovery state repair is represented by the accepted decision, not an extra command"
    );
    assert!(
        store.commands_for(workflow_id).await?.is_empty(),
        "recovery should not create a command outbox row"
    );
    Ok(())
}

#[tokio::test]
async fn recovery_tick_resets_stuck_planning_batch_to_idle() -> anyhow::Result<()> {
    let Some(store) = open_recovery_test_store().await else {
        return Ok(());
    };
    let mut instance = stuck_instance("test::stale-recovery::planning::1", "planning_batch");
    // Backdate updated_at to 2h ago by serializing into the store. The store's
    // CURRENT_TIMESTAMP DEFAULT writes a fresh value, so we immediately
    // overwrite it via raw SQL.
    store.upsert_instance(&instance).await?;
    instance.state = "planning_batch".to_string();
    // Confirm pre-condition: instance.updated_at is now (within seconds).
    let Some(pre) = store.get_instance(&instance.id).await? else {
        anyhow::bail!("seeded planning workflow should exist before recovery");
    };
    assert_eq!(pre.state, "planning_batch");

    // Stale_after = 0 means "anything not currently being modified counts as
    // stale" — gives the test a deterministic boundary without sleeping.
    let tick = run_stale_workflow_recovery_tick(&store, 0).await?;
    assert!(
        tick.scanned >= 1,
        "should scan at least the seeded instance"
    );
    assert!(tick.recovered >= 1, "should recover at least one");

    let Some(post) = store.get_instance(&instance.id).await? else {
        anyhow::bail!("seeded planning workflow should still exist after recovery");
    };
    assert_eq!(post.state, "idle", "stuck workflow should be reset to idle");
    assert!(post.version > pre.version, "version should bump");
    let events = store.events_for(&instance.id).await?;
    let Some(recovery_event) = events
        .iter()
        .find(|event| event.event_type == RECOVERY_EVENT_TYPE)
    else {
        anyhow::bail!("stale recovery should record workflow evidence");
    };
    assert_eq!(recovery_event.source, RECOVERY_EVENT_SOURCE);
    assert_eq!(
        recovery_event.event["definition_id"],
        RECOVERED_DEFINITION_ID
    );
    assert_eq!(recovery_event.event["from_state"], "planning_batch");
    assert_eq!(recovery_event.event["to_state"], RECOVERY_TARGET_STATE);
    assert_eq!(recovery_event.event["decision"], RECOVERY_DECISION_NAME);
    assert_eq!(recovery_event.event["stale_after_secs"], 0);
    assert!(
        recovery_event.event["stuck_secs"].as_i64().is_some(),
        "recovery event should include stale duration evidence"
    );
    assert_eq!(recovery_event.workflow_id, post.id);
    assert_recovery_decision_committed(&store, &instance.id, recovery_event, "planning_batch")
        .await?;
    assert_eq!(post.data["last_decision"], RECOVERY_DECISION_NAME);
    assert_eq!(post.data["last_recovery"]["event_id"], recovery_event.id);
    Ok(())
}

#[tokio::test]
async fn recovery_tick_resets_stuck_blocked_to_idle() -> anyhow::Result<()> {
    let Some(store) = open_recovery_test_store().await else {
        return Ok(());
    };
    let instance = stuck_instance("test::stale-recovery::blocked::1", "blocked");
    store.upsert_instance(&instance).await?;

    let Some(pre) = store.get_instance(&instance.id).await? else {
        anyhow::bail!("seeded blocked workflow should exist before recovery");
    };
    assert_eq!(pre.state, "blocked");

    let tick = run_stale_workflow_recovery_tick(&store, 0).await?;

    assert!(
        tick.scanned >= 1,
        "should scan at least the blocked instance"
    );
    assert!(tick.recovered >= 1, "should recover blocked instance");

    let Some(post) = store.get_instance(&instance.id).await? else {
        anyhow::bail!("seeded blocked workflow should still exist after recovery");
    };
    assert_eq!(
        post.state, "idle",
        "blocked workflow should be reset to idle"
    );
    assert!(post.version > pre.version, "version should bump");
    let events = store.events_for(&instance.id).await?;
    let Some(recovery_event) = events
        .iter()
        .find(|event| event.event_type == RECOVERY_EVENT_TYPE)
    else {
        anyhow::bail!("blocked recovery should record workflow evidence");
    };
    assert_recovery_decision_committed(&store, &instance.id, recovery_event, "blocked").await?;
    Ok(())
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
    let Some(store) = open_recovery_test_store().await else {
        return;
    };
    let id = "test::stale-recovery::no-thrash";
    let mut seed = stuck_instance(id, "dispatching");
    seed.updated_at = Utc::now() - chrono::Duration::hours(1);
    store.upsert_instance(&seed).await.unwrap();

    let first = run_stale_workflow_recovery_tick(&store, 300).await.unwrap();
    assert!(
        first.recovered >= 1,
        "first tick should reset stuck workflow"
    );

    let post_first = store.get_instance(id).await.unwrap().unwrap();
    assert_eq!(post_first.state, "idle");

    let mut transitioned = post_first.clone();
    transitioned.state = "dispatching".to_string();
    transitioned.version = transitioned.version.saturating_add(1);
    store.upsert_instance(&transitioned).await.unwrap();

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
async fn recovery_commit_skips_when_locked_workflow_changed() -> anyhow::Result<()> {
    let Some(store) = open_recovery_test_store().await else {
        return Ok(());
    };
    let id = "test::stale-recovery::changed-before-commit";
    let mut instance = stuck_instance(id, "dispatching");
    instance.updated_at = Utc::now() - chrono::Duration::hours(1);
    store.upsert_instance(&instance).await?;
    let mut changed = instance.clone();
    changed.state = "idle".to_string();
    changed.version = changed.version.saturating_add(1);
    changed.updated_at = Utc::now();
    store.upsert_instance(&changed).await?;

    let outcome = commit_stale_recovery_decision(
        &store,
        id,
        "dispatching",
        300,
        Utc::now() - chrono::Duration::seconds(300),
    )
    .await?;

    assert_eq!(outcome, RecoveryCommitOutcome::SkippedChanged);
    let Some(after) = store.get_instance(id).await? else {
        anyhow::bail!("seeded workflow should still exist after changed skip");
    };
    assert_eq!(after.state, "idle");
    let events = store.events_for(id).await?;
    assert!(
        events
            .iter()
            .all(|event| event.event_type != RECOVERY_EVENT_TYPE),
        "changed workflow should not receive recovery evidence"
    );
    assert!(
        store.decisions_for(id).await?.is_empty(),
        "changed workflow should not receive a recovery decision"
    );
    assert!(
        store.commands_for(id).await?.is_empty(),
        "changed workflow should not receive recovery commands"
    );
    Ok(())
}

#[tokio::test]
async fn recovery_commit_rolls_back_event_after_event_insert_error() -> anyhow::Result<()> {
    let Some(store) = open_recovery_test_store().await else {
        return Ok(());
    };
    let id = "test::stale-recovery::forced-rollback";
    let mut instance = stuck_instance(id, "dispatching");
    instance.updated_at = Utc::now() - chrono::Duration::hours(1);
    store.upsert_instance(&instance).await?;

    let result = commit_stale_recovery_decision_inner(
        &store,
        id,
        "dispatching",
        300,
        Utc::now() - chrono::Duration::seconds(300),
        true,
        false,
    )
    .await;

    assert!(result.is_err(), "forced rollback should fail the commit");
    let Some(after) = store.get_instance(id).await? else {
        anyhow::bail!("seeded workflow should still exist after rollback");
    };
    assert_eq!(after.state, "dispatching");
    assert_eq!(after.version, instance.version);
    let events = store.events_for(id).await?;
    assert!(
        events
            .iter()
            .all(|event| event.event_type != RECOVERY_EVENT_TYPE),
        "recovery event should roll back when the atomic commit fails"
    );
    assert!(
        store.decisions_for(id).await?.is_empty(),
        "recovery decision should roll back when the atomic commit fails"
    );
    assert!(
        store.commands_for(id).await?.is_empty(),
        "recovery command should roll back when the atomic commit fails"
    );
    Ok(())
}

#[tokio::test]
async fn recovery_commit_rolls_back_decision_after_decision_insert_error() -> anyhow::Result<()> {
    let Some(store) = open_recovery_test_store().await else {
        return Ok(());
    };
    let id = "test::stale-recovery::forced-decision-rollback";
    let mut instance = stuck_instance(id, "dispatching");
    instance.updated_at = Utc::now() - chrono::Duration::hours(1);
    store.upsert_instance(&instance).await?;

    let result = commit_stale_recovery_decision_inner(
        &store,
        id,
        "dispatching",
        300,
        Utc::now() - chrono::Duration::seconds(300),
        false,
        true,
    )
    .await;

    assert!(result.is_err(), "forced rollback should fail the commit");
    let Some(after) = store.get_instance(id).await? else {
        anyhow::bail!("seeded workflow should still exist after rollback");
    };
    assert_eq!(after.state, "dispatching");
    assert_eq!(after.version, instance.version);
    assert!(
        store.events_for(id).await?.is_empty(),
        "recovery event should roll back after decision insert failure"
    );
    assert!(
        store.decisions_for(id).await?.is_empty(),
        "recovery decision should roll back after decision insert failure"
    );
    assert!(
        store.commands_for(id).await?.is_empty(),
        "recovery should still not leave recovery commands"
    );
    Ok(())
}

#[tokio::test]
async fn recovery_tick_skips_workflow_with_active_repo_backlog_runtime_work() -> anyhow::Result<()>
{
    let Some(store) = open_recovery_test_store().await else {
        return Ok(());
    };
    let cases = vec![
        (
            "scanning",
            WorkflowCommand::enqueue_activity(REPO_BACKLOG_POLL_ACTIVITY, "active-scan"),
        ),
        (
            "planning_batch",
            WorkflowCommand::enqueue_activity(REPO_BACKLOG_SPRINT_PLAN_ACTIVITY, "active-plan"),
        ),
        (
            "dispatching",
            WorkflowCommand::start_child_workflow("github_issue_pr", "issue:1", "active-dispatch"),
        ),
        (
            "reconciling",
            WorkflowCommand::enqueue_activity(
                REPO_BACKLOG_MARK_BOUND_ISSUE_DONE_ACTIVITY,
                "active-mark",
            ),
        ),
        (
            "reconciling",
            WorkflowCommand::enqueue_activity(
                REPO_BACKLOG_RECOVER_ISSUE_WORKFLOW_ACTIVITY,
                "active-recover",
            ),
        ),
    ];
    for (index, (state, command)) in cases.into_iter().enumerate() {
        let id = format!("test::stale-recovery::active-runtime-work::{index}");
        let mut instance = stuck_instance(&id, state);
        instance.updated_at = Utc::now() - chrono::Duration::hours(1);
        store.upsert_instance(&instance).await?;
        let activity = command.runtime_activity_key().to_string();
        let command_id = store.enqueue_command(&id, None, &command).await?;
        store
            .enqueue_runtime_job(
                &command_id,
                RuntimeKind::CodexJsonrpc,
                "codex-default",
                json!({ "activity": activity }),
            )
            .await?;
        store
            .mark_command_status(&command_id, WorkflowCommandStatus::Dispatched)
            .await?;

        let tick = run_stale_workflow_recovery_tick(&store, 300).await?;
        let after = store
            .get_instance(&id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("active workflow missing after recovery tick"))?;
        assert_eq!(after.state, state);
        assert!(tick.skipped_active >= 1);
        let events = store.events_for(&id).await?;
        assert!(events
            .iter()
            .all(|event| event.event_type != RECOVERY_EVENT_TYPE));
    }
    Ok(())
}
