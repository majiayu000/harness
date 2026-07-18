use crate::http::AppState;
use harness_workflow::runtime::WorkflowInstance;
use serde_json::Value;

#[derive(Debug, PartialEq, Eq)]
struct RuntimeExecutionQueueRequest {
    project_id: String,
    priority: u8,
}

fn runtime_execution_queue_request(
    workflow: &WorkflowInstance,
) -> anyhow::Result<Option<RuntimeExecutionQueueRequest>> {
    let Some(policy) = crate::workflow_runtime_submission::prompt_execution_policy(&workflow.data)?
    else {
        return Ok(None);
    };
    if policy.queue_domain
        != crate::workflow_runtime_submission::runtime_models::QueueDomain::Review
    {
        return Ok(None);
    }
    let project_id = workflow
        .data
        .get("project_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "runtime review workflow {} is missing project_id",
                workflow.id
            )
        })?;
    Ok(Some(RuntimeExecutionQueueRequest {
        project_id: project_id.to_string(),
        priority: policy.priority,
    }))
}

pub(super) async fn acquire_runtime_execution_queue_permit(
    state: &AppState,
    workflow: Option<&WorkflowInstance>,
) -> anyhow::Result<Option<crate::task_queue::TaskPermit>> {
    acquire_runtime_execution_queue_permit_from_queue(
        state.concurrency.review_task_queue.as_ref(),
        workflow,
    )
    .await
}

async fn acquire_runtime_execution_queue_permit_from_queue(
    review_task_queue: &crate::task_queue::TaskQueue,
    workflow: Option<&WorkflowInstance>,
) -> anyhow::Result<Option<crate::task_queue::TaskPermit>> {
    let Some(workflow) = workflow else {
        return Ok(None);
    };
    let Some(request) = runtime_execution_queue_request(workflow)? else {
        return Ok(None);
    };
    tracing::debug!(
        workflow_id = %workflow.id,
        project_id = request.project_id,
        priority = request.priority,
        queue_domain = "review",
        "workflow runtime waiting for execution queue permit"
    );
    review_task_queue
        .acquire(&request.project_id, request.priority)
        .await
        .map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_workflow::runtime::WorkflowSubject;
    use serde_json::json;

    fn prompt_workflow(queue_domain: &str) -> WorkflowInstance {
        WorkflowInstance::new(
            harness_workflow::runtime::PROMPT_TASK_DEFINITION_ID,
            1,
            "implementing",
            WorkflowSubject::new("prompt", "review:test"),
        )
        .with_id("runtime-review-policy")
        .with_data(json!({
            "project_id": "/tmp/project",
            "execution_policy": {
                "task_kind": "review",
                "agent": "codex",
                "turn_timeout_secs": 90,
                "queue_domain": queue_domain,
                "priority": 2,
            }
        }))
    }

    #[test]
    fn review_policy_routes_to_review_queue_with_priority() -> anyhow::Result<()> {
        assert_eq!(
            runtime_execution_queue_request(&prompt_workflow("review"))?,
            Some(RuntimeExecutionQueueRequest {
                project_id: "/tmp/project".to_string(),
                priority: 2,
            })
        );
        assert_eq!(
            runtime_execution_queue_request(&prompt_workflow("primary"))?,
            None
        );
        Ok(())
    }

    #[tokio::test]
    async fn review_queue_permit_serializes_runtime_reviews() -> anyhow::Result<()> {
        let mut config = harness_core::config::misc::ConcurrencyConfig::default();
        config.max_concurrent_tasks = 1;
        config.max_queue_size = 4;
        let queue = crate::task_queue::TaskQueue::new(&config);
        let workflow = prompt_workflow("review");
        let first = acquire_runtime_execution_queue_permit_from_queue(&queue, Some(&workflow))
            .await?
            .expect("review workflow should acquire a permit");
        let mut second = Box::pin(acquire_runtime_execution_queue_permit_from_queue(
            &queue,
            Some(&workflow),
        ));

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(25), &mut second)
                .await
                .is_err()
        );
        drop(first);
        let second = tokio::time::timeout(std::time::Duration::from_secs(1), second).await??;
        assert!(second.is_some());
        Ok(())
    }
}
