use crate::task_runner::TaskId;
use harness_workflow::runtime::{
    WorkflowInstance, WorkflowRuntimeStore, PROMPT_TASK_DEFINITION_ID,
};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
};

use super::optional_string_field;

static PROMPT_SUBMISSION_PROMPTS: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

pub(crate) fn lookup_prompt_submission_prompt(prompt_ref: &str) -> Option<String> {
    prompt_submission_prompts()
        .lock()
        .ok()
        .and_then(|prompts| prompts.get(prompt_ref).cloned())
}

pub(crate) async fn lookup_prompt_submission_prompt_durable(
    store: &WorkflowRuntimeStore,
    prompt_ref: &str,
) -> anyhow::Result<Option<String>> {
    if let Some(prompt) = lookup_prompt_submission_prompt(prompt_ref) {
        return Ok(Some(prompt));
    }
    let Some(prompt) = store.get_prompt_payload(prompt_ref).await? else {
        return Ok(None);
    };
    cache_prompt_submission_prompt(prompt_ref, &prompt);
    Ok(Some(prompt))
}

pub(super) async fn persist_prompt_submission_prompt(
    store: &WorkflowRuntimeStore,
    prompt_ref: &str,
    prompt: &str,
) -> anyhow::Result<()> {
    store.upsert_prompt_payload(prompt_ref, prompt).await?;
    cache_prompt_submission_prompt(prompt_ref, prompt);
    Ok(())
}

pub(super) fn cache_prompt_submission_prompt(prompt_ref: &str, prompt: &str) {
    if let Ok(mut prompts) = prompt_submission_prompts().lock() {
        prompts.insert(prompt_ref.to_string(), prompt.to_string());
    }
}

pub(super) async fn remove_prompt_submission_prompt_durable(
    store: &WorkflowRuntimeStore,
    prompt_ref: Option<&str>,
) -> anyhow::Result<()> {
    let Some(prompt_ref) = prompt_ref else {
        return Ok(());
    };
    store.delete_prompt_payload(prompt_ref).await?;
    remove_prompt_submission_prompt(Some(prompt_ref));
    Ok(())
}

pub(super) fn remove_prompt_submission_prompt(prompt_ref: Option<&str>) {
    let Some(prompt_ref) = prompt_ref else {
        return;
    };
    if let Ok(mut prompts) = prompt_submission_prompts().lock() {
        prompts.remove(prompt_ref);
    }
}

#[cfg(test)]
pub(crate) fn clear_prompt_submission_prompt_cache_for_test(prompt_ref: &str) {
    if let Ok(mut prompts) = prompt_submission_prompts().lock() {
        prompts.remove(prompt_ref);
    }
}

#[cfg(test)]
pub(crate) fn remove_terminal_prompt_submission_prompt(instance: &WorkflowInstance) {
    if instance.definition_id != PROMPT_TASK_DEFINITION_ID || !instance.is_terminal() {
        return;
    }
    remove_prompt_submission_prompt(optional_string_field(&instance.data, "prompt_ref").as_deref());
}

pub(crate) async fn remove_terminal_prompt_submission_payload(
    store: &WorkflowRuntimeStore,
    instance: &WorkflowInstance,
) -> anyhow::Result<()> {
    if instance.definition_id != PROMPT_TASK_DEFINITION_ID || !instance.is_terminal() {
        return Ok(());
    }
    remove_prompt_submission_prompt_durable(
        store,
        optional_string_field(&instance.data, "prompt_ref").as_deref(),
    )
    .await
}

fn prompt_submission_prompts() -> &'static Mutex<HashMap<String, String>> {
    PROMPT_SUBMISSION_PROMPTS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(super) fn prompt_ref_for_submission(
    project_id: &str,
    external_id: Option<&str>,
    task_id: &TaskId,
    prompt: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(project_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(external_id.unwrap_or("").as_bytes());
    hasher.update(b"\0");
    hasher.update(task_id.as_str().as_bytes());
    hasher.update(b"\0");
    hasher.update(prompt.as_bytes());
    let digest = hasher.finalize();
    let digest_hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("prompt-memory:{digest_hex}")
}
