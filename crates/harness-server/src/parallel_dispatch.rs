use crate::task_runner::TaskId;
use crate::workspace::WorkspaceManager;
use harness_core::{
    agent::AgentRequest, agent::AgentResponse, agent::CodeAgent, capability::CapabilityToken,
    types::ContextItem,
};
use std::path::Path;
use std::sync::Arc;
use tokio::time::Duration;

/// RAII guard that aborts a spawned Tokio task when dropped.
///
/// This ensures that if the parent future is cancelled (e.g. via the external
/// cancel endpoint), the child task is also aborted rather than running to
/// completion detached.
struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Maximum number of parallel subtasks — caps both chunk count in `decompose`
/// and concurrent agent executions in `run_parallel_subtasks`.
/// Wire up `--max-parallel` CLI flag to override this in a follow-up (see #638).
const MAX_PARALLEL: usize = 8;

/// Maximum number of sequential steps accepted from a numbered-list prompt.
///
/// Each step executes serially with the full `turn_timeout` (default 3600 s).
/// Without this cap a single numbered-list prompt with N steps would occupy a
/// worker for up to `N × turn_timeout` seconds — a practical queue-starvation
/// / DoS path. The limit is intentionally generous (20 × 3600 s = 20 h worst
/// case) but cuts off adversarially large inputs.
const MAX_SEQUENTIAL_STEPS: usize = 20;

const PARALLEL_EXTENSIONS: &[&str] = &[
    "rs", "ts", "tsx", "js", "jsx", "py", "go", "java", "kt", "swift", "cpp", "c", "h", "toml",
    "yaml", "yml", "json", "sh", "md",
];

/// Well-known filenames that have no extension but represent source files.
const EXTENSIONLESS_FILENAMES: &[&str] = &[
    "Dockerfile",
    "Makefile",
    "Jenkinsfile",
    "Vagrantfile",
    "Procfile",
    "Rakefile",
    "Gemfile",
    "Brewfile",
    ".gitignore",
    ".gitattributes",
    ".env",
    ".editorconfig",
];

pub(crate) fn extract_file_refs(prompt: &str) -> Vec<String> {
    let mut refs: Vec<String> = prompt
        .split_whitespace()
        .filter_map(|token| {
            let token = token.trim_matches(|c: char| {
                !c.is_alphanumeric() && c != '.' && c != '_' && c != '-' && c != '/'
            });
            // Normalize: strip leading "./" so "./src/auth.rs" == "src/auth.rs".
            let token = token.strip_prefix("./").unwrap_or(token);
            if token.is_empty() {
                return None;
            }
            // Accept tokens with a recognised file extension.
            let has_known_ext = token
                .rfind('.')
                .map(|dot_pos| {
                    let ext = &token[dot_pos + 1..];
                    PARALLEL_EXTENSIONS.contains(&ext)
                })
                .unwrap_or(false);
            if has_known_ext {
                return Some(token.to_string());
            }
            // Accept path-like tokens containing '/' regardless of extension
            // (e.g. "docker/Dockerfile"). Exclude URL-like strings.
            if token.contains('/') && !token.starts_with("http") {
                return Some(token.to_string());
            }
            // Accept bare well-known extensionless filenames (e.g. "Dockerfile").
            if EXTENSIONLESS_FILENAMES.contains(&token) {
                return Some(token.to_string());
            }
            None
        })
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    refs.sort();
    refs
}

/// A subtask produced by decomposing a complex prompt.
#[derive(Debug)]
pub struct SubtaskSpec {
    /// The full prompt for this subtask (including focus directive).
    pub prompt: String,
    /// Zero-based indices of subtasks that must complete before this one starts.
    /// Empty means this subtask can run immediately (parallel).
    pub depends_on_indices: Vec<usize>,
}

/// Returns true if the prompt uses numbered list ordering (sequential intent).
///
/// Detects patterns like "1. ...", "1) ..." at the start of lines.
fn is_numbered_list(prompt: &str) -> bool {
    prompt.lines().filter(|l| !l.trim().is_empty()).any(|l| {
        let trimmed = l.trim_start();
        trimmed.starts_with("1. ") || trimmed.starts_with("1) ")
    })
}

/// Decompose a complex prompt into subtask specs.
///
/// Numbered lists (e.g. "1. write X\n2. refactor Y") produce sequential specs
/// where each spec depends on the previous. Plain file-ref partitioning produces
/// parallel specs with no dependencies.
///
/// Returns a single-element vec when decomposition is not meaningful.
pub fn decompose(prompt: &str) -> Result<Vec<SubtaskSpec>, String> {
    // Numbered list → sequential subtasks.
    if is_numbered_list(prompt) {
        let items: Vec<&str> = prompt
            .lines()
            .filter(|l| {
                let t = l.trim_start();
                t.starts_with(|c: char| c.is_ascii_digit())
                    && (t.contains(". ") || t.contains(") "))
            })
            .collect();
        if items.len() >= 2 {
            // Reject over-limit lists explicitly to prevent silent partial execution.
            // Each step runs serially with the full turn_timeout (default 3600 s);
            // an unbounded list would occupy a worker for N × timeout seconds.
            if items.len() > MAX_SEQUENTIAL_STEPS {
                return Err(format!(
                    "Prompt contains {} sequential steps, which exceeds the {} step limit. \
                     Please split your request into smaller tasks.",
                    items.len(),
                    MAX_SEQUENTIAL_STEPS
                ));
            }
            let total = items.len();
            return Ok(items
                .iter()
                .enumerate()
                .map(|(i, item)| SubtaskSpec {
                    prompt: format!(
                        "{}\n\n[Sequential subtask {}/{}] {}",
                        prompt,
                        i + 1,
                        total,
                        item.trim()
                    ),
                    depends_on_indices: if i == 0 { vec![] } else { vec![i - 1] },
                })
                .collect());
        }
    }

    // File-ref partitioning → parallel subtasks.
    let files = extract_file_refs(prompt);
    if files.len() < 2 {
        return Ok(vec![SubtaskSpec {
            prompt: prompt.to_string(),
            depends_on_indices: vec![],
        }]);
    }
    // Scale chunk count linearly with file count, floor at 2, cap at MAX_PARALLEL.
    let n_chunks = (files.len() / 3).clamp(2, MAX_PARALLEL);
    // Partition into exactly n_chunks groups (array-split style) so that actual
    // parallelism is monotonically non-decreasing as file count grows.
    // Using div_ceil for chunk_size causes files.chunks() to produce fewer than
    // n_chunks groups (e.g. 25 files → 7 groups instead of 8).  Instead,
    // distribute files as evenly as possible: first `extra` groups get one extra
    // file, the rest get `base` files each.
    let base = files.len() / n_chunks;
    let extra = files.len() % n_chunks;
    let mut groups: Vec<Vec<String>> = Vec::with_capacity(n_chunks);
    let mut start = 0;
    for i in 0..n_chunks {
        let size = base + usize::from(i < extra);
        groups.push(files[start..start + size].to_vec());
        start += size;
    }
    let actual_count = n_chunks;
    Ok(groups
        .into_iter()
        .enumerate()
        .map(|(i, group)| SubtaskSpec {
            prompt: format!(
                "{}\n\n[Parallel subtask {}/{}] Focus on these files: {}",
                prompt,
                i + 1,
                actual_count,
                group.join(", ")
            ),
            depends_on_indices: vec![],
        })
        .collect())
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

/// Combined result returned by `run_parallel_subtasks`.
pub struct ParallelRunResult {
    /// Per-subtask outcomes (may be shorter than input when sequential execution
    /// aborted early after a step failure).
    pub results: Vec<SubtaskResult>,
    /// True when subtasks ran serially in dependency order (numbered-list mode).
    /// Callers must require *all* steps succeeded; `any_success` is not sufficient.
    pub is_sequential: bool,
}

/// Run multiple agent executions, either serially (sequential deps) or concurrently
/// (no deps), each in an isolated git worktree.
///
/// **Sequential mode** (any subtask has `depends_on_indices`): subtasks execute
/// one-at-a-time in order. The first failure aborts the remaining steps so that
/// later steps never run without their prerequisites.
///
/// **Parallel mode** (no deps): subtasks execute concurrently, bounded by
/// `MAX_PARALLEL`. Individual failures are captured and do not abort siblings.
///
/// Workspaces are removed after all executions finish.
pub async fn run_parallel_subtasks(
    task_id: &TaskId,
    agent: Arc<dyn CodeAgent>,
    subtasks: Vec<SubtaskSpec>,
    workspace_mgr: Arc<WorkspaceManager>,
    source_repo: &Path,
    remote: &str,
    base_branch: &str,
    context: Vec<ContextItem>,
    turn_timeout: Duration,
) -> ParallelRunResult {
    let is_sequential = subtasks.iter().any(|s| !s.depends_on_indices.is_empty());

    if is_sequential {
        return run_sequential_subtasks(
            task_id,
            agent,
            subtasks,
            workspace_mgr,
            source_repo,
            remote,
            base_branch,
            context,
            turn_timeout,
        )
        .await;
    }

    run_concurrent_subtasks(
        task_id,
        agent,
        subtasks,
        workspace_mgr,
        source_repo,
        remote,
        base_branch,
        context,
        turn_timeout,
    )
    .await
}

/// Execute subtasks one-at-a-time in order, stopping on the first failure.
///
/// All steps share a **single workspace** so that step N can observe the
/// filesystem outputs of step N-1 (written files, applied patches, etc.).
/// Creating a fresh workspace per step would give each step a clean clone of
/// `source_repo`/`base_branch`, making the dependency chain meaningless.
///
/// Each `agent.execute` call is spawned into its own `tokio::task` so that a
/// panic inside the agent surfaces as a `JoinError` rather than unwinding
/// through this function — which would bypass the workspace cleanup and leave
/// the task in an inconsistent in-progress state.
async fn run_sequential_subtasks(
    task_id: &TaskId,
    agent: Arc<dyn CodeAgent>,
    subtasks: Vec<SubtaskSpec>,
    workspace_mgr: Arc<WorkspaceManager>,
    source_repo: &Path,
    remote: &str,
    base_branch: &str,
    context: Vec<ContextItem>,
    turn_timeout: Duration,
) -> ParallelRunResult {
    let total = subtasks.len();
    let mut results = Vec::with_capacity(total);

    // One shared workspace for all sequential steps — step N sees step N-1 outputs.
    let seq_id = harness_core::types::TaskId(format!("{}-seq", task_id.0));
    let workspace = match workspace_mgr
        .create_workspace(&seq_id, source_repo, remote, base_branch)
        .await
    {
        Ok(ws) => ws,
        Err(e) => {
            tracing::warn!("parallel_dispatch: workspace creation failed for sequential run: {e}");
            return ParallelRunResult {
                results: vec![SubtaskResult {
                    index: 0,
                    response: None,
                    error: Some(format!("workspace creation failed: {e}")),
                }],
                is_sequential: true,
            };
        }
    };

    // Single token covers the full sequential run — all steps share one workspace.
    let seq_token = CapabilityToken::new(
        0,
        vec![workspace.clone()],
        turn_timeout + Duration::from_secs(60),
    );

    for (i, spec) in subtasks.into_iter().enumerate() {
        let req = AgentRequest {
            prompt: spec.prompt,
            project_root: workspace.clone(),
            context: context.clone(),
            capability_token: Some(seq_token.clone()),
            ..Default::default()
        };
        // Spawn into a task so a panic in agent.execute surfaces as JoinError
        // instead of unwinding through this function and skipping cleanup.
        //
        // The AbortOnDrop guard ensures the child task is aborted whenever
        // `handle` is dropped — including when the *parent* future is cancelled
        // by an external abort (task_runner cancel endpoint).  Without it,
        // dropping a JoinHandle merely detaches the child; the agent would keep
        // running and writing to the shared workspace after cancellation.
        let agent_clone = agent.clone();
        let mut handle = tokio::spawn(async move { agent_clone.execute(req).await });
        let _abort_guard = AbortOnDrop(handle.abort_handle());
        let outcome = match tokio::time::timeout(turn_timeout, &mut handle).await {
            Ok(Ok(Ok(resp))) => Ok(resp),
            Ok(Ok(Err(e))) => Err(format!("agent error: {e}")),
            Ok(Err(join_err)) => Err(format!("subtask panicked: {join_err}")),
            Err(_) => {
                // Abort and await the task so it is fully stopped before
                // workspace cleanup begins.  Tokio abort() is asynchronous —
                // the task can still run until its next yield point — so
                // awaiting guarantees no background mutations occur after
                // remove_workspace is called.
                handle.abort();
                if let Err(e) = handle.await {
                    if !e.is_cancelled() {
                        tracing::warn!(
                            "sequential subtask {i} did not exit cleanly after abort: {e}"
                        );
                    }
                }
                Err(format!(
                    "subtask timed out after {}s",
                    turn_timeout.as_secs()
                ))
            }
        };

        let (response, error) = match outcome {
            Ok(resp) => (Some(resp), None),
            Err(e) => {
                tracing::warn!(
                    "sequential subtask {i} failed: {e}; aborting remaining {} step(s)",
                    total - i - 1
                );
                (None, Some(e))
            }
        };
        let failed = response.is_none();
        results.push(SubtaskResult {
            index: i,
            response,
            error,
        });

        if failed {
            break;
        }
    }

    // Workspace is cleaned up once after all steps complete (or on early abort).
    if let Err(e) = workspace_mgr.remove_workspace(&seq_id).await {
        tracing::warn!("parallel_dispatch: workspace cleanup failed for {seq_id:?}: {e}");
    }

    ParallelRunResult {
        results,
        is_sequential: true,
    }
}

/// Execute subtasks concurrently, bounded by `MAX_PARALLEL`.
async fn run_concurrent_subtasks(
    task_id: &TaskId,
    agent: Arc<dyn CodeAgent>,
    subtasks: Vec<SubtaskSpec>,
    workspace_mgr: Arc<WorkspaceManager>,
    source_repo: &Path,
    remote: &str,
    base_branch: &str,
    context: Vec<ContextItem>,
    turn_timeout: Duration,
) -> ParallelRunResult {
    let count = subtasks.len();
    let mut handles: Vec<tokio::task::JoinHandle<(usize, Result<AgentResponse, String>)>> =
        Vec::with_capacity(count);
    let mut sub_ids: Vec<Option<TaskId>> = Vec::with_capacity(count);
    // RAII abort guards: when this Vec is dropped (including when the parent
    // future is cancelled via the cancel endpoint), every spawned task is
    // aborted.  Without these guards, dropping a JoinHandle merely detaches
    // the child; agent processes would keep running and mutating worktrees
    // even after the parent task is cancelled.
    let mut abort_guards: Vec<AbortOnDrop> = Vec::with_capacity(count);
    let sem = Arc::new(tokio::sync::Semaphore::new(MAX_PARALLEL));

    for (i, spec) in subtasks.into_iter().enumerate() {
        let sub_id = harness_core::types::TaskId(format!("{}-p{i}", task_id.0));
        match workspace_mgr
            .create_workspace(&sub_id, source_repo, remote, base_branch)
            .await
        {
            Ok(workspace) => {
                sub_ids.push(Some(sub_id));
                let agent = agent.clone();
                let context = context.clone();
                let token = CapabilityToken::new(
                    i,
                    vec![workspace.clone()],
                    turn_timeout + Duration::from_secs(60),
                );
                let req = AgentRequest {
                    prompt: spec.prompt,
                    project_root: workspace,
                    context,
                    capability_token: Some(token),
                    ..Default::default()
                };
                let sem = Arc::clone(&sem);
                let handle = tokio::spawn(async move {
                    // Acquire semaphore first (unbounded wait), then apply timeout
                    // only to the actual agent execution — subtasks beyond the first
                    // MAX_PARALLEL do not time out while waiting in the queue.
                    let _permit = match sem.acquire_owned().await {
                        Ok(p) => p,
                        Err(_) => return (i, Err("semaphore closed unexpectedly".to_string())),
                    };
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
                });
                abort_guards.push(AbortOnDrop(handle.abort_handle()));
                handles.push(handle);
            }
            Err(e) => {
                tracing::warn!("parallel_dispatch: workspace creation failed for subtask {i}: {e}");
                sub_ids.push(None);
                let handle =
                    tokio::spawn(
                        async move { (i, Err(format!("workspace creation failed: {e}"))) },
                    );
                abort_guards.push(AbortOnDrop(handle.abort_handle()));
                handles.push(handle);
            }
        }
    }

    let mut results = Vec::with_capacity(count);
    for (i, handle) in handles.into_iter().enumerate() {
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
                tracing::warn!("parallel subtask {i} join error: {join_err}");
                results.push(SubtaskResult {
                    index: i,
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

    // All tasks have completed — dropping abort_guards here is a no-op.
    // On parent-future cancellation they would have been dropped earlier,
    // aborting every task before this point is reached.
    drop(abort_guards);

    ParallelRunResult {
        results,
        is_sequential: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decompose_no_files_returns_original() {
        let prompt = "Fix the login bug";
        let subtasks = decompose(prompt).unwrap();
        assert_eq!(subtasks.len(), 1);
        assert_eq!(subtasks[0].prompt, prompt);
    }

    #[test]
    fn decompose_one_file_returns_original() {
        let prompt = "Fix the bug in src/main.rs";
        let subtasks = decompose(prompt).unwrap();
        assert_eq!(subtasks.len(), 1);
        assert_eq!(subtasks[0].prompt, prompt);
    }

    #[test]
    fn decompose_two_files_yields_two_subtasks() {
        let prompt = "Update src/auth.rs and src/db.rs";
        let subtasks = decompose(prompt).unwrap();
        assert_eq!(subtasks.len(), 2);
    }

    #[test]
    fn decompose_multiple_files_splits_into_two() {
        let prompt = "Refactor src/a.rs src/b.rs src/c.rs src/d.rs";
        let subtasks = decompose(prompt).unwrap();
        assert_eq!(subtasks.len(), 2);
        assert!(subtasks[0].prompt.contains("[Parallel subtask 1/2]"));
        assert!(subtasks[1].prompt.contains("[Parallel subtask 2/2]"));
    }

    #[test]
    fn decompose_subtasks_start_with_original_prompt() {
        let prompt = "Fix auth.rs and db.rs together";
        let subtasks = decompose(prompt).unwrap();
        for subtask in &subtasks {
            assert!(subtask.prompt.starts_with(prompt));
        }
    }

    #[test]
    fn decompose_six_files_yields_two_subtasks() {
        let prompt = "Refactor src/a.rs src/b.rs src/c.rs src/d.rs src/e.rs src/f.rs";
        let subtasks = decompose(prompt).unwrap();
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
    fn extract_file_refs_normalizes_dot_slash_prefix() {
        let files = extract_file_refs("update ./src/auth.rs and src/auth.rs");
        // Both should normalise to "src/auth.rs" and deduplicate.
        assert_eq!(files, vec!["src/auth.rs"]);
    }

    #[test]
    fn extract_file_refs_dot_slash_groups_with_plain_path() {
        let a = extract_file_refs("./src/auth.rs");
        let b = extract_file_refs("src/auth.rs");
        assert_eq!(a, b);
    }

    #[test]
    fn extract_file_refs_extensionless_in_path() {
        // "docker/Dockerfile" has no recognised extension but contains '/'.
        let files = extract_file_refs("update docker/Dockerfile");
        assert_eq!(files, vec!["docker/Dockerfile"]);
    }

    #[test]
    fn extract_file_refs_bare_dockerfile() {
        let files = extract_file_refs("update Dockerfile and src/main.rs");
        assert!(files.contains(&"Dockerfile".to_string()));
        assert!(files.contains(&"src/main.rs".to_string()));
    }

    #[test]
    fn extract_file_refs_excludes_urls() {
        let files = extract_file_refs("see https://example.com/path for details");
        assert!(files.is_empty());
    }

    #[test]
    fn decompose_subtask_contains_focus_directive() {
        let prompt = "Update auth.rs and db.rs";
        let subtasks = decompose(prompt).unwrap();
        assert_eq!(subtasks.len(), 2);
        assert!(subtasks[0].prompt.contains("Focus on these files:"));
        assert!(subtasks[1].prompt.contains("Focus on these files:"));
    }

    #[test]
    fn decompose_numbered_list_yields_sequential_specs() {
        let prompt = "1. Write the auth module\n2. Refactor the db layer";
        let subtasks = decompose(prompt).unwrap();
        assert_eq!(subtasks.len(), 2);
        assert!(subtasks[0].depends_on_indices.is_empty());
        assert_eq!(subtasks[1].depends_on_indices, vec![0]);
    }

    #[test]
    fn decompose_parallel_specs_have_no_dependencies() {
        let prompt = "Update src/auth.rs and src/db.rs";
        let subtasks = decompose(prompt).unwrap();
        assert_eq!(subtasks.len(), 2);
        assert!(subtasks[0].depends_on_indices.is_empty());
        assert!(subtasks[1].depends_on_indices.is_empty());
    }

    // --- dynamic chunk-count tests ---

    fn make_prompt(n: usize) -> String {
        (0..n)
            .map(|i| format!("src/file{i:02}.rs"))
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn test_decompose_small() {
        // 2 files: (2/3).clamp(2,8) = 2 — minimum clamp
        let subtasks = decompose(&make_prompt(2)).unwrap();
        assert_eq!(subtasks.len(), 2);
    }

    #[test]
    fn test_decompose_medium() {
        // 9 files: (9/3).clamp(2,8) = 3
        let subtasks = decompose(&make_prompt(9)).unwrap();
        assert_eq!(subtasks.len(), 3);
    }

    #[test]
    fn test_decompose_large() {
        // 24 files: (24/3).clamp(2,8) = 8
        let subtasks = decompose(&make_prompt(24)).unwrap();
        assert_eq!(subtasks.len(), 8);
    }

    #[test]
    fn test_decompose_very_large() {
        // 30 files: (30/3).clamp(2,8) = 8 — cap
        let subtasks = decompose(&make_prompt(30)).unwrap();
        assert_eq!(subtasks.len(), 8);
    }

    #[test]
    fn decompose_labels_reflect_actual_chunk_count() {
        // 16 files: n_chunks = (16/3).clamp(2,8) = 5; array-split gives exactly 5
        // groups, so labels must be X/5.
        let subtasks = decompose(&make_prompt(16)).unwrap();
        let actual = subtasks.len();
        assert_eq!(actual, 5, "expected 5 actual chunks for 16 files");
        for (i, s) in subtasks.iter().enumerate() {
            let expected = format!("[Parallel subtask {}/{}]", i + 1, actual);
            assert!(
                s.prompt.contains(&expected),
                "label mismatch: expected '{}' in prompt",
                expected
            );
        }
    }

    #[test]
    fn test_decompose_chunk_count_monotone() {
        // Chunk count must be monotonically non-decreasing as file count grows
        // within the [2, MAX_PARALLEL] band (24→25 was a known regression).
        let chunks_24 = decompose(&make_prompt(24)).unwrap().len();
        let chunks_25 = decompose(&make_prompt(25)).unwrap().len();
        assert!(
            chunks_25 >= chunks_24,
            "expected monotone: 25 files ({chunks_25} chunks) >= 24 files ({chunks_24} chunks)"
        );
    }

    #[test]
    fn decompose_numbered_list_preserves_all_steps_within_cap() {
        // Steps within the cap must ALL be preserved — dropping steps is a
        // correctness regression (e.g. a migration checklist must run every step).
        let n = MAX_PARALLEL + 2; // 10 < MAX_SEQUENTIAL_STEPS (20)
        let prompt: String = (1..=n)
            .map(|i| format!("{}. task {}", i, i))
            .collect::<Vec<_>>()
            .join("\n");
        let subtasks = decompose(&prompt).unwrap();
        assert_eq!(
            subtasks.len(),
            n,
            "expected all {} steps preserved, got {}",
            n,
            subtasks.len()
        );
    }

    #[test]
    fn decompose_numbered_list_rejects_over_limit() {
        // Inputs exceeding MAX_SEQUENTIAL_STEPS must be rejected with an explicit
        // error — silent truncation causes partial execution marked as Done.
        let n = MAX_SEQUENTIAL_STEPS + 5;
        let prompt: String = (1..=n)
            .map(|i| format!("{}. task {}", i, i))
            .collect::<Vec<_>>()
            .join("\n");
        let err = decompose(&prompt).unwrap_err();
        assert!(
            err.contains(&n.to_string()),
            "error should mention actual step count: {err}"
        );
        assert!(
            err.contains(&MAX_SEQUENTIAL_STEPS.to_string()),
            "error should mention the limit: {err}"
        );
    }

    #[test]
    fn test_decompose_covers_all_files() {
        // 12 files: every file appears exactly once across all chunks
        let n = 12;
        let prompt = make_prompt(n);
        let subtasks = decompose(&prompt).unwrap();
        let expected: Vec<String> = (0..n).map(|i| format!("src/file{i:02}.rs")).collect();
        let mut found: Vec<String> = subtasks
            .iter()
            .flat_map(|s| {
                s.prompt
                    .split_whitespace()
                    .filter(|t| t.ends_with(".rs") && t.starts_with("src/file"))
                    .map(|t| t.to_string())
                    .collect::<Vec<_>>()
            })
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        found.sort();
        let mut sorted_expected = expected.clone();
        sorted_expected.sort();
        assert_eq!(found, sorted_expected);
    }
}
