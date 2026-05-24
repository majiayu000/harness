use crate::task_runner::TaskId;
use harness_workflow::runtime::{WorkflowInstance, PROMPT_TASK_DEFINITION_ID};
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

pub(super) fn cache_prompt_submission_prompt(prompt_ref: &str, prompt: &str) {
    if let Ok(mut prompts) = prompt_submission_prompts().lock() {
        prompts.insert(prompt_ref.to_string(), prompt.to_string());
    }
}

pub(super) fn remove_prompt_submission_prompt(prompt_ref: Option<&str>) {
    let Some(prompt_ref) = prompt_ref else {
        return;
    };
    if let Ok(mut prompts) = prompt_submission_prompts().lock() {
        prompts.remove(prompt_ref);
    }
}

pub(crate) fn remove_terminal_prompt_submission_prompt(instance: &WorkflowInstance) {
    if instance.definition_id != PROMPT_TASK_DEFINITION_ID || !instance.is_terminal() {
        return;
    }
    remove_prompt_submission_prompt(optional_string_field(&instance.data, "prompt_ref").as_deref());
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
