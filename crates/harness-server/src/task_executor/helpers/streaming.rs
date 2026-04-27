use crate::task_runner::{TaskId, TaskStore};
use chrono::{DateTime, Utc};
use harness_core::agent::{AgentRequest, AgentResponse, CodeAgent, StreamItem};
use harness_core::error::HarnessError;
use harness_core::prompts;
use harness_core::types::{Item, TokenUsage, TurnFailure, TurnTelemetry};
use tokio::time::Instant;

use super::{is_prompt_only_task, persist_artifact, TurnExecutionFailure, TurnExecutionSuccess};

#[derive(Debug, Clone, Copy)]
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

    let is_prompt_only = is_prompt_only_task(store, task_id);
    let phase_str = req
        .execution_phase
        .map(|p| format!("{p:?}").to_lowercase())
        .unwrap_or_else(|| "unknown".into());
    if !is_prompt_only {
        let redacted_prompt = crate::redact::redact_secrets(&req.prompt, &req.env_vars);
        if let Err(e) = store
            .save_prompt(task_id, turn, &phase_str, &redacted_prompt)
            .await
        {
            tracing::warn!(task_id = %task_id, turn, "failed to persist prompt: {e}");
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
