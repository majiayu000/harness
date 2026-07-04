use super::*;

#[tokio::test]
async fn health_degraded_when_runtime_state_dirty() -> anyhow::Result<()> {
    use std::sync::atomic::Ordering;
    let dir = tempfile::tempdir()?;
    let state = make_read_only_route_test_state(dir.path()).await?;
    state.runtime_state_dirty.store(true, Ordering::Release);
    let health = call_health(state).await?;
    assert_eq!(health.status, "degraded");
    assert!(health.persistence.degraded_subsystems.is_empty());
    assert!(health.persistence.runtime_state_dirty);
    Ok(())
}

#[tokio::test]
async fn health_runtime_logs_reports_active_path_hint() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut state = make_read_only_route_test_state(dir.path()).await?;
    let state_mut = Arc::get_mut(&mut state).expect("unique state");
    let server = Arc::get_mut(&mut state_mut.core.server).expect("unique server");
    let active_path = dir
        .path()
        .join("logs/harness-serve-20260430T120000Z-pid1.log");
    let expected_path = active_path.to_string_lossy().into_owned();
    server.runtime_logs = crate::server::RuntimeLogMetadata::enabled(active_path, 45);

    let health = call_health(state).await?;
    assert_eq!(health.runtime_logs.state, "enabled");
    assert_eq!(
        health.runtime_logs.path_hint.as_deref(),
        Some(expected_path.as_str())
    );
    assert_eq!(health.runtime_logs.retention_days, 45);
    Ok(())
}

#[tokio::test]
async fn health_runtime_logs_can_report_degraded_without_raw_error_text() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut state = make_read_only_route_test_state(dir.path()).await?;
    let state_mut = Arc::get_mut(&mut state).expect("unique state");
    let server = Arc::get_mut(&mut state_mut.core.server).expect("unique server");
    server.runtime_logs = crate::server::RuntimeLogMetadata::degraded(
        Some("logs/harness-serve-20260430T120000Z-pid1.log".to_string()),
        7,
    );

    let health = call_health(state).await?;
    assert_eq!(health.runtime_logs.state, "degraded");
    assert_eq!(
        health.runtime_logs.path_hint.as_deref(),
        Some("logs/harness-serve-20260430T120000Z-pid1.log")
    );
    assert_eq!(health.runtime_logs.retention_days, 7);
    assert!(!format!("{health:?}").contains("permission denied"));
    Ok(())
}

#[tokio::test]
async fn health_degraded_when_runtime_circuit_breaker_open() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let state = make_read_only_route_test_state(dir.path()).await?;
    let now = Utc::now();
    for index in 0..5 {
        state.runtime_circuit_breakers.record_failure(
            "codex-high",
            &format!("job-{index}"),
            crate::runtime_circuit_breaker::FailureClass::ZeroOutputSpawnFailure,
            now,
        );
    }

    let health = call_health(state).await?;

    assert_eq!(health.status, "degraded");
    let breakers = health.runtime["circuit_breakers"]
        .as_array()
        .expect("runtime circuit breakers should be an array");
    assert_eq!(breakers.len(), 1);
    assert_eq!(breakers[0]["profile"], "codex-high");
    assert_eq!(breakers[0]["state"], "open");
    assert_eq!(breakers[0]["class"], "zero-output-spawn-failure");
    assert_eq!(breakers[0]["consecutive"], 5);
    assert!(breakers[0]["cooldown_until"].is_string());
    Ok(())
}

#[tokio::test]
async fn health_degraded_when_required_isolation_tier_unavailable() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let mut state = make_read_only_route_test_state(dir.path()).await?;
    let state_mut = Arc::get_mut(&mut state).expect("unique state");
    let server = Arc::get_mut(&mut state_mut.core.server).expect("unique server");
    server.config.isolation.rules = vec![harness_core::config::isolation::IsolationRule {
        trust: harness_core::config::isolation::IsolationTrustClass::NonCollaborator,
        tier: harness_core::config::isolation::IsolationTier::Container,
    }];
    state_mut.isolation_availability =
        harness_core::config::isolation::IsolationAvailability::new(vec![
            harness_core::config::isolation::IsolationTierStatus::available(
                harness_core::config::isolation::IsolationTier::Host,
            ),
            harness_core::config::isolation::IsolationTierStatus::unavailable(
                harness_core::config::isolation::IsolationTier::Container,
                "docker missing",
            ),
        ]);

    let health = call_health(state).await?;

    assert_eq!(health.status, "degraded");
    assert!(health.persistence.degraded_subsystems.is_empty());
    assert_eq!(
        health.isolation["unavailable_required_tiers"][0]["tier"],
        "container"
    );
    assert_eq!(
        health.isolation["unavailable_required_tiers"][0]["reason"],
        "docker missing"
    );
    Ok(())
}

#[tokio::test]
async fn health_startup_errors_are_redacted() -> anyhow::Result<()> {
    let _home_lock = crate::test_helpers::HOME_LOCK.lock().await;
    let dir = tempfile::tempdir()?;
    let mut state = make_read_only_route_test_state(dir.path()).await?;
    let state_mut = Arc::get_mut(&mut state).unwrap();
    state_mut.degraded_subsystems = vec!["review_store"];
    state_mut.startup_statuses =
        vec![
            crate::http::state::StoreStartupResult::optional("review_store")
                .failed("failed to connect to postgres://user:secret@db.internal/harness"),
        ];

    let health = call_health(state).await?;
    assert_eq!(health.status, "degraded");
    assert_eq!(health.persistence.startup.stores.len(), 1);
    let store = &health.persistence.startup.stores[0];
    assert_eq!(store.name, "review_store");
    assert!(!store.critical);
    assert!(!store.ready);
    assert_eq!(store.error.as_deref(), Some("database_unavailable"));
    assert!(
        !format!("{health:?}").contains("secret"),
        "health response must not expose raw startup error text"
    );
    Ok(())
}

#[tokio::test]
async fn health_degraded_multiple_subsystems() -> anyhow::Result<()> {
    let _home_lock = crate::test_helpers::HOME_LOCK.lock().await;
    let dir = tempfile::tempdir()?;
    let mut state = make_read_only_route_test_state(dir.path()).await?;
    Arc::get_mut(&mut state).unwrap().startup_statuses = vec![
        crate::http::state::StoreStartupResult::optional("eval_store")
            .failed("pool timed out while waiting for an open connection"),
        crate::http::state::StoreStartupResult::optional("runtime_state_store")
            .failed("runtime state snapshot restore failed"),
    ];
    Arc::get_mut(&mut state).unwrap().degraded_subsystems =
        vec!["eval_store", "runtime_state_store"];
    let health = call_health(state).await?;
    assert_eq!(health.status, "degraded");
    assert_eq!(
        health.persistence.degraded_subsystems,
        ["eval_store", "runtime_state_store"]
    );
    assert_eq!(health.persistence.startup.stores.len(), 2);
    Ok(())
}

#[tokio::test]
async fn health_degraded_both_conditions() -> anyhow::Result<()> {
    use std::sync::atomic::Ordering;
    let _home_lock = crate::test_helpers::HOME_LOCK.lock().await;
    let dir = tempfile::tempdir()?;
    let mut state = make_read_only_route_test_state(dir.path()).await?;
    Arc::get_mut(&mut state).unwrap().degraded_subsystems = vec!["workspace_manager"];
    state.runtime_state_dirty.store(true, Ordering::Release);
    let health = call_health(state).await?;
    assert_eq!(health.status, "degraded");
    assert_eq!(
        health.persistence.degraded_subsystems,
        ["workspace_manager"]
    );
    assert!(health.persistence.runtime_state_dirty);
    Ok(())
}

#[tokio::test]
async fn health_reports_critical_store_failure_details() -> anyhow::Result<()> {
    let _home_lock = crate::test_helpers::HOME_LOCK.lock().await;
    let dir = tempfile::tempdir()?;
    let mut state = make_read_only_route_test_state(dir.path()).await?;
    Arc::get_mut(&mut state).unwrap().startup_statuses = vec![
        crate::http::state::StoreStartupResult::critical("event_store")
            .failed("failed to open Postgres bootstrap pool"),
    ];
    Arc::get_mut(&mut state).unwrap().degraded_subsystems = vec!["event_store"];
    let health = call_health(state).await?;
    assert_eq!(health.status, "degraded");
    assert_eq!(health.persistence.startup.stores.len(), 1);
    assert_eq!(health.persistence.startup.stores[0].name, "event_store");
    assert!(health.persistence.startup.stores[0].critical);
    assert!(!health.persistence.startup.stores[0].ready);
    Ok(())
}

#[tokio::test]
async fn token_usage_route_is_registered() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_read_only_route_test_state(dir.path()).await?;
    let app = token_usage_app(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/token-usage")
                .body(Body::empty())?,
        )
        .await?;

    assert_ne!(response.status(), StatusCode::NOT_FOUND);
    assert!(
        response.status() == StatusCode::OK
            || response.status() == StatusCode::INTERNAL_SERVER_ERROR
    );
    Ok(())
}
