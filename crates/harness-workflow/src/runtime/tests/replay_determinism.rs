use super::*;
use crate::runtime::RUNTIME_JOB_COMPLETED_EVENT;

#[test]
fn runtime_completion_validation_uses_event_time_for_replayed_lease() -> anyhow::Result<()> {
    let event_created_at = Utc::now() - Duration::hours(1);
    let instance = issue_instance("implementing")
        .with_lease("runtime-1", event_created_at + Duration::minutes(5));
    let decision = WorkflowDecision::new(
        &instance.id,
        "implementing",
        "bind_pr_from_agent",
        "pr_open",
        "agent reported a newly created pull request",
    )
    .with_command(WorkflowCommand::bind_pr(
        123,
        "https://github.com/owner/repo/pull/123",
        "bind-pr-123",
    ));
    let result =
        ActivityResult::succeeded("implement_issue", "created pull request").with_artifact(
            ActivityArtifact::new("workflow_decision", serde_json::to_value(&decision)?),
        );
    let mut event = WorkflowEvent::new(&instance.id, 1, RUNTIME_JOB_COMPLETED_EVENT, "runtime-1")
        .with_payload(json!({
            "activity_result": result,
        }));
    event.created_at = event_created_at;

    let reduced = reduce_runtime_job_completed(&instance, &event)?
        .expect("historical event should reduce to the structured decision");

    assert_eq!(reduced.decision, "bind_pr_from_agent");
    assert_eq!(reduced.next_state, "pr_open");
    Ok(())
}
