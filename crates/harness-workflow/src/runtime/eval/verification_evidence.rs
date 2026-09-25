use crate::runtime::completion_evidence::ARTIFACT_SERVER_VALIDATION_DIGEST;
use crate::runtime::ActivityResult;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalValidationCommandEvidence {
    pub command: String,
    #[serde(default)]
    pub argv: Vec<String>,
    pub exit_code: Option<i64>,
    pub output_sha256: Option<String>,
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verifier_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verifier_sha256: Option<String>,
}

pub(super) fn validation_command_evidence(
    result: Option<&ActivityResult>,
) -> Vec<EvalValidationCommandEvidence> {
    let Some(commands) = result
        .into_iter()
        .flat_map(|result| &result.artifacts)
        .find(|artifact| artifact.artifact_type == ARTIFACT_SERVER_VALIDATION_DIGEST)
        .and_then(|artifact| artifact.artifact.get("commands"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };

    commands.iter().filter_map(command_evidence).collect()
}

fn command_evidence(value: &Value) -> Option<EvalValidationCommandEvidence> {
    let command = value.get("command")?.as_str()?.to_string();
    let argv = value
        .get("argv")
        .and_then(Value::as_array)
        .map(|arguments| {
            arguments
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let (verifier_id, verifier_sha256) = trusted_verifier_identity(&argv);
    Some(EvalValidationCommandEvidence {
        command,
        argv,
        exit_code: value.get("exit_code").and_then(Value::as_i64),
        output_sha256: value
            .get("output_sha256")
            .and_then(Value::as_str)
            .map(str::to_string),
        duration_ms: value.get("duration_ms").and_then(Value::as_u64),
        verifier_id,
        verifier_sha256,
    })
}

fn trusted_verifier_identity(argv: &[String]) -> (Option<String>, Option<String>) {
    if argv.first().map(String::as_str) != Some("harness")
        || argv.get(1).map(String::as_str) != Some("eval")
        || argv.get(2).map(String::as_str) != Some("verify-trusted")
    {
        return (None, None);
    }
    let verifier_id = argv.get(3).cloned();
    let verifier_sha256 = argv
        .windows(2)
        .find(|window| window[0] == "--verifier-sha256")
        .map(|window| window[1].clone());
    (verifier_id, verifier_sha256)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{ActivityArtifact, EvalTrustedVerifier};
    use serde_json::json;

    #[test]
    fn trusted_verifier_identity_and_output_digest_are_collected() {
        let verifier = EvalTrustedVerifier::Gh1454CiContractV1;
        let result = ActivityResult::succeeded("run_quality_gate", "passed").with_artifact(
            ActivityArtifact::new(
                ARTIFACT_SERVER_VALIDATION_DIGEST,
                json!({"commands": [{
                    "command": "harness eval verify-trusted",
                    "argv": verifier.validation_argv(),
                    "exit_code": 0,
                    "output_sha256": "a".repeat(64),
                    "duration_ms": 12
                }]}),
            ),
        );

        let evidence = validation_command_evidence(Some(&result));

        assert_eq!(evidence[0].verifier_id.as_deref(), Some(verifier.id()));
        assert_eq!(
            evidence[0].verifier_sha256.as_deref(),
            Some(verifier.sha256())
        );
        let output_sha256 = "a".repeat(64);
        assert_eq!(
            evidence[0].output_sha256.as_deref(),
            Some(output_sha256.as_str())
        );
    }
}
