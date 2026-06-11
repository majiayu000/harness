use crate::task_runner::TaskId;
use crate::workspace::WorkspaceManager;
use harness_core::{
    agent::AgentRequest, agent::AgentResponse, agent::CodeAgent, capability::CapabilityToken,
    types::ContextItem,
};
use std::path::{Path, PathBuf};
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

/// Build the `allowed_write_paths` list for a capability token.
///
/// The sandbox policy suppresses the blanket `/tmp` grant whenever token paths
/// are present (to prevent sibling-worktree escape via shared `/tmp`).  To
/// preserve temp-file access for Claude/Codex and child tools we include the
/// standard temp directories explicitly alongside the workspace path.
fn token_write_paths(workspace: PathBuf) -> Vec<PathBuf> {
    vec![
        workspace,
        PathBuf::from("/tmp"),
        PathBuf::from("/private/tmp"), // macOS: /tmp is a symlink to /private/tmp
        PathBuf::from("/var/tmp"),
    ]
}

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
    // Sub-tasks use synthetic IDs and intentionally keep UUID-based workspace keys.
    let workspace = match workspace_mgr
        .create_workspace(&seq_id, source_repo, remote, base_branch, 1, None, None)
        .await
    {
        Ok(lease) => lease.workspace_path,
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
    // TTL must span every step: each step can run for up to `turn_timeout`, so
    // a single-step TTL would expire partway through a multi-step chain.
    // Use saturating arithmetic to avoid panic on absurdly large turn_timeout values.
    let seq_token = CapabilityToken::new(
        0,
        token_write_paths(workspace.clone()),
        turn_timeout
            .saturating_mul(total as u32)
            .saturating_add(Duration::from_secs(60)),
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
            Ok(resp) if resp.output.trim().is_empty() => {
                let e = "agent returned empty output".to_string();
                tracing::warn!(
                    "sequential subtask {i} failed: {e}; aborting remaining {} step(s)",
                    total - i - 1
                );
                (None, Some(e))
            }
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
        // Sub-tasks use synthetic IDs and intentionally keep UUID-based workspace keys.
        match workspace_mgr
            .create_workspace(&sub_id, source_repo, remote, base_branch, 1, None, None)
            .await
        {
            Ok(lease) => {
                let workspace = lease.workspace_path;
                sub_ids.push(Some(sub_id));
                let agent = agent.clone();
                let context = context.clone();
                let token = CapabilityToken::new(
                    i,
                    token_write_paths(workspace.clone()),
                    turn_timeout.saturating_add(Duration::from_secs(60)),
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
#[path = "parallel_dispatch_tests.rs"]
mod tests;
