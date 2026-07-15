use super::runtime_hosts_workflow_api_tests as support;
use axum::{body::Body, http::Request};
use chrono::Utc;
use harness_workflow::runtime::{RuntimeJobStatus, RuntimeKind};
use serde_json::json;
use std::sync::Arc;
use tokio::sync::{oneshot, Barrier};
use tower::ServiceExt;

#[tokio::test]
async fn runtime_job_lease_renewal_for_draining_host_is_audited_and_sanitized() -> anyhow::Result<()>
{
    let dir = tempfile::tempdir()?;
    let Some((state, store)) = support::make_test_state_with_runtime_store(dir.path()).await?
    else {
        return Ok(());
    };
    let app = support::runtime_hosts_workflow_app(state.clone());
    support::register_host(&app, "host-a").await?;
    let job = support::enqueue_runtime_host_test_job(
        &store,
        "draining-renewal",
        RuntimeKind::RemoteHost,
        "remote-host-default",
        json!({ "activity": "remote_check" }),
    )
    .await?;
    let claimed = support::post_json(
        &app,
        "/api/runtime-hosts/host-a/runtime-jobs/claim".to_string(),
        json!({ "lease_secs": 60 }),
    )
    .await?;
    assert_eq!(
        state.runtime_hosts.mark_draining("host-a"),
        Some(crate::runtime_hosts::RuntimeHostLifecycle::Active)
    );

    let request = json!({
        "lease_generation": claimed["lease_generation"],
        "lease_expires_at": claimed["lease_expires_at"],
        "renewal_id": uuid::Uuid::new_v4(),
        "lease_secs": 60,
    });
    let (status, body) = support::post_json_with_status(
        &app,
        format!(
            "/api/runtime-hosts/host-a/runtime-jobs/{}/lease/renew",
            job.id
        ),
        request.clone(),
    )
    .await?;
    assert_eq!(status, axum::http::StatusCode::CONFLICT);
    assert_eq!(
        body,
        json!({ "error_code": "lease_lost", "must_stop": true })
    );
    let events = store.runtime_events_for(&job.id).await?;
    let rejected = events.last().expect("draining rejection must be audited");
    assert_eq!(rejected.event_type, "RuntimeJobLeaseRenewalRejected");
    assert_eq!(rejected.event["reason"], "host_draining");

    let (status, _) = support::post_json_with_status(
        &app,
        "/api/runtime-hosts/host-a/runtime-jobs/missing/lease/renew".to_string(),
        request,
    )
    .await?;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
    assert_eq!(store.runtime_events_for(&job.id).await?.len(), events.len());
    Ok(())
}

#[tokio::test]
async fn runtime_job_lease_renew_route_requires_api_authentication() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some((mut state, _store)) = support::make_test_state_with_runtime_store(dir.path()).await?
    else {
        return Ok(());
    };
    let state_mut = Arc::get_mut(&mut state).expect("test state must be uniquely owned");
    let mut config = state_mut.core.server.config.clone();
    config.server.api_token = Some("runtime-host-secret".to_string());
    config.server.allow_unauthenticated = false;
    state_mut.core.server = Arc::new(crate::server::HarnessServer::new(
        config,
        crate::thread_manager::ThreadManager::new(),
        harness_agents::registry::AgentRegistry::new("test"),
    ));
    let app = support::runtime_hosts_workflow_app(state.clone()).layer(
        axum::middleware::from_fn_with_state(state, crate::http::auth::api_auth_middleware),
    );
    let body = json!({
        "lease_generation": 1,
        "lease_expires_at": Utc::now(),
        "renewal_id": uuid::Uuid::new_v4(),
        "lease_secs": 60,
    });

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/runtime-hosts/host-a/runtime-jobs/missing/lease/renew")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))?,
        )
        .await?;
    assert_eq!(response.status(), axum::http::StatusCode::UNAUTHORIZED);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/runtime-hosts/host-a/runtime-jobs/missing/lease/renew")
                .header("content-type", "application/json")
                .header("authorization", "Bearer runtime-host-secret")
                .body(Body::from(body.to_string()))?,
        )
        .await?;
    assert_eq!(response.status(), axum::http::StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test]
async fn runtime_host_operation_boundary_orders_claim_before_deregister() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some((state, store)) = support::make_test_state_with_runtime_store(dir.path()).await?
    else {
        return Ok(());
    };
    state
        .runtime_hosts
        .register("host-a".to_string(), None, vec![]);
    let job = support::enqueue_runtime_host_test_job(
        &store,
        "claim-deregister-order",
        RuntimeKind::RemoteHost,
        "remote-host-default",
        json!({ "activity": "remote_check" }),
    )
    .await?;
    let held = state.runtime_hosts.lock_operation("host-a").await;

    let claim_barrier = Arc::new(Barrier::new(2));
    let (claim_waiting_tx, claim_waiting_rx) = oneshot::channel();
    let claim_manager = state.runtime_hosts.clone();
    let claim_store = store.clone();
    let claim_barrier_task = claim_barrier.clone();
    let claim = tokio::spawn(async move {
        claim_barrier_task.wait().await;
        claim_waiting_tx
            .send(())
            .expect("claim waiter receiver must exist");
        let _operation = claim_manager.lock_operation("host-a").await;
        assert!(claim_manager.is_active("host-a"));
        claim_store
            .claim_next_runtime_job_for_runtime_kind(
                RuntimeKind::RemoteHost,
                "host-a",
                Utc::now() + chrono::TimeDelta::seconds(60),
            )
            .await
    });
    claim_barrier.wait().await;
    claim_waiting_rx.await?;
    tokio::task::yield_now().await;

    let drain_barrier = Arc::new(Barrier::new(2));
    let drain_manager = state.runtime_hosts.clone();
    let drain_store = store.clone();
    let drain_barrier_task = drain_barrier.clone();
    let drain = tokio::spawn(async move {
        drain_barrier_task.wait().await;
        let _operation = drain_manager.lock_operation("host-a").await;
        assert_eq!(
            drain_manager.mark_draining("host-a"),
            Some(crate::runtime_hosts::RuntimeHostLifecycle::Active)
        );
        let revoked = drain_store
            .revoke_remote_host_runtime_job_leases("host-a", Utc::now())
            .await?;
        assert_eq!(revoked, 1);
        assert!(drain_manager.deregister("host-a"));
        anyhow::Ok(())
    });
    drain_barrier.wait().await;
    drop(held);

    let claimed = claim
        .await??
        .expect("admitted claim must commit before draining");
    assert_eq!(claimed.id, job.id);
    drain.await??;
    let persisted = store
        .get_runtime_job(&job.id)
        .await?
        .expect("revoked job must remain persisted");
    assert_eq!(persisted.status, RuntimeJobStatus::Pending);
    assert!(persisted.lease.is_none());
    let events = store.runtime_events_for(&job.id).await?;
    assert_eq!(events[0].event_type, "RuntimeJobClaimed");
    assert_eq!(events[1].event_type, "RuntimeJobLeaseRevoked");
    Ok(())
}

#[tokio::test]
async fn runtime_host_partial_deregister_remains_draining_and_retryable() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some((state, store)) = support::make_test_state_with_runtime_store(dir.path()).await?
    else {
        return Ok(());
    };
    let app = support::runtime_hosts_workflow_app(state.clone());
    support::register_host(&app, "host-a").await?;
    sqlx::query("DROP TABLE runtime_jobs CASCADE")
        .execute(store.pool())
        .await?;

    for _ in 0..2 {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/runtime-hosts/host-a/deregister")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(
            response.status(),
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            state.runtime_hosts.lifecycle("host-a"),
            Some(crate::runtime_hosts::RuntimeHostLifecycle::Draining)
        );
    }
    Ok(())
}
