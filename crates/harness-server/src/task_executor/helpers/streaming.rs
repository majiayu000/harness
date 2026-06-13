use crate::task_runner::{TaskId, TaskStore};
use chrono::{DateTime, Utc};
use harness_core::agent::{AgentRequest, AgentResponse, CodeAgent, StreamItem};
use harness_core::error::HarnessError;
use harness_core::prompts;
use harness_core::types::{
    Decision, Event, EventMetadata, ExecutionPhase, Item, SessionId, TokenUsage, TurnFailure,
    TurnTelemetry,
};
use harness_observe::event_store::EventStore;
use harness_observe::usage::UsageMetrics;
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::RwLock;
use tokio::time::{Duration, Instant};

static USAGE_EVENT_STORE: OnceLock<RwLock<Option<Arc<EventStore>>>> = OnceLock::new();
const USAGE_EVENT_LOG_TIMEOUT: Duration = Duration::from_secs(2);

use super::{is_prompt_only_task, persist_artifact, TurnExecutionFailure, TurnExecutionSuccess};

#[derive(Clone, Copy)]
pub(crate) struct RunAgentStreamingOptions {
    pub persist_artifacts: bool,
    /// Scan agent output for CREATED_ISSUE= sentinels and back-fill the task's
    /// external_id.  Must be set explicitly by the implement phase for auto-fix
    /// tasks; left false for review/planning turns to prevent reviewer output
    /// from accidentally corrupting external_id.
    pub backfill_auto_fix_issue: bool,
}

impl Default for RunAgentStreamingOptions {
    fn default() -> Self {
        Self {
            persist_artifacts: true,
            backfill_auto_fix_issue: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_llm_usage_event_records_usage_payload() -> anyhow::Result<()> {
        let task_id = TaskId::from_str("usage-task");
        let usage = TokenUsage {
            input_tokens: 10,
            output_tokens: 4,
            total_tokens: 18,
            cost_usd: 0.0,
        };

        let event = build_llm_usage_event(
            &task_id,
            2,
            "implementing",
            "codex",
            Some("gpt-5"),
            "/repo",
            &usage,
        )
        .ok_or_else(|| anyhow::anyhow!("missing llm_usage event"))?;
        assert_eq!(event.hook, "llm_usage");
        assert_eq!(event.tool, "codex");
        assert_eq!(event.metadata.as_ref().and_then(|m| m.turn), Some(2));
        let content = event
            .content
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("missing llm_usage content"))?;
        let payload: serde_json::Value = serde_json::from_str(content)?;
        assert_eq!(payload["agent"], "codex");
        assert_eq!(payload["model"], "gpt-5");
        assert_eq!(payload["task_id"], "usage-task");
        assert_eq!(payload["project"], "/repo");
        assert_eq!(payload["input_tokens"], 10);
        assert_eq!(payload["output_tokens"], 4);
        assert_eq!(payload["cache_read_input_tokens"], 4);
        assert_eq!(payload["cache_creation_input_tokens"], 0);

        Ok(())
    }

    #[test]
    fn build_llm_usage_event_skips_zero_placeholder_usage() {
        let task_id = TaskId::from_str("usage-task");

        let event = build_llm_usage_event(
            &task_id,
            1,
            "implementing",
            "claude",
            None,
            "/repo",
            &TokenUsage::default(),
        );
        assert!(event.is_none());
    }

    #[test]
    fn build_prompt_input_event_records_size_payload() -> anyhow::Result<()> {
        let task_id = TaskId::from_str("prompt-size-task");
        let event = build_prompt_input_event(
            &task_id,
            3,
            "planning",
            "claude",
            Some("sonnet"),
            "/repo",
            "hello world",
            2,
        );

        assert_eq!(event.hook, "llm_prompt_input");
        assert_eq!(event.tool, "claude");
        assert_eq!(event.metadata.as_ref().and_then(|m| m.turn), Some(3));
        assert_eq!(
            event.metadata.as_ref().and_then(|m| m.phase.as_deref()),
            Some("planning")
        );
        let payload: serde_json::Value = serde_json::from_str(
            event
                .content
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("missing prompt input content"))?,
        )?;
        assert_eq!(payload["agent"], "claude");
        assert_eq!(payload["prompt_chars"], 11);
        assert_eq!(payload["prompt_bytes"], 11);
        assert_eq!(payload["context_items"], 2);

        Ok(())
    }

    #[test]
    fn execution_phase_label_preserves_simple_review_snake_case() {
        assert_eq!(
            execution_phase_label(Some(ExecutionPhase::SimpleReview)),
            "simple_review"
        );
        assert_eq!(execution_phase_label(None), "unknown");
    }

    #[tokio::test]
    async fn usage_event_logging_timeout_is_non_blocking() {
        let status = log_usage_event_with_timeout(
            &TaskId::new(),
            "claude",
            "llm_prompt_input",
            Duration::from_millis(1),
            std::future::pending::<anyhow::Result<()>>(),
        )
        .await;

        assert_eq!(status, UsageEventLogStatus::TimedOut);
    }

    #[test]
    fn usage_project_label_prefers_canonical_task_root() {
        let canonical = Path::new("/repo/main");
        let workspace = Path::new("/tmp/harness-worktree");

        assert_eq!(
            usage_project_label(Some(canonical), workspace),
            "/repo/main"
        );
    }

    #[test]
    fn usage_project_label_falls_back_to_request_root() {
        let workspace = Path::new("/tmp/harness-worktree");

        assert_eq!(
            usage_project_label(None, workspace),
            "/tmp/harness-worktree"
        );
    }
}

pub(crate) fn set_usage_event_store(events: Arc<EventStore>) {
    let slot = USAGE_EVENT_STORE.get_or_init(|| RwLock::new(None));
    let mut guard = slot.write().unwrap_or_else(|error| error.into_inner());
    *guard = Some(events);
}

fn current_usage_event_store() -> Option<Arc<EventStore>> {
    let slot = USAGE_EVENT_STORE.get_or_init(|| RwLock::new(None));
    let guard = slot.read().unwrap_or_else(|error| error.into_inner());
    guard.clone()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UsageEventLogStatus {
    Logged,
    Failed,
    TimedOut,
}

async fn log_usage_event_with_timeout<F>(
    task_id: &TaskId,
    agent: &str,
    hook: &str,
    timeout_duration: Duration,
    log: F,
) -> UsageEventLogStatus
where
    F: Future<Output = anyhow::Result<()>>,
{
    match tokio::time::timeout(timeout_duration, log).await {
        Ok(Ok(())) => UsageEventLogStatus::Logged,
        Ok(Err(error)) => {
            tracing::warn!(
                task_id = %task_id,
                agent,
                hook,
                "failed to log usage event: {error}"
            );
            UsageEventLogStatus::Failed
        }
        Err(_) => {
            tracing::warn!(
                task_id = %task_id,
                agent,
                hook,
                timeout_secs = timeout_duration.as_secs_f64(),
                "timed out logging usage event"
            );
            UsageEventLogStatus::TimedOut
        }
    }
}

async fn log_llm_usage_event(
    events: &EventStore,
    task_id: &TaskId,
    turn: u32,
    phase: &str,
    agent: &str,
    model: Option<&str>,
    project: &str,
    usage: &TokenUsage,
) {
    let Some(event) = build_llm_usage_event(task_id, turn, phase, agent, model, project, usage)
    else {
        return;
    };

    log_usage_event_with_timeout(
        task_id,
        agent,
        "llm_usage",
        USAGE_EVENT_LOG_TIMEOUT,
        async { events.log(&event).await.map(|_| ()) },
    )
    .await;
}

async fn log_prompt_input_event(
    events: &EventStore,
    task_id: &TaskId,
    turn: u32,
    phase: &str,
    agent: &str,
    model: Option<&str>,
    project: &str,
    prompt: &str,
    context_items: usize,
) {
    let event = build_prompt_input_event(
        task_id,
        turn,
        phase,
        agent,
        model,
        project,
        prompt,
        context_items,
    );
    log_usage_event_with_timeout(
        task_id,
        agent,
        "llm_prompt_input",
        USAGE_EVENT_LOG_TIMEOUT,
        async { events.log(&event).await.map(|_| ()) },
    )
    .await;
}

fn build_prompt_input_event(
    task_id: &TaskId,
    turn: u32,
    phase: &str,
    agent: &str,
    model: Option<&str>,
    project: &str,
    prompt: &str,
    context_items: usize,
) -> Event {
    let reported_at = Utc::now();
    let prompt_chars = prompt.chars().count();
    let prompt_bytes = prompt.len();
    let payload = serde_json::json!({
        "agent": agent,
        "model": model.unwrap_or("unknown"),
        "task_id": task_id.as_str(),
        "project": project,
        "phase": phase,
        "turn": turn,
        "ts": reported_at.to_rfc3339(),
        "prompt_chars": prompt_chars,
        "prompt_bytes": prompt_bytes,
        "context_items": context_items,
    });
    let mut event = Event::new(
        SessionId::new(),
        "llm_prompt_input",
        agent,
        Decision::Complete,
    );
    event.ts = reported_at;
    event.detail = Some(format!(
        "task_id={} project={} model={} prompt_chars={} prompt_bytes={}",
        task_id.as_str(),
        project,
        model.unwrap_or("unknown"),
        prompt_chars,
        prompt_bytes
    ));
    event.content = Some(payload.to_string());
    event.metadata = Some(EventMetadata {
        task_id: Some(task_id.clone()),
        turn: Some(turn),
        phase: Some(phase.to_string()),
        telemetry: None,
        failure: None,
    });
    event
}

fn build_llm_usage_event(
    task_id: &TaskId,
    turn: u32,
    phase: &str,
    agent: &str,
    model: Option<&str>,
    project: &str,
    usage: &TokenUsage,
) -> Option<Event> {
    if usage.input_tokens == 0 && usage.output_tokens == 0 && usage.total_tokens == 0 {
        return None;
    }
    let usage_metrics = UsageMetrics::from_token_usage(usage);
    let reported_at = Utc::now();
    let payload = serde_json::json!({
        "agent": agent,
        "model": model.unwrap_or("unknown"),
        "task_id": task_id.as_str(),
        "project": project,
        "ts": reported_at.to_rfc3339(),
        "input_tokens": usage_metrics.input_tokens,
        "output_tokens": usage_metrics.output_tokens,
        "cache_read_input_tokens": usage_metrics.cache_read_input_tokens,
        "cache_creation_input_tokens": usage_metrics.cache_creation_input_tokens,
        "reported_total_tokens": usage_metrics.reported_total_tokens,
    });
    let mut event = Event::new(SessionId::new(), "llm_usage", agent, Decision::Complete);
    event.ts = reported_at;
    event.detail = Some(format!(
        "task_id={} project={} model={} tokens={}",
        task_id.as_str(),
        project,
        model.unwrap_or("unknown"),
        usage.total_tokens
    ));
    event.content = Some(payload.to_string());
    event.metadata = Some(EventMetadata {
        task_id: Some(task_id.clone()),
        turn: Some(turn),
        phase: Some(phase.to_string()),
        telemetry: None,
        failure: None,
    });
    Some(event)
}

fn usage_project_label(task_project_root: Option<&Path>, request_project_root: &Path) -> String {
    task_project_root
        .unwrap_or(request_project_root)
        .to_string_lossy()
        .into_owned()
}

fn execution_phase_label(phase: Option<ExecutionPhase>) -> String {
    phase.map_or_else(|| "unknown".to_string(), |phase| phase.label().to_string())
}

/// Scan `output_slice` for a `CREATED_ISSUE=` sentinel and, when a new issue
/// number is found, write it to the database.
async fn backfill_issue_if_found(
    output_slice: &str,
    last_backfilled_issue: &mut Option<u64>,
    store: &TaskStore,
    task_id: &TaskId,
) {
    if let Some(issue_num) = prompts::parse_created_issue_number(output_slice) {
        if Some(issue_num) != *last_backfilled_issue {
            let eid = format!("issue:{issue_num}");
            match store.overwrite_external_id_auto_fix(task_id, &eid).await {
                Ok(()) => {
                    tracing::info!(
                        task_id = %task_id,
                        external_id = %eid,
                        "streaming: backfilled external_id for auto-fix task"
                    );
                    *last_backfilled_issue = Some(issue_num);
                }
                Err(e) => tracing::warn!(
                    task_id = %task_id,
                    "streaming: failed to backfill external_id: {e}"
                ),
            }
        }
    }
}

/// Execute an agent request via [`CodeAgent::execute_stream`], broadcasting
/// each [`StreamItem`] to the per-task channel in real time, and reconstruct
/// an [`AgentResponse`] from the collected stream events.
pub(crate) async fn run_agent_streaming(
    agent: &dyn CodeAgent,
    req: AgentRequest,
    task_id: &TaskId,
    store: &TaskStore,
    turn: u32,
    prompt_built_at: DateTime<Utc>,
    agent_started_at: DateTime<Utc>,
) -> Result<TurnExecutionSuccess, TurnExecutionFailure> {
    run_agent_streaming_with_options(
        agent,
        req,
        task_id,
        store,
        turn,
        prompt_built_at,
        agent_started_at,
        RunAgentStreamingOptions::default(),
    )
    .await
}

pub(crate) async fn run_agent_streaming_with_options(
    agent: &dyn CodeAgent,
    req: AgentRequest,
    task_id: &TaskId,
    store: &TaskStore,
    turn: u32,
    prompt_built_at: DateTime<Utc>,
    agent_started_at: DateTime<Utc>,
    options: RunAgentStreamingOptions,
) -> Result<TurnExecutionSuccess, TurnExecutionFailure> {
    let turn_start = Instant::now();

    let agent_name = agent.name().to_string();
    let requested_model = req.model.clone();
    let task_project_root = store.get(task_id).and_then(|state| state.project_root);
    let project = usage_project_label(task_project_root.as_deref(), &req.project_root);
    let is_prompt_only = is_prompt_only_task(store, task_id);
    let phase_str = execution_phase_label(req.execution_phase);
    if !is_prompt_only {
        let redacted_prompt = crate::redact::redact_secrets(&req.prompt, &req.env_vars);
        if let Err(e) = store
            .save_prompt(task_id, turn, &phase_str, &redacted_prompt)
            .await
        {
            tracing::warn!(task_id = %task_id, turn, "failed to persist prompt: {e}");
        }
    }
    if agent_name == "claude" {
        if let Some(events) = current_usage_event_store() {
            log_prompt_input_event(
                &events,
                task_id,
                turn,
                &phase_str,
                &agent_name,
                requested_model.as_deref(),
                &project,
                &req.prompt,
                req.context.len(),
            )
            .await;
        }
    }

    let (tx, mut rx) = tokio::sync::mpsc::channel::<StreamItem>(128);
    let mut exec = std::pin::pin!(agent.execute_stream(req, tx));
    let mut exec_result: Option<harness_core::error::Result<()>> = None;
    let mut channel_closed = false;
    let mut output = String::new();
    let mut token_usage = TokenUsage::default();
    let mut first_token_latency_ms: Option<u64> = None;
    let mut first_output_at: Option<DateTime<Utc>> = None;
    let mut last_backfilled_issue: Option<u64> = None;
    // Tracks how many bytes of `output` have already been scanned for
    // CREATED_ISSUE= so each MessageDelta only scans newly appended lines,
    // keeping the overall scan cost O(N) instead of O(N²).
    let mut issue_scan_pos: usize = 0;
    // Pre-check whether this task should scan for CREATED_ISSUE= sentinels.
    // Controlled by the caller via options.backfill_auto_fix_issue so that
    // review/planning turns never corrupt external_id with false-positive matches.
    let is_auto_fix_execution = options.backfill_auto_fix_issue
        && store
            .get(task_id)
            .is_some_and(|s| s.source.as_deref() == Some("auto-fix"));

    loop {
        tokio::select! {
            result = &mut exec, if exec_result.is_none() => {
                exec_result = Some(result);
            }
            item = rx.recv(), if !channel_closed => {
                match item {
                    Some(item) => {
                        store.publish_stream_item(task_id, item.clone());
                        match &item {
                            StreamItem::MessageDelta { text } => {
                                if first_token_latency_ms.is_none() {
                                    first_output_at = Some(Utc::now());
                                    first_token_latency_ms = Some(turn_start.elapsed().as_millis() as u64);
                                }
                                output.push_str(text);

                                if is_auto_fix_execution && text.contains('\n') {
                                    let complete_len =
                                        output.rfind('\n').map(|i| i + 1).unwrap_or(0);
                                    if complete_len > issue_scan_pos {
                                        backfill_issue_if_found(
                                            &output[issue_scan_pos..complete_len],
                                            &mut last_backfilled_issue,
                                            store,
                                            task_id,
                                        )
                                        .await;
                                        issue_scan_pos = complete_len;
                                    }
                                }
                            }
                            StreamItem::ItemCompleted {
                                item: Item::AgentReasoning { content },
                            } => {
                                output = content.clone();
                                // Reset scan position to end of new content so subsequent
                                // MessageDelta items don't use a stale byte offset into
                                // the replaced string.
                                issue_scan_pos = output.len();
                                if first_output_at.is_none() && !content.is_empty() {
                                    first_output_at = Some(Utc::now());
                                }
                                // For non-streaming adapters the full output arrives
                                // here; apply the same sentinel scan.
                                if is_auto_fix_execution {
                                    backfill_issue_if_found(
                                        &output,
                                        &mut last_backfilled_issue,
                                        store,
                                        task_id,
                                    )
                                    .await;
                                }
                            }
                            StreamItem::ItemCompleted { item: completed_item }
                                if options.persist_artifacts =>
                            {
                                persist_artifact(store, task_id, turn, completed_item).await;
                            }
                            StreamItem::TokenUsage { usage } => {
                                token_usage = usage.clone();
                            }
                            StreamItem::Done => {
                                channel_closed = true;
                            }
                            _ => {}
                        }
                    }
                    None => {
                        channel_closed = true;
                    }
                }
            }
        }
        if exec_result.is_some() && channel_closed {
            break;
        }
    }

    // Final scan for any trailing output that didn't end in a newline and
    // was therefore not covered by a MessageDelta or ItemCompleted event.
    if is_auto_fix_execution && output.len() > issue_scan_pos {
        backfill_issue_if_found(
            &output[issue_scan_pos..],
            &mut last_backfilled_issue,
            store,
            task_id,
        )
        .await;
    }

    if let Some(events) = current_usage_event_store() {
        log_llm_usage_event(
            &events,
            task_id,
            turn,
            &phase_str,
            &agent_name,
            requested_model.as_deref(),
            &project,
            &token_usage,
        )
        .await;
    }

    let completed_at = Utc::now();
    let completed_latency_ms = completed_at
        .signed_duration_since(agent_started_at)
        .num_milliseconds()
        .max(0) as u64;

    let base_telemetry = TurnTelemetry {
        prompt_built_at: Some(prompt_built_at),
        agent_started_at: Some(agent_started_at),
        first_output_at,
        completed_at: Some(completed_at),
        first_token_latency_ms,
        completed_latency_ms: Some(completed_latency_ms),
        retry_count: None,
        exit_code: None,
    };

    match exec_result.unwrap_or_else(|| {
        Err(HarnessError::AgentExecution(
            "agent execution completed without result".into(),
        ))
    }) {
        Ok(()) => Ok(TurnExecutionSuccess {
            response: AgentResponse {
                output,
                stderr: String::new(),
                items: Vec::new(),
                token_usage,
                model: String::new(),
                exit_code: Some(0),
            },
            telemetry: TurnTelemetry {
                exit_code: Some(0),
                ..base_telemetry
            },
        }),
        Err(error) => {
            let failure = error.turn_failure().unwrap_or(TurnFailure {
                kind: harness_core::types::TurnFailureKind::Unknown,
                provider: None,
                upstream_status: None,
                message: Some(error.to_string()),
                body_excerpt: None,
            });
            Err(TurnExecutionFailure {
                error,
                telemetry: base_telemetry,
                failure,
            })
        }
    }
}
