use super::runtime_hosts_workflow_api_tests as support;
use axum::{body::Body, http::Request};
use chrono::Utc;
use harness_workflow::runtime::{ActivityResult, RuntimeJobStatus, RuntimeKind};
use serde_json::json;
use std::future::{poll_fn, Future};
use std::pin::Pin;
use std::sync::Arc;
use std::task::Poll;
use tokio::sync::{oneshot, Barrier};
use tower::ServiceExt;

async fn poll_to_pending<F: Future>(mut future: Pin<&mut F>) {
    poll_fn(|context| match future.as_mut().poll(context) {
        Poll::Pending => Poll::Ready(()),
        Poll::Ready(_) => panic!("handler unexpectedly completed before race barrier release"),
    })
    .await;
}

async fn required_runtime_store_state(
    dir: &std::path::Path,
) -> anyhow::Result<(
    Arc<crate::http::AppState>,
    Arc<harness_workflow::runtime::WorkflowRuntimeStore>,
)> {
    if std::env::var_os("HARNESS_DATABASE_URL").is_none() {
        anyhow::bail!(
            "GH1602 PostgreSQL tests require HARNESS_DATABASE_URL pointing to an isolated disposable database"
        );
    }
    support::make_test_state_with_runtime_store(dir)
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "GH1602 PostgreSQL test database is configured but unavailable or timed out"
            )
        })
}

#[tokio::test]
async fn legacy_proofless_remote_completion_accepts_missing_generation() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, store) = required_runtime_store_state(dir.path()).await?;
    let app = support::runtime_hosts_workflow_app(state);
    support::register_host(&app, "legacy-host").await?;
    let job = support::enqueue_runtime_host_test_job(
        &store,
        "legacy-proofless-completion",
        RuntimeKind::RemoteHost,
        "remote-host-default",
        json!({ "activity": "remote_check" }),
    )
    .await?;
    let claimed = support::post_json(
        &app,
        "/api/runtime-hosts/legacy-host/runtime-jobs/claim".to_string(),
        json!({ "lease_secs": 60 }),
    )
    .await?;
    sqlx::query(
        "UPDATE runtime_job_lease_issuances
         SET lease_proof = NULL
         WHERE runtime_job_id = $1 AND lease_generation = $2",
    )
    .bind(&job.id)
    .bind(i64::try_from(job.lease_generation + 1)?)
    .execute(store.pool())
    .await?;

    let completed = support::post_json(
        &app,
        format!(
            "/api/runtime-hosts/legacy-host/runtime-jobs/{}/complete",
            job.id
        ),
        json!({
            "lease_expires_at": claimed["lease_expires_at"],
            "result": ActivityResult::succeeded("remote_check", "legacy completion"),
        }),
    )
    .await?;
    assert_eq!(completed["completed"], true);
    assert_eq!(completed["runtime_job"]["status"], "succeeded");
    Ok(())
}

#[tokio::test]
async fn stale_remote_completion_from_reclaimed_generation_is_dead_lettered() -> anyhow::Result<()>
{
    let dir = tempfile::tempdir()?;
    let (state, store) = required_runtime_store_state(dir.path()).await?;
    let app = support::runtime_hosts_workflow_app(state);
    support::register_host(&app, "host-a").await?;
    support::register_host(&app, "host-b").await?;
    let job = support::enqueue_runtime_host_test_job(
        &store,
        "stale-remote-completion",
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
    let claimed_generation = claimed["lease_generation"]
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("claim must return a lease generation"))?;
    let (missing_proof_renew_status, _) = support::post_json_with_status(
        &app,
        format!(
            "/api/runtime-hosts/host-a/runtime-jobs/{}/lease/renew",
            job.id
        ),
        json!({
            "lease_generation": claimed_generation,
            "lease_expires_at": claimed["lease_expires_at"],
            "renewal_id": uuid::Uuid::new_v4(),
            "lease_secs": 60,
        }),
    )
    .await?;
    assert_eq!(missing_proof_renew_status, axum::http::StatusCode::CONFLICT);
    let (missing_proof_completion_status, _) = support::post_json_with_status(
        &app,
        format!("/api/runtime-hosts/host-a/runtime-jobs/{}/complete", job.id),
        json!({
            "lease_generation": claimed_generation,
            "lease_expires_at": claimed["lease_expires_at"],
            "result": ActivityResult::succeeded("remote_check", "missing proof result"),
        }),
    )
    .await?;
    assert_eq!(
        missing_proof_completion_status,
        axum::http::StatusCode::CONFLICT
    );
    let (forged_renew_status, _) = support::post_json_with_status(
        &app,
        format!(
            "/api/runtime-hosts/host-a/runtime-jobs/{}/lease/renew",
            job.id
        ),
        json!({
            "lease_generation": claimed_generation,
            "lease_expires_at": claimed["lease_expires_at"],
            "lease_proof": uuid::Uuid::new_v4(),
            "renewal_id": uuid::Uuid::new_v4(),
            "lease_secs": 60,
        }),
    )
    .await?;
    assert_eq!(forged_renew_status, axum::http::StatusCode::CONFLICT);
    let renewed = support::post_json(
        &app,
        format!(
            "/api/runtime-hosts/host-a/runtime-jobs/{}/lease/renew",
            job.id
        ),
        json!({
            "lease_generation": claimed_generation,
            "lease_expires_at": claimed["lease_expires_at"],
            "lease_proof": claimed["lease_proof"],
            "renewal_id": uuid::Uuid::new_v4(),
            "lease_secs": 60,
        }),
    )
    .await?;
    let issued_expires_at = renewed["lease_expires_at"].clone();
    let issued_proof = renewed["lease_proof"].clone();
    let (forged_status, forged_body) = support::post_json_with_status(
        &app,
        format!("/api/runtime-hosts/host-a/runtime-jobs/{}/complete", job.id),
        json!({
            "lease_generation": claimed_generation,
            "lease_expires_at": issued_expires_at,
            "lease_proof": uuid::Uuid::new_v4(),
            "result": ActivityResult::succeeded("remote_check", "forged stale result"),
        }),
    )
    .await?;
    assert_eq!(forged_status, axum::http::StatusCode::CONFLICT);
    assert_eq!(forged_body["error_code"], "lease_lost");
    let (forged_dlq_count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM runtime_job_completions_dlq WHERE runtime_job_id = $1",
    )
    .bind(&job.id)
    .fetch_one(store.pool())
    .await?;
    assert_eq!(
        forged_dlq_count, 0,
        "unissued host lease must not poison DLQ"
    );

    let newer_generation = i64::try_from(claimed_generation + 1)?;
    sqlx::query(
        r#"UPDATE runtime_jobs
           SET status = 'cancelled',
               data = jsonb_set(
                   jsonb_set(
                       jsonb_set(data, '{status}', '"cancelled"'),
                       '{lease_generation}', to_jsonb($2::bigint)
                   ),
                   '{lease}', 'null'
               )
           WHERE id = $1"#,
    )
    .bind(&job.id)
    .bind(newer_generation)
    .execute(store.pool())
    .await?;

    let (status, body) = support::post_json_with_status(
        &app,
        format!("/api/runtime-hosts/host-a/runtime-jobs/{}/complete", job.id),
        json!({
            "lease_generation": claimed_generation,
            "lease_expires_at": issued_expires_at,
            "lease_proof": issued_proof,
            "result": ActivityResult::succeeded("remote_check", "stale remote result"),
        }),
    )
    .await?;
    assert_eq!(status, axum::http::StatusCode::CONFLICT);
    assert_eq!(body["error_code"], "lease_lost");
    assert_eq!(body["must_stop"], true);
    assert_eq!(body["dead_lettered"], true);
    let (dlq_count, dlq_generation): (i64, Option<i64>) = sqlx::query_as(
        "SELECT COUNT(*), MAX(lease_generation)
         FROM runtime_job_completions_dlq WHERE runtime_job_id = $1",
    )
    .bind(&job.id)
    .fetch_one(store.pool())
    .await?;
    assert_eq!(dlq_count, 1);
    assert_eq!(dlq_generation, Some(i64::try_from(claimed_generation)?));
    Ok(())
}

#[tokio::test]
async fn duplicate_remote_completion_does_not_enter_dead_letter() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, store) = required_runtime_store_state(dir.path()).await?;
    let app = support::runtime_hosts_workflow_app(state);
    support::register_host(&app, "host-a").await?;
    let job = support::enqueue_runtime_host_test_job(
        &store,
        "duplicate-remote-completion",
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
    let payload = json!({
        "lease_generation": claimed["lease_generation"],
        "lease_expires_at": claimed["lease_expires_at"],
        "lease_proof": claimed["lease_proof"],
        "result": ActivityResult::succeeded("remote_check", "completed once"),
    });
    let uri = format!("/api/runtime-hosts/host-a/runtime-jobs/{}/complete", job.id);
    assert_eq!(
        support::post_json(&app, uri.clone(), payload.clone()).await?["completed"],
        true
    );
    let (status, replay) = support::post_json_with_status(&app, uri, payload).await?;
    assert_eq!(status, axum::http::StatusCode::CONFLICT);
    assert_eq!(replay["dead_lettered"], false);
    let (dlq_count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM runtime_job_completions_dlq WHERE runtime_job_id = $1",
    )
    .bind(&job.id)
    .fetch_one(store.pool())
    .await?;
    assert_eq!(dlq_count, 0);
    Ok(())
}

#[tokio::test]
async fn expired_unreclaimed_remote_completion_enters_dead_letter() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, store) = required_runtime_store_state(dir.path()).await?;
    let app = support::runtime_hosts_workflow_app(state);
    support::register_host(&app, "host-a").await?;
    let job = support::enqueue_runtime_host_test_job(
        &store,
        "expired-unreclaimed-remote-completion",
        RuntimeKind::RemoteHost,
        "remote-host-default",
        json!({ "activity": "remote_check" }),
    )
    .await?;
    let claimed = support::post_json(
        &app,
        "/api/runtime-hosts/host-a/runtime-jobs/claim".to_string(),
        json!({ "lease_secs": 1 }),
    )
    .await?;
    tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;

    let (status, body) = support::post_json_with_status(
        &app,
        format!("/api/runtime-hosts/host-a/runtime-jobs/{}/complete", job.id),
        json!({
            "lease_generation": claimed["lease_generation"],
            "lease_expires_at": claimed["lease_expires_at"],
            "lease_proof": claimed["lease_proof"],
            "result": ActivityResult::succeeded("remote_check", "finished at lease boundary"),
        }),
    )
    .await?;
    assert_eq!(status, axum::http::StatusCode::CONFLICT);
    assert_eq!(body["dead_lettered"], true);
    let (dlq_count, dlq_generation): (i64, Option<i64>) = sqlx::query_as(
        "SELECT COUNT(*), MAX(lease_generation)
         FROM runtime_job_completions_dlq WHERE runtime_job_id = $1",
    )
    .bind(&job.id)
    .fetch_one(store.pool())
    .await?;
    assert_eq!(dlq_count, 1);
    assert_eq!(
        dlq_generation,
        Some(i64::try_from(
            claimed["lease_generation"]
                .as_u64()
                .expect("claim returns generation")
        )?)
    );
    let current = store.get_runtime_job(&job.id).await?.expect("job exists");
    assert_eq!(current.status, RuntimeJobStatus::Running);
    Ok(())
}

#[tokio::test]
async fn historical_dead_letter_generation_remains_explicitly_unknown() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (_state, store) = required_runtime_store_state(dir.path()).await?;
    sqlx::query(
        "INSERT INTO runtime_job_completions_dlq
            (id, runtime_job_id, owner, lease_expires_at, result)
         VALUES ($1, $1, $2, CURRENT_TIMESTAMP, $3::jsonb)",
    )
    .bind("historical-dlq-without-provenance")
    .bind("legacy-worker")
    .bind(json!({ "status": "succeeded", "summary": "legacy result" }))
    .execute(store.pool())
    .await?;
    let (lease_generation,): (Option<i64>,) =
        sqlx::query_as("SELECT lease_generation FROM runtime_job_completions_dlq WHERE id = $1")
            .bind("historical-dlq-without-provenance")
            .fetch_one(store.pool())
            .await?;
    assert_eq!(lease_generation, None);
    Ok(())
}

#[tokio::test]
async fn runtime_job_lease_renewal_for_draining_host_is_audited_and_sanitized() -> anyhow::Result<()>
{
    let dir = tempfile::tempdir()?;
    let (state, store) = required_runtime_store_state(dir.path()).await?;
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
        "lease_proof": claimed["lease_proof"],
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
    let (mut state, _store) = required_runtime_store_state(dir.path()).await?;
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
        "lease_proof": uuid::Uuid::new_v4(),
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
    let (state, store) = required_runtime_store_state(dir.path()).await?;
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
    let (state, store) = required_runtime_store_state(dir.path()).await?;
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

#[tokio::test]
async fn runtime_host_handler_barrier_orders_deregister_before_renew_without_orphan(
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, store) = required_runtime_store_state(dir.path()).await?;
    let app = support::runtime_hosts_workflow_app(state.clone());
    support::register_host(&app, "host-a").await?;
    let job = support::enqueue_runtime_host_test_job(
        &store,
        "deregister-renew-handler-race",
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
    let renewal_body = json!({
        "lease_generation": claimed["lease_generation"],
        "lease_expires_at": claimed["lease_expires_at"],
        "lease_proof": claimed["lease_proof"],
        "renewal_id": uuid::Uuid::new_v4(),
        "lease_secs": 60,
    });

    let barrier = state.runtime_hosts.lock_operation("host-a").await;
    let mut deregister = Box::pin(
        app.clone().oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/runtime-hosts/host-a/deregister")
                .body(Body::empty())?,
        ),
    );
    poll_to_pending(deregister.as_mut()).await;
    let mut renew = Box::pin(
        app.clone().oneshot(
            Request::builder()
                .method("POST")
                .uri(format!(
                    "/api/runtime-hosts/host-a/runtime-jobs/{}/lease/renew",
                    job.id
                ))
                .header("content-type", "application/json")
                .body(Body::from(renewal_body.to_string()))?,
        ),
    );
    poll_to_pending(renew.as_mut()).await;
    drop(barrier);

    let (deregister, renew) = tokio::join!(deregister, renew);
    assert_eq!(deregister?.status(), axum::http::StatusCode::OK);
    assert_eq!(renew?.status(), axum::http::StatusCode::NOT_FOUND);
    assert_eq!(state.runtime_hosts.lifecycle("host-a"), None);

    let persisted = store
        .get_runtime_job(&job.id)
        .await?
        .expect("deregistered job must remain reclaimable");
    assert_eq!(persisted.status, RuntimeJobStatus::Pending);
    assert!(persisted.lease.is_none());
    assert_eq!(
        store.count_remote_host_runtime_job_leases("host-a").await?,
        0
    );
    let events = store.runtime_events_for(&job.id).await?;
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "RuntimeJobLeaseRevoked")
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "RuntimeJobLeaseRenewed")
            .count(),
        0
    );
    Ok(())
}
