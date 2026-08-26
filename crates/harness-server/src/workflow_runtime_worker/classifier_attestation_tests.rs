use super::*;
use harness_core::config::workflow::WorkflowClassifierPolicy;
use harness_workflow::runtime::{ActivityArtifact, ActivityResult, RuntimeJob, RuntimeKind};
use serde_json::json;

#[test]
fn pr_attestation_carries_head_from_classifier_packet_into_assessment() {
    let policy = WorkflowClassifierPolicy {
        verdicts: vec!["allow".to_string()],
        environment: vec!["Judge only supplied facts.".to_string()],
        ..WorkflowClassifierPolicy::default()
    };
    let job = RuntimeJob::pending(
        "command-1",
        RuntimeKind::CodexJsonrpc,
        "classifier-default",
        json!({"activity": "classify_scope"}),
    );
    let result = ActivityResult::succeeded("classify_scope", "classified").with_artifact(
        ActivityArtifact::new(
            CLASSIFIER_OUTPUT_ARTIFACT,
            json!({
                "verdict": "allow",
                "rationale": "The complete head-bound diff matches the issue plan.",
                "evidence_refs": ["/classifier_facts/facts/server_pr_snapshot/snapshot/head_oid"]
            }),
        ),
    );
    let prompt_packet = json!({
        "classifier_facts": {
            "facts": {
                "server_pr_snapshot": {
                    "snapshot": {"head_oid": "head-456"}
                }
            }
        }
    });

    let attested = attest_result(
        &policy,
        &job,
        "gpt-requested",
        &["gpt-requested".to_string()],
        &prompt_packet,
        "sha256:prompt",
        result,
    );

    assert_eq!(
        attested.artifacts[0].artifact["subject_head_oid"],
        "head-456"
    );
    assert_eq!(attested.signals[0].signal["subject_head_oid"], "head-456");
}
