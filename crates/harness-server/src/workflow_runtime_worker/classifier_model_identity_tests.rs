use super::classifier::attest_result;
use harness_core::config::workflow::WorkflowClassifierPolicy;
use harness_workflow::runtime::{
    ActivityArtifact, ActivityResult, ActivityStatus, RuntimeJob, RuntimeKind,
};
use serde_json::json;

#[test]
fn any_conflicting_model_identity_fails_closed() {
    let policy = WorkflowClassifierPolicy {
        verdicts: vec!["allow".to_string()],
        allow: vec!["Allow a coherent change.".to_string()],
        ..WorkflowClassifierPolicy::default()
    };
    let job = RuntimeJob::pending(
        "command-1",
        RuntimeKind::ClaudeCode,
        "classifier-claude",
        json!({ "activity": "classify_scope" }),
    );
    let result = ActivityResult::succeeded("classify_scope", "classified").with_artifact(
        ActivityArtifact::new(
            "classifier_output",
            json!({ "verdict": "allow", "rationale": "looks coherent" }),
        ),
    );

    let attested = attest_result(
        &policy,
        &job,
        "requested-model",
        &[
            "substituted-model".to_string(),
            "requested-model".to_string(),
        ],
        &json!({}),
        "sha256:prompt",
        result,
    );

    assert_eq!(attested.status, ActivityStatus::Failed);
    assert!(attested.signals.is_empty());
    assert_eq!(
        attested.artifacts[0].artifact["outcome"],
        "model_identity_mismatch"
    );
    assert_eq!(
        attested.artifacts[0].artifact["attestation"]["reported_models"],
        json!(["substituted-model", "requested-model"])
    );
}
