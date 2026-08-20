use super::*;

fn fixture(
    marker: Value,
) -> (
    RuntimeJob,
    WorkflowCommandRecord,
    WorkflowInstance,
    ActivityResult,
) {
    let now = Utc::now();
    let job = RuntimeJob::pending(
        "command-1",
        RuntimeKind::RemoteHost,
        "remote-host-default",
        json!({ "cancellation_requested": marker }),
    );
    let command = WorkflowCommandRecord {
        id: "command-1".to_string(),
        workflow_id: "workflow-1".to_string(),
        decision_id: None,
        status: WorkflowCommandStatus::Dispatched,
        dispatch_owner: None,
        dispatch_lease_expires_at: None,
        dispatch_not_before: None,
        dispatch_attempt_count: 1,
        dispatch_claim_generation: 1,
        dispatch_barrier: None,
        command: WorkflowCommand::enqueue_activity("implement_issue", "command-1"),
        created_at: now,
        updated_at: now,
        attempt_generation: 3,
        superseded_by_command_id: None,
    };
    let mut workflow = WorkflowInstance::new(
        "github_issue_pr",
        1,
        "implementing",
        crate::runtime::WorkflowSubject::new("issue", "issue:1"),
    );
    workflow.version = 7;
    let result = ActivityResult::cancelled("implement_issue", "cleanup complete");
    (job, command, workflow, result)
}

#[test]
fn legacy_cancellation_ack_without_provenance_is_not_stale() {
    let (job, command, workflow, result) = fixture(json!({ "reason": "cancelled" }));

    assert!(!cancellation_ack_is_stale_for_workflow(
        &job, &command, &workflow, &result
    ));
}

#[test]
fn mismatched_or_malformed_cancellation_provenance_is_stale() {
    for marker in [
        json!({ "workflow_version": 6, "command_attempt_generation": 3 }),
        json!({ "workflow_version": 7, "command_attempt_generation": 2 }),
        json!({ "workflow_version": "7", "command_attempt_generation": 3 }),
        json!({ "workflow_version": 7, "command_attempt_generation": null }),
    ] {
        let (job, command, workflow, result) = fixture(marker);
        assert!(cancellation_ack_is_stale_for_workflow(
            &job, &command, &workflow, &result
        ));
    }
}
