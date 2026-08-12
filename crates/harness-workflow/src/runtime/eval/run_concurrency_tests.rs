use super::manifest::EvalBenchmarkCase;
use super::run::*;
use crate::runtime::{DataProvenance, WorkflowCommandStatus, WorkflowRuntimeStore};
use harness_core::db::resolve_database_url;
use serde_json::json;
use tokio::time::{sleep, timeout, Duration};

fn benchmark_case(case_id: &str) -> EvalBenchmarkCase {
    EvalBenchmarkCase {
        case_id: case_id.to_string(),
        repo: "owner/repo".to_string(),
        issue: 42,
        base_commit: "abcdef1".to_string(),
        verify_commands: vec!["cargo test -p harness-workflow eval_run".to_string()],
        paths: Vec::new(),
        risk: None,
        evidence: Vec::new(),
        resolution_prs: Vec::new(),
        resolution_commits: Vec::new(),
        commit_resolution: None,
        verdict: None,
        timeout_secs: 120,
        resource_limits: harness_sandbox::ResourceLimits::evaluation_defaults(120)
            .cap_by(harness_sandbox::ResourceLimits::operator_default_maxima())
            .expect("default resource limits should be valid"),
    }
}

async fn wait_for_command_cancellation(
    lock_tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    command_id: &str,
) -> anyhow::Result<()> {
    timeout(Duration::from_secs(5), async {
        loop {
            let status: Option<(String,)> =
                sqlx::query_as("SELECT status FROM workflow_commands WHERE id = $1")
                    .bind(command_id)
                    .fetch_optional(&mut **lock_tx)
                    .await?;
            if status
                .as_ref()
                .is_some_and(|(status,)| status == WorkflowCommandStatus::Cancelled.as_str())
            {
                return Ok::<(), anyhow::Error>(());
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("cleanup did not cancel its command before the lock timeout"))?
}

#[tokio::test]
async fn eval_enqueue_does_not_return_a_plan_for_a_same_state_stale_snapshot() -> anyhow::Result<()>
{
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime_store")).await?;
    let case = benchmark_case("owner/repo#stale-enqueue");
    let project_id = dir.path().to_string_lossy();
    let input = EvalCaseWorkflowInput {
        eval_run_id: "run-stale-enqueue",
        case: &case,
        project_id: project_id.as_ref(),
        task_id: "eval-stale-enqueue-task",
        additional_prompt: None,
    };
    let mut concurrent_instance = eval_case_initial_instance(input);
    concurrent_instance.version = concurrent_instance.version.saturating_add(1);
    concurrent_instance.set_data_field(
        "concurrent_marker",
        json!("preserve-me"),
        DataProvenance::Server,
    )?;
    store
        .force_upsert_lifecycle_state_for_test(&concurrent_instance)
        .await?;

    let error = enqueue_eval_case_workflow(&store, input)
        .await
        .expect_err("a stale atomic transition must not return an enqueue plan");

    assert!(
        error
            .to_string()
            .contains("changed or disappeared before commit"),
        "unexpected error: {error}"
    );
    let stored = store
        .get_instance(&concurrent_instance.id)
        .await?
        .expect("the concurrent instance must remain");
    assert_eq!(stored.version, concurrent_instance.version);
    assert_eq!(stored.data["concurrent_marker"], "preserve-me");
    assert!(store.events_for(&concurrent_instance.id).await?.is_empty());
    assert!(store
        .decisions_for(&concurrent_instance.id)
        .await?
        .is_empty());
    assert!(store
        .commands_for(&concurrent_instance.id)
        .await?
        .is_empty());
    Ok(())
}

#[tokio::test]
async fn eval_cleanup_reloads_latest_instance_after_a_same_state_stale_transition(
) -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store_path = dir.path().join("workflow_runtime_store");
    let store = WorkflowRuntimeStore::open(&store_path).await?;
    let cleanup_store = WorkflowRuntimeStore::open(&store_path).await?;
    let case = benchmark_case("owner/repo#stale-cleanup");
    let enqueue = enqueue_eval_case_workflow(
        &store,
        EvalCaseWorkflowInput {
            eval_run_id: "run-stale-cleanup",
            case: &case,
            project_id: dir.path().to_string_lossy().as_ref(),
            task_id: "eval-stale-cleanup-task",
            additional_prompt: None,
        },
    )
    .await?;
    let workflow_id = enqueue.plan.workflow_id.clone();
    let command_id = enqueue
        .command_ids
        .first()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("eval enqueue must create a command"))?;
    let mut concurrent_instance = store
        .get_instance(&workflow_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("eval workflow must exist"))?;
    concurrent_instance.version = concurrent_instance
        .version
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("test workflow version overflow"))?;
    let mut eval_section = concurrent_instance.data["eval"].clone();
    eval_section["workspace_path"] = json!("/tmp/eval-stale-worktree");
    concurrent_instance.set_data_field("eval", eval_section, DataProvenance::Server)?;
    let concurrent_data = serde_json::to_string(&concurrent_instance)?;

    let mut lock_tx = store.pool().begin().await?;
    let _: (String,) = sqlx::query_as("SELECT id FROM workflow_instances WHERE id = $1 FOR UPDATE")
        .bind(&workflow_id)
        .fetch_one(&mut *lock_tx)
        .await?;

    let cleanup_case = case.clone();
    let cleanup_task = tokio::spawn(async move {
        cleanup_cancelled_eval_run(
            &cleanup_store,
            EvalRunCleanupInput {
                eval_run_id: "run-stale-cleanup",
                cases: std::slice::from_ref(&cleanup_case),
                reason: "operator cancelled eval run",
            },
        )
        .await
    });

    wait_for_command_cancellation(&mut lock_tx, &command_id).await?;

    let updated = sqlx::query(
        "UPDATE workflow_instances
         SET data = $2::jsonb, version = $3, updated_at = CURRENT_TIMESTAMP
         WHERE id = $1",
    )
    .bind(&workflow_id)
    .bind(&concurrent_data)
    .bind(i64::try_from(concurrent_instance.version)?)
    .execute(&mut *lock_tx)
    .await?;
    assert_eq!(updated.rows_affected(), 1);
    lock_tx.commit().await?;

    let joined = timeout(Duration::from_secs(5), cleanup_task)
        .await
        .map_err(|_| anyhow::anyhow!("cleanup did not finish after releasing the row lock"))?;
    let summary = joined??;

    assert_eq!(summary.workflows_seen, 1);
    assert_eq!(summary.commands_cancelled, 1);
    assert_eq!(summary.workflows_cancelled, 0);
    assert_eq!(summary.active_workflows, 1);
    assert_eq!(summary.active_commands, 0);
    assert_eq!(summary.orphan_workspaces, 1);
    assert!(summary.cleanup_failures.is_empty());
    let stored = store
        .get_instance(&workflow_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("concurrent workflow must remain"))?;
    assert_eq!(stored.version, concurrent_instance.version);
    assert_eq!(
        stored.data["eval"]["workspace_path"],
        "/tmp/eval-stale-worktree"
    );
    assert!(
        store
            .decisions_for(&workflow_id)
            .await?
            .iter()
            .all(|record| record.decision.decision != "cancel_eval_run"),
        "a stale cleanup transition must not record a cancellation decision"
    );
    Ok(())
}

#[tokio::test]
async fn eval_cleanup_reports_reload_failure_when_the_workflow_disappears() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store_path = dir.path().join("workflow_runtime_store");
    let store = WorkflowRuntimeStore::open(&store_path).await?;
    let cleanup_store = WorkflowRuntimeStore::open(&store_path).await?;
    let case = benchmark_case("owner/repo#missing-cleanup");
    let enqueue = enqueue_eval_case_workflow(
        &store,
        EvalCaseWorkflowInput {
            eval_run_id: "run-missing-cleanup",
            case: &case,
            project_id: dir.path().to_string_lossy().as_ref(),
            task_id: "eval-missing-cleanup-task",
            additional_prompt: None,
        },
    )
    .await?;
    let workflow_id = enqueue.plan.workflow_id.clone();
    let command_id = enqueue
        .command_ids
        .first()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("eval enqueue must create a command"))?;
    let mut prior_instance = store
        .get_instance(&workflow_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("eval workflow must exist"))?;
    prior_instance.version = prior_instance
        .version
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("test workflow version overflow"))?;
    let mut eval_section = prior_instance.data["eval"].clone();
    eval_section["workspace_path"] = json!("/tmp/eval-missing-worktree");
    eval_section["pr_number"] = json!(99);
    prior_instance.set_data_field("eval", eval_section, DataProvenance::Server)?;
    store
        .force_upsert_lifecycle_state_for_test(&prior_instance)
        .await?;

    let mut lock_tx = store.pool().begin().await?;
    let _: (String,) = sqlx::query_as("SELECT id FROM workflow_instances WHERE id = $1 FOR UPDATE")
        .bind(&workflow_id)
        .fetch_one(&mut *lock_tx)
        .await?;

    let cleanup_case = case.clone();
    let cleanup_task = tokio::spawn(async move {
        cleanup_cancelled_eval_run(
            &cleanup_store,
            EvalRunCleanupInput {
                eval_run_id: "run-missing-cleanup",
                cases: std::slice::from_ref(&cleanup_case),
                reason: "operator cancelled eval run",
            },
        )
        .await
    });

    wait_for_command_cancellation(&mut lock_tx, &command_id).await?;
    let deleted = sqlx::query("DELETE FROM workflow_instances WHERE id = $1")
        .bind(&workflow_id)
        .execute(&mut *lock_tx)
        .await?;
    assert_eq!(deleted.rows_affected(), 1);
    lock_tx.commit().await?;

    let joined = timeout(Duration::from_secs(5), cleanup_task)
        .await
        .map_err(|_| anyhow::anyhow!("cleanup did not finish after releasing the row lock"))?;
    let summary = joined??;

    assert_eq!(summary.workflows_seen, 1);
    assert_eq!(summary.commands_cancelled, 1);
    assert_eq!(summary.workflows_cancelled, 0);
    assert_eq!(summary.active_workflows, 1);
    assert_eq!(summary.orphan_workspaces, 1);
    assert_eq!(summary.orphan_pull_requests, 1);
    assert!(!summary.is_clean());
    assert_eq!(summary.cleanup_failures.len(), 1);
    assert_eq!(summary.cleanup_failures[0].step, "reload_workflow");
    assert!(summary.cleanup_failures[0]
        .error
        .contains("workflow disappeared"));
    assert!(store.get_instance(&workflow_id).await?.is_none());
    Ok(())
}
