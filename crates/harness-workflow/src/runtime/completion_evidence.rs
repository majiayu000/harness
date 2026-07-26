//! Server-verified completion evidence (GH-1766).
//!
//! Evidence kinds named here gate fact-minting transitions of the built-in
//! workflow definitions. They are minted by reducers only from
//! server-authored proof (reserved artifacts attached by the runtime worker
//! after the agent turn) or from server-observed GitHub facts. Agent-authored
//! artifacts may inform these kinds but can never impersonate them: reserved
//! artifact types are stripped from agent output before server attachment.

use super::model::ActivityResult;
use serde_json::Value;

/// Evidence kind required on `github_issue_pr` `implementing -> pr_open`.
pub const EVIDENCE_VERIFIED_PR_BINDING: &str = "verified_pr_binding";
/// Evidence kind required on `quality_gate` `checking -> passed`.
pub const EVIDENCE_SERVER_VALIDATION_DIGEST: &str = "server_validation_digest";
/// Umbrella evidence kind required on `prompt_task` `implementing -> done`.
pub const EVIDENCE_PROMPT_COMPLETION: &str = "prompt_completion_evidence";
/// Umbrella evidence kind required on non-reconciliation `github_issue_pr`
/// `-> done` transitions. Minted only for server-recognized terminal proof
/// (server-verified merged PR, structured closed-issue evidence).
pub const EVIDENCE_GITHUB_TERMINAL: &str = "github_terminal_evidence";
/// Evidence kind required on `pr_feedback` `inspecting -> ready_to_merge`.
pub const EVIDENCE_SERVER_PR_SNAPSHOT: &str = "server_pr_snapshot";

/// Server-attached artifact carrying a successful PR-binding verification.
pub const ARTIFACT_VERIFIED_PR_BINDING: &str = "verified_pr_binding";
/// Server-attached artifact recording a failed PR-binding verification.
pub const ARTIFACT_PR_BINDING_VERIFICATION_FAILED: &str = "pr_binding_verification_failed";
/// Server-attached artifact carrying the server validation digest for a
/// quality-gate run (per-command exit codes and output hashes).
pub const ARTIFACT_SERVER_VALIDATION_DIGEST: &str = "server_validation_digest";
/// Server-attached artifact declaring the completion-evidence enforcement
/// policy for this activity result. Absent means enforced (fail closed).
pub const ARTIFACT_COMPLETION_EVIDENCE_POLICY: &str = "completion_evidence_policy";

/// Blocked-decision reason when a prompt task completes without validation
/// evidence or an explicit no-change rationale.
pub const REASON_PROMPT_COMPLETION_EVIDENCE_MISSING: &str = "prompt_completion_evidence_missing";
/// Blocked-decision reason when a claimed PR binding fails server
/// verification (or was never server-verified while enforcement is active).
pub const REASON_PR_BINDING_VERIFICATION_FAILED: &str = "pr_binding_verification_failed";

/// Artifact types only the server may author on an [`ActivityResult`].
/// Agent-authored artifacts with these types must be stripped before the
/// server attaches its own.
pub const SERVER_RESERVED_ARTIFACT_TYPES: [&str; 4] = [
    ARTIFACT_VERIFIED_PR_BINDING,
    ARTIFACT_PR_BINDING_VERIFICATION_FAILED,
    ARTIFACT_SERVER_VALIDATION_DIGEST,
    ARTIFACT_COMPLETION_EVIDENCE_POLICY,
];

/// Summary recorded on waiver evidence when the operator kill switch
/// (`runtime_completion.evidence_enforced = false`) is active.
pub const WAIVER_SUMMARY: &str = "enforcement_disabled_by_config";

/// Whether completion-evidence enforcement is active for this result.
///
/// Enforcement is on unless the server explicitly attached a
/// `completion_evidence_policy` artifact with `enforced: false` (the
/// one-release operator kill switch). A missing or malformed policy artifact
/// means enforced — fail closed.
pub fn completion_evidence_enforced(result: &ActivityResult) -> bool {
    result
        .artifacts
        .iter()
        .filter(|artifact| artifact.artifact_type == ARTIFACT_COMPLETION_EVIDENCE_POLICY)
        .all(|artifact| {
            artifact
                .artifact
                .get("enforced")
                .and_then(Value::as_bool)
                .unwrap_or(true)
        })
}

/// The server-attached verified-PR-binding payload, if present.
pub fn verified_pr_binding_artifact(result: &ActivityResult) -> Option<&Value> {
    result
        .artifacts
        .iter()
        .find(|artifact| artifact.artifact_type == ARTIFACT_VERIFIED_PR_BINDING)
        .map(|artifact| &artifact.artifact)
}

/// The server-attached PR-binding verification failure payload, if present.
pub fn pr_binding_verification_failure(result: &ActivityResult) -> Option<&Value> {
    result
        .artifacts
        .iter()
        .find(|artifact| artifact.artifact_type == ARTIFACT_PR_BINDING_VERIFICATION_FAILED)
        .map(|artifact| &artifact.artifact)
}

/// The server-attached validation digest payload, if present.
pub fn server_validation_digest_artifact(result: &ActivityResult) -> Option<&Value> {
    result
        .artifacts
        .iter()
        .find(|artifact| artifact.artifact_type == ARTIFACT_SERVER_VALIDATION_DIGEST)
        .map(|artifact| &artifact.artifact)
}

/// Whether a server validation digest is present and every recorded command
/// exited zero. Missing digest or malformed payload is a failure — fail
/// closed.
pub fn server_validation_digest_passed(result: &ActivityResult) -> bool {
    let Some(digest) = server_validation_digest_artifact(result) else {
        return false;
    };
    let Some(commands) = digest.get("commands").and_then(Value::as_array) else {
        return false;
    };
    !commands.is_empty()
        && commands.iter().all(|command| {
            command.get("exit_code").and_then(Value::as_i64) == Some(0)
                && !command
                    .get("startup_error")
                    .and_then(Value::as_str)
                    .is_some_and(|error| !error.is_empty())
        })
}

/// Strip artifact types only the server may author. Called on agent-parsed
/// results before the server attaches its own reserved artifacts, so an
/// agent cannot forge server-verified evidence.
pub fn strip_server_reserved_artifacts(mut result: ActivityResult) -> ActivityResult {
    result.artifacts.retain(|artifact| {
        !SERVER_RESERVED_ARTIFACT_TYPES.contains(&artifact.artifact_type.as_str())
    });
    result
}

/// The satisfied alternative for prompt-task completion evidence, if any.
///
/// `validation_report`: a non-empty structured validation record set (or a
/// `validation_report` artifact). `no_change_rationale`: an explicit
/// structured artifact stating why no change was required.
pub fn prompt_completion_alternative(result: &ActivityResult) -> Option<&'static str> {
    let has_validation = result
        .validation
        .iter()
        .any(|record| !record.command.trim().is_empty())
        || result
            .artifacts
            .iter()
            .any(|artifact| artifact.artifact_type == "validation_report");
    if has_validation {
        return Some("validation_report");
    }
    let has_rationale = result.artifacts.iter().any(|artifact| {
        artifact.artifact_type == "no_change_rationale"
            && !artifact.artifact.is_null()
            && artifact
                .artifact
                .as_str()
                .is_none_or(|value| !value.trim().is_empty())
    });
    if has_rationale {
        return Some("no_change_rationale");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::model::{ActivityArtifact, ValidationRecord};
    use serde_json::json;

    fn result() -> ActivityResult {
        ActivityResult::succeeded("implement_prompt", "summary")
    }

    #[test]
    fn enforcement_defaults_on_and_honors_explicit_waiver() {
        assert!(completion_evidence_enforced(&result()));
        let waived = result().with_artifact(ActivityArtifact::new(
            ARTIFACT_COMPLETION_EVIDENCE_POLICY,
            json!({ "enforced": false }),
        ));
        assert!(!completion_evidence_enforced(&waived));
        let malformed = result().with_artifact(ActivityArtifact::new(
            ARTIFACT_COMPLETION_EVIDENCE_POLICY,
            json!({}),
        ));
        assert!(completion_evidence_enforced(&malformed));
    }

    #[test]
    fn digest_passes_only_when_every_command_exits_zero() {
        assert!(!server_validation_digest_passed(&result()));
        let passing = result().with_artifact(ActivityArtifact::new(
            ARTIFACT_SERVER_VALIDATION_DIGEST,
            json!({ "commands": [
                { "command": "cargo test", "exit_code": 0, "output_sha256": "a" },
            ]}),
        ));
        assert!(server_validation_digest_passed(&passing));
        let failing = result().with_artifact(ActivityArtifact::new(
            ARTIFACT_SERVER_VALIDATION_DIGEST,
            json!({ "commands": [
                { "command": "cargo test", "exit_code": 0 },
                { "command": "cargo clippy", "exit_code": 101 },
            ]}),
        ));
        assert!(!server_validation_digest_passed(&failing));
        let empty = result().with_artifact(ActivityArtifact::new(
            ARTIFACT_SERVER_VALIDATION_DIGEST,
            json!({ "commands": [] }),
        ));
        assert!(!server_validation_digest_passed(&empty));
        let startup_error = result().with_artifact(ActivityArtifact::new(
            ARTIFACT_SERVER_VALIDATION_DIGEST,
            json!({ "commands": [
                { "command": "cargo test", "exit_code": 0, "startup_error": "spawn failed" },
            ]}),
        ));
        assert!(!server_validation_digest_passed(&startup_error));
    }

    #[test]
    fn agent_cannot_forge_server_reserved_artifacts() {
        let forged = result()
            .with_artifact(ActivityArtifact::new(
                ARTIFACT_VERIFIED_PR_BINDING,
                json!({ "pr_number": 1 }),
            ))
            .with_artifact(ActivityArtifact::new(
                ARTIFACT_SERVER_VALIDATION_DIGEST,
                json!({ "commands": [{ "command": "true", "exit_code": 0 }] }),
            ))
            .with_artifact(ActivityArtifact::new(
                "pull_request",
                json!({ "pr_number": 1 }),
            ));
        let stripped = strip_server_reserved_artifacts(forged);
        assert_eq!(stripped.artifacts.len(), 1);
        assert_eq!(stripped.artifacts[0].artifact_type, "pull_request");
    }

    #[test]
    fn prompt_completion_alternatives() {
        assert_eq!(prompt_completion_alternative(&result()), None);
        let validated = result().with_validation(ValidationRecord::new("cargo test", "passed"));
        assert_eq!(
            prompt_completion_alternative(&validated),
            Some("validation_report")
        );
        let rationale = result().with_artifact(ActivityArtifact::new(
            "no_change_rationale",
            json!("question answered without code change"),
        ));
        assert_eq!(
            prompt_completion_alternative(&rationale),
            Some("no_change_rationale")
        );
        let empty_rationale =
            result().with_artifact(ActivityArtifact::new("no_change_rationale", json!("   ")));
        assert_eq!(prompt_completion_alternative(&empty_rationale), None);
    }
}
