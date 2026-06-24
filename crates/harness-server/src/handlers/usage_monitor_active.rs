use std::collections::BTreeMap;

use harness_workflow::runtime::{RuntimeJobStatus, RuntimeKind};

use super::{ActiveCount, AgentInvocation};

pub(super) fn aggregate_active_counts<F>(
    invocations: &[AgentInvocation],
    key_fn: F,
) -> Vec<ActiveCount>
where
    F: Fn(&AgentInvocation) -> String,
{
    let mut counts: BTreeMap<String, ActiveCount> = BTreeMap::new();
    for invocation in invocations {
        if !matches!(invocation.status, "running" | "pending") {
            continue;
        }

        let entry = counts
            .entry(key_fn(invocation))
            .or_insert_with_key(|name| ActiveCount {
                name: name.clone(),
                running: 0,
                active_leased: 0,
                expired_or_missing_lease: 0,
                pending: 0,
                high_burn: 0,
            });
        match invocation.status {
            "running" => entry.running += 1,
            "pending" => entry.pending += 1,
            _ => {}
        }
        match invocation.lease_state {
            Some("active_leased") => entry.active_leased += 1,
            Some("expired_lease" | "missing_lease") => entry.expired_or_missing_lease += 1,
            _ => {}
        }
        if invocation.burn_level == "high" {
            entry.high_burn += 1;
        }
    }
    let mut rows = counts.into_values().collect::<Vec<_>>();
    rows.sort_by(|a, b| {
        let a_total = a.running + a.pending;
        let b_total = b.running + b.pending;
        b_total.cmp(&a_total).then_with(|| a.name.cmp(&b.name))
    });
    rows
}

pub(super) fn burn_level(
    status: &str,
    _activity: &str,
    reasoning_effort: Option<&str>,
    age_secs: i64,
    stale: bool,
) -> &'static str {
    if stale {
        return "high";
    }
    if status != "running" {
        return "low";
    }
    if matches!(reasoning_effort, Some("xhigh" | "high")) || age_secs >= 1800 {
        return "high";
    }
    if age_secs >= 900 {
        return "medium";
    }
    "low"
}

pub(super) fn runtime_job_status(status: RuntimeJobStatus) -> &'static str {
    match status {
        RuntimeJobStatus::Pending => "pending",
        RuntimeJobStatus::Running => "running",
        RuntimeJobStatus::Succeeded => "succeeded",
        RuntimeJobStatus::Failed => "failed",
        RuntimeJobStatus::Cancelled => "cancelled",
    }
}

pub(super) fn runtime_kind(kind: RuntimeKind) -> &'static str {
    match kind {
        RuntimeKind::CodexExec => "codex_exec",
        RuntimeKind::CodexJsonrpc => "codex_jsonrpc",
        RuntimeKind::ClaudeCode => "claude_code",
        RuntimeKind::AnthropicApi => "anthropic_api",
        RuntimeKind::RemoteHost => "remote_host",
    }
}
