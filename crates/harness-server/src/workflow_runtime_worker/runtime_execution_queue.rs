use crate::http::AppState;
use futures::FutureExt;
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

pub(super) enum QueueAdmission {
    Ready(Option<crate::task_queue::TaskPermit>),
    Busy,
}

pub(super) fn try_runtime_execution_queue_permit(
    state: &AppState,
    workflow: Option<&WorkflowInstance>,
) -> anyhow::Result<QueueAdmission> {
    try_queue_permit(state.concurrency.review_task_queue.as_ref(), workflow)
}

fn try_queue_permit(
    queue: &crate::task_queue::TaskQueue,
    workflow: Option<&WorkflowInstance>,
) -> anyhow::Result<QueueAdmission> {
    let request = workflow
        .map(runtime_execution_queue_request)
        .transpose()?
        .flatten();
    let Some(request) = request else {
        return Ok(QueueAdmission::Ready(None));
    };
    // acquire is cancellation-safe: an unavailable permit must not retain a
    // worker, a project permit or a waiter after this single poll.
    match queue
        .acquire(&request.project_id, request.priority)
        .now_or_never()
    {
        Some(result) => result.map(|permit| QueueAdmission::Ready(Some(permit))),
        None => Ok(QueueAdmission::Busy),
    }
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
        .with_server_data(json!({
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
        let QueueAdmission::Ready(Some(first)) = try_queue_permit(&queue, Some(&workflow))? else {
            panic!("first review should acquire a permit");
        };
        assert!(matches!(
            try_queue_permit(&queue, Some(&workflow))?,
            QueueAdmission::Busy
        ));
        assert!(matches!(
            try_queue_permit(&queue, Some(&prompt_workflow("primary")))?,
            QueueAdmission::Ready(None)
        ));
        drop(first);
        assert!(matches!(
            try_queue_permit(&queue, Some(&workflow))?,
            QueueAdmission::Ready(Some(_))
        ));
        Ok(())
    }
}
