use super::super::activity_result::StructuredOutputCorrection;
use harness_core::types::Item;
use harness_workflow::runtime::{ActivityArtifact, RuntimeJob, RuntimeKind, WorkflowRuntimeStore};
use serde_json::{json, Value};

const CORRECTION_TRANSCRIPT_EXCERPT_LIMIT: usize = 4000;
const CORRECTION_TRANSCRIPT_SEPARATOR: &str = "\n\n";

pub(super) struct CodexOutputSchemaFile {
    pub(super) path: std::path::PathBuf,
    _directory: tempfile::TempDir,
}

pub(super) fn codex_output_schema_file(
    force_code_agent: bool,
    job: &RuntimeJob,
    prompt_packet: &Value,
) -> anyhow::Result<Option<CodexOutputSchemaFile>> {
    if !force_code_agent
        || !matches!(
            job.runtime_kind,
            RuntimeKind::CodexExec | RuntimeKind::CodexJsonrpc
        )
    {
        return Ok(None);
    }
    let Some(schema) = prompt_packet.pointer("/activity_result_schema/json_schema") else {
        return Ok(None);
    };
    let directory = tempfile::Builder::new()
        .prefix(".harness-codex-output-schema-")
        .tempdir()?;
    let path = directory.path().join("activity-result-schema.json");
    std::fs::write(&path, serde_json::to_vec_pretty(schema)?)?;
    Ok(Some(CodexOutputSchemaFile {
        path,
        _directory: directory,
    }))
}

pub(super) fn structured_output_correction_prompt(
    original_prompt: &str,
    correction: &StructuredOutputCorrection,
    items: &[Item],
) -> String {
    format!(
        "{original_prompt}\n\nStructured output correction retry:\n\
         The previous attempt completed, but Harness could not parse its ActivityResult.\n\
         Do not edit files, run commands, push, create PRs, comment, or mutate workflow state.\n\
         Return only a corrected ActivityResult JSON object matching activity_result_schema. \
         Use string `error`, scalar string `error_kind`, and arrays for `artifacts`, `signals`, and `validation`.\n\
         Diagnostic: outcome={} error={} extracted_activity={}\n\
         Previous turn transcript excerpt:\n```text\n{}\n```\n",
        correction.outcome,
        correction.error,
        correction.extracted_activity.as_deref().unwrap_or("<none>"),
        previous_output_tail(items)
    )
}

pub(super) async fn reserve_structured_output_correction_turn(
    store: Option<&WorkflowRuntimeStore>,
    job: &RuntimeJob,
    max_turns: Option<u32>,
    attempt: u32,
) -> anyhow::Result<bool> {
    let Some(store) = store else {
        return Ok(true);
    };
    let payload = json!({
        "owner": "structured_output_correction_retry",
        "lease_generation": job.lease_generation,
        "attempt": attempt,
        "reservation_key": format!(
            "structured_output_correction_retry:{}:{attempt}",
            job.lease_generation
        ),
    });
    if let Some(max_turns) = max_turns {
        let Some(command) = store.get_command(&job.command_id).await? else {
            return Ok(false);
        };
        return Ok(store
            .reserve_runtime_turn_started_for_workflow(
                &command.workflow_id,
                &job.id,
                max_turns,
                payload,
            )
            .await?
            .is_some());
    }
    store
        .record_runtime_event(&job.id, "RuntimeTurnStarted", payload)
        .await?;
    Ok(true)
}

pub(super) fn structured_output_correction_artifact(
    correction: &StructuredOutputCorrection,
    attempts: u32,
) -> ActivityArtifact {
    ActivityArtifact::new(
        "structured_output_correction_retry",
        json!({"schema":"harness.runtime.structured_output_correction_retry.v1","attempts":attempts,"initial_outcome":correction.outcome,"initial_error":correction.error,"initial_extracted_activity":correction.extracted_activity}),
    )
}

fn previous_output_tail(items: &[Item]) -> String {
    let mut excerpts = Vec::new();
    let mut used = 0;
    for content in items
        .iter()
        .rev()
        .filter_map(correction_context_content)
        .map(str::trim)
        .filter(|content| !content.is_empty())
    {
        let separator_len = if excerpts.is_empty() {
            0
        } else {
            CORRECTION_TRANSCRIPT_SEPARATOR.len()
        };
        if used + separator_len >= CORRECTION_TRANSCRIPT_EXCERPT_LIMIT {
            break;
        }
        let remaining = CORRECTION_TRANSCRIPT_EXCERPT_LIMIT - used - separator_len;
        let excerpt = tail_chars(content, remaining);
        used += separator_len + excerpt.chars().count();
        excerpts.push(excerpt);
        if used >= CORRECTION_TRANSCRIPT_EXCERPT_LIMIT {
            break;
        }
    }
    excerpts.reverse();
    excerpts.join(CORRECTION_TRANSCRIPT_SEPARATOR)
}

fn correction_context_content(item: &Item) -> Option<&str> {
    match item {
        Item::AgentReasoning { content } => Some(content),
        Item::Error { message, .. } => Some(message),
        _ => None,
    }
}

fn tail_chars(value: &str, limit: usize) -> String {
    value
        .chars()
        .rev()
        .take(limit)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_output_schema_file_is_readable_until_guard_drops() -> anyhow::Result<()> {
        let job = RuntimeJob::pending("c", RuntimeKind::CodexJsonrpc, "p", json!({}));
        let prompt_packet = json!({"activity_result_schema":{"json_schema":{"type":"object","required":["activity"]}}});
        let schema_file = codex_output_schema_file(true, &job, &prompt_packet)?.unwrap();
        let path = schema_file.path.clone();
        let schema: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
        assert_eq!(schema["type"], "object");
        drop(schema_file);
        assert!(!path.exists());
        Ok(())
    }
    #[test]
    fn previous_output_tail_keeps_recent_relevant_bounded_context() {
        assert_eq!(
            previous_output_tail(&[
                reasoning("first"),
                Item::Error {
                    code: 1,
                    message: "last error".to_string(),
                },
            ]),
            "first\n\nlast error"
        );
        let latest = format!(
            "{}LATEST_SENTINEL",
            "x".repeat(CORRECTION_TRANSCRIPT_EXCERPT_LIMIT + 1000)
        );
        let items = vec![reasoning("OLD_SENTINEL".repeat(1000)), reasoning(latest)];
        let excerpt = previous_output_tail(&items);
        assert!(excerpt.chars().count() <= CORRECTION_TRANSCRIPT_EXCERPT_LIMIT);
        assert!(excerpt.contains("LATEST_SENTINEL"));
        assert!(!excerpt.contains("OLD_SENTINEL"));
    }

    fn reasoning(content: impl Into<String>) -> Item {
        Item::AgentReasoning {
            content: content.into(),
        }
    }
}
