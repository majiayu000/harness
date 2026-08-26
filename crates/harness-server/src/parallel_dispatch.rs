use crate::workspace::{
    run_until_repository_lease_loss, WorkspaceExecutionGuard, WorkspaceLease, WorkspaceManager,
};
use crate::workspace_lease_store::RepositoryLeaseState;
use futures::FutureExt;
use harness_core::{
    agent::AgentRequest,
    agent::AgentResponse,
    agent::CodeAgent,
    capability::CapabilityToken,
    config::HarnessConfig,
    types::{ContextItem, TaskId},
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::time::Duration;

struct CancelSubtasksOnDrop {
    sender: Option<tokio::sync::watch::Sender<bool>>,
}

impl CancelSubtasksOnDrop {
    fn disarm(&mut self) {
        self.sender = None;
    }
}

impl Drop for CancelSubtasksOnDrop {
    fn drop(&mut self) {
        if let Some(sender) = self.sender.take() {
            sender.send_replace(true);
        }
    }
}

async fn await_agent_execution<F>(
    execution: F,
    turn_timeout: Duration,
    repository_lease_lost: Option<tokio::sync::watch::Receiver<RepositoryLeaseState>>,
) -> Result<AgentResponse, String>
where
    F: std::future::Future<Output = harness_core::error::Result<AgentResponse>>,
{
    let outcome = run_until_repository_lease_loss(
        repository_lease_lost,
        tokio::time::timeout(
            turn_timeout,
            std::panic::AssertUnwindSafe(execution).catch_unwind(),
        ),
    )
    .await;
    let Some(outcome) = outcome else {
        return Err("repository lease was lost during agent execution".to_string());
    };
    match outcome {
        Ok(Ok(Ok(response))) => Ok(response),
        Ok(Ok(Err(error))) => Err(format!("agent error: {error}")),
        Ok(Err(panic)) => Err(format!("subtask panicked: {}", panic_message(panic))),
        Err(_) => Err(format!(
            "subtask timed out after {}s",
            turn_timeout.as_secs()
        )),
    }
}

fn panic_message(panic: Box<dyn std::any::Any + Send + 'static>) -> String {
    panic
        .downcast_ref::<&'static str>()
        .copied()
        .map(str::to_string)
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic payload".to_string())
}

async fn await_spawned_agent_execution(
    mut handle: tokio::task::JoinHandle<harness_core::error::Result<AgentResponse>>,
    turn_timeout: Duration,
    repository_lease_lost: Option<tokio::sync::watch::Receiver<RepositoryLeaseState>>,
    mut dispatch_cancelled: tokio::sync::watch::Receiver<bool>,
    label: &str,
) -> Result<AgentResponse, String> {
    enum AwaitOutcome<T> {
        DispatchCancelled,
        Execution(Option<T>),
    }

    let outcome = tokio::select! {
        biased;
        () = wait_for_dispatch_cancellation(&mut dispatch_cancelled) => {
            AwaitOutcome::DispatchCancelled
        }
        outcome = run_until_repository_lease_loss(
            repository_lease_lost,
            tokio::time::timeout(turn_timeout, &mut handle),
        ) => AwaitOutcome::Execution(outcome),
    };
    match outcome {
        AwaitOutcome::DispatchCancelled => {
            abort_and_await_agent(handle, label, "dispatch cancellation").await;
            Err("parallel dispatch was cancelled during agent execution".to_string())
        }
        AwaitOutcome::Execution(None) => {
            abort_and_await_agent(handle, label, "repository lease loss").await;
            Err("repository lease was lost during agent execution".to_string())
        }
        AwaitOutcome::Execution(Some(Ok(Ok(Ok(response))))) => Ok(response),
        AwaitOutcome::Execution(Some(Ok(Ok(Err(error))))) => Err(format!("agent error: {error}")),
        AwaitOutcome::Execution(Some(Ok(Err(error)))) => Err(format!("subtask panicked: {error}")),
        AwaitOutcome::Execution(Some(Err(_))) => {
            abort_and_await_agent(handle, label, "timeout").await;
            Err(format!(
                "subtask timed out after {}s",
                turn_timeout.as_secs()
            ))
        }
    }
}

async fn wait_for_dispatch_cancellation(receiver: &mut tokio::sync::watch::Receiver<bool>) {
    loop {
        if *receiver.borrow() {
            return;
        }
        if receiver.changed().await.is_err() {
            return;
        }
    }
}

async fn abort_and_await_agent(
    handle: tokio::task::JoinHandle<harness_core::error::Result<AgentResponse>>,
    label: &str,
    reason: &str,
) {
    handle.abort();
    if let Err(error) = handle.await {
        if !error.is_cancelled() {
            tracing::warn!("{label} did not exit cleanly after {reason}: {error}");
        }
    }
}

async fn cleanup_parallel_workspace(
    workspace_mgr: &WorkspaceManager,
    task_id: &TaskId,
    lease: &WorkspaceLease,
    execution_guard: &WorkspaceExecutionGuard,
) -> anyhow::Result<()> {
    workspace_mgr.begin_workspace_finalization(
        task_id,
        &lease.acquisition_id,
        execution_guard.execution_id(),
    )?;
    let removal = run_until_repository_lease_loss(
        lease.repository_lease_lost.clone(),
        workspace_mgr.remove_workspace_acquisition(task_id, &lease.acquisition_id),
    )
    .await;
    let result = if let Some(result) = removal {
        result
    } else {
        workspace_mgr.mark_workspace_cleanup_required(task_id, &lease.acquisition_id);
        workspace_mgr
            .cleanup_required_workspace_for_retry(task_id, None, 0)
            .await
    };
    if result.is_ok() {
        execution_guard.complete();
    }
    result
}

/// Maximum number of parallel subtasks — caps both chunk count in `decompose`
/// and concurrent agent executions in `run_parallel_subtasks`.
/// Wire up `--max-parallel` CLI flag to override this in a follow-up (see #638).
pub(crate) const MAX_PARALLEL: usize = 8;

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

pub(crate) fn sequential_subtask_id(task_id: &TaskId) -> TaskId {
    TaskId::from_str(&format!("{}-seq", task_id.as_str()))
}

pub(crate) fn parallel_subtask_id(task_id: &TaskId, index: usize) -> TaskId {
    TaskId::from_str(&format!("{}-p{index}", task_id.as_str()))
}

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
    config: &HarnessConfig,
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
            config,
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
        config,
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
    config: &HarnessConfig,
) -> ParallelRunResult {
    let total = subtasks.len();
    let mut results = Vec::with_capacity(total);

    // One shared workspace for all sequential steps — step N sees step N-1 outputs.
    let seq_id = sequential_subtask_id(task_id);
    // Sub-tasks use synthetic IDs and intentionally keep UUID-based workspace keys.
    let workspace_lease = match workspace_mgr
        .create_workspace(&seq_id, source_repo, remote, base_branch, 1, None, None)
        .await
    {
        Ok(lease) => lease,
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
    let workspace = workspace_lease.workspace_path.clone();
    let execution_guard =
        match workspace_mgr.claim_workspace_execution(&seq_id, &workspace_lease.acquisition_id) {
            Ok(guard) => guard,
            Err(error) => {
                let cleanup_error = workspace_mgr
                    .remove_workspace_acquisition(&seq_id, &workspace_lease.acquisition_id)
                    .await
                    .err()
                    .map(|cleanup| format!("; cleanup also failed: {cleanup}"))
                    .unwrap_or_default();
                return ParallelRunResult {
                    results: vec![SubtaskResult {
                        index: 0,
                        response: None,
                        error: Some(format!(
                            "workspace execution claim failed: {error}{cleanup_error}"
                        )),
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
        let mut req = AgentRequest {
            prompt: spec.prompt,
            project_root: workspace.clone(),
            context: context.clone(),
            capability_token: Some(seq_token.clone()),
            ..Default::default()
        };
        req.apply_configured_policy(config);
        let agent_clone = agent.clone();
        let outcome = await_agent_execution(
            async move { agent_clone.execute(req).await },
            turn_timeout,
            workspace_lease.repository_lease_lost.clone(),
        )
        .await;

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
    if let Err(e) =
        cleanup_parallel_workspace(&workspace_mgr, &seq_id, &workspace_lease, &execution_guard)
            .await
    {
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
    config: &HarnessConfig,
) -> ParallelRunResult {
    let count = subtasks.len();
    let mut handles: Vec<tokio::task::JoinHandle<(usize, Result<AgentResponse, String>)>> =
        Vec::with_capacity(count);
    let (dispatch_cancelled, _) = tokio::sync::watch::channel(false);
    let mut cancellation_owner = CancelSubtasksOnDrop {
        sender: Some(dispatch_cancelled.clone()),
    };
    let sem = Arc::new(tokio::sync::Semaphore::new(MAX_PARALLEL));

    for (i, spec) in subtasks.into_iter().enumerate() {
        let sub_id = parallel_subtask_id(task_id, i);
        let agent = agent.clone();
        let context = context.clone();
        let workspace_mgr = workspace_mgr.clone();
        let source_repo = source_repo.to_path_buf();
        let remote = remote.to_string();
        let base_branch = base_branch.to_string();
        let sem = Arc::clone(&sem);
        let config = config.clone();
        let mut dispatch_cancelled = dispatch_cancelled.subscribe();
        let handle = tokio::spawn(async move {
            // Acquire semaphore first (unbounded wait), then apply timeout only to
            // the actual agent execution. Workspace acquisition is part of the
            // subtask lifecycle so earlier completions can release pool slots
            // before later subtasks acquire theirs.
            let _permit = match sem.acquire_owned().await {
                Ok(p) => p,
                Err(_) => return (i, Err("semaphore closed unexpectedly".to_string())),
            };
            // Sub-tasks use synthetic IDs and intentionally keep UUID-based workspace keys.
            let workspace_acquisition = workspace_mgr.create_workspace(
                &sub_id,
                &source_repo,
                &remote,
                &base_branch,
                1,
                None,
                None,
            );
            tokio::pin!(workspace_acquisition);
            let workspace_result = tokio::select! {
                biased;
                result = &mut workspace_acquisition => result,
                () = wait_for_dispatch_cancellation(&mut dispatch_cancelled) => {
                    return (
                        i,
                        Err("parallel dispatch was cancelled during workspace acquisition".to_string()),
                    );
                }
            };
            let workspace_lease = match workspace_result {
                Ok(lease) => lease,
                Err(e) => {
                    tracing::warn!(
                        "parallel_dispatch: workspace creation failed for subtask {i}: {e}"
                    );
                    return (i, Err(format!("workspace creation failed: {e}")));
                }
            };
            let workspace = workspace_lease.workspace_path.clone();
            let execution_guard = match workspace_mgr
                .claim_workspace_execution(&sub_id, &workspace_lease.acquisition_id)
            {
                Ok(guard) => guard,
                Err(error) => {
                    let cleanup_error = workspace_mgr
                        .remove_workspace_acquisition(&sub_id, &workspace_lease.acquisition_id)
                        .await
                        .err()
                        .map(|cleanup| format!("; cleanup also failed: {cleanup}"))
                        .unwrap_or_default();
                    return (
                        i,
                        Err(format!(
                            "workspace execution claim failed: {error}{cleanup_error}"
                        )),
                    );
                }
            };
            let token = CapabilityToken::new(
                i,
                token_write_paths(workspace.clone()),
                turn_timeout.saturating_add(Duration::from_secs(60)),
            );
            let mut req = AgentRequest {
                prompt: spec.prompt,
                project_root: workspace,
                context,
                capability_token: Some(token),
                ..Default::default()
            };
            req.apply_configured_policy(&config);
            if *dispatch_cancelled.borrow() {
                let cleanup_error = cleanup_parallel_workspace(
                    &workspace_mgr,
                    &sub_id,
                    &workspace_lease,
                    &execution_guard,
                )
                .await
                .err()
                .map(|cleanup| format!("; cleanup also failed: {cleanup}"))
                .unwrap_or_default();
                return (
                    i,
                    Err(format!(
                        "parallel dispatch was cancelled before agent start{cleanup_error}"
                    )),
                );
            }
            let agent_clone = agent.clone();
            let agent_handle = tokio::spawn(async move { agent_clone.execute(req).await });
            let result = await_spawned_agent_execution(
                agent_handle,
                turn_timeout,
                workspace_lease.repository_lease_lost.clone(),
                dispatch_cancelled,
                &format!("parallel subtask {i}"),
            )
            .await;
            if let Err(e) = cleanup_parallel_workspace(
                &workspace_mgr,
                &sub_id,
                &workspace_lease,
                &execution_guard,
            )
            .await
            {
                tracing::warn!("parallel_dispatch: workspace cleanup failed for {sub_id:?}: {e}");
            }
            (i, result)
        });
        handles.push(handle);
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
            Err(join_error) => {
                tracing::warn!("parallel subtask {i} join error: {join_error}");
                results.push(SubtaskResult {
                    index: i,
                    response: None,
                    error: Some(format!("subtask panicked: {join_error}")),
                });
            }
        }
    }
    cancellation_owner.disarm();

    ParallelRunResult {
        results,
        is_sequential: false,
    }
}

#[cfg(test)]
#[path = "parallel_dispatch_tests.rs"]
mod tests;
