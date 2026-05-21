use super::super::*;
use super::helpers::{wait_for_captured_prompts, wait_until, PhaseCapturingAgent};
use tokio::time::Duration;

fn legacy_hosted_bot_server_config() -> std::sync::Arc<harness_core::config::HarnessConfig> {
    let mut config = harness_core::config::HarnessConfig::default();
    config.agents.review.review_bot_auto_trigger = true;
    std::sync::Arc::new(config)
}

#[tokio::test]
async fn resumed_pr_manual_conflict_fails_before_review_loop() -> anyhow::Result<()> {
    let _lock = crate::test_helpers::HOME_LOCK.lock().await;
    let dir = crate::test_helpers::tempdir_in_home("harness-test-")?;
    let database_url = crate::test_helpers::test_database_url()?;
    let store =
        TaskStore::open_with_database_url(&dir.path().join("tasks.db"), Some(&database_url))
            .await?;
    let skills = Arc::new(RwLock::new(harness_skills::store::SkillStore::new()));
    let events = Arc::new(
        harness_observe::event_store::EventStore::new_with_database_url(
            dir.path(),
            Some(&database_url),
        )
        .await?,
    );

    let agent = PhaseCapturingAgent::new(vec![
        "REBASE_CONFLICT paths=src/lib.rs\nPR_URL=https://github.com/owner/repo/pull/7".into(),
    ]);

    let req = CreateTaskRequest {
        pr: Some(7),
        project: Some(dir.path().to_path_buf()),
        wait_secs: 0,
        max_rounds: Some(1),
        turn_timeout_secs: 30,
        ..Default::default()
    };

    let queue = crate::task_queue::TaskQueue::unbounded();
    let permit = queue.acquire("test", 0).await?;
    let task_id = spawn_task(
        store.clone(),
        agent.clone(),
        None,
        Default::default(),
        skills,
        events,
        vec![],
        req,
        None,
        permit,
        None,
        None,
        None,
        vec![],
    )
    .await;

    wait_until(Duration::from_secs(15), || {
        store
            .get(&task_id)
            .is_some_and(|state| matches!(state.status, TaskStatus::Failed))
    })
    .await?;

    let phases = agent.captured_phases().await;
    assert_eq!(
        phases.len(),
        1,
        "rebase-conflict outcome must stop before the normal review loop"
    );
    let state = store.get(&task_id).expect("task should exist");
    assert!(
        state
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("manual resolution required"),
        "failure must preserve manual-resolution wording for intake safeguards"
    );
    Ok(())
}

#[tokio::test]
async fn resumed_pr_clean_conflict_gate_enters_review_loop() -> anyhow::Result<()> {
    let _lock = crate::test_helpers::HOME_LOCK.lock().await;
    let dir = crate::test_helpers::tempdir_in_home("harness-test-")?;
    let database_url = crate::test_helpers::test_database_url()?;
    let store =
        TaskStore::open_with_database_url(&dir.path().join("tasks.db"), Some(&database_url))
            .await?;
    let skills = Arc::new(RwLock::new(harness_skills::store::SkillStore::new()));
    let events = Arc::new(
        harness_observe::event_store::EventStore::new_with_database_url(
            dir.path(),
            Some(&database_url),
        )
        .await?,
    );

    let agent = PhaseCapturingAgent::new(vec![
        "REBASE_SKIPPED\nPR_URL=https://github.com/owner/repo/pull/8".into(),
        "LGTM".into(),
    ]);

    let req = CreateTaskRequest {
        pr: Some(8),
        project: Some(dir.path().to_path_buf()),
        wait_secs: 0,
        max_rounds: Some(1),
        turn_timeout_secs: 30,
        ..Default::default()
    };

    let queue = crate::task_queue::TaskQueue::unbounded();
    let permit = queue.acquire("test", 0).await?;
    let task_id = spawn_task(
        store.clone(),
        agent.clone(),
        None,
        legacy_hosted_bot_server_config(),
        skills,
        events,
        vec![],
        req,
        None,
        permit,
        None,
        None,
        None,
        vec![],
    )
    .await;

    wait_until(Duration::from_secs(15), || {
        store
            .get(&task_id)
            .is_some_and(|state| matches!(state.status, TaskStatus::Done))
    })
    .await?;

    let phases = agent.captured_phases().await;
    assert!(
        phases.len() >= 2,
        "clean pr:N prep must enter the review loop after rebase preparation"
    );
    Ok(())
}

#[tokio::test]
async fn resumed_pr_rebase_push_requires_fresh_review_prompt() -> anyhow::Result<()> {
    let _lock = crate::test_helpers::HOME_LOCK.lock().await;
    let dir = crate::test_helpers::tempdir_in_home("harness-test-")?;
    let database_url = crate::test_helpers::test_database_url()?;
    let store =
        TaskStore::open_with_database_url(&dir.path().join("tasks.db"), Some(&database_url))
            .await?;
    let skills = Arc::new(RwLock::new(harness_skills::store::SkillStore::new()));
    let events = Arc::new(
        harness_observe::event_store::EventStore::new_with_database_url(
            dir.path(),
            Some(&database_url),
        )
        .await?,
    );

    let agent = PhaseCapturingAgent::new(vec![
        "REBASE_PUSHED\nPR_URL=https://github.com/owner/repo/pull/9".into(),
        "LGTM".into(),
    ]);

    let req = CreateTaskRequest {
        pr: Some(9),
        project: Some(dir.path().to_path_buf()),
        wait_secs: 0,
        max_rounds: Some(1),
        turn_timeout_secs: 30,
        ..Default::default()
    };

    let queue = crate::task_queue::TaskQueue::unbounded();
    let permit = queue.acquire("test", 0).await?;
    let task_id = spawn_task(
        store.clone(),
        agent.clone(),
        None,
        legacy_hosted_bot_server_config(),
        skills,
        events,
        vec![],
        req,
        None,
        permit,
        None,
        None,
        None,
        vec![],
    )
    .await;

    wait_until(Duration::from_secs(15), || {
        store
            .get(&task_id)
            .is_some_and(|state| matches!(state.status, TaskStatus::Done))
    })
    .await?;

    let prompts = wait_for_captured_prompts(agent.as_ref(), 2).await;
    assert!(
        prompts
            .get(1)
            .is_some_and(|prompt| prompt.contains("IMPORTANT — New review verification")),
        "rebased pr:N tasks must require a fresh reviewer pass before accepting LGTM"
    );
    Ok(())
}
