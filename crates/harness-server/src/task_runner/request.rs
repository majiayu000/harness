use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use super::types::TaskId;

/// Maximum allowed scheduling priority. Values above this are rejected at the
/// API boundary to prevent scheduler-level starvation of normal-priority tasks.
/// `0` = normal (default), `1` = high, `2` = critical.
pub const MAX_TASK_PRIORITY: u8 = 2;

#[derive(Debug, Clone, Deserialize)]
pub struct CreateTaskRequest {
    /// Free-text task description (prompt, issue URL, etc.).
    pub prompt: Option<String>,
    /// GitHub issue number to implement from.
    pub issue: Option<u64>,
    /// When true, issue-backed tasks bypass the triage/plan pipeline and go
    /// straight to implementation using the legacy direct-implement path.
    #[serde(default)]
    pub skip_triage: bool,
    /// GitHub PR number to review/fix.
    pub pr: Option<u64>,
    /// Explicit agent name; if omitted, uses the default agent.
    pub agent: Option<String>,
    /// Project root; defaults to the main git worktree resolved at task spawn time.
    pub project: Option<PathBuf>,
    #[serde(default = "default_wait")]
    pub wait_secs: u64,
    /// Maximum review rounds. When absent, triage complexity provides the default
    /// (Low=2, Medium=system default, High=8). When explicitly set by the caller,
    /// this value always wins over triage-derived defaults.
    #[serde(default)]
    pub max_rounds: Option<u32>,
    /// Maximum total agent API calls across all phases (implementation + validation retries +
    /// review rounds). When None, falls back to the global `concurrency.max_turns` config value.
    #[serde(default)]
    pub max_turns: Option<u32>,
    /// Per-turn timeout in seconds; defaults to 3600 (1 hour).
    #[serde(default = "default_turn_timeout")]
    pub turn_timeout_secs: u64,
    /// Maximum spend budget for the agent in USD; None means unlimited.
    #[serde(default)]
    pub max_budget_usd: Option<f64>,
    /// Base delay in milliseconds for the first validation retry; subsequent retries double the
    /// delay up to `retry_max_backoff_ms`. Default: 10 000 ms (10 s).
    #[serde(default = "default_retry_base_backoff_ms")]
    pub retry_base_backoff_ms: u64,
    /// Maximum backoff cap in milliseconds for validation retries. Default: 300 000 ms (5 min).
    #[serde(default = "default_retry_max_backoff_ms")]
    pub retry_max_backoff_ms: u64,
    /// Seconds of silence from the agent stream before declaring a stall; defaults to 300.
    /// Overrides the global `concurrency.stall_timeout_secs` for this task.
    #[serde(default = "default_stall_timeout")]
    pub stall_timeout_secs: u64,
    /// Intake source name (e.g. "github", "feishu", "periodic_review"). None for manual tasks.
    #[serde(default)]
    pub source: Option<String>,
    /// Source-specific identifier (e.g. GitHub issue number). Stored in TaskState for traceability.
    #[serde(default)]
    pub external_id: Option<String>,
    /// Repository slug (e.g. "owner/repo"). Stored in TaskState for traceability.
    #[serde(default)]
    pub repo: Option<String>,
    /// Explicit parent task ID.
    #[serde(default)]
    pub parent_task_id: Option<TaskId>,
    /// Task IDs that must complete (Done) before this task starts.
    #[serde(default)]
    pub depends_on: Vec<TaskId>,
    /// Scheduling priority: 0 = normal (default), 1 = high, 2 = critical.
    /// Higher values are served first when multiple tasks are waiting for a slot.
    #[serde(default)]
    pub priority: u8,
}

/// Execution limits that survive a server restart.
///
/// Serialised as a JSON blob in `tasks.request_settings`. Restored at startup
/// redispatch so recovered tasks honour the original budget / timeout
/// guardrails instead of silently falling back to server-wide defaults.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PersistedRequestSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_rounds: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u32>,
    #[serde(default)]
    pub skip_triage: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_budget_usd: Option<f64>,
    pub wait_secs: u64,
    pub retry_base_backoff_ms: u64,
    pub retry_max_backoff_ms: u64,
    pub stall_timeout_secs: u64,
    pub turn_timeout_secs: u64,
    /// Additional caller-supplied prompt context for issue tasks.
    ///
    /// Callers may submit `{ issue: N, prompt: "extra context" }` to augment
    /// issue resolution.  The prompt is not stored in `description` (privacy),
    /// so we persist it here for issue tasks only to survive a server restart.
    /// Pure prompt tasks and PR tasks leave this `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub additional_prompt: Option<String>,
    /// Primary prompt for prompt-only tasks (no issue or pr).
    ///
    /// Kept in memory so that `AwaitingDeps` tasks can reconstruct the original
    /// request when their dependencies resolve within the same server session.
    /// Intentionally NOT serialised to the database: prompts may contain
    /// credentials or sensitive data and must not be written at rest.
    /// LIMITATION: after a server restart the prompt will be absent; prompt-only
    /// `AwaitingDeps` tasks will transition to `Pending` once their dependencies
    /// resolve, then fail immediately in the dep-watcher dispatch path because
    /// no prompt is available to replay.
    #[serde(skip)]
    pub prompt: Option<String>,
}

impl PersistedRequestSettings {
    pub(crate) fn from_req(req: &CreateTaskRequest) -> Self {
        Self {
            agent: req.agent.clone(),
            max_rounds: req.max_rounds,
            max_turns: req.max_turns,
            skip_triage: req.skip_triage,
            max_budget_usd: req.max_budget_usd,
            wait_secs: req.wait_secs,
            retry_base_backoff_ms: req.retry_base_backoff_ms,
            retry_max_backoff_ms: req.retry_max_backoff_ms,
            stall_timeout_secs: req.stall_timeout_secs,
            turn_timeout_secs: req.turn_timeout_secs,
            // Only persist the caller's prompt for issue tasks — it serves as
            // additional context that is safe to store (unlike pure prompt
            // tasks which may contain credentials).  PR and prompt-only tasks
            // leave this None.
            additional_prompt: if req.issue.is_some() {
                req.prompt.clone()
            } else {
                None
            },
            // For prompt-only tasks (no issue/pr), store the prompt so that
            // AwaitingDeps tasks can reconstruct the original request when deps resolve.
            prompt: if req.issue.is_none() && req.pr.is_none() {
                req.prompt.clone()
            } else {
                None
            },
        }
    }

    pub(crate) fn apply_to_req(&self, req: &mut CreateTaskRequest) {
        req.agent = self.agent.clone();
        req.max_rounds = self.max_rounds;
        req.max_turns = self.max_turns;
        req.skip_triage = self.skip_triage;
        req.max_budget_usd = self.max_budget_usd;
        req.wait_secs = self.wait_secs;
        req.retry_base_backoff_ms = self.retry_base_backoff_ms;
        req.retry_max_backoff_ms = self.retry_max_backoff_ms;
        req.stall_timeout_secs = self.stall_timeout_secs;
        req.turn_timeout_secs = self.turn_timeout_secs;
        // Restore the additional prompt context for recovered issue tasks.
        if self.additional_prompt.is_some() {
            req.prompt = self.additional_prompt.clone();
        }
        // Restore the primary prompt for recovered prompt-only tasks.
        if self.prompt.is_some() {
            req.prompt = self.prompt.clone();
        }
    }
}

impl Default for CreateTaskRequest {
    fn default() -> Self {
        Self {
            prompt: None,
            issue: None,
            skip_triage: false,
            pr: None,
            agent: None,
            project: None,
            wait_secs: default_wait(),
            max_rounds: None,
            max_turns: None,
            turn_timeout_secs: default_turn_timeout(),
            max_budget_usd: None,
            retry_base_backoff_ms: default_retry_base_backoff_ms(),
            retry_max_backoff_ms: default_retry_max_backoff_ms(),
            stall_timeout_secs: default_stall_timeout(),
            source: None,
            external_id: None,
            repo: None,
            parent_task_id: None,
            depends_on: Vec::new(),
            priority: 0,
        }
    }
}

pub(super) fn summarize_request_description(req: &CreateTaskRequest) -> Option<String> {
    // Only persist structured safe labels — never raw prompt text, which may contain
    // credentials or customer data.
    if let Some(n) = req.issue {
        return Some(format!("issue #{n}"));
    }
    if let Some(n) = req.pr {
        return Some(format!("PR #{n}"));
    }
    // Prompt-only tasks: store a generic label so that:
    //   (a) sibling-awareness can include them (prevents parallel agents stomping the same files),
    //   (b) operators can identify crashed tasks in the DB/dashboard after a restart.
    // The prompt itself is deliberately not stored.
    if req.prompt.is_some() {
        return Some("prompt task".to_string());
    }
    None
}

pub async fn fill_missing_repo_from_project(req: &mut CreateTaskRequest) {
    if req.repo.is_some() {
        return;
    }
    let Some(project) = req.project.as_deref() else {
        return;
    };
    req.repo = crate::task_executor::pr_detection::detect_repo_slug(project).await;
}

/// Detect the main git worktree root using a blocking subprocess call.
/// Must be called via `tokio::task::spawn_blocking` in async contexts.
pub(super) fn detect_main_worktree() -> PathBuf {
    std::process::Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| {
            s.lines()
                .next()
                .and_then(|line| line.strip_prefix("worktree "))
                .map(|p| PathBuf::from(p.trim()))
        })
        .unwrap_or_else(|| {
            tracing::warn!(
                "detect_main_worktree: could not detect git worktree root, falling back to '.'"
            );
            PathBuf::from(".")
        })
}

pub(super) fn default_wait() -> u64 {
    120
}

pub(super) fn default_retry_base_backoff_ms() -> u64 {
    10_000
}

pub(super) fn default_retry_max_backoff_ms() -> u64 {
    300_000
}

pub(super) fn default_turn_timeout() -> u64 {
    // 1 hour: parallel subtasks and complex agent turns on large codebases
    // regularly exceed the previous 10-minute default when running CI checks,
    // building dependencies from source, or iterating on review feedback.
    3600
}

pub(super) fn default_stall_timeout() -> u64 {
    300
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_task_request_deserializes_skip_triage() {
        let req: CreateTaskRequest =
            serde_json::from_str(r#"{"issue": 749, "skip_triage": true}"#).expect("deserialize");
        assert_eq!(req.issue, Some(749));
        assert!(req.skip_triage);
    }

    #[test]
    fn create_task_request_default_skip_triage_is_false() {
        let req = CreateTaskRequest::default();
        assert!(!req.skip_triage);
    }

    #[test]
    fn persisted_request_settings_roundtrip_preserves_skip_triage() {
        let req = CreateTaskRequest {
            issue: Some(42),
            skip_triage: true,
            ..CreateTaskRequest::default()
        };
        let settings = PersistedRequestSettings::from_req(&req);
        assert!(settings.skip_triage);

        let mut restored = CreateTaskRequest::default();
        settings.apply_to_req(&mut restored);
        assert!(restored.skip_triage);
    }
}
