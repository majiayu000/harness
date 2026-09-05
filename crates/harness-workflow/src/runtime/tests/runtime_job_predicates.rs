#[test]
fn runtime_job_status_active_classification_matches_runtime_states() {
    for status in [RuntimeJobStatus::Pending, RuntimeJobStatus::Running] {
        assert!(status.is_active(), "{status:?} should be active");
    }

    for status in [
        RuntimeJobStatus::Succeeded,
        RuntimeJobStatus::Failed,
        RuntimeJobStatus::Cancelled,
    ] {
        assert!(!status.is_active(), "{status:?} should be terminal");
    }
}

#[test]
fn runtime_job_eval_detection_accepts_supported_payload_shapes() {
    let top_level = RuntimeJob::pending(
        "command-id",
        RuntimeKind::CodexExec,
        "codex-default",
        json!({ "eval": { "case_id": "case-a" } }),
    );
    let nested_command = RuntimeJob::pending(
        "command-id",
        RuntimeKind::CodexExec,
        "codex-default",
        json!({ "command": { "eval": { "case_id": "case-a" } } }),
    );
    let non_eval = RuntimeJob::pending(
        "command-id",
        RuntimeKind::CodexExec,
        "codex-default",
        json!({ "command": { "activity": "implement_issue" } }),
    );

    assert!(top_level.is_eval_job());
    assert!(nested_command.is_eval_job());
    assert!(!non_eval.is_eval_job());
}
