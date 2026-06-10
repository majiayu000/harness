use super::super::*;
use super::helpers::{
    wait_for_captured_phases, wait_until, BlockingInterceptor, CapturingAgent, PhaseCapturingAgent,
};
use harness_core::types::{ContextItem, EventFilters, ExecutionPhase};
use tokio::time::Duration;

fn legacy_hosted_bot_server_config() -> std::sync::Arc<harness_core::config::HarnessConfig> {
    let mut config = harness_core::config::HarnessConfig::default();
    config.agents.review.strategy = harness_core::config::agents::ReviewStrategy::LegacyHostedBot;
    config.agents.review.review_bot_auto_trigger = true;
    std::sync::Arc::new(config)
}

#[tokio::test]
async fn skills_are_injected_into_agent_context() -> anyhow::Result<()> {
    let _lock = crate::test_helpers::HOME_LOCK.lock().await;
    let dir = crate::test_helpers::tempdir_in_home("harness-test-")?;
    let database_url = crate::test_helpers::test_database_url()?;
    let store =
        TaskStore::open_with_database_url(&dir.path().join("tasks.db"), Some(&database_url))
            .await?;

    let mut skill_store = harness_skills::store::SkillStore::new();
    skill_store.create(
        "test-skill".to_string(),
        "<!-- trigger-patterns: test task -->\ndo something useful".to_string(),
    );
    let skills = Arc::new(RwLock::new(skill_store));

    let agent = CapturingAgent::new();
    let agent_clone = agent.clone();

    let req = CreateTaskRequest {
        prompt: Some("test task".into()),
        issue: None,
        pr: None,
        agent: None,
        project: Some(dir.path().to_path_buf()),
        wait_secs: 0,
        max_rounds: Some(0),
        turn_timeout_secs: 30,
        max_budget_usd: None,
        ..Default::default()
    };

    let events = Arc::new(
        harness_observe::event_store::EventStore::new_with_database_url(
            dir.path(),
            Some(&database_url),
        )
        .await?,
    );
    let queue = crate::task_queue::TaskQueue::unbounded();
    let permit = queue.acquire("test", 0).await?;
    spawn_task(
        store,
        agent_clone,
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

    wait_until(Duration::from_secs(3), || {
        agent
            .captured
            .try_lock()
            .map(|captured| !captured.is_empty())
            .unwrap_or(false)
    })
    .await?;

    let captured = agent.captured.lock().await;
    assert!(
        !captured.is_empty(),
        "expected skills to be injected into AgentRequest.context"
    );
    assert!(
        captured
            .iter()
            .any(|item| matches!(item, ContextItem::Skill { .. })),
        "expected at least one ContextItem::Skill"
    );
    Ok(())
}

#[tokio::test]
async fn blocking_interceptor_fails_task() -> anyhow::Result<()> {
    let _lock = crate::test_helpers::HOME_LOCK.lock().await;
    let dir = crate::test_helpers::tempdir_in_home("harness-test-")?;
    let database_url = crate::test_helpers::test_database_url()?;
    let store =
        TaskStore::open_with_database_url(&dir.path().join("tasks.db"), Some(&database_url))
            .await?;
    let skills = Arc::new(RwLock::new(harness_skills::store::SkillStore::new()));
    let agent = CapturingAgent::new();
    let events = Arc::new(
        harness_observe::event_store::EventStore::new_with_database_url(
            dir.path(),
            Some(&database_url),
        )
        .await?,
    );

    let interceptors: Vec<Arc<dyn harness_core::interceptor::TurnInterceptor>> =
        vec![Arc::new(BlockingInterceptor)];

    let req = CreateTaskRequest {
        prompt: Some("blocked task".into()),
        issue: None,
        pr: None,
        agent: None,
        project: Some(dir.path().to_path_buf()),
        wait_secs: 0,
        max_rounds: Some(0),
        turn_timeout_secs: 30,
        max_budget_usd: None,
        ..Default::default()
    };

    let queue = crate::task_queue::TaskQueue::unbounded();
    let permit = queue.acquire("test", 0).await?;
    let task_id = spawn_task(
        store.clone(),
        agent,
        None,
        Default::default(),
        skills,
        events,
        interceptors,
        req,
        None,
        permit,
        None,
        None,
        None,
        vec![],
    )
    .await;

    wait_until(Duration::from_secs(3), || {
        store
            .get(&task_id)
            .is_some_and(|state| matches!(state.status, TaskStatus::Failed))
    })
    .await?;

    let state = store
        .get(&task_id)
        .ok_or_else(|| anyhow::anyhow!("task not found in store — possible concurrent deletion"))?;
    assert!(
        matches!(state.status, TaskStatus::Failed),
        "expected Failed, got {:?}",
        state.status
    );
    assert!(
        state
            .error
            .as_deref()
            .unwrap_or("")
            .contains("Blocked by interceptor"),
        "error message should mention blocked: {:?}",
        state.error
    );
    Ok(())
}

#[tokio::test]
async fn spawn_blocking_panic_surfaces_error_and_event() -> anyhow::Result<()> {
    let _lock = crate::test_helpers::HOME_LOCK.lock().await;
    let dir = crate::test_helpers::tempdir_in_home("harness-test-")?;
    let database_url = crate::test_helpers::test_database_url()?;
    let store =
        TaskStore::open_with_database_url(&dir.path().join("tasks.db"), Some(&database_url))
            .await?;
    let skills = Arc::new(RwLock::new(harness_skills::store::SkillStore::new()));
    let agent = CapturingAgent::new();
    let events = Arc::new(
        harness_observe::event_store::EventStore::new_with_database_url(
            dir.path(),
            Some(&database_url),
        )
        .await?,
    );

    let req = CreateTaskRequest {
        prompt: Some("panic path".into()),
        issue: None,
        pr: None,
        agent: None,
        project: None,
        wait_secs: 0,
        max_rounds: Some(0),
        turn_timeout_secs: 30,
        max_budget_usd: None,
        ..Default::default()
    };

    let queue = crate::task_queue::TaskQueue::unbounded();
    let permit = queue.acquire("test", 0).await?;
    let task_id = spawn_task_with_worktree_detector(
        store.clone(),
        agent,
        None,
        Default::default(),
        skills,
        events.clone(),
        vec![],
        req,
        || -> PathBuf {
            panic!("forced detect_main_worktree panic");
        },
        None,
        permit,
        None,
        None,
        None,
        vec![],
        None,
        None,
    )
    .await;

    wait_until(Duration::from_secs(3), || {
        store
            .get(&task_id)
            .is_some_and(|state| matches!(state.status, TaskStatus::Failed))
    })
    .await?;

    let state = store
        .get(&task_id)
        .ok_or_else(|| anyhow::anyhow!("task not found in store — possible concurrent deletion"))?;
    assert!(
        matches!(state.status, TaskStatus::Failed),
        "expected Failed, got {:?}",
        state.status
    );
    let error = state.error.unwrap_or_default();
    assert!(
        error.contains("detect_main_worktree panicked"),
        "expected panic reason in task error, got: {error}"
    );
    assert!(
        error.contains("forced detect_main_worktree panic"),
        "expected panic payload in task error, got: {error}"
    );

    let expected_detail = format!("task_id={}", task_id.0);
    let failure_events = events
        .query(&EventFilters {
            hook: Some("task_failure".to_string()),
            ..Default::default()
        })
        .await?;
    assert!(
        failure_events.iter().any(|event| {
            event.detail.as_deref() == Some(expected_detail.as_str())
                && event
                    .reason
                    .as_deref()
                    .unwrap_or_default()
                    .contains("forced detect_main_worktree panic")
        }),
        "expected task_failure event containing panic payload, got: {:?}",
        failure_events
    );
    Ok(())
}

#[tokio::test]
async fn execution_phase_is_set_on_initial_implementation_turn() -> anyhow::Result<()> {
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

    // Agent returns empty output (no PR URL) → task completes after implementation.
    let agent = PhaseCapturingAgent::new(vec![String::new()]);
    let agent_clone = agent.clone();

    let req = CreateTaskRequest {
        prompt: Some("implement something".into()),
        project: Some(dir.path().to_path_buf()),
        wait_secs: 0,
        max_rounds: Some(0),
        turn_timeout_secs: 30,
        ..Default::default()
    };

    let queue = crate::task_queue::TaskQueue::unbounded();
    let permit = queue.acquire("test", 0).await?;
    spawn_task(
        store,
        agent_clone,
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

    let phases = wait_for_captured_phases(agent.as_ref(), 1).await;
    assert!(
        !phases.is_empty(),
        "expected at least one agent call, got none"
    );
    assert_eq!(
        phases[0],
        Some(ExecutionPhase::Execution),
        "initial implementation turn must use Execution phase"
    );
    Ok(())
}

#[tokio::test]
async fn validation_phase_is_set_on_review_loop_turns() -> anyhow::Result<()> {
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

    // Call 1 (execute_stream): return a PR URL to trigger the review loop.
    // Call 2 (execute): return LGTM to complete the review loop.
    let agent = PhaseCapturingAgent::new(vec![
        "PR_URL=https://github.com/owner/repo/pull/1".into(),
        "LGTM".into(),
    ]);
    let agent_clone = agent.clone();

    let req = CreateTaskRequest {
        prompt: Some("implement something".into()),
        project: Some(dir.path().to_path_buf()),
        wait_secs: 0,
        max_rounds: Some(1),
        turn_timeout_secs: 30,
        ..Default::default()
    };

    let queue = crate::task_queue::TaskQueue::unbounded();
    let permit = queue.acquire("test", 0).await?;
    spawn_task(
        store,
        agent_clone,
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

    let phases = wait_for_captured_phases(agent.as_ref(), 2).await;
    assert!(
        phases.len() >= 2,
        "expected at least 2 agent calls (implementation + review check), got {}",
        phases.len()
    );
    assert_eq!(
        phases[0],
        Some(ExecutionPhase::Execution),
        "implementation turn must use Execution phase"
    );
    assert_eq!(
        phases[1],
        Some(ExecutionPhase::Execution),
        "review loop turn must use Execution phase (agent needs write access to fix bot comments)"
    );
    Ok(())
}
