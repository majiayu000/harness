use crate::http::AppState;
use chrono::Utc;
use harness_core::types::{Decision, Event, EventMetadata, ExecutionPhase, SessionId};
use harness_workflow::runtime::RuntimeJob;
use serde_json::{json, Value};
use std::path::Path;

pub(super) async fn record_runtime_prompt_input(
    state: &AppState,
    job: &RuntimeJob,
    agent_name: &str,
    project_root: &Path,
    activity: &str,
    execution_phase: Option<ExecutionPhase>,
    prompt: &str,
) {
    if agent_name != "claude" {
        return;
    }
    let event = runtime_prompt_input_event(
        job,
        agent_name,
        project_root,
        activity,
        execution_phase,
        prompt,
    );
    if let Err(error) = state.observability.events.log(&event).await {
        tracing::warn!(
            runtime_job_id = %job.id,
            activity,
            "failed to log runtime llm_prompt_input event: {error}"
        );
    }
}

fn runtime_prompt_input_event(
    job: &RuntimeJob,
    agent_name: &str,
    project_root: &Path,
    activity: &str,
    execution_phase: Option<ExecutionPhase>,
    prompt: &str,
) -> Event {
    let reported_at = Utc::now();
    let phase_label = execution_phase
        .map(runtime_execution_phase_label)
        .unwrap_or("unknown");
    let prompt_chars = prompt.chars().count();
    let prompt_bytes = prompt.len();
    let workflow_id = job.input.get("workflow_id").and_then(Value::as_str);
    let payload = json!({
        "agent": agent_name,
        "runtime_job_id": job.id,
        "workflow_id": workflow_id.unwrap_or("unknown"),
        "activity": activity,
        "project": project_root.to_string_lossy(),
        "phase": phase_label,
        "ts": reported_at.to_rfc3339(),
        "prompt_chars": prompt_chars,
        "prompt_bytes": prompt_bytes,
    });
    let mut event = Event::new(
        SessionId::new(),
        "llm_prompt_input",
        agent_name,
        Decision::Complete,
    );
    event.ts = reported_at;
    event.detail = Some(format!(
        "runtime_job_id={} workflow_id={} activity={} project={} prompt_chars={} prompt_bytes={}",
        job.id,
        workflow_id.unwrap_or("unknown"),
        activity,
        project_root.display(),
        prompt_chars,
        prompt_bytes
    ));
    event.content = Some(payload.to_string());
    event.metadata = Some(EventMetadata {
        task_id: None,
        turn: None,
        phase: Some(phase_label.to_string()),
        telemetry: None,
        failure: None,
    });
    event
}

pub(super) fn execution_phase_for_runtime_activity(activity: &str) -> Option<ExecutionPhase> {
    match activity {
        "triage_issue" | "poll_repo_backlog" => Some(ExecutionPhase::Triage),
        "plan_issue" | "replan_issue" | "plan_repo_sprint" => Some(ExecutionPhase::Planning),
        "implement_issue" | "implement_prompt" | "address_pr_feedback" => {
            Some(ExecutionPhase::Execution)
        }
        "quality_gate" => Some(ExecutionPhase::Validation),
        "run_local_review" | "inspect_pr_feedback" => Some(ExecutionPhase::SimpleReview),
        other if other.contains("triage") => Some(ExecutionPhase::Triage),
        other if other.contains("plan") => Some(ExecutionPhase::Planning),
        other if other.contains("rebase") => Some(ExecutionPhase::Rebase),
        other if other.contains("review") || other.contains("feedback") => {
            Some(ExecutionPhase::SimpleReview)
        }
        other if other.contains("quality") || other.contains("validation") => {
            Some(ExecutionPhase::Validation)
        }
        other if other.contains("implement") => Some(ExecutionPhase::Execution),
        _ => None,
    }
}

fn runtime_execution_phase_label(phase: ExecutionPhase) -> &'static str {
    match phase {
        ExecutionPhase::Planning => "planning",
        ExecutionPhase::Execution => "execution",
        ExecutionPhase::Validation => "validation",
        ExecutionPhase::Rebase => "rebase",
        ExecutionPhase::SimpleReview => "simple_review",
        ExecutionPhase::Triage => "triage",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_workflow::runtime::{RuntimeJob, RuntimeKind};
    use serde_json::json;
    use std::path::Path;

    #[test]
    fn runtime_activity_phase_mapping_groups_planning_and_review() {
        assert_eq!(
            execution_phase_for_runtime_activity("replan_issue"),
            Some(ExecutionPhase::Planning)
        );
        assert_eq!(
            execution_phase_for_runtime_activity("implement_issue"),
            Some(ExecutionPhase::Execution)
        );
        assert_eq!(
            execution_phase_for_runtime_activity("inspect_pr_feedback"),
            Some(ExecutionPhase::SimpleReview)
        );
        assert_eq!(execution_phase_for_runtime_activity("unknown"), None);
    }

    #[test]
    fn runtime_prompt_input_event_records_size_without_prompt_text() -> anyhow::Result<()> {
        let job = RuntimeJob::pending(
            "cmd-1",
            RuntimeKind::ClaudeCode,
            "claude-default",
            json!({ "workflow_id": "workflow-1" }),
        );
        let event = runtime_prompt_input_event(
            &job,
            "claude",
            Path::new("/repo"),
            "implement_issue",
            Some(ExecutionPhase::Execution),
            "hello world",
        );

        assert_eq!(event.hook, "llm_prompt_input");
        assert_eq!(
            event.metadata.as_ref().and_then(|m| m.phase.as_deref()),
            Some("execution")
        );
        let content = event
            .content
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("missing event content"))?;
        assert!(!content.contains("hello world"));
        let payload: serde_json::Value = serde_json::from_str(content)?;
        assert_eq!(payload["workflow_id"], "workflow-1");
        assert_eq!(payload["prompt_chars"], 11);
        assert_eq!(payload["prompt_bytes"], 11);

        Ok(())
    }
}
