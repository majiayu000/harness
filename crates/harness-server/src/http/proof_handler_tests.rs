use super::*;
use crate::task_runner::{RoundResult, TaskState};
use harness_core::proof_of_work::{ACTION_AGENT_REVIEW, RESULT_APPROVED, RESULT_QUOTA_EXHAUSTED};
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Pure-logic unit tests (proof_from_state helper — no DB, no async)
// ---------------------------------------------------------------------------

fn make_state_with_rounds(status: TaskStatus, rounds: Vec<RoundResult>) -> TaskState {
    let mut s = TaskState::new(harness_core::types::TaskId("test-id".to_string()));
    s.status = status;
    s.rounds = rounds;
    s
}

fn review_round(result: &str, detail: Option<&str>) -> RoundResult {
    RoundResult {
        turn: 1,
        action: ACTION_REVIEW.to_string(),
        result: result.to_string(),
        detail: detail.map(|d| d.to_string()),
        first_token_latency_ms: None,
    }
}

fn agent_review_round(result: &str, detail: Option<&str>) -> RoundResult {
    RoundResult {
        turn: 0,
        action: ACTION_AGENT_REVIEW.to_string(),
        result: result.to_string(),
        detail: detail.map(|d| d.to_string()),
        first_token_latency_ms: None,
    }
}

#[test]
fn from_task_done_with_approved_review() {
    let rounds = vec![
        review_round("fixed", None),
        review_round(RESULT_LGTM, Some("all good")),
    ];
    let state = make_state_with_rounds(TaskStatus::Done, rounds);
    let proof = proof_from_state(&state);

    assert_eq!(proof.review_outcome, ReviewOutcome::Approved);
    assert_eq!(proof.ci_status, CiStatus::Passed);
    assert_eq!(proof.review_rounds, 2);
    assert_eq!(proof.final_review_detail.as_deref(), Some("all good"));
}

#[test]
fn from_task_done_ci_failed_no_lgtm() {
    let rounds = vec![review_round("fixed", None)];
    let state = make_state_with_rounds(TaskStatus::Failed, rounds);
    let proof = proof_from_state(&state);

    assert_eq!(proof.review_outcome, ReviewOutcome::ChangesRequested);
    assert_eq!(proof.ci_status, CiStatus::Failed);
}

#[test]
fn from_task_no_review_rounds() {
    let state = make_state_with_rounds(TaskStatus::Done, vec![]);
    let proof = proof_from_state(&state);

    assert_eq!(proof.review_outcome, ReviewOutcome::Skipped);
    assert_eq!(proof.ci_status, CiStatus::Unknown);
    assert_eq!(proof.review_rounds, 0);
    assert!(proof.final_review_detail.is_none());
}

#[test]
fn from_task_periodic_review_not_applicable() {
    let rounds = vec![review_round(RESULT_COMPLETED, Some("weekly summary"))];
    let state = make_state_with_rounds(TaskStatus::Done, rounds);
    let proof = proof_from_state(&state);

    assert_eq!(proof.review_outcome, ReviewOutcome::NotApplicable);
    assert_eq!(proof.ci_status, CiStatus::Unknown);
}

#[test]
fn from_task_agent_review_approved() {
    // When review_bot_auto_trigger is disabled the executor uses agent_review.
    // Rounds carry action="agent_review" and result="approved" instead of "lgtm".
    let rounds = vec![
        agent_review_round("2 issues", None),
        agent_review_round(RESULT_APPROVED, Some("agent signed off")),
    ];
    let state = make_state_with_rounds(TaskStatus::Done, rounds);
    let proof = proof_from_state(&state);

    assert_eq!(proof.review_outcome, ReviewOutcome::Approved);
    assert_eq!(proof.ci_status, CiStatus::Passed);
    assert_eq!(proof.review_rounds, 2);
    assert_eq!(
        proof.final_review_detail.as_deref(),
        Some("agent signed off")
    );
}

#[test]
fn from_task_quota_heuristic_graduation() {
    // Quota-heuristic: external reviewer quota exhausted on every round,
    // test gate passed → task is Done. Must report Approved/Passed, not
    // ChangesRequested/Unknown.
    let rounds = vec![
        review_round(RESULT_QUOTA_EXHAUSTED, None),
        review_round(RESULT_QUOTA_EXHAUSTED, None),
        review_round(RESULT_QUOTA_EXHAUSTED, None),
    ];
    let state = make_state_with_rounds(TaskStatus::Done, rounds);
    let proof = proof_from_state(&state);

    assert_eq!(proof.review_outcome, ReviewOutcome::Approved);
    assert_eq!(proof.ci_status, CiStatus::Passed);
    assert_eq!(proof.review_rounds, 3);
}

// ---------------------------------------------------------------------------
// Route-level integration tests — single AppState to stay under pool limit
// ---------------------------------------------------------------------------

async fn make_proof_state(dir: &std::path::Path) -> anyhow::Result<Arc<AppState>> {
    let config = harness_core::config::HarnessConfig::default();
    let thread_manager = crate::thread_manager::ThreadManager::new();
    let agent_registry = harness_agents::registry::AgentRegistry::new("test");
    let server = Arc::new(crate::server::HarnessServer::new(
        config,
        thread_manager,
        agent_registry,
    ));
    let tasks = crate::task_runner::TaskStore::open(&harness_core::config::dirs::default_db_path(
        dir, "tasks",
    ))
    .await?;
    let events = Arc::new(harness_observe::event_store::EventStore::new(dir).await?);
    let signal_detector = harness_gc::signal_detector::SignalDetector::new(
        server.config.gc.signal_thresholds.clone().into(),
        harness_core::types::ProjectId::new(),
    );
    let draft_store = harness_gc::draft_store::DraftStore::new(dir)?;
    let gc_agent = Arc::new(harness_gc::gc_agent::GcAgent::new(
        server.config.gc.clone(),
        signal_detector,
        draft_store,
        dir.to_path_buf(),
    ));
    let thread_db = crate::thread_db::ThreadDb::open(&harness_core::config::dirs::default_db_path(
        dir, "threads",
    ))
    .await?;
    let project_svc_tmp = crate::project_registry::ProjectRegistry::open(
        &harness_core::config::dirs::default_db_path(dir, "projects"),
    )
    .await?;
    let project_svc =
        crate::services::project::DefaultProjectService::new(project_svc_tmp, dir.to_path_buf());
    let task_svc = crate::services::task::DefaultTaskService::new(tasks.clone());
    let execution_svc = crate::services::execution::DefaultExecutionService::new(
        tasks.clone(),
        server.agent_registry.clone(),
        Arc::new(server.config.clone()),
        Default::default(),
        events.clone(),
        vec![],
        None,
        Arc::new(crate::task_queue::TaskQueue::new(&Default::default())),
        None,
        None,
        vec![],
    );
    Ok(Arc::new(AppState {
        core: crate::http::CoreServices {
            server,
            project_root: dir.to_path_buf(),
            home_dir: std::env::var("HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| dir.to_path_buf()),
            tasks,
            thread_db: Some(thread_db),
            plan_db: None,
            plan_cache: Arc::new(dashmap::DashMap::new()),
            project_registry: None,
            runtime_state_store: None,
            q_values: None,
            maintenance_active: Arc::new(AtomicBool::new(false)),
        },
        engines: crate::http::EngineServices {
            skills: Arc::new(tokio::sync::RwLock::new(
                harness_skills::store::SkillStore::new(),
            )),
            rules: Arc::new(tokio::sync::RwLock::new(
                harness_rules::engine::RuleEngine::new(),
            )),
            gc_agent,
        },
        observability: crate::http::ObservabilityServices {
            events,
            signal_rate_limiter: Arc::new(crate::http::rate_limit::SignalRateLimiter::new(100)),
            password_reset_rate_limiter: Arc::new(
                crate::http::rate_limit::PasswordResetRateLimiter::new(5),
            ),
            review_store: None,
        },
        concurrency: crate::http::ConcurrencyServices {
            task_queue: Arc::new(crate::task_queue::TaskQueue::new(&Default::default())),
            workspace_mgr: None,
        },
        runtime_hosts: Arc::new(crate::runtime_hosts::RuntimeHostManager::new()),
        runtime_project_cache: Arc::new(
            crate::runtime_project_cache::RuntimeProjectCacheManager::new(),
        ),
        runtime_state_persist_lock: tokio::sync::Mutex::new(()),
        runtime_state_dirty: AtomicBool::new(false),
        notifications: crate::http::NotificationServices {
            notification_tx: tokio::sync::broadcast::channel(32).0,
            notification_lagged_total: Arc::new(AtomicU64::new(0)),
            notification_lag_log_every: 1,
            notify_tx: None,
            initializing: Arc::new(AtomicBool::new(true)),
            initialized: Arc::new(AtomicBool::new(true)),
            ws_shutdown_tx: tokio::sync::broadcast::channel(1).0,
        },
        interceptors: vec![],
        degraded_subsystems: vec![],
        intake: crate::http::IntakeServices {
            feishu_intake: None,
            github_pollers: vec![],
            completion_callback: None,
        },
        project_svc,
        task_svc,
        execution_svc,
    }))
}

fn proof_route(state: Arc<AppState>) -> axum::Router {
    axum::Router::new()
        .route("/tasks/{id}/proof", axum::routing::get(get_task_proof))
        .with_state(state)
}

/// Route-level coverage: 404 / 422 / 200 / DB-fallback / review-only.
///
/// All sub-cases share one AppState so they open exactly one DB connection
/// set and stay well under the 15-connection session pool limit.
#[tokio::test]
async fn proof_endpoint_route_coverage() -> anyhow::Result<()> {
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    let dir = tempfile::tempdir()?;
    let state = make_proof_state(dir.path()).await?;

    // --- 404: task does not exist ---
    {
        let resp = proof_route(state.clone())
            .oneshot(
                Request::builder()
                    .uri("/tasks/nonexistent/proof")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND, "404 case");
    }

    // --- 422: task exists but is not Done ---
    {
        let id = harness_core::types::TaskId("pending-task".to_string());
        state.core.tasks.insert(&TaskState::new(id)).await;

        let resp = proof_route(state.clone())
            .oneshot(
                Request::builder()
                    .uri("/tasks/pending-task/proof")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(
            resp.status(),
            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
            "422 case"
        );
    }

    // --- 200: Done task with LGTM rounds, issue preserved from cache ---
    {
        let id = harness_core::types::TaskId("done-task".to_string());
        let mut task = TaskState::new(id);
        task.status = TaskStatus::Done;
        task.issue = Some(42);
        task.pr_url = Some("https://github.com/owner/repo/pull/7".to_string());
        task.rounds = vec![
            RoundResult {
                turn: 1,
                action: ACTION_REVIEW.to_string(),
                result: "fixed".to_string(),
                detail: None,
                first_token_latency_ms: None,
            },
            RoundResult {
                turn: 2,
                action: ACTION_REVIEW.to_string(),
                result: RESULT_LGTM.to_string(),
                detail: Some("looks great".to_string()),
                first_token_latency_ms: None,
            },
        ];
        state.core.tasks.insert(&task).await;

        let resp = proof_route(state.clone())
            .oneshot(
                Request::builder()
                    .uri("/tasks/done-task/proof")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(resp.status(), axum::http::StatusCode::OK, "200 case");
        let body = resp.into_body().collect().await?.to_bytes();
        let proof: serde_json::Value = serde_json::from_slice(&body)?;
        assert_eq!(proof["task_id"], "done-task");
        assert_eq!(proof["issue"], 42, "issue in cache path");
        assert_eq!(proof["review_outcome"], "approved");
        assert_eq!(proof["ci_status"], "passed");
        assert_eq!(proof["review_rounds"], 2);
        assert_eq!(proof["final_review_detail"], "looks great");
    }

    // --- 200: DB-fallback path — issue survives eviction from cache ---
    {
        let id = harness_core::types::TaskId("db-task".to_string());
        let mut task = TaskState::new(id.clone());
        task.status = TaskStatus::Done;
        task.issue = Some(99);
        state.core.tasks.insert(&task).await;
        state.core.tasks.cache.remove(&id); // evict to force DB path

        let resp = proof_route(state.clone())
            .oneshot(
                Request::builder()
                    .uri("/tasks/db-task/proof")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(resp.status(), axum::http::StatusCode::OK, "DB-fallback 200");
        let body = resp.into_body().collect().await?.to_bytes();
        let proof: serde_json::Value = serde_json::from_slice(&body)?;
        assert_eq!(proof["issue"], 99, "issue must survive DB roundtrip");
    }

    // --- 200: periodic_review task → review_outcome = not_applicable ---
    {
        let id = harness_core::types::TaskId("review-only".to_string());
        let mut task = TaskState::new(id);
        task.status = TaskStatus::Done;
        task.source = Some("periodic_review".to_string());
        task.rounds = vec![RoundResult {
            turn: 1,
            action: ACTION_REVIEW.to_string(),
            result: RESULT_COMPLETED.to_string(),
            detail: Some("weekly report".to_string()),
            first_token_latency_ms: None,
        }];
        state.core.tasks.insert(&task).await;

        let resp = proof_route(state.clone())
            .oneshot(
                Request::builder()
                    .uri("/tasks/review-only/proof")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(resp.status(), axum::http::StatusCode::OK, "review-only 200");
        let body = resp.into_body().collect().await?.to_bytes();
        let proof: serde_json::Value = serde_json::from_slice(&body)?;
        assert_eq!(
            proof["review_outcome"], "not_applicable",
            "periodic_review must not report changes_requested"
        );
    }

    Ok(())
}
