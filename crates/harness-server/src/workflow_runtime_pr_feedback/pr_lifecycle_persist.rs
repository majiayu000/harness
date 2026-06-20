use super::pr_workflow_id;
use crate::task_runner::TaskId;
use harness_workflow::runtime::{WorkflowInstance, WorkflowRuntimeStore};
use serde_json::json;
use std::{future::Future, path::Path, time::Duration};

pub(super) const PR_LIFECYCLE_PERSIST_MAX_ATTEMPTS: usize = 3;
const PR_LIFECYCLE_PERSIST_RETRY_BACKOFF: Duration = Duration::from_millis(50);
const PR_LIFECYCLE_PERSIST_FAILURE_EVENT: &str = "PrLifecyclePersistenceFailed";
const PR_LIFECYCLE_PERSIST_FAILURE_SOURCE: &str = "workflow_runtime_pr_feedback";

pub(super) fn issue_workflow_id(
    project_root: &Path,
    repo: Option<&str>,
    issue_number: u64,
) -> String {
    let project_id = project_root.to_string_lossy();
    harness_workflow::issue_lifecycle::workflow_id(&project_id, repo, issue_number)
}

pub(super) fn pr_lifecycle_workflow_id(
    project_root: &Path,
    repo: Option<&str>,
    issue_number: Option<u64>,
    pr_number: u64,
) -> String {
    if let Some(issue_number) = issue_number {
        issue_workflow_id(project_root, repo, issue_number)
    } else {
        let project_id = project_root.to_string_lossy();
        pr_workflow_id(&project_id, repo, pr_number)
    }
}

pub(super) async fn persist_pr_lifecycle_with_retry<F, Fut>(
    store: &WorkflowRuntimeStore,
    workflow_id: &str,
    operation: &'static str,
    task_id: &TaskId,
    pr_number: u64,
    failure_instance: WorkflowInstance,
    failure_payload: serde_json::Value,
    mut persist: F,
) -> anyhow::Result<()>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = anyhow::Result<()>>,
{
    let mut attempt = 1;
    loop {
        let result = match maybe_inject_pr_lifecycle_persist_failure_for_test(task_id) {
            Ok(()) => persist().await,
            Err(error) => Err(error),
        };
        match result {
            Ok(()) => {
                if attempt > 1 {
                    tracing::info!(
                        workflow_id,
                        operation,
                        pr = pr_number,
                        task_id = %task_id.0,
                        attempt,
                        "workflow runtime PR lifecycle write succeeded after retry"
                    );
                }
                return Ok(());
            }
            Err(error) if attempt < PR_LIFECYCLE_PERSIST_MAX_ATTEMPTS => {
                tracing::warn!(
                    workflow_id,
                    operation,
                    pr = pr_number,
                    task_id = %task_id.0,
                    attempt,
                    max_attempts = PR_LIFECYCLE_PERSIST_MAX_ATTEMPTS,
                    "workflow runtime PR lifecycle write failed; retrying: {error}"
                );
                tokio::time::sleep(PR_LIFECYCLE_PERSIST_RETRY_BACKOFF).await;
                attempt += 1;
            }
            Err(error) => {
                record_pr_lifecycle_persist_failure(
                    store,
                    workflow_id,
                    operation,
                    task_id,
                    pr_number,
                    &failure_instance,
                    failure_payload,
                    &error,
                    attempt,
                )
                .await;
                return Err(error);
            }
        }
    }
}

async fn record_pr_lifecycle_persist_failure(
    store: &WorkflowRuntimeStore,
    workflow_id: &str,
    operation: &'static str,
    task_id: &TaskId,
    pr_number: u64,
    failure_instance: &WorkflowInstance,
    mut payload: serde_json::Value,
    error: &anyhow::Error,
    attempts: usize,
) {
    if let Some(object) = payload.as_object_mut() {
        object.insert("operation".to_string(), json!(operation));
        object.insert("attempts".to_string(), json!(attempts));
        object.insert("error".to_string(), json!(error.to_string()));
    }
    if let Err(instance_error) =
        ensure_pr_lifecycle_failure_workflow(store, workflow_id, failure_instance).await
    {
        tracing::error!(
            workflow_id,
            operation,
            pr = pr_number,
            task_id = %task_id.0,
            "workflow runtime PR lifecycle failure workflow write failed: {instance_error}"
        );
        return;
    }
    if let Err(event_error) = store
        .append_event(
            workflow_id,
            PR_LIFECYCLE_PERSIST_FAILURE_EVENT,
            PR_LIFECYCLE_PERSIST_FAILURE_SOURCE,
            payload,
        )
        .await
    {
        tracing::error!(
            workflow_id,
            operation,
            pr = pr_number,
            task_id = %task_id.0,
            "workflow runtime PR lifecycle failure event write failed: {event_error}"
        );
    }
}

async fn ensure_pr_lifecycle_failure_workflow(
    store: &WorkflowRuntimeStore,
    workflow_id: &str,
    failure_instance: &WorkflowInstance,
) -> anyhow::Result<()> {
    if failure_instance.id != workflow_id {
        anyhow::bail!(
            "PR lifecycle failure instance `{}` does not match workflow `{}`",
            failure_instance.id,
            workflow_id
        );
    }
    if store.get_instance(workflow_id).await?.is_none() {
        store.upsert_instance(failure_instance).await?;
    }
    Ok(())
}

#[cfg(test)]
static PR_LIFECYCLE_PERSIST_TEST_FAILURES: std::sync::Mutex<
    std::collections::BTreeMap<String, usize>,
> = std::sync::Mutex::new(std::collections::BTreeMap::new());

#[cfg(test)]
pub(super) struct PrLifecyclePersistTestFailuresGuard {
    task_id: String,
}

#[cfg(test)]
impl Drop for PrLifecyclePersistTestFailuresGuard {
    fn drop(&mut self) {
        if let Ok(mut plans) = PR_LIFECYCLE_PERSIST_TEST_FAILURES.lock() {
            plans.remove(&self.task_id);
        }
    }
}

#[cfg(test)]
#[must_use]
pub(super) fn set_pr_lifecycle_persist_test_failures(
    task_id: &str,
    failures: usize,
) -> PrLifecyclePersistTestFailuresGuard {
    let Ok(mut plans) = PR_LIFECYCLE_PERSIST_TEST_FAILURES.lock() else {
        return PrLifecyclePersistTestFailuresGuard {
            task_id: task_id.to_string(),
        };
    };
    if failures == 0 {
        plans.remove(task_id);
    } else {
        plans.insert(task_id.to_string(), failures);
    }
    PrLifecyclePersistTestFailuresGuard {
        task_id: task_id.to_string(),
    }
}

#[cfg(test)]
fn maybe_inject_pr_lifecycle_persist_failure_for_test(task_id: &TaskId) -> anyhow::Result<()> {
    let mut plans = PR_LIFECYCLE_PERSIST_TEST_FAILURES
        .lock()
        .map_err(|_| anyhow::anyhow!("PR lifecycle persist test failure mutex poisoned"))?;
    let Some(remaining) = plans.get_mut(task_id.as_str()) else {
        return Ok(());
    };
    if *remaining == 0 {
        plans.remove(task_id.as_str());
        return Ok(());
    }
    *remaining -= 1;
    if *remaining == 0 {
        plans.remove(task_id.as_str());
    }
    anyhow::bail!(
        "injected PR lifecycle persist failure for {}",
        task_id.as_str()
    )
}

#[cfg(not(test))]
fn maybe_inject_pr_lifecycle_persist_failure_for_test(_task_id: &TaskId) -> anyhow::Result<()> {
    Ok(())
}
