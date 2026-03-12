use crate::task_runner::TaskId;
use crate::workspace::WorkspaceManager;
use harness_core::{AgentRequest, AgentResponse, CodeAgent, ContextItem};
use std::path::Path;
use std::sync::Arc;
use tokio::time::Duration;

const PARALLEL_EXTENSIONS: &[&str] = &[
    "rs", "ts", "tsx", "js", "jsx", "py", "go", "java", "kt", "swift", "cpp", "c", "h", "toml",
    "yaml", "yml", "json", "sh", "md",
];

fn extract_file_refs(prompt: &str) -> Vec<String> {
    let mut refs: Vec<String> = prompt
        .split_whitespace()
        .filter_map(|token| {
            let token = token.trim_matches(|c: char| {
                !c.is_alphanumeric() && c != '.' && c != '_' && c != '-' && c != '/'
            });
            token.rfind('.').and_then(|dot_pos| {
                let ext = &token[dot_pos + 1..];
                PARALLEL_EXTENSIONS
                    .contains(&ext)
                    .then_some(token.to_string())
            })
        })
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    refs.sort();
    refs
}

/// Decompose a complex prompt into parallel subtask prompts.
///
/// Partitions file references evenly across two subtasks. Each subtask receives
/// the original prompt with an added focus directive. Returns a single-element
/// vec when decomposition is not meaningful (< 2 file references found).
pub fn decompose(prompt: &str) -> Vec<String> {
    let files = extract_file_refs(prompt);
    if files.len() < 2 {
        return vec![prompt.to_string()];
    }
    let mid = files.len().div_ceil(2);
    let total_chunks = files.chunks(mid).count();
    files
        .chunks(mid)
        .enumerate()
        .map(|(i, group)| {
            format!(
                "{}\n\n[Parallel subtask {}/{}] Focus on these files: {}",
                prompt,
                i + 1,
                total_chunks,
                group.join(", ")
            )
        })
        .collect()
}

/// Result of a single parallel subtask execution.
pub struct SubtaskResult {
    /// Zero-based index of this subtask within the parallel batch.
    pub index: usize,
    /// Agent response when execution succeeded.
    pub response: Option<AgentResponse>,
    /// Error description when execution failed.
    pub error: Option<String>,
}

/// Run multiple agent executions concurrently, each in an isolated git worktree.
///
/// Each subtask gets a derived `TaskId` of the form `{task_id}-p{index}` and its
/// own worktree branch. Workspaces are removed after all executions finish,
/// regardless of individual subtask outcome.
///
/// Individual subtask failures are captured in `SubtaskResult::error` and do not
/// abort the other subtasks.
pub async fn run_parallel_subtasks(
    task_id: &TaskId,
    agent: Arc<dyn CodeAgent>,
    subtask_prompts: Vec<String>,
    workspace_mgr: Arc<WorkspaceManager>,
    source_repo: &Path,
    base_branch: &str,
    context: Vec<ContextItem>,
    turn_timeout: Duration,
) -> Vec<SubtaskResult> {
    let count = subtask_prompts.len();
    let mut handles: Vec<tokio::task::JoinHandle<(usize, Result<AgentResponse, String>)>> =
        Vec::with_capacity(count);
    let mut sub_ids: Vec<Option<TaskId>> = Vec::with_capacity(count);

    for (i, prompt) in subtask_prompts.into_iter().enumerate() {
        let sub_id = TaskId(format!("{}-p{i}", task_id.0));
        match workspace_mgr
            .create_workspace(&sub_id, source_repo, base_branch)
            .await
        {
            Ok(workspace) => {
                sub_ids.push(Some(sub_id));
                let agent = agent.clone();
                let context = context.clone();
                let req = AgentRequest {
                    prompt,
                    project_root: workspace,
                    context,
                    ..Default::default()
                };
                handles.push(tokio::spawn(async move {
                    let result = match tokio::time::timeout(turn_timeout, agent.execute(req)).await
                    {
                        Ok(Ok(resp)) => Ok(resp),
                        Ok(Err(e)) => Err(format!("agent error: {e}")),
                        Err(_) => Err(format!(
                            "subtask timed out after {}s",
                            turn_timeout.as_secs()
                        )),
                    };
                    (i, result)
                }));
            }
            Err(e) => {
                tracing::warn!("parallel_dispatch: workspace creation failed for subtask {i}: {e}");
                sub_ids.push(None);
                handles.push(tokio::spawn(async move {
                    (i, Err(format!("workspace creation failed: {e}")))
                }));
            }
        }
    }

    let mut results = Vec::with_capacity(count);
    for handle in handles {
        match handle.await {
            Ok((index, Ok(resp))) => results.push(SubtaskResult {
                index,
                response: Some(resp),
                error: None,
            }),
            Ok((index, Err(err))) => {
                tracing::warn!("parallel subtask {index} failed: {err}");
                results.push(SubtaskResult {
                    index,
                    response: None,
                    error: Some(err),
                });
            }
            Err(join_err) => {
                tracing::warn!("parallel subtask join error: {join_err}");
                results.push(SubtaskResult {
                    index: usize::MAX,
                    response: None,
                    error: Some(format!("subtask panicked: {join_err}")),
                });
            }
        }
    }

    // Clean up subtask workspaces after all executions complete.
    for sub_id in sub_ids.into_iter().flatten() {
        if let Err(e) = workspace_mgr.remove_workspace(&sub_id).await {
            tracing::warn!("parallel_dispatch: workspace cleanup failed for {sub_id:?}: {e}");
        }
    }

    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decompose_no_files_returns_original() {
        let prompt = "Fix the login bug";
        let subtasks = decompose(prompt);
        assert_eq!(subtasks.len(), 1);
        assert_eq!(subtasks[0], prompt);
    }

    #[test]
    fn decompose_one_file_returns_original() {
        let prompt = "Fix the bug in src/main.rs";
        let subtasks = decompose(prompt);
        assert_eq!(subtasks.len(), 1);
        assert_eq!(subtasks[0], prompt);
    }

    #[test]
    fn decompose_two_files_yields_two_subtasks() {
        let prompt = "Update src/auth.rs and src/db.rs";
        let subtasks = decompose(prompt);
        assert_eq!(subtasks.len(), 2);
    }

    #[test]
    fn decompose_multiple_files_splits_into_two() {
        let prompt = "Refactor src/a.rs src/b.rs src/c.rs src/d.rs";
        let subtasks = decompose(prompt);
        assert_eq!(subtasks.len(), 2);
        assert!(subtasks[0].contains("[Parallel subtask 1/2]"));
        assert!(subtasks[1].contains("[Parallel subtask 2/2]"));
    }

    #[test]
    fn decompose_subtasks_start_with_original_prompt() {
        let prompt = "Fix auth.rs and db.rs together";
        let subtasks = decompose(prompt);
        for subtask in &subtasks {
            assert!(subtask.starts_with(prompt));
        }
    }

    #[test]
    fn decompose_six_files_yields_two_subtasks() {
        let prompt = "Refactor src/a.rs src/b.rs src/c.rs src/d.rs src/e.rs src/f.rs";
        let subtasks = decompose(prompt);
        assert_eq!(subtasks.len(), 2);
    }

    #[test]
    fn extract_file_refs_no_files() {
        let files = extract_file_refs("Fix the login bug");
        assert!(files.is_empty());
    }

    #[test]
    fn extract_file_refs_deduplicates() {
        let files = extract_file_refs("src/auth.rs and src/auth.rs again");
        assert_eq!(files.len(), 1);
    }

    #[test]
    fn extract_file_refs_returns_sorted() {
        let files = extract_file_refs("src/b.rs src/a.rs src/c.rs");
        assert_eq!(files, vec!["src/a.rs", "src/b.rs", "src/c.rs"]);
    }

    #[test]
    fn decompose_subtask_contains_focus_directive() {
        let prompt = "Update auth.rs and db.rs";
        let subtasks = decompose(prompt);
        assert_eq!(subtasks.len(), 2);
        assert!(subtasks[0].contains("Focus on these files:"));
        assert!(subtasks[1].contains("Focus on these files:"));
    }
}
