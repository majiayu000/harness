use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use super::types::{TaskId, TaskKind};

/// Maximum allowed scheduling priority. Values above this are rejected at the
/// API boundary to prevent scheduler-level starvation of normal-priority tasks.
/// `0` = normal (default), `1` = high, `2` = critical.
pub const MAX_TASK_PRIORITY: u8 = 2;

const SYSTEM_PROMPT_RESTART_BUNDLE_VERSION: u8 = 1;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PromptTaskOrigin {
    Manual,
    SystemGenerated,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PeriodicReviewPromptInputs {
    pub project_root: String,
    pub since_arg: String,
    pub review_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guard_scan_output: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SprintPlannerIssueSummaryInput {
    pub external_id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SprintPlannerPromptInputs {
    pub issues: Vec<SprintPlannerIssueSummaryInput>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SystemPromptRestartBundle {
    pub version: u8,
    pub kind: String,
    pub data: serde_json::Value,
}

impl SystemPromptRestartBundle {
    pub fn periodic_review(inputs: PeriodicReviewPromptInputs) -> Self {
        Self {
            version: SYSTEM_PROMPT_RESTART_BUNDLE_VERSION,
            kind: "periodic_review".to_string(),
            data: serde_json::json!({
                "project_root": inputs.project_root,
                "since_arg": inputs.since_arg,
                "review_type": inputs.review_type,
                "guard_scan_output": inputs.guard_scan_output,
            }),
        }
    }

    pub fn sprint_planner(inputs: SprintPlannerPromptInputs) -> Self {
        Self {
            version: SYSTEM_PROMPT_RESTART_BUNDLE_VERSION,
            kind: "sprint_planner".to_string(),
            data: serde_json::json!({ "issues": inputs.issues }),
        }
    }

    pub(crate) fn rebuild_prompt(&self) -> Result<String, String> {
        match (self.version, self.kind.as_str()) {
            (SYSTEM_PROMPT_RESTART_BUNDLE_VERSION, "periodic_review") => {
                let inputs: PeriodicReviewPromptInputs =
                    serde_json::from_value(self.data.clone()).map_err(|err| {
                        format!("invalid periodic_review restart bundle payload: {err}")
                    })?;
                Ok(harness_core::prompts::periodic_review_prompt_with_guard_scan(
                    &inputs.project_root,
                    &inputs.since_arg,
                    &inputs.review_type,
                    inputs.guard_scan_output.as_deref(),
                ))
            }
            (SYSTEM_PROMPT_RESTART_BUNDLE_VERSION, "sprint_planner") => {
                let inputs: SprintPlannerPromptInputs =
                    serde_json::from_value(self.data.clone()).map_err(|err| {
                        format!("invalid sprint_planner restart bundle payload: {err}")
                    })?;
                Ok(harness_core::prompts::sprint_plan_prompt(
                    &build_sprint_issue_summary(&inputs),
                ))
            }
            (version, kind) => Err(format!(
                "unsupported system prompt restart bundle version/kind: version={version}, kind={kind}"
            )),
        }
    }
}

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
    /// When true, agent-raised plan concerns must not block implementation.
    /// The concern is recorded, but the issue contract remains authoritative.
    #[serde(default)]
    pub force_execute: bool,
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
    /// Snapshot of source labels for workflow policy decisions.
    #[serde(default)]
    pub labels: Vec<String>,
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
    /// Structured restart-safe inputs for system-generated prompt-only tasks.
    ///
    /// Internal-only: arbitrary API callers must not inject raw prompt recovery
    /// metadata. Scheduler-created tasks attach a bundle explicitly in Rust code.
    #[serde(skip)]
    pub system_prompt_restart_bundle: Option<SystemPromptRestartBundle>,
}

impl CreateTaskRequest {
    pub fn task_kind(&self) -> TaskKind {
        TaskKind::classify(None, self.issue, self.pr)
    }
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
    #[serde(default)]
    pub force_execute: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
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
    /// Typed classification for prompt-only tasks so privacy and restart logic
    /// no longer depend on the `"prompt task"` description placeholder.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_task_origin: Option<PromptTaskOrigin>,
    /// Restart-safe structured inputs for system-generated prompt-only tasks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt_restart_bundle: Option<SystemPromptRestartBundle>,
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
        let prompt_task_origin = if req.issue.is_none() && req.pr.is_none() && req.prompt.is_some()
        {
            Some(if req.system_prompt_restart_bundle.is_some() {
                PromptTaskOrigin::SystemGenerated
            } else {
                PromptTaskOrigin::Manual
            })
        } else {
            None
        };

        Self {
            agent: req.agent.clone(),
            max_rounds: req.max_rounds,
            max_turns: req.max_turns,
            skip_triage: req.skip_triage,
            force_execute: req.force_execute,
            labels: req.labels.clone(),
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
            prompt_task_origin,
            system_prompt_restart_bundle: req.system_prompt_restart_bundle.clone(),
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
        req.force_execute = self.force_execute;
        req.labels = self.labels.clone();
        req.max_budget_usd = self.max_budget_usd;
        req.wait_secs = self.wait_secs;
        req.retry_base_backoff_ms = self.retry_base_backoff_ms;
        req.retry_max_backoff_ms = self.retry_max_backoff_ms;
        req.stall_timeout_secs = self.stall_timeout_secs;
        req.turn_timeout_secs = self.turn_timeout_secs;
        req.system_prompt_restart_bundle = self.system_prompt_restart_bundle.clone();
        // Restore the additional prompt context for recovered issue tasks.
        if self.additional_prompt.is_some() {
            req.prompt = self.additional_prompt.clone();
        }
        // Restore the primary prompt for recovered prompt-only tasks.
        if self.prompt.is_some() {
            req.prompt = self.prompt.clone();
        }
    }

    pub(crate) fn is_manual_prompt_only(&self) -> bool {
        self.prompt_task_origin == Some(PromptTaskOrigin::Manual)
    }

    pub(crate) fn is_system_generated_prompt_only(&self) -> bool {
        self.prompt_task_origin == Some(PromptTaskOrigin::SystemGenerated)
    }

    pub(crate) fn rebuild_system_prompt(&self) -> Result<Option<String>, String> {
        if !self.is_system_generated_prompt_only() {
            return Ok(None);
        }
        let bundle = self
            .system_prompt_restart_bundle
            .as_ref()
            .ok_or_else(|| "system-generated prompt task is missing restart bundle".to_string())?;
        bundle.rebuild_prompt().map(Some)
    }
}

impl Default for CreateTaskRequest {
    fn default() -> Self {
        Self {
            prompt: None,
            issue: None,
            skip_triage: false,
            force_execute: false,
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
            labels: Vec::new(),
            parent_task_id: None,
            depends_on: Vec::new(),
            priority: 0,
            system_prompt_restart_bundle: None,
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
    if req.prompt.is_some() || req.system_prompt_restart_bundle.is_some() {
        return Some("prompt task".to_string());
    }
    None
}

fn build_sprint_issue_summary(inputs: &SprintPlannerPromptInputs) -> String {
    inputs
        .issues
        .iter()
        .map(|issue| {
            let labels = if issue.labels.is_empty() {
                String::new()
            } else {
                format!(" [{}]", issue.labels.join(", "))
            };
            let repo = issue.repo.as_deref().unwrap_or("");
            format!(
                "- #{}: {}{} ({})",
                issue.external_id, issue.title, labels, repo
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
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
        assert!(!req.force_execute);
    }

    #[test]
    fn persisted_request_settings_roundtrip_preserves_skip_triage() {
        let req = CreateTaskRequest {
            issue: Some(42),
            skip_triage: true,
            force_execute: true,
            ..CreateTaskRequest::default()
        };
        let settings = PersistedRequestSettings::from_req(&req);
        assert!(settings.skip_triage);
        assert!(settings.force_execute);

        let mut restored = CreateTaskRequest::default();
        settings.apply_to_req(&mut restored);
        assert!(restored.skip_triage);
        assert!(restored.force_execute);
    }

    #[test]
    fn request_settings_roundtrip_preserves_system_prompt_bundle() {
        let settings = PersistedRequestSettings {
            wait_secs: 120,
            retry_base_backoff_ms: 10_000,
            retry_max_backoff_ms: 300_000,
            stall_timeout_secs: 300,
            turn_timeout_secs: 3_600,
            prompt_task_origin: Some(PromptTaskOrigin::SystemGenerated),
            system_prompt_restart_bundle: Some(SystemPromptRestartBundle::periodic_review(
                PeriodicReviewPromptInputs {
                    project_root: "/tmp/project".to_string(),
                    since_arg: "2026-04-20T00:00:00Z".to_string(),
                    review_type: "mixed".to_string(),
                    guard_scan_output: Some("guard output".to_string()),
                },
            )),
            ..Default::default()
        };

        let json = serde_json::to_string(&settings).expect("settings serialize");
        let decoded: PersistedRequestSettings =
            serde_json::from_str(&json).expect("settings deserialize");

        assert_eq!(decoded.prompt_task_origin, settings.prompt_task_origin);
        assert_eq!(
            decoded.system_prompt_restart_bundle,
            settings.system_prompt_restart_bundle
        );
    }

    #[test]
    fn rebuild_system_prompt_fails_closed_on_unsupported_bundle_version() {
        let settings = PersistedRequestSettings {
            wait_secs: 120,
            retry_base_backoff_ms: 10_000,
            retry_max_backoff_ms: 300_000,
            stall_timeout_secs: 300,
            turn_timeout_secs: 3_600,
            prompt_task_origin: Some(PromptTaskOrigin::SystemGenerated),
            system_prompt_restart_bundle: Some(SystemPromptRestartBundle {
                version: 99,
                kind: "periodic_review".to_string(),
                data: serde_json::json!({
                    "project_root": "/tmp/project",
                    "since_arg": "2026-04-20T00:00:00Z",
                    "review_type": "mixed"
                }),
            }),
            ..Default::default()
        };

        let err = settings
            .rebuild_system_prompt()
            .expect_err("unsupported bundle must fail");
        assert!(err.contains("unsupported system prompt restart bundle"));
    }
}
