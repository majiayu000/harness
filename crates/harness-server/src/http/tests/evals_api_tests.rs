use super::*;
use axum::routing::{get, post};

fn evals_app(state: Arc<AppState>) -> Router {
    Router::new()
        .route(
            "/api/evals/runs",
            post(crate::handlers::evals::create_eval_run)
                .get(crate::handlers::evals::list_eval_runs),
        )
        .route(
            "/api/evals/runs/{run_id}",
            get(crate::handlers::evals::get_eval_run),
        )
        .route(
            "/api/evals/runs/{run_id}/artifacts",
            post(crate::handlers::evals::add_eval_artifact)
                .get(crate::handlers::evals::list_eval_artifacts),
        )
        .route(
            "/api/evals/runs/{run_id}/score",
            post(crate::handlers::evals::score_eval_run),
        )
        .route(
            "/api/evals/quality-snapshots/{snapshot_id}",
            get(crate::handlers::evals::get_eval_quality_snapshot),
        )
        .route(
            "/api/evals/pr/{owner}/{repo}/{pr_number}",
            get(crate::handlers::evals::list_eval_quality_snapshots_for_pr),
        )
        .with_state(state)
}

#[tokio::test]
async fn eval_routes_persist_score_and_pr_lookup() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let mut config = harness_core::config::HarnessConfig::default();
    config.server.data_dir = dir.path().to_path_buf();
    let state = make_test_state_with(
        dir.path(),
        config,
        harness_agents::registry::AgentRegistry::new("test"),
    )
    .await?;
    let app = evals_app(state);

    let create_body = pr_repair_run_body();
    let create = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/evals/runs")
                .header("content-type", "application/json")
                .body(Body::from(create_body.to_string()))?,
        )
        .await?;
    assert_eq!(create.status(), axum::http::StatusCode::CREATED);
    let created = response_json(create).await?;
    let run_id = created["run"]["id"].as_str().expect("run id").to_string();

    let artifact_body = serde_json::json!({
        "artifact_type": " pr_repair_eval_input ",
        "label": "canonical input",
        "content_type": "application/json",
        "body": pr_repair_input().to_string()
    });
    let artifact = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/evals/runs/{run_id}/artifacts"))
                .header("content-type", "application/json")
                .body(Body::from(artifact_body.to_string()))?,
        )
        .await?;
    assert_eq!(artifact.status(), axum::http::StatusCode::CREATED);
    let artifact_json = response_json(artifact).await?;
    assert_eq!(
        artifact_json["artifact"]["artifact_type"],
        "pr_repair_eval_input"
    );

    let score_body = serde_json::json!({});
    let score = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/evals/runs/{run_id}/score"))
                .header("content-type", "application/json")
                .body(Body::from(score_body.to_string()))?,
        )
        .await?;
    assert_eq!(score.status(), axum::http::StatusCode::OK);
    let scored = response_json(score).await?;
    assert_eq!(scored["run"]["status"], "scored");
    assert!(
        scored["quality_snapshot"]["snapshot"]["final_score"]
            .as_u64()
            .expect("final score")
            > 0
    );
    let snapshot_id = scored["quality_snapshot"]["id"]
        .as_str()
        .expect("snapshot id")
        .to_string();

    let snapshot = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/evals/quality-snapshots/{snapshot_id}"))
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(snapshot.status(), axum::http::StatusCode::OK);
    let snapshot_json = response_json(snapshot).await?;
    assert_eq!(snapshot_json["quality_snapshot"]["id"], snapshot_id);

    let pr_lookup = app
        .oneshot(
            Request::builder()
                .uri("/api/evals/pr/owner/repo/7")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(pr_lookup.status(), axum::http::StatusCode::OK);
    let pr_lookup_json = response_json(pr_lookup).await?;
    assert_eq!(pr_lookup_json["quality_snapshots"][0]["id"], snapshot_id);
    Ok(())
}

#[tokio::test]
async fn score_missing_eval_run_returns_not_found_before_artifact_fallback() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let mut config = harness_core::config::HarnessConfig::default();
    config.server.data_dir = dir.path().to_path_buf();
    let state = make_test_state_with(
        dir.path(),
        config,
        harness_agents::registry::AgentRegistry::new("test"),
    )
    .await?;
    let app = evals_app(state);

    let score = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/evals/runs/missing-run/score")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::json!({}).to_string()))?,
        )
        .await?;
    assert_eq!(score.status(), axum::http::StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test]
async fn score_malformed_eval_input_artifact_returns_bad_request() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let mut config = harness_core::config::HarnessConfig::default();
    config.server.data_dir = dir.path().to_path_buf();
    let state = make_test_state_with(
        dir.path(),
        config,
        harness_agents::registry::AgentRegistry::new("test"),
    )
    .await?;
    let app = evals_app(state);
    let run_id = create_pr_repair_run(app.clone()).await?;

    let artifact_body = serde_json::json!({
        "artifact_type": "pr_repair_eval_input",
        "body": "{not-json"
    });
    let artifact = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/evals/runs/{run_id}/artifacts"))
                .header("content-type", "application/json")
                .body(Body::from(artifact_body.to_string()))?,
        )
        .await?;
    assert_eq!(artifact.status(), axum::http::StatusCode::CREATED);

    let score = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/evals/runs/{run_id}/score"))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::json!({}).to_string()))?,
        )
        .await?;
    assert_eq!(score.status(), axum::http::StatusCode::BAD_REQUEST);
    let body = response_json(score).await?;
    assert!(body["error"]
        .as_str()
        .expect("error message")
        .contains("failed to parse pr_repair_eval_input artifact"));
    Ok(())
}

async fn create_pr_repair_run(app: Router) -> anyhow::Result<String> {
    let create = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/evals/runs")
                .header("content-type", "application/json")
                .body(Body::from(pr_repair_run_body().to_string()))?,
        )
        .await?;
    assert_eq!(create.status(), axum::http::StatusCode::CREATED);
    let created = response_json(create).await?;
    Ok(created["run"]["id"].as_str().expect("run id").to_string())
}

fn pr_repair_run_body() -> serde_json::Value {
    serde_json::json!({
        "scenario": "pr_repair",
        "target": {
            "kind": "pull_request",
            "repo": "owner/repo",
            "pr_number": 7,
            "base_ref": "main",
            "head_ref": "fix/review"
        },
        "source_task_id": "task-7"
    })
}

fn pr_repair_input() -> serde_json::Value {
    serde_json::json!({
        "scenario": "pr_repair",
        "target": {
            "kind": "pull_request",
            "repo": "owner/repo",
            "pr_number": 7,
            "base_ref": "main",
            "head_ref": "fix/review"
        },
        "baseline_pr": pr_snapshot_json("base-head", "passing", "clean", true),
        "final_pr": pr_snapshot_json("final-head", "passing", "clean", false),
        "final_evidence_head_oid": "final-head",
        "runtime": {
            "task_id": "task-7",
            "workflow_id": "workflow-7",
            "workflow_state": "done",
            "runtime_jobs": [{
                "runtime_job_id": "job-7",
                "state": "completed",
                "artifact_count": 1,
                "terminal_state": "succeeded"
            }],
            "latest_activity": "address_pr_feedback",
            "terminal_state": "done",
            "collected_at": "2026-06-05T00:00:00Z"
        },
        "usage": [{
            "agent_invocation_id": "agent-7",
            "runtime_job_id": "job-7",
            "workflow_id": "workflow-7",
            "model": "codex-test",
            "reasoning_effort": "medium",
            "input_tokens": 1000,
            "output_tokens": 500,
            "cached_input_tokens": 0,
            "total_tokens": 1500,
            "cost_usd_micros": 1234,
            "token_confidence": "exact",
            "cost_confidence": "estimated"
        }],
        "reviewer_judgment": {
            "reviewer_kind": "llm",
            "judged_head_oid": "final-head",
            "code_quality_score": 90,
            "trajectory_score": 90,
            "findings": [],
            "residual_risks": []
        },
        "created_unrelated_pr": false,
        "scope_violations": []
    })
}

fn pr_snapshot_json(
    head_oid: &str,
    check_state: &str,
    merge_state: &str,
    unresolved_thread: bool,
) -> serde_json::Value {
    let active_unresolved_review_threads = if unresolved_thread {
        serde_json::json!([{
            "id": "thread-1",
            "path": "src/lib.rs",
            "is_resolved": false,
            "is_outdated": false
        }])
    } else {
        serde_json::json!([])
    };
    serde_json::json!({
        "repo": "owner/repo",
        "pr_number": 7,
        "url": "https://github.com/owner/repo/pull/7",
        "title": "Fix review feedback",
        "base_ref": "main",
        "head_ref": "fix/review",
        "head_oid": head_oid,
        "is_draft": false,
        "merge_state": merge_state,
        "check_state": check_state,
        "review_decision": "approved",
        "active_unresolved_review_threads": active_unresolved_review_threads,
        "changed_files": [{
            "path": "src/lib.rs",
            "additions": 4,
            "deletions": 1,
            "status": "modified"
        }],
        "collected_at": "2026-06-05T00:00:00Z"
    })
}
