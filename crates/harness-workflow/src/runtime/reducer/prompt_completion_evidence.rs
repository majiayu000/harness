//! The prompt-task completion contract.
//!
//! A prompt task may claim Done only if it presented something a reader can
//! check: the commands it ran with their exit codes, or an explicit statement
//! that no change was needed. Agent-authored prose in a `ValidationRecord`
//! names a command but proves nothing about whether it ran.

use crate::runtime::model::{ActivityResult, WorkflowEvidence, EVIDENCE_PROMPT_COMPLETION};
use crate::runtime::prompt_task::PROMPT_TASK_DEFINITION_ID;
use crate::runtime::state_registry::transition_requires_evidence;
use serde_json::Value;

/// The artifact carrying the commands a prompt task ran, as
/// `[{ "command": ..., "exit_code": ... }]`.
pub const PROMPT_VALIDATION_REPORT_ARTIFACT: &str = "validation_report";

/// The artifact carrying a prompt task's structured explanation for producing
/// no change.
pub const PROMPT_NO_CHANGE_RATIONALE_ARTIFACT: &str = "no_change_rationale";

/// Which alternative satisfied the completion contract, or why neither did.
pub(super) enum PromptCompletionEvidence {
    ValidationReport { commands: usize, failures: usize },
    NoChangeRationale,
}

impl PromptCompletionEvidence {
    pub(super) fn evidence(&self) -> WorkflowEvidence {
        match self {
            Self::ValidationReport { commands, failures } => WorkflowEvidence::new(
                EVIDENCE_PROMPT_COMPLETION,
                format!(
                    "validation_report: {commands} command(s) reported, {failures} non-zero exit(s)"
                ),
            ),
            Self::NoChangeRationale => WorkflowEvidence::new(
                EVIDENCE_PROMPT_COMPLETION,
                "no_change_rationale: the task reported no change with a stated reason",
            ),
        }
    }
}

/// Resolve the disjunctive completion contract: a prompt task may claim Done
/// only if it presented a validation report or an explicit no-change rationale.
///
/// `TransitionRule::required_evidence` is a conjunctive set, so the OR is
/// resolved here and a single umbrella evidence kind is minted. The transition
/// table then requires only that kind, and a done-decision that bypasses this
/// check still fails validation.
///
/// `Ok(None)` means the operator disabled enforcement for this release: the
/// transition tables have had their requirements stripped too, so minting Done
/// without evidence is consistent rather than a hole.
pub(super) fn prompt_completion_evidence(
    result: &ActivityResult,
) -> Result<Option<PromptCompletionEvidence>, String> {
    if !enforced() {
        return Ok(None);
    }
    resolve_completion_evidence(result).map(Some)
}

/// The transition table is the authority: if it no longer demands the kind,
/// the operator lifted the contract and the reducer must not block on it.
fn enforced() -> bool {
    transition_requires_evidence(
        PROMPT_TASK_DEFINITION_ID,
        "implementing",
        "done",
        EVIDENCE_PROMPT_COMPLETION,
    )
}

fn resolve_completion_evidence(
    result: &ActivityResult,
) -> Result<PromptCompletionEvidence, String> {
    let mut invalid_alternatives = Vec::new();
    for artifact in artifacts_of_type(result, PROMPT_VALIDATION_REPORT_ARTIFACT) {
        match validation_report_evidence(artifact) {
            Ok(evidence) => return Ok(evidence),
            Err(error) => invalid_alternatives.push(error),
        }
    }
    for artifact in artifacts_of_type(result, PROMPT_NO_CHANGE_RATIONALE_ARTIFACT) {
        match no_change_rationale_evidence(artifact) {
            Ok(evidence) => return Ok(evidence),
            Err(error) => invalid_alternatives.push(error),
        }
    }
    if invalid_alternatives.is_empty() {
        Err(format!(
            "completion requires a `{PROMPT_VALIDATION_REPORT_ARTIFACT}` artifact ([{{command, exit_code}}]) or a `{PROMPT_NO_CHANGE_RATIONALE_ARTIFACT}` string artifact"
        ))
    } else {
        Err(invalid_alternatives.join("; "))
    }
}

fn validation_report_evidence(artifact: &Value) -> Result<PromptCompletionEvidence, String> {
    let entries = artifact.as_array().ok_or_else(|| {
        format!(
            "`{PROMPT_VALIDATION_REPORT_ARTIFACT}` must be an array of {{command, exit_code}} entries"
        )
    })?;
    if entries.is_empty() {
        return Err(format!(
            "`{PROMPT_VALIDATION_REPORT_ARTIFACT}` is empty; report the commands you ran or supply `{PROMPT_NO_CHANGE_RATIONALE_ARTIFACT}`"
        ));
    }
    let mut failures = 0usize;
    for entry in entries {
        let command = entry.get("command").and_then(Value::as_str);
        let exit_code = entry.get("exit_code").and_then(Value::as_i64);
        match (command, exit_code) {
            (Some(command), Some(exit_code)) => {
                if command.trim().is_empty() {
                    return Err(format!(
                        "`{PROMPT_VALIDATION_REPORT_ARTIFACT}` entry has an empty `command`"
                    ));
                }
                if exit_code != 0 {
                    failures += 1;
                }
            }
            _ => {
                return Err(format!(
                    "each `{PROMPT_VALIDATION_REPORT_ARTIFACT}` entry needs a string `command` and an integer `exit_code`"
                ));
            }
        }
    }
    Ok(PromptCompletionEvidence::ValidationReport {
        commands: entries.len(),
        failures,
    })
}

fn no_change_rationale_evidence(artifact: &Value) -> Result<PromptCompletionEvidence, String> {
    let rationale = artifact.as_str().ok_or_else(|| {
        format!(
            "`{PROMPT_NO_CHANGE_RATIONALE_ARTIFACT}` must be a string explaining why no change was made"
        )
    })?;
    if rationale.trim().is_empty() {
        return Err(format!(
            "`{PROMPT_NO_CHANGE_RATIONALE_ARTIFACT}` is empty; state why no change was made"
        ));
    }
    Ok(PromptCompletionEvidence::NoChangeRationale)
}

fn artifacts_of_type<'a>(
    result: &'a ActivityResult,
    artifact_type: &'a str,
) -> impl Iterator<Item = &'a Value> {
    result
        .artifacts
        .iter()
        .filter(move |artifact| artifact.artifact_type == artifact_type)
        .map(|artifact| &artifact.artifact)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::model::ActivityArtifact;
    use serde_json::json;

    fn result_with_artifacts(
        artifacts: impl IntoIterator<Item = ActivityArtifact>,
    ) -> ActivityResult {
        artifacts.into_iter().fold(
            ActivityResult::succeeded("implement_prompt", "Prompt task completed."),
            ActivityResult::with_artifact,
        )
    }

    #[test]
    fn valid_no_change_rationale_wins_over_a_malformed_validation_report() {
        let result = result_with_artifacts([
            ActivityArtifact::new(PROMPT_VALIDATION_REPORT_ARTIFACT, json!([])),
            ActivityArtifact::new(
                PROMPT_NO_CHANGE_RATIONALE_ARTIFACT,
                json!("No repository change was necessary."),
            ),
        ]);

        assert!(matches!(
            resolve_completion_evidence(&result),
            Ok(PromptCompletionEvidence::NoChangeRationale)
        ));
    }

    #[test]
    fn later_valid_validation_report_wins_over_an_earlier_malformed_report() {
        let result = result_with_artifacts([
            ActivityArtifact::new(
                PROMPT_VALIDATION_REPORT_ARTIFACT,
                json!([{ "command": "cargo test" }]),
            ),
            ActivityArtifact::new(
                PROMPT_VALIDATION_REPORT_ARTIFACT,
                json!([{ "command": "cargo test", "exit_code": 0 }]),
            ),
        ]);

        assert!(matches!(
            resolve_completion_evidence(&result),
            Ok(PromptCompletionEvidence::ValidationReport {
                commands: 1,
                failures: 0
            })
        ));
    }
}
