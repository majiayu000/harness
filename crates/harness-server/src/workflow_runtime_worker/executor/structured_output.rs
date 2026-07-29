use super::super::activity_result::StructuredOutputCorrection;
use anyhow::Context;
use harness_core::types::Item;
use harness_workflow::runtime::{ActivityArtifact, RuntimeJob, RuntimeKind};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub(super) struct CodexOutputSchemaFile(PathBuf);

impl CodexOutputSchemaFile {
    pub(super) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for CodexOutputSchemaFile {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_file(&self.0) {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(
                    path = %self.0.display(),
                    "failed to remove temporary codex output schema file: {error}"
                );
            }
        }
    }
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
    let directory = std::env::temp_dir().join("harness-codex-output-schema");
    std::fs::create_dir_all(&directory).with_context(|| {
        format!(
            "failed to create codex output schema directory {}",
            directory.display()
        )
    })?;
    let path = directory.join(format!(
        "{}-activity-result-schema.json",
        sanitize_file_component(&job.id)
    ));
    let bytes = serde_json::to_vec_pretty(schema)
        .context("failed to serialize codex ActivityResult output schema")?;
    std::fs::write(&path, bytes).with_context(|| {
        format!(
            "failed to write codex ActivityResult output schema {}",
            path.display()
        )
    })?;
    Ok(Some(CodexOutputSchemaFile(path)))
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
         Previous final output:\n```text\n{}\n```\n",
        correction.outcome,
        correction.error,
        correction.extracted_activity.as_deref().unwrap_or("<none>"),
        previous_output_tail(items)
    )
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
    let mut output = items
        .iter()
        .rev()
        .filter_map(|item| match item {
            Item::AgentReasoning { content }
            | Item::Error {
                message: content, ..
            } => {
                let content = content.trim();
                (!content.is_empty()).then_some(content)
            }
            _ => None,
        })
        .take(3)
        .collect::<Vec<_>>();
    output.reverse();
    truncate_chars(&output.join("\n\n"), 4000)
}

fn truncate_chars(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        return value.to_string();
    }
    let mut truncated = value.chars().take(limit).collect::<String>();
    truncated.push_str("...");
    truncated
}

fn sanitize_file_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}
