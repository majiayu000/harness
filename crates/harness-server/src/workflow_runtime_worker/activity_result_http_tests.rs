use super::*;
use axum::{http::StatusCode, routing::post, Router};
use harness_agents::anthropic_api::AnthropicApiAgent;
use harness_core::agent::{AgentRequest, CodeAgent};
use harness_core::error::HarnessError;
use harness_workflow::runtime::reducer::{
    reduce_runtime_job_completed, RUNTIME_JOB_COMPLETED_EVENT,
};
use harness_workflow::runtime::{
    ActivityStatus, RuntimeKind, WorkflowCommand, WorkflowCommandType, WorkflowEvent,
    WorkflowInstance, WorkflowSubject, GITHUB_ISSUE_PR_DEFINITION_ID,
};
use std::time::Duration;

async fn fake_anthropic_failure(status: u16, body: &'static str) -> HarnessError {
    let status = StatusCode::from_u16(status).expect("valid fake HTTP status");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fake Anthropic server");
    let address = listener.local_addr().expect("fake server address");
    let app = Router::new().route("/v1/messages", post(move || async move { (status, body) }));
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("fake HTTP server");
    });
    let agent = AnthropicApiAgent::new(
        "fake-key".to_string(),
        format!("http://{address}"),
        "fake-model".to_string(),
        16,
    );
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        agent.execute_stream(AgentRequest::default(), tx),
    )
    .await;
    server.abort();
    let _ = server.await;
    assert!(
        rx.recv().await.is_none(),
        "HTTP failure emitted no success items"
    );
    result
        .expect("fake HTTP request should finish")
        .expect_err("fake Anthropic response should fail")
}

#[tokio::test]
async fn anthropic_http_failures_preserve_retry_semantics_through_runtime_reducer() {
    for (status, body, expected_failure, expected_activity) in [
        (
            400,
            "invalid request",
            TurnFailureKind::RequestRejected,
            ActivityErrorKind::Configuration,
        ),
        (
            401,
            "invalid key",
            TurnFailureKind::RequestRejected,
            ActivityErrorKind::Configuration,
        ),
        (
            403,
            "permission denied",
            TurnFailureKind::RequestRejected,
            ActivityErrorKind::Configuration,
        ),
        (
            404,
            "unknown model",
            TurnFailureKind::RequestRejected,
            ActivityErrorKind::Configuration,
        ),
        (
            413,
            "request too large",
            TurnFailureKind::RequestRejected,
            ActivityErrorKind::Configuration,
        ),
        (
            415,
            "unsupported media type",
            TurnFailureKind::RequestRejected,
            ActivityErrorKind::Configuration,
        ),
        (
            422,
            "invalid message content",
            TurnFailureKind::RequestRejected,
            ActivityErrorKind::Configuration,
        ),
        (
            408,
            "request timeout",
            TurnFailureKind::Upstream,
            ActivityErrorKind::ExternalDependency,
        ),
        (
            409,
            "request conflict",
            TurnFailureKind::Upstream,
            ActivityErrorKind::ExternalDependency,
        ),
        (
            421,
            "misdirected request",
            TurnFailureKind::Upstream,
            ActivityErrorKind::ExternalDependency,
        ),
        (
            425,
            "too early",
            TurnFailureKind::Upstream,
            ActivityErrorKind::ExternalDependency,
        ),
        (
            500,
            "internal error",
            TurnFailureKind::Upstream,
            ActivityErrorKind::ExternalDependency,
        ),
        (
            529,
            "overloaded",
            TurnFailureKind::Upstream,
            ActivityErrorKind::ExternalDependency,
        ),
        (
            429,
            "rate limit exceeded",
            TurnFailureKind::Quota,
            ActivityErrorKind::Configuration,
        ),
        (
            429,
            "monthly spend cap reached",
            TurnFailureKind::Quota,
            ActivityErrorKind::Configuration,
        ),
        (
            429,
            "insufficient balance",
            TurnFailureKind::Billing,
            ActivityErrorKind::Configuration,
        ),
        (
            400,
            "quota exhausted",
            TurnFailureKind::Quota,
            ActivityErrorKind::Configuration,
        ),
        (
            400,
            "insufficient available balance",
            TurnFailureKind::Billing,
            ActivityErrorKind::Configuration,
        ),
        (
            402,
            "payment required",
            TurnFailureKind::Billing,
            ActivityErrorKind::Configuration,
        ),
    ] {
        let error = fake_anthropic_failure(status, body).await;
        assert!(
            matches!(&error, HarnessError::AgentHttpResponse { status: actual, .. } if *actual == status)
        );
        let failure = error.turn_failure().expect("typed HTTP turn failure");
        assert_eq!(failure.kind, expected_failure, "HTTP {status}: {body}");
        assert_eq!(failure.upstream_status, Some(status));
        assert_eq!(failure.provider.as_deref(), Some("anthropic-api"));
        assert_eq!(failure.body_excerpt.as_deref(), Some(body));

        // Use the same typed-error handoff as turn_lifecycle, including the
        // transcript serialization boundary before activity-result extraction.
        let item = Item::typed_error(error.to_string(), failure.kind);
        let item: Item =
            serde_json::from_value(serde_json::to_value(item).expect("serialize item"))
                .expect("deserialize item");
        let job = RuntimeJob::pending(
            "command-1",
            RuntimeKind::ClaudeCode,
            "anthropic-test",
            json!({ "activity": "implement_issue" }),
        );
        let result = activity_result_from_turn(
            &job,
            &TurnStatus::Failed,
            &[item],
            &ThreadId::from_str("thread-1"),
            &TurnId::from_str("turn-1"),
            "anthropic-api",
            Path::new("/project"),
            "digest-1",
        );
        assert_eq!(result.status, ActivityStatus::Failed);
        assert_eq!(
            result.error_kind,
            Some(expected_activity),
            "HTTP {status}: {body}"
        );

        // A transient failure still needs an explicit policy and unused budget.
        for (retry_limit, retry_attempt) in [
            (None::<u64>, 0_u64),
            (Some(0), 0),
            (Some(2), 0),
            (Some(2), 2),
        ] {
            let mut instance = WorkflowInstance::new(
                GITHUB_ISSUE_PR_DEFINITION_ID,
                1,
                "implementing",
                WorkflowSubject::new("issue", "2131"),
            );
            if let Some(limit) = retry_limit {
                instance = instance.with_server_data(json!({
                    "runtime_retry_policy": { "max_failed_activity_retries": limit }
                }));
            }
            let mut command = WorkflowCommand::enqueue_activity("implement_issue", "implement-1");
            command.command["retry_attempt"] = json!(retry_attempt);
            let event =
                WorkflowEvent::new(&instance.id, 1, RUNTIME_JOB_COMPLETED_EVENT, "runtime-1")
                    .with_payload(json!({
                        "command_id": "command-1",
                        "command": command,
                        "runtime_job_id": "job-1",
                        "activity_result": result,
                    }));
            let decision = reduce_runtime_job_completed(&instance, &event)
                .expect("HTTP failure completion parses")
                .expect("HTTP failure produces a decision");
            let should_retry = expected_activity == ActivityErrorKind::ExternalDependency
                && retry_limit.is_some_and(|limit| retry_attempt < limit);
            assert_eq!(
                decision.decision,
                if should_retry {
                    "retry_failed_runtime_activity"
                } else {
                    "fail_after_runtime_activity"
                },
                "HTTP {status}: {body}, retry limit {retry_limit:?}, attempt {retry_attempt}"
            );
            if should_retry {
                assert_eq!(decision.next_state, "implementing");
                assert_eq!(decision.commands.len(), 1);
                assert_eq!(
                    decision.commands[0].command_type,
                    WorkflowCommandType::EnqueueActivity
                );
                assert_eq!(
                    decision.commands[0].command["retry_attempt"],
                    retry_attempt + 1
                );
                assert_eq!(
                    decision.commands[0].command["max_failed_activity_retries"],
                    2
                );
            } else {
                assert_eq!(decision.next_state, "failed");
                assert!(decision
                    .commands
                    .iter()
                    .any(|command| command.command_type == WorkflowCommandType::MarkFailed));
                assert!(!decision
                    .commands
                    .iter()
                    .any(|command| command.command_type == WorkflowCommandType::EnqueueActivity));
            }
        }

        if expected_failure == TurnFailureKind::RequestRejected {
            assert_eq!(
                super::super::agent_contract_attempt::contract_attempt_failure_kind(
                    &anyhow::Error::new(error)
                ),
                ActivityErrorKind::Fatal,
                "request rejection must not become a retryable contract attempt"
            );
        }
    }
}
