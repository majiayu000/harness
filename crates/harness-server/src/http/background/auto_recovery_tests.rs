use super::*;
use crate::test_helpers;
use harness_core::config::intake::GitHubRepoConfig;
use harness_workflow::runtime::{
    RuntimeKind, WorkflowCommand, WorkflowCommandType, WorkflowSubject,
};

const TEST_REPO: &str = "owner/auto";

fn test_github_config(max_attempts: u32) -> GitHubIntakeConfig {
    GitHubIntakeConfig {
        enabled: true,
        repos: vec![GitHubRepoConfig {
            repo: TEST_REPO.to_string(),
            label: "harness".to_string(),
            project_root: None,
            auto_merge: None,
            auto_recovery: Some(true),
            merge_method: None,
            delete_branch: None,
            require_review_threads_resolved: None,
            require_clean_merge_state: None,
        }],
        auto_recovery: GitHubAutoRecoveryConfig {
            enabled: true,
            max_attempts,
            initial_backoff_secs: 300,
            max_backoff_secs: 14400,
            // Deterministic backoff timestamps in tests.
            jitter_ratio: 0.0,
            tick_interval_secs: 60,
        },
        ..GitHubIntakeConfig::default()
    }
}

async fn open_test_store(
    prefix: &str,
) -> anyhow::Result<(tempfile::TempDir, WorkflowRuntimeStore)> {
    let dir = test_helpers::tempdir_in_home(prefix)?;
    let store = WorkflowRuntimeStore::open_with_database_url(
        &harness_core::config::dirs::default_db_path(dir.path(), "workflow_runtime"),
        Some(&test_helpers::test_database_url()?),
    )
    .await?;
    Ok((dir, store))
}

/// Seed a stopped instance whose structured stop metadata points at a real
/// runtime job + command, so `recover_stopped_instance` can replay it.
async fn seed_stopped_instance(
    store: &WorkflowRuntimeStore,
    id: &str,
    state: &str,
    data: Value,
) -> anyhow::Result<WorkflowInstance> {
    let mut workflow = WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        state,
        WorkflowSubject::new("issue", format!("issue:{id}")),
    )
    .with_id(id.to_string())
    .with_data(data);
    store.upsert_instance(&workflow).await?;
    let command = WorkflowCommand::new(
        WorkflowCommandType::EnqueueActivity,
        format!("{id}-source"),
        json!({ "activity": "implement_issue" }),
    );
    let command_id = store.enqueue_command(id, None, &command).await?;
    let job = store
        .enqueue_runtime_job(
            &command_id,
            RuntimeKind::CodexJsonrpc,
            "codex-test",
            command.command,
        )
        .await?;
    workflow.data["last_stop"]["runtime_job_id"] = json!(job.id);
    store.upsert_instance(&workflow).await?;
    Ok(workflow)
}

fn transient_blocked_data(episode: &str) -> Value {
    json!({
        "repo": TEST_REPO,
        "blocked_reason": "GitHub API rate limited",
        "stop_reason_code": "rate_limited",
        "reason_class": "transient",
        "last_stop": {
            "state": "blocked",
            "activity": "implement_issue",
            "event_id": episode,
            "stop_reason_code": "rate_limited",
            "reason_class": "transient",
        },
    })
}

fn instance_attempt_state(instance: &WorkflowInstance) -> Option<AutoRecoveryState> {
    instance
        .data
        .get("auto_recovery")
        .and_then(|value| serde_json::from_value(value.clone()).ok())
}

async fn refreshed(store: &WorkflowRuntimeStore, id: &str) -> anyhow::Result<WorkflowInstance> {
    store
        .get_instance(id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("instance {id} missing"))
}

fn event_types(events: &[harness_workflow::runtime::WorkflowEvent]) -> Vec<&str> {
    events
        .iter()
        .map(|event| event.event_type.as_str())
        .collect()
}

// -- B-003: disabled policy is byte-identical to GH-1567 semantics ----------

#[tokio::test]
async fn auto_recovery_disabled_default_config_never_touches_instances() -> anyhow::Result<()> {
    if !test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let _lock = test_helpers::HOME_LOCK.lock().await;
    let (_dir, store) = open_test_store("harness-test-ar-disabled-").await?;
    let instance = seed_stopped_instance(
        &store,
        "ar-disabled-1",
        "blocked",
        transient_blocked_data("episode-1"),
    )
    .await?;

    // The global default config never spawns the scheduler at all; a tick
    // against a disabled policy is inert even if it were driven directly.
    let disabled = GitHubIntakeConfig::default();
    assert!(!disabled.auto_recovery.enabled, "policy must default OFF");
    for _ in 0..3 {
        let tick = run_auto_recovery_tick(&store, &disabled, Utc::now()).await?;
        assert_eq!(tick, AutoRecoveryTick::default());
    }
    let after = refreshed(&store, &instance.id).await?;
    assert_eq!(after.state, "blocked");
    assert_eq!(after.data, instance.data);
    assert!(store.events_for(&instance.id).await?.is_empty());
    Ok(())
}

// -- B-004 / B-012 / B-014: only transient-classified stops are scheduled ---

#[tokio::test]
async fn auto_recovery_selects_transient_and_skips_terminal_and_legacy() -> anyhow::Result<()> {
    if !test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let _lock = test_helpers::HOME_LOCK.lock().await;
    let (_dir, store) = open_test_store("harness-test-ar-select-").await?;
    let github = test_github_config(3);

    let transient = seed_stopped_instance(
        &store,
        "ar-select-transient",
        "blocked",
        transient_blocked_data("episode-1"),
    )
    .await?;
    let terminal = seed_stopped_instance(
        &store,
        "ar-select-terminal",
        "blocked",
        json!({
            "repo": TEST_REPO,
            "stop_reason_code": "maintainer_input_required",
            "reason_class": "terminal",
            "last_stop": {
                "state": "blocked",
                "activity": "implement_issue",
                "event_id": "episode-1",
            },
        }),
    )
    .await?;
    // Forged transient code next to a fatal failure stays terminal (B-012).
    let forged = seed_stopped_instance(
        &store,
        "ar-select-forged",
        "failed",
        json!({
            "repo": TEST_REPO,
            "failure_reason": "fatal failure",
            "error_kind": "fatal",
            "stop_reason_code": "rate_limited",
            "last_stop": {
                "state": "failed",
                "activity": "implement_issue",
                "event_id": "episode-1",
                "error_kind": "fatal",
            },
        }),
    )
    .await?;
    // Legacy row: no structured classification fields at all (B-014).
    let legacy = {
        let workflow = WorkflowInstance::new(
            GITHUB_ISSUE_PR_DEFINITION_ID,
            1,
            "blocked",
            WorkflowSubject::new("issue", "issue:ar-select-legacy"),
        )
        .with_id("ar-select-legacy".to_string())
        .with_data(json!({ "repo": TEST_REPO, "blocked_reason": "legacy free text" }));
        store.upsert_instance(&workflow).await?;
        workflow
    };

    let tick = run_auto_recovery_tick(&store, &github, Utc::now()).await?;
    assert_eq!(tick.recovered, 1);
    assert_eq!(tick.selected, 1);

    let recovered = refreshed(&store, &transient.id).await?;
    assert_eq!(recovered.state, "implementing");
    assert!(
        recovered.data.get("auto_recovery").is_none(),
        "successful recovery clears attempt state"
    );
    // last_stop evidence is preserved by the recovery path (B-005).
    assert_eq!(
        recovered.data.pointer("/last_stop/stop_reason_code"),
        Some(&json!("rate_limited"))
    );
    for untouched in [&terminal, &forged, &legacy] {
        let after = refreshed(&store, &untouched.id).await?;
        assert_eq!(
            after.state, untouched.state,
            "{} must stay stopped",
            untouched.id
        );
        assert!(store.events_for(&untouched.id).await?.is_empty());
    }
    Ok(())
}

// -- B-005: automated recovery re-runs the manual recovery path -------------

#[tokio::test]
async fn auto_recovery_transition_matches_manual_unblock_except_actor() -> anyhow::Result<()> {
    if !test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let _lock = test_helpers::HOME_LOCK.lock().await;
    let (_dir, store) = open_test_store("harness-test-ar-parity-").await?;
    let github = test_github_config(3);

    let auto = seed_stopped_instance(
        &store,
        "ar-parity-auto",
        "blocked",
        transient_blocked_data("episode-1"),
    )
    .await?;
    // Same stop shape, but in a repo that did not opt in: the scheduler
    // leaves it alone and the operator recovers it manually.
    let mut manual_data = transient_blocked_data("episode-1");
    manual_data["repo"] = json!("owner/manual");
    let manual = seed_stopped_instance(&store, "ar-parity-manual", "blocked", manual_data).await?;

    let tick = run_auto_recovery_tick(&store, &github, Utc::now()).await?;
    assert_eq!(tick.recovered, 1);
    let manual_outcome = store
        .recover_stopped_instance(WorkflowRuntimeRecoveryRequest {
            workflow_id: &manual.id,
            action: WorkflowRuntimeRecoveryAction::Unblock,
            reason: "operator resolved the rate limit",
            actor: "operator",
        })
        .await?;
    assert!(matches!(
        manual_outcome,
        WorkflowRuntimeRecoveryOutcome::Recovered { .. }
    ));

    let auto_after = refreshed(&store, &auto.id).await?;
    let manual_after = refreshed(&store, &manual.id).await?;
    assert_eq!(auto_after.state, manual_after.state);
    // Both paths preserve their own last_stop evidence unchanged (the two
    // instances differ only in their seeded runtime_job_id).
    assert_eq!(auto_after.data.get("last_stop"), auto.data.get("last_stop"));
    assert_eq!(
        manual_after.data.get("last_stop"),
        manual.data.get("last_stop")
    );
    assert_eq!(
        auto_after.data.pointer("/last_operator_recovery/actor"),
        Some(&json!(AUTO_RECOVERY_ACTOR))
    );
    assert_eq!(
        manual_after.data.pointer("/last_operator_recovery/actor"),
        Some(&json!("operator"))
    );
    // Both paths enqueue the same replayed activity command.
    let command_activity = |commands: Vec<harness_workflow::runtime::WorkflowCommandRecord>| {
        commands
            .into_iter()
            .filter(|record| {
                record.status == harness_workflow::runtime::WorkflowCommandStatus::Pending
            })
            .map(|record| record.command.activity_name().map(str::to_string))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        command_activity(store.commands_for(&auto.id).await?),
        command_activity(store.commands_for(&manual.id).await?),
    );
    Ok(())
}

// -- B-006: bounded attempts persisted in instance data ---------------------
// -- B-010: exhaustion emits one event, instance stays visible ---------------

#[tokio::test]
async fn auto_recovery_state_attempt_bound_survives_restart_and_exhaustion_is_idempotent(
) -> anyhow::Result<()> {
    if !test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let _lock = test_helpers::HOME_LOCK.lock().await;
    let (_dir, store) = open_test_store("harness-test-ar-exhaust-").await?;
    let github = test_github_config(2);

    let mut data = transient_blocked_data("episode-1");
    data["auto_recovery"] = json!({
        "episode_event_id": "episode-1",
        "attempts": 2,
        "last_outcome": "recheck_failed",
    });
    let instance = seed_stopped_instance(&store, "ar-exhaust-1", "blocked", data).await?;

    // Every tick reads the persisted counter (a fresh scheduler after restart
    // sees the same state): the bound holds and exhaustion fires exactly once.
    let first = run_auto_recovery_tick(&store, &github, Utc::now()).await?;
    assert_eq!(first.exhausted_emitted, 1);
    assert_eq!(first.recovered, 0);
    let second = run_auto_recovery_tick(&store, &github, Utc::now()).await?;
    assert_eq!(second.exhausted_emitted, 0);
    assert_eq!(second, AutoRecoveryTick::default());

    let after = refreshed(&store, &instance.id).await?;
    assert_eq!(
        after.state, "blocked",
        "instance stays stopped for the operator"
    );
    let attempt_state = instance_attempt_state(&after).expect("attempt state persists");
    assert_eq!(attempt_state.attempts, 2);
    assert!(attempt_state.exhausted);
    let events = store.events_for(&instance.id).await?;
    assert_eq!(
        event_types(&events)
            .iter()
            .filter(|event_type| **event_type == AUTO_RECOVERY_EXHAUSTED_EVENT)
            .count(),
        1,
        "exactly one AutoRecoveryExhausted per episode"
    );
    // Stop evidence is never deleted or masked by exhaustion.
    assert_eq!(
        after.data.pointer("/last_stop/stop_reason_code"),
        Some(&json!("rate_limited"))
    );
    Ok(())
}

// -- B-011: a fresh stop episode resets the attempt counter -----------------

#[tokio::test]
async fn auto_recovery_state_resets_for_new_stop_episode() -> anyhow::Result<()> {
    if !test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let _lock = test_helpers::HOME_LOCK.lock().await;
    let (_dir, store) = open_test_store("harness-test-ar-episode-").await?;
    let github = test_github_config(2);

    let mut data = transient_blocked_data("episode-2");
    // Exhausted counter from a previous stop episode.
    data["auto_recovery"] = json!({
        "episode_event_id": "episode-1",
        "attempts": 2,
        "exhausted": true,
    });
    let instance = seed_stopped_instance(&store, "ar-episode-1", "blocked", data).await?;

    let tick = run_auto_recovery_tick(&store, &github, Utc::now()).await?;
    assert_eq!(tick.recovered, 1, "new episode starts from attempt 1");
    let after = refreshed(&store, &instance.id).await?;
    assert_eq!(after.state, "implementing");
    Ok(())
}

// -- B-007: persisted backoff schedule ---------------------------------------

#[tokio::test]
async fn auto_recovery_state_honors_persisted_future_next_attempt() -> anyhow::Result<()> {
    if !test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let _lock = test_helpers::HOME_LOCK.lock().await;
    let (_dir, store) = open_test_store("harness-test-ar-backoff-").await?;
    let github = test_github_config(3);

    let now = Utc::now();
    let mut data = transient_blocked_data("episode-1");
    data["auto_recovery"] = json!({
        "episode_event_id": "episode-1",
        "attempts": 1,
        "last_outcome": "recheck_failed",
        "next_attempt_at": now + ChronoDuration::hours(1),
    });
    let instance = seed_stopped_instance(&store, "ar-backoff-1", "blocked", data).await?;

    // A fresh scheduler (e.g. after restart) derives the schedule entirely
    // from persisted state: no recheck fires before next_attempt_at.
    let early = run_auto_recovery_tick(&store, &github, now).await?;
    assert_eq!(early, AutoRecoveryTick::default());
    assert_eq!(refreshed(&store, &instance.id).await?.state, "blocked");

    // Once the persisted timestamp elapses the recheck runs.
    let due = run_auto_recovery_tick(&store, &github, now + ChronoDuration::hours(2)).await?;
    assert_eq!(due.recovered, 1);
    Ok(())
}

#[test]
fn auto_recovery_backoff_grows_exponentially_and_caps() {
    let policy = GitHubAutoRecoveryConfig {
        enabled: true,
        max_attempts: 16,
        initial_backoff_secs: 300,
        max_backoff_secs: 14400,
        jitter_ratio: 0.0,
        tick_interval_secs: 60,
    };
    assert_eq!(backoff_delay_secs(&policy, 1, 1.0), 300);
    assert_eq!(backoff_delay_secs(&policy, 2, 1.0), 600);
    assert_eq!(backoff_delay_secs(&policy, 3, 1.0), 1200);
    // Capped at max_backoff_secs; large attempt counts cannot overflow.
    assert_eq!(backoff_delay_secs(&policy, 8, 1.0), 14400);
    assert_eq!(backoff_delay_secs(&policy, 16, 1.0), 14400);
    assert_eq!(backoff_delay_secs(&policy, u32::MAX, 1.0), 14400);
}

#[test]
fn auto_recovery_jitter_factor_stays_within_configured_band() {
    for ratio in [0.0, 0.2, 1.0] {
        for seed in ["a", "workflow-1:episode-1:1", "workflow-2:episode-9:4"] {
            let factor = jitter_factor(seed, ratio);
            assert!(
                (1.0 - ratio..=1.0 + ratio).contains(&factor),
                "factor {factor} outside [1-{ratio}, 1+{ratio}] for seed {seed}"
            );
        }
    }
    // Deterministic for a fixed seed (stable across restarts).
    assert_eq!(jitter_factor("seed", 0.2), jitter_factor("seed", 0.2));
}

// -- B-008: audit event precedes the transition; crash reconciliation --------

#[tokio::test]
async fn auto_recovery_audit_attempt_event_precedes_recovery_event() -> anyhow::Result<()> {
    if !test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let _lock = test_helpers::HOME_LOCK.lock().await;
    let (_dir, store) = open_test_store("harness-test-ar-audit-").await?;
    let github = test_github_config(3);
    let instance = seed_stopped_instance(
        &store,
        "ar-audit-1",
        "blocked",
        transient_blocked_data("episode-1"),
    )
    .await?;

    let tick = run_auto_recovery_tick(&store, &github, Utc::now()).await?;
    assert_eq!(tick.recovered, 1);
    let events = store.events_for(&instance.id).await?;
    let attempt_seq = events
        .iter()
        .find(|event| event.event_type == AUTO_RECOVERY_ATTEMPT_EVENT)
        .expect("attempt audit event exists")
        .sequence;
    let recovery_seq = events
        .iter()
        .find(|event| event.event_type == "WorkflowRuntimeUnblocked")
        .expect("recovery event exists")
        .sequence;
    let outcome = events
        .iter()
        .find(|event| event.event_type == AUTO_RECOVERY_OUTCOME_EVENT)
        .expect("outcome event exists");
    assert!(
        attempt_seq < recovery_seq,
        "audit is appended BEFORE the state transition"
    );
    assert_eq!(outcome.event["outcome"], json!("succeeded"));
    assert_eq!(outcome.event["episode_event_id"], json!("episode-1"));
    Ok(())
}

#[tokio::test]
async fn auto_recovery_reconciles_dangling_attempt_as_interrupted() -> anyhow::Result<()> {
    if !test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let _lock = test_helpers::HOME_LOCK.lock().await;
    let (_dir, store) = open_test_store("harness-test-ar-interrupt-").await?;
    let github = test_github_config(3);

    let mut data = transient_blocked_data("episode-1");
    // Simulates a crash between the attempt audit and its outcome.
    data["auto_recovery"] = json!({
        "episode_event_id": "episode-1",
        "attempts": 0,
        "pending_attempt_event_id": "evt-dangling",
        "pending_attempt_number": 1,
    });
    let instance = seed_stopped_instance(&store, "ar-interrupt-1", "blocked", data).await?;

    let now = Utc::now();
    let tick = run_auto_recovery_tick(&store, &github, now).await?;
    assert_eq!(tick.interrupted, 1);
    assert_eq!(
        tick.recovered, 0,
        "the reconciling tick does not retry immediately"
    );

    let after = refreshed(&store, &instance.id).await?;
    assert_eq!(after.state, "blocked");
    let attempt_state = instance_attempt_state(&after).expect("attempt state persists");
    assert_eq!(
        attempt_state.attempts, 1,
        "the interrupted attempt is consumed"
    );
    assert!(attempt_state.pending_attempt_event_id.is_none());
    assert_eq!(attempt_state.last_outcome.as_deref(), Some("interrupted"));
    assert!(attempt_state.next_attempt_at.is_some_and(|at| at > now));
    let events = store.events_for(&instance.id).await?;
    let outcome = events
        .iter()
        .find(|event| event.event_type == AUTO_RECOVERY_OUTCOME_EVENT)
        .expect("interrupted outcome recorded");
    assert_eq!(outcome.event["outcome"], json!("interrupted"));
    assert_eq!(outcome.event["attempt_event_id"], json!("evt-dangling"));
    Ok(())
}

// -- B-009: concurrent transitions supersede the attempt --------------------

#[tokio::test]
async fn auto_recovery_recover_race_records_superseded_without_consuming_attempt(
) -> anyhow::Result<()> {
    if !test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let _lock = test_helpers::HOME_LOCK.lock().await;
    let (_dir, store) = open_test_store("harness-test-ar-race-").await?;
    let github = test_github_config(3);
    let stale_snapshot = seed_stopped_instance(
        &store,
        "ar-race-1",
        "blocked",
        transient_blocked_data("episode-1"),
    )
    .await?;

    // Concurrent manual operator unblock wins between listing and the recheck.
    let manual = store
        .recover_stopped_instance(WorkflowRuntimeRecoveryRequest {
            workflow_id: &stale_snapshot.id,
            action: WorkflowRuntimeRecoveryAction::Unblock,
            reason: "manual unblock during pending recheck",
            actor: "operator",
        })
        .await?;
    assert!(matches!(
        manual,
        WorkflowRuntimeRecoveryOutcome::Recovered { .. }
    ));

    let outcome = process_instance(&store, &github, &stale_snapshot, Utc::now()).await?;
    assert_eq!(outcome, InstanceOutcome::Superseded);

    let after = refreshed(&store, &stale_snapshot.id).await?;
    assert_eq!(
        after.state, "implementing",
        "exactly one recovery transition"
    );
    assert!(
        instance_attempt_state(&after).is_none(),
        "no attempt consumed"
    );
    let events = store.events_for(&stale_snapshot.id).await?;
    let outcome_event = events
        .iter()
        .find(|event| event.event_type == AUTO_RECOVERY_OUTCOME_EVENT)
        .expect("superseded outcome recorded");
    assert_eq!(outcome_event.event["outcome"], json!("superseded"));
    assert_eq!(
        event_types(&events)
            .iter()
            .filter(|event_type| **event_type == "WorkflowRuntimeUnblocked")
            .count(),
        1,
        "no double recovery"
    );
    Ok(())
}

// -- B-013: manual recovery stays available while a recheck is pending ------

#[tokio::test]
async fn auto_recovery_manual_unblock_wins_during_pending_backoff() -> anyhow::Result<()> {
    if !test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let _lock = test_helpers::HOME_LOCK.lock().await;
    let (_dir, store) = open_test_store("harness-test-ar-manual-").await?;

    let mut data = transient_blocked_data("episode-1");
    data["auto_recovery"] = json!({
        "episode_event_id": "episode-1",
        "attempts": 1,
        "last_outcome": "recheck_failed",
        "next_attempt_at": Utc::now() + ChronoDuration::hours(1),
    });
    let instance = seed_stopped_instance(&store, "ar-manual-1", "blocked", data).await?;

    let outcome = store
        .recover_stopped_instance(WorkflowRuntimeRecoveryRequest {
            workflow_id: &instance.id,
            action: WorkflowRuntimeRecoveryAction::Unblock,
            reason: "manual unblock while backoff pending",
            actor: "operator",
        })
        .await?;
    assert!(matches!(
        outcome,
        WorkflowRuntimeRecoveryOutcome::Recovered { .. }
    ));
    let after = refreshed(&store, &instance.id).await?;
    assert_eq!(after.state, "implementing");
    assert!(
        instance_attempt_state(&after).is_none(),
        "manual recovery clears auto-recovery attempt state"
    );
    Ok(())
}

// -- B-015: a failed recheck consumes the attempt without a transition ------

#[tokio::test]
async fn auto_recovery_recheck_failure_increments_and_reschedules() -> anyhow::Result<()> {
    if !test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let _lock = test_helpers::HOME_LOCK.lock().await;
    let (_dir, store) = open_test_store("harness-test-ar-recheck-").await?;
    let github = test_github_config(3);

    // Structured transient stop pointing at a runtime job that no longer
    // exists: recovery cannot determine a supported stopped activity, so the
    // recheck fails without any state transition.
    let mut data = transient_blocked_data("episode-1");
    data["last_stop"]["runtime_job_id"] = json!("job-missing");
    let workflow = WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "blocked",
        WorkflowSubject::new("issue", "issue:ar-recheck-1"),
    )
    .with_id("ar-recheck-1".to_string())
    .with_data(data);
    store.upsert_instance(&workflow).await?;

    let now = Utc::now();
    let tick = run_auto_recovery_tick(&store, &github, now).await?;
    assert_eq!(tick.recheck_failed, 1);

    let after = refreshed(&store, &workflow.id).await?;
    assert_eq!(
        after.state, "blocked",
        "no state transition on failed recheck"
    );
    let attempt_state = instance_attempt_state(&after).expect("attempt state persists");
    assert_eq!(attempt_state.attempts, 1);
    assert_eq!(
        attempt_state.last_outcome.as_deref(),
        Some("recheck_failed")
    );
    let first_next = attempt_state.next_attempt_at.expect("rescheduled");
    assert!(first_next > now);

    // Second failure grows the backoff (exponential, B-007).
    let after_first = now + ChronoDuration::seconds(400);
    let tick = run_auto_recovery_tick(&store, &github, after_first).await?;
    assert_eq!(tick.recheck_failed, 1);
    let after = refreshed(&store, &workflow.id).await?;
    let attempt_state = instance_attempt_state(&after).expect("attempt state persists");
    assert_eq!(attempt_state.attempts, 2);
    let second_delay =
        (attempt_state.next_attempt_at.expect("rescheduled") - after_first).num_seconds();
    let first_delay = (first_next - now).num_seconds();
    assert!(
        second_delay > first_delay,
        "backoff must grow: first {first_delay}s, second {second_delay}s"
    );
    let events = store.events_for(&workflow.id).await?;
    assert_eq!(
        event_types(&events)
            .iter()
            .filter(|event_type| **event_type == AUTO_RECOVERY_OUTCOME_EVENT)
            .count(),
        2
    );
    Ok(())
}
