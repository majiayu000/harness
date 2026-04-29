use super::helpers::{
    augment_prompt_with_skills, build_task_event, run_agent_streaming,
    run_agent_streaming_with_options, telemetry_for_timeout, update_status,
    RunAgentStreamingOptions,
};
use crate::task_runner::{
    mutate_and_persist, CreateTaskRequest, RoundResult, TaskId, TaskPhase, TaskStatus, TaskStore,
};
use chrono::Utc;
use harness_core::agent::{AgentRequest, CodeAgent};
use harness_core::config::project::ProjectTriageConfig;
use harness_core::prompts;
use harness_core::types::{Decision, ExecutionPhase, TurnFailure, TurnFailureKind, TurnTelemetry};
use harness_observe::event_store::EventStore;
use reqwest::header::{ACCEPT, USER_AGENT};
use serde::Deserialize;
use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::time::Duration;
use tokio::sync::RwLock;

const GITHUB_ISSUE_LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);
const UNKNOWN_REPO_SLUG: &str = "{owner}/{repo}";

type IssueFetchFuture<'a> =
    Pin<Box<dyn Future<Output = anyhow::Result<TypedIssueSnapshot>> + Send + 'a>>;
type IssueFetchFn = for<'a> fn(&'a str, u64, Option<&'a str>) -> IssueFetchFuture<'a>;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct TypedIssueSnapshot {
    body: String,
    labels: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct GitHubIssueSnapshotResponse {
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    labels: Vec<GitHubIssueLabel>,
}

#[derive(Debug, Deserialize)]
struct GitHubIssueLabel {
    name: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ActionabilitySignals {
    file_line_reference: bool,
    recommended_section: bool,
    acceptance_criteria: bool,
    code_block_or_step_list: bool,
}

impl ActionabilitySignals {
    fn count(self) -> usize {
        [
            self.file_line_reference,
            self.recommended_section,
            self.acceptance_criteria,
            self.code_block_or_step_list,
        ]
        .into_iter()
        .filter(|present| *present)
        .count()
    }

    fn is_actionable(self) -> bool {
        self.file_line_reference
            && self.code_block_or_step_list
            && (self.recommended_section || self.acceptance_criteria)
            && self.count() >= 3
    }

    fn names(self) -> Vec<&'static str> {
        let mut names = Vec::new();
        if self.file_line_reference {
            names.push("file_line");
        }
        if self.recommended_section {
            names.push("recommended_section");
        }
        if self.acceptance_criteria {
            names.push("acceptance_criteria");
        }
        if self.code_block_or_step_list {
            names.push("code_or_steps");
        }
        names
    }

    fn promotion_reason(self) -> String {
        format!("actionable_issue_markers:{}", self.names().join(","))
    }

    fn non_actionable_reason(self) -> String {
        let names = self.names();
        if names.is_empty() {
            "non_actionable_issue_markers:none".to_string()
        } else {
            format!("non_actionable_issue_markers:{}", names.join(","))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedTriageDecision {
    decision: prompts::TriageDecision,
    reason: String,
}

fn github_api_base_url() -> String {
    std::env::var("HARNESS_GITHUB_API_BASE_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "https://api.github.com".to_string())
        .trim_end_matches('/')
        .to_string()
}

fn issue_has_review_label(labels: &[String]) -> bool {
    labels
        .iter()
        .any(|label| label.trim().eq_ignore_ascii_case("review"))
}

fn effective_skip_on_review_label(config: Option<&ProjectTriageConfig>) -> bool {
    config
        .and_then(|triage| triage.skip_on_review_label)
        .unwrap_or(false)
}

fn trim_token(candidate: &str) -> &str {
    candidate.trim_matches(|ch: char| {
        matches!(
            ch,
            '`' | '"' | '\'' | ',' | '.' | ';' | ':' | '(' | ')' | '[' | ']' | '{' | '}'
        )
    })
}

fn contains_file_line_reference(body: &str) -> bool {
    body.split_whitespace().any(|token| {
        let candidate = trim_token(token);
        let Some((path, line)) = candidate.rsplit_once(':') else {
            return false;
        };
        !path.is_empty()
            && !line.is_empty()
            && line.chars().all(|ch| ch.is_ascii_digit())
            && (path.contains('/') || path.contains('\\') || path.contains('.'))
            && path.chars().any(|ch| ch.is_ascii_alphabetic())
    })
}

fn has_section(body: &str, marker: &str) -> bool {
    body.lines()
        .map(|line| line.trim().to_ascii_lowercase())
        .any(|line| line.starts_with(marker) || line == marker)
}

fn is_numbered_step(line: &str) -> bool {
    let trimmed = line.trim_start();
    let digit_count = trimmed.chars().take_while(|ch| ch.is_ascii_digit()).count();
    if digit_count == 0 {
        return false;
    }
    matches!(trimmed.chars().nth(digit_count), Some('.') | Some(')'))
        && trimmed[digit_count + 1..].starts_with(' ')
}

fn has_step_list(body: &str) -> bool {
    body.lines()
        .filter(|line| is_numbered_step(line))
        .take(2)
        .count()
        >= 2
}

fn detect_actionability_signals(body: &str) -> ActionabilitySignals {
    let lower = body.to_ascii_lowercase();
    ActionabilitySignals {
        file_line_reference: contains_file_line_reference(body),
        recommended_section: has_section(&lower, "## recommended fix")
            || has_section(&lower, "## recommended action")
            || has_section(&lower, "recommended fix:")
            || has_section(&lower, "recommended action:"),
        acceptance_criteria: has_section(&lower, "## acceptance criteria")
            || has_section(&lower, "acceptance criteria:"),
        code_block_or_step_list: body.contains("```") || has_step_list(body),
    }
}

fn default_triage_reason(decision: &prompts::TriageDecision) -> &'static str {
    match decision {
        prompts::TriageDecision::Proceed => "agent_proceed",
        prompts::TriageDecision::ProceedWithPlan => "agent_proceed_with_plan",
        prompts::TriageDecision::NeedsClarification => "agent_needs_clarification",
        prompts::TriageDecision::Skip => "agent_skip",
    }
}

fn resolve_triage_decision(
    agent_decision: prompts::TriageDecision,
    agent_reason: Option<&str>,
    issue_snapshot: Option<&TypedIssueSnapshot>,
    triage_config: Option<&ProjectTriageConfig>,
) -> ResolvedTriageDecision {
    let fallback_reason = agent_reason
        .filter(|reason| !reason.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| default_triage_reason(&agent_decision).to_string());

    if agent_decision != prompts::TriageDecision::Skip {
        return ResolvedTriageDecision {
            decision: agent_decision,
            reason: fallback_reason,
        };
    }

    let Some(issue_snapshot) = issue_snapshot else {
        return ResolvedTriageDecision {
            decision: prompts::TriageDecision::Skip,
            reason: fallback_reason,
        };
    };

    let signals = detect_actionability_signals(&issue_snapshot.body);
    if signals.is_actionable() {
        return ResolvedTriageDecision {
            decision: prompts::TriageDecision::ProceedWithPlan,
            reason: signals.promotion_reason(),
        };
    }

    if issue_has_review_label(&issue_snapshot.labels)
        && effective_skip_on_review_label(triage_config)
    {
        return ResolvedTriageDecision {
            decision: prompts::TriageDecision::Skip,
            reason: "review_label_skip_allowed_non_actionable".to_string(),
        };
    }

    ResolvedTriageDecision {
        decision: prompts::TriageDecision::Skip,
        reason: agent_reason
            .filter(|reason| !reason.trim().is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| signals.non_actionable_reason()),
    }
}

fn final_triage_event_result(decision: &prompts::TriageDecision) -> &'static str {
    match decision {
        prompts::TriageDecision::Skip => "skip",
        prompts::TriageDecision::Proceed
        | prompts::TriageDecision::ProceedWithPlan
        | prompts::TriageDecision::NeedsClarification => "promote",
    }
}

fn final_triage_event_decision(decision: &prompts::TriageDecision) -> Decision {
    match decision {
        prompts::TriageDecision::Skip => Decision::Block,
        prompts::TriageDecision::Proceed
        | prompts::TriageDecision::ProceedWithPlan
        | prompts::TriageDecision::NeedsClarification => Decision::Complete,
    }
}

async fn emit_final_triage_decision_event(
    events: &EventStore,
    task_id: &TaskId,
    turn: u32,
    decision: &ResolvedTriageDecision,
) {
    let event = build_task_event(
        task_id,
        turn,
        "triage",
        "triage_decision",
        final_triage_event_decision(&decision.decision),
        Some(decision.reason.clone()),
        Some(format!(
            "result={}",
            final_triage_event_result(&decision.decision)
        )),
        None,
        None,
        None,
    );
    if let Err(error) = events.log(&event).await {
        tracing::warn!(task_id = %task_id, "failed to log triage_decision event: {error}");
    }
}

async fn fetch_typed_issue_snapshot(
    repo_slug: &str,
    issue: u64,
    github_token: Option<&str>,
) -> anyhow::Result<TypedIssueSnapshot> {
    if repo_slug.trim().is_empty() || repo_slug == UNKNOWN_REPO_SLUG {
        anyhow::bail!("repository slug unavailable for issue triage lookup");
    }

    let client = reqwest::Client::builder()
        .timeout(GITHUB_ISSUE_LOOKUP_TIMEOUT)
        .build()?;
    let url = format!("{}/repos/{repo_slug}/issues/{issue}", github_api_base_url());
    let mut request = client
        .get(url)
        .header(ACCEPT, "application/vnd.github+json")
        .header(USER_AGENT, "harness-server");
    if let Some(token) = crate::github_auth::resolve_github_token(github_token) {
        request = request.bearer_auth(token);
    }
    let response = request.send().await?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("GitHub issue lookup failed with status {status}: {body}");
    }
    let snapshot: GitHubIssueSnapshotResponse = response.json().await?;
    Ok(TypedIssueSnapshot {
        body: snapshot.body.unwrap_or_default(),
        labels: snapshot
            .labels
            .into_iter()
            .map(|label| label.name)
            .collect(),
    })
}

fn fetch_typed_issue_snapshot_boxed<'a>(
    repo_slug: &'a str,
    issue: u64,
    github_token: Option<&'a str>,
) -> IssueFetchFuture<'a> {
    Box::pin(fetch_typed_issue_snapshot(repo_slug, issue, github_token))
}

async fn record_phase_observability(
    store: &TaskStore,
    events: &EventStore,
    task_id: &TaskId,
    turn: u32,
    action: &str,
    result: &str,
    detail: Option<String>,
    telemetry: Option<TurnTelemetry>,
    failure: Option<TurnFailure>,
    decision: Decision,
    reason: Option<String>,
) -> anyhow::Result<()> {
    mutate_and_persist(store, task_id, |state| {
        state.rounds.push(RoundResult::new(
            turn,
            action,
            result,
            detail.clone(),
            telemetry.clone(),
            failure.clone(),
        ));
    })
    .await?;

    let event = build_task_event(
        task_id,
        turn,
        action,
        &format!("task_{action}"),
        decision,
        reason,
        None,
        telemetry,
        failure,
        detail,
    );
    if let Err(error) = events.log(&event).await {
        tracing::warn!(task_id = %task_id, action, "failed to log {action} event: {error}");
    }

    Ok(())
}

fn redact_prompt_plan_failure(failure: TurnFailure) -> TurnFailure {
    TurnFailure {
        message: None,
        body_excerpt: None,
        ..failure
    }
}

fn turn_failure_kind_label(kind: TurnFailureKind) -> &'static str {
    match kind {
        TurnFailureKind::Timeout => "timeout",
        TurnFailureKind::Quota => "quota",
        TurnFailureKind::Billing => "billing",
        TurnFailureKind::LocalProcess => "local_process",
        TurnFailureKind::Upstream => "upstream",
        TurnFailureKind::Protocol => "protocol",
        TurnFailureKind::Unknown => "unknown",
    }
}

fn redact_prompt_plan_error_message(failure: &TurnFailure) -> String {
    let mut message = format!(
        "plan phase agent error (details redacted for prompt-only task privacy; kind={}",
        turn_failure_kind_label(failure.kind)
    );
    if let Some(provider) = &failure.provider {
        message.push_str(&format!(", provider={provider}"));
    }
    if let Some(status) = failure.upstream_status {
        message.push_str(&format!(", upstream_status={status}"));
    }
    message.push(')');
    message
}

/// Run a plan step for a complex prompt-only task when the planning gate has
/// forced `TaskPhase::Plan`.
///
/// This gives the planning gate real effect: instead of silently skipping
/// to Implement, the agent produces a plan first, which is then threaded into
/// the implementation prompt just like issue-based plans.
pub(crate) async fn run_plan_for_prompt(
    agent: &dyn CodeAgent,
    store: &TaskStore,
    task_id: &TaskId,
    cargo_env: &HashMap<String, String>,
    project: &Path,
    req: &CreateTaskRequest,
    skills: &RwLock<harness_skills::store::SkillStore>,
    events: &EventStore,
) -> anyhow::Result<(Option<String>, prompts::TriageComplexity, u32)> {
    let prompt_text = req.prompt.as_deref().unwrap_or_default();
    tracing::info!(task_id = %task_id, "pipeline: starting plan phase for prompt-only task");

    mutate_and_persist(store, task_id, |state| {
        state.phase = TaskPhase::Plan;
    })
    .await?;

    let plan_prompt = prompts::plan_for_prompt_task(prompt_text);
    let plan_prompt = augment_prompt_with_skills(skills, events, task_id, plan_prompt).await;
    let prompt_built_at = Utc::now();
    let agent_started_at = Utc::now();

    let plan_req = AgentRequest {
        prompt: plan_prompt,
        project_root: project.to_path_buf(),
        env_vars: cargo_env.clone(),
        execution_phase: Some(ExecutionPhase::Planning),
        ..Default::default()
    };

    let turn_timeout = crate::task_runner::effective_turn_timeout(req.turn_timeout_secs);

    let plan_resp = match tokio::time::timeout(
        turn_timeout,
        run_agent_streaming_with_options(
            agent,
            plan_req,
            task_id,
            store,
            0,
            prompt_built_at,
            agent_started_at,
            RunAgentStreamingOptions {
                persist_artifacts: false,
                backfill_auto_fix_issue: false,
            },
        ),
    )
    .await
    {
        Ok(Ok(success)) => {
            record_phase_observability(
                store,
                events,
                task_id,
                0,
                "plan",
                "completed",
                None,
                Some(success.telemetry.clone()),
                None,
                Decision::Complete,
                Some("prompt task plan completed".to_string()),
            )
            .await?;
            success.response
        }
        Ok(Err(failure)) => {
            let persisted_failure = redact_prompt_plan_failure(failure.failure.clone());
            let redacted_error = redact_prompt_plan_error_message(&persisted_failure);
            record_phase_observability(
                store,
                events,
                task_id,
                0,
                "plan",
                "failed",
                None,
                Some(failure.telemetry),
                Some(persisted_failure),
                Decision::Block,
                Some("prompt task plan failed".to_string()),
            )
            .await?;
            return Err(anyhow::anyhow!("{redacted_error}"));
        }
        Err(_) => {
            let telemetry =
                telemetry_for_timeout(prompt_built_at, agent_started_at, Utc::now(), None);
            let failure = harness_core::types::TurnFailure {
                kind: harness_core::types::TurnFailureKind::Timeout,
                provider: None,
                upstream_status: None,
                message: Some(format!(
                    "plan phase timed out after {}s",
                    req.turn_timeout_secs
                )),
                body_excerpt: None,
            };
            record_phase_observability(
                store,
                events,
                task_id,
                0,
                "plan",
                "timeout",
                None,
                Some(telemetry),
                Some(failure),
                Decision::Block,
                Some("prompt task plan timed out".to_string()),
            )
            .await?;
            return Err(anyhow::anyhow!(
                "plan phase timed out after {}s",
                req.turn_timeout_secs
            ));
        }
    };

    let plan_text = plan_resp.output.clone();
    mutate_and_persist(store, task_id, |state| {
        state.phase = TaskPhase::Implement;
    })
    .await?;
    // NOTE: prompt-only task plans stay in-memory only. The returned `plan_text`
    // feeds the immediate implement phase, but we intentionally do not persist
    // it in task state, round detail, or checkpoints because it can echo raw
    // user prompt content, including secrets.

    tracing::info!(task_id = %task_id, plan_len = plan_text.len(), "plan phase complete (prompt-only)");
    Ok((Some(plan_text), prompts::TriageComplexity::Medium, 1))
}

/// Result of the triage and optional plan pipeline.
pub(crate) enum TriagePlanPipelineOutcome {
    /// Triage chose to continue into implementation, optionally with a plan.
    Continue {
        plan_output: Option<String>,
        complexity: prompts::TriageComplexity,
        turns: u32,
    },
    /// Triage intentionally skipped the issue. The task has already been marked
    /// as a successful terminal state and must not continue into implementation.
    Skipped,
}

/// Run triage → plan pipeline for a fresh issue-based task.
///
/// Returns `Continue` if the triage decided implementation should proceed.
/// Returns `Skipped` when triage says SKIP. Agent and parsing failures still
/// propagate as errors.
pub(crate) async fn run_triage_plan_pipeline(
    agent: &dyn CodeAgent,
    store: &TaskStore,
    task_id: &TaskId,
    issue: u64,
    repo_slug: &str,
    github_token: Option<&str>,
    triage_config: Option<&ProjectTriageConfig>,
    cargo_env: &HashMap<String, String>,
    project: &Path,
    req: &CreateTaskRequest,
    skills: &RwLock<harness_skills::store::SkillStore>,
    events: &EventStore,
) -> anyhow::Result<TriagePlanPipelineOutcome> {
    run_triage_plan_pipeline_with_issue_fetcher(
        agent,
        store,
        task_id,
        issue,
        repo_slug,
        github_token,
        triage_config,
        cargo_env,
        project,
        req,
        skills,
        events,
        fetch_typed_issue_snapshot_boxed,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn run_triage_plan_pipeline_with_issue_fetcher(
    agent: &dyn CodeAgent,
    store: &TaskStore,
    task_id: &TaskId,
    issue: u64,
    repo_slug: &str,
    github_token: Option<&str>,
    triage_config: Option<&ProjectTriageConfig>,
    cargo_env: &HashMap<String, String>,
    project: &Path,
    req: &CreateTaskRequest,
    skills: &RwLock<harness_skills::store::SkillStore>,
    events: &EventStore,
    issue_fetcher: IssueFetchFn,
) -> anyhow::Result<TriagePlanPipelineOutcome> {
    // --- Phase 1: Triage ---
    tracing::info!(task_id = %task_id, issue, "pipeline: starting triage phase");
    mutate_and_persist(store, task_id, |state| {
        state.phase = TaskPhase::Triage;
    })
    .await?;
    update_status(store, task_id, TaskStatus::Triaging, 0).await?;

    let triage_prompt = prompts::triage_prompt(issue).to_prompt_string();
    let triage_prompt = augment_prompt_with_skills(skills, events, task_id, triage_prompt).await;
    let triage_prompt_built_at = Utc::now();
    let triage_started_at = Utc::now();
    let triage_req = AgentRequest {
        prompt: triage_prompt,
        project_root: project.to_path_buf(),
        env_vars: cargo_env.clone(),
        execution_phase: Some(ExecutionPhase::Triage),
        ..Default::default()
    };

    let turn_timeout = crate::task_runner::effective_turn_timeout(req.turn_timeout_secs);
    // Defer "completed" observability until after parse_triage succeeds so the
    // DB never shows "completed" for a triage turn that produced unparseable output.
    let (triage_resp, triage_telemetry) = match tokio::time::timeout(
        turn_timeout,
        run_agent_streaming(
            agent,
            triage_req,
            task_id,
            store,
            0,
            triage_prompt_built_at,
            triage_started_at,
        ),
    )
    .await
    {
        Ok(Ok(success)) => (success.response, success.telemetry),
        Ok(Err(failure)) => {
            record_phase_observability(
                store,
                events,
                task_id,
                0,
                "triage",
                "failed",
                None,
                Some(failure.telemetry),
                Some(failure.failure),
                Decision::Block,
                Some(format!("issue #{issue} triage failed")),
            )
            .await?;
            return Err(anyhow::anyhow!(
                "triage phase agent error: {}",
                failure.error
            ));
        }
        Err(_) => {
            let telemetry =
                telemetry_for_timeout(triage_prompt_built_at, triage_started_at, Utc::now(), None);
            let failure = harness_core::types::TurnFailure {
                kind: harness_core::types::TurnFailureKind::Timeout,
                provider: None,
                upstream_status: None,
                message: Some(format!(
                    "triage phase timed out after {}s",
                    req.turn_timeout_secs
                )),
                body_excerpt: None,
            };
            record_phase_observability(
                store,
                events,
                task_id,
                0,
                "triage",
                "timeout",
                None,
                Some(telemetry),
                Some(failure),
                Decision::Block,
                Some(format!("issue #{issue} triage timed out")),
            )
            .await?;
            return Err(anyhow::anyhow!(
                "triage phase timed out after {}s",
                req.turn_timeout_secs
            ));
        }
    };

    let triage_text = triage_resp.output.clone();
    mutate_and_persist(store, task_id, |state| {
        state.triage_output = Some(triage_text.clone());
    })
    .await?;
    if let Err(e) = store
        .write_checkpoint(task_id, Some(&triage_text), None, None, "triage_done")
        .await
    {
        tracing::warn!(task_id = %task_id, "failed to write triage checkpoint: {e}");
    }

    // Log "completed" only after parse_triage succeeds; log "failed" if the
    // agent output did not include a parseable TRIAGE= decision.
    let decision = match prompts::parse_triage(&triage_resp.output) {
        Some(d) => {
            record_phase_observability(
                store,
                events,
                task_id,
                0,
                "triage",
                "completed",
                Some(triage_resp.output.clone()),
                Some(triage_telemetry),
                None,
                Decision::Complete,
                Some(format!("issue #{issue} triage completed")),
            )
            .await?;
            d
        }
        None => {
            record_phase_observability(
                store,
                events,
                task_id,
                0,
                "triage",
                "failed",
                None,
                Some(triage_telemetry),
                Some(harness_core::types::TurnFailure {
                    kind: harness_core::types::TurnFailureKind::Protocol,
                    provider: None,
                    upstream_status: None,
                    message: Some(
                        "triage output unparseable — agent did not produce TRIAGE=<decision>"
                            .to_string(),
                    ),
                    body_excerpt: None,
                }),
                Decision::Block,
                Some(format!("issue #{issue} triage parse failed")),
            )
            .await?;
            anyhow::bail!("triage output unparseable — agent did not produce TRIAGE=<decision>")
        }
    };
    let triage_reason = prompts::parse_triage_reason(&triage_resp.output);
    let complexity = prompts::parse_complexity(&triage_resp.output);
    let fetched_issue = if decision == prompts::TriageDecision::Skip {
        match issue_fetcher(repo_slug, issue, github_token).await {
            Ok(issue_snapshot) => Some(issue_snapshot),
            Err(error) => {
                tracing::warn!(
                    task_id = %task_id,
                    issue,
                    repo = repo_slug,
                    "triage issue fetch failed, falling back to agent decision: {error}"
                );
                None
            }
        }
    } else {
        None
    };
    let resolved_decision = resolve_triage_decision(
        decision.clone(),
        triage_reason.as_deref(),
        fetched_issue.as_ref(),
        triage_config,
    );
    emit_final_triage_decision_event(events, task_id, 0, &resolved_decision).await;
    tracing::info!(
        task_id = %task_id,
        raw_decision = ?decision,
        final_decision = ?resolved_decision.decision,
        ?complexity,
        triage_reason = %resolved_decision.reason,
        "triage decision resolved"
    );

    match resolved_decision.decision {
        prompts::TriageDecision::Skip => {
            mutate_and_persist(store, task_id, |state| {
                state.status = TaskStatus::Done;
                state.phase = TaskPhase::Terminal;
                state.error = Some(format!(
                    "Triage skipped issue #{issue}: {}",
                    resolved_decision.reason
                ));
            })
            .await?;
            return Ok(TriagePlanPipelineOutcome::Skipped);
        }
        prompts::TriageDecision::NeedsClarification => {
            // Treat as ProceedWithPlan — let the planner figure out ambiguities
            // instead of failing the task outright.
            tracing::info!(task_id = %task_id, "triage: NEEDS_CLARIFICATION → treating as PROCEED_WITH_PLAN");
        }
        prompts::TriageDecision::Proceed => {
            tracing::info!(task_id = %task_id, "triage: PROCEED — skipping plan phase");
            return Ok(TriagePlanPipelineOutcome::Continue {
                plan_output: None,
                complexity,
                turns: 1,
            });
        }
        prompts::TriageDecision::ProceedWithPlan => {
            // Fall through to plan phase.
        }
    }

    // --- Phase 2: Plan ---
    tracing::info!(task_id = %task_id, issue, "pipeline: starting plan phase");
    mutate_and_persist(store, task_id, |state| {
        state.phase = TaskPhase::Plan;
    })
    .await?;
    update_status(store, task_id, TaskStatus::Planning, 0).await?;

    let plan_prompt = prompts::plan_prompt(issue, &triage_resp.output).to_prompt_string();
    let plan_prompt = augment_prompt_with_skills(skills, events, task_id, plan_prompt).await;
    let plan_prompt_built_at = Utc::now();
    let plan_started_at = Utc::now();
    let plan_req = AgentRequest {
        prompt: plan_prompt,
        project_root: project.to_path_buf(),
        env_vars: cargo_env.clone(),
        execution_phase: Some(ExecutionPhase::Planning),
        ..Default::default()
    };

    let plan_resp = match tokio::time::timeout(
        turn_timeout,
        run_agent_streaming(
            agent,
            plan_req,
            task_id,
            store,
            0,
            plan_prompt_built_at,
            plan_started_at,
        ),
    )
    .await
    {
        Ok(Ok(success)) => {
            record_phase_observability(
                store,
                events,
                task_id,
                0,
                "plan",
                "completed",
                Some(success.response.output.clone()),
                Some(success.telemetry.clone()),
                None,
                Decision::Complete,
                Some(format!("issue #{issue} plan completed")),
            )
            .await?;
            success.response
        }
        Ok(Err(failure)) => {
            record_phase_observability(
                store,
                events,
                task_id,
                0,
                "plan",
                "failed",
                None,
                Some(failure.telemetry),
                Some(failure.failure),
                Decision::Block,
                Some(format!("issue #{issue} plan failed")),
            )
            .await?;
            return Err(anyhow::anyhow!("plan phase agent error: {}", failure.error));
        }
        Err(_) => {
            let telemetry =
                telemetry_for_timeout(plan_prompt_built_at, plan_started_at, Utc::now(), None);
            let failure = harness_core::types::TurnFailure {
                kind: harness_core::types::TurnFailureKind::Timeout,
                provider: None,
                upstream_status: None,
                message: Some(format!(
                    "plan phase timed out after {}s",
                    req.turn_timeout_secs
                )),
                body_excerpt: None,
            };
            record_phase_observability(
                store,
                events,
                task_id,
                0,
                "plan",
                "timeout",
                None,
                Some(telemetry),
                Some(failure),
                Decision::Block,
                Some(format!("issue #{issue} plan timed out")),
            )
            .await?;
            return Err(anyhow::anyhow!(
                "plan phase timed out after {}s",
                req.turn_timeout_secs
            ));
        }
    };

    let plan_text = plan_resp.output.clone();
    mutate_and_persist(store, task_id, |state| {
        state.plan_output = Some(plan_text.clone());
        state.phase = TaskPhase::Implement;
    })
    .await?;
    if let Err(e) = store
        .write_checkpoint(task_id, None, Some(&plan_text), None, "plan_done")
        .await
    {
        tracing::warn!(task_id = %task_id, "failed to write plan checkpoint: {e}");
    }

    tracing::info!(task_id = %task_id, plan_len = plan_text.len(), "plan phase complete");
    Ok(TriagePlanPipelineOutcome::Continue {
        plan_output: Some(plan_text),
        complexity,
        turns: 2,
    })
}

/// Run a repair-plan step after an implementation attempt emitted `PLAN_ISSUE=...`.
///
/// Returns the corrected plan text and advances the task phase back to
/// `Implement`. The caller decides whether to retry implementation or fail.
pub(crate) async fn run_replan_for_issue(
    agent: &dyn CodeAgent,
    store: &TaskStore,
    task_id: &TaskId,
    issue: u64,
    prior_plan: Option<&str>,
    plan_issue: &str,
    cargo_env: &HashMap<String, String>,
    project: &Path,
    req: &CreateTaskRequest,
    skills: &RwLock<harness_skills::store::SkillStore>,
    events: &EventStore,
) -> anyhow::Result<String> {
    tracing::info!(task_id = %task_id, issue, "pipeline: starting replan phase");
    mutate_and_persist(store, task_id, |state| {
        state.phase = TaskPhase::Plan;
    })
    .await?;

    let prompt = prompts::replan_prompt(issue, prior_plan, plan_issue).to_prompt_string();
    let prompt = augment_prompt_with_skills(skills, events, task_id, prompt).await;
    let plan_req = AgentRequest {
        prompt,
        project_root: project.to_path_buf(),
        env_vars: cargo_env.clone(),
        execution_phase: Some(ExecutionPhase::Planning),
        ..Default::default()
    };

    let turn_timeout = crate::task_runner::effective_turn_timeout(req.turn_timeout_secs);
    let plan_resp = tokio::time::timeout(turn_timeout, agent.execute(plan_req))
        .await
        .map_err(|_| anyhow::anyhow!("replan phase timed out after {}s", req.turn_timeout_secs))?
        .map_err(|e| anyhow::anyhow!("replan phase agent error: {e}"))?;

    let plan_text = plan_resp.output.clone();
    mutate_and_persist(store, task_id, |state| {
        state.plan_output = Some(plan_text.clone());
        state.phase = TaskPhase::Implement;
        state.rounds.push(RoundResult::new(
            state.turn,
            "replan",
            "plan_ready",
            Some(plan_text.clone()),
            None,
            None,
        ));
    })
    .await?;
    if let Err(e) = store
        .write_checkpoint(task_id, None, Some(&plan_text), None, "replan_done")
        .await
    {
        tracing::warn!(task_id = %task_id, "failed to write replan checkpoint: {e}");
    }

    tracing::info!(task_id = %task_id, plan_len = plan_text.len(), "replan phase complete");
    Ok(plan_text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use harness_core::agent::{AgentResponse, StreamItem};
    use harness_core::types::{Capability, TokenUsage};

    struct StaticStreamAgent {
        output: String,
    }

    impl StaticStreamAgent {
        fn new(output: &str) -> Self {
            Self {
                output: output.to_string(),
            }
        }
    }

    #[async_trait]
    impl CodeAgent for StaticStreamAgent {
        fn name(&self) -> &str {
            "static-stream-agent"
        }

        fn capabilities(&self) -> Vec<Capability> {
            Vec::new()
        }

        async fn execute(&self, _req: AgentRequest) -> harness_core::error::Result<AgentResponse> {
            Ok(AgentResponse {
                output: self.output.clone(),
                stderr: String::new(),
                items: Vec::new(),
                token_usage: TokenUsage::default(),
                model: "test".to_string(),
                exit_code: Some(0),
            })
        }

        async fn execute_stream(
            &self,
            _req: AgentRequest,
            tx: tokio::sync::mpsc::Sender<StreamItem>,
        ) -> harness_core::error::Result<()> {
            tx.send(StreamItem::MessageDelta {
                text: self.output.clone(),
            })
            .await
            .map_err(|e| harness_core::error::HarnessError::AgentExecution(e.to_string()))?;
            tx.send(StreamItem::Done)
                .await
                .map_err(|e| harness_core::error::HarnessError::AgentExecution(e.to_string()))?;
            Ok(())
        }
    }

    #[tokio::test]
    async fn triage_skip_returns_successful_terminal_outcome() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let database_url = crate::test_helpers::test_database_url()?;
        let store =
            TaskStore::open_with_database_url(&dir.path().join("tasks.db"), Some(&database_url))
                .await?;
        let task_id = TaskId::new();
        let mut state = crate::task_runner::TaskState::new(task_id.clone());
        state.task_kind = crate::task_runner::TaskKind::Issue;
        store.insert(&state).await;

        let agent = StaticStreamAgent::new("Not actionable.\nCOMPLEXITY=low\nTRIAGE=SKIP");
        let req = CreateTaskRequest {
            issue: Some(123),
            turn_timeout_secs: 30,
            ..CreateTaskRequest::default()
        };
        let skills = RwLock::new(harness_skills::store::SkillStore::new());
        let events = EventStore::new_noop_for_tests();

        let outcome = run_triage_plan_pipeline(
            &agent,
            &store,
            &task_id,
            123,
            UNKNOWN_REPO_SLUG,
            None,
            None,
            &HashMap::new(),
            dir.path(),
            &req,
            &skills,
            &events,
        )
        .await?;

        assert!(matches!(outcome, TriagePlanPipelineOutcome::Skipped));
        let final_state = store
            .get(&task_id)
            .ok_or_else(|| anyhow::anyhow!("task must exist"))?;
        assert_eq!(final_state.status, TaskStatus::Done);
        assert_eq!(final_state.phase, TaskPhase::Terminal);
        assert!(final_state
            .error
            .as_deref()
            .is_some_and(|error| error.contains("Triage skipped issue #123")));
        assert_eq!(
            final_state.scheduler.authority_state,
            crate::task_runner::SchedulerAuthorityState::Done
        );
        Ok(())
    }
}

#[cfg(test)]
#[path = "triage_pipeline_tests.rs"]
mod triage_pipeline_tests;
