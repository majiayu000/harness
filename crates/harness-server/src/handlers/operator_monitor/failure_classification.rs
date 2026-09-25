pub(super) fn classify_failure_family(message: &str) -> &'static str {
    let lower = message.to_ascii_lowercase();
    if lower.contains("ssl_error_syscall")
        || lower.contains("failed to fetch")
        || lower.contains("git fetch")
        || lower.contains("github.com")
    {
        "github_fetch"
    } else if lower.contains("timed out") || lower.contains("timeout") {
        "timeout"
    } else if lower.contains("rate limit") || lower.contains("secondary rate limit") {
        "rate_limit"
    } else if lower.contains("missing structured output")
        || lower.contains("activity_result")
        || lower.contains("structured activity")
    {
        "missing_structured_output"
    } else if lower.contains("worktree") || lower.contains("workspace") {
        "worktree_collision"
    } else if lower.contains("agent turn") || lower.contains("agent failed") {
        "agent_turn_failed"
    } else {
        "internal"
    }
}

pub(super) fn failure_severity(family: &str) -> &'static str {
    match family {
        "github_fetch" | "timeout" | "rate_limit" => "warn",
        "missing_structured_output" | "worktree_collision" | "agent_turn_failed" => "error",
        _ => "error",
    }
}

pub(super) fn failure_retryable(family: &str) -> bool {
    matches!(family, "github_fetch" | "timeout" | "rate_limit")
}

pub(super) fn failure_next_action(family: &str) -> &'static str {
    match family {
        "github_fetch" => "Retry after GitHub connectivity recovers",
        "timeout" => "Retry or inspect the long-running turn",
        "rate_limit" => "Wait for the rate limit window",
        "missing_structured_output" => "Inspect agent output and prompt contract",
        "worktree_collision" => "Inspect workspace ownership",
        "agent_turn_failed" => "Inspect agent logs",
        _ => "Inspect task logs",
    }
}

pub(super) fn normalize_failure_message(message: &str) -> String {
    let first_line = message.lines().next().unwrap_or(message).trim();
    let collapsed = first_line.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() > 180 {
        collapsed.chars().take(177).collect::<String>() + "..."
    } else if collapsed.is_empty() {
        "unknown failure".to_string()
    } else {
        collapsed
    }
}

pub(super) fn earlier_timestamp(current: Option<&str>, candidate: Option<&str>) -> Option<String> {
    current
        .into_iter()
        .chain(candidate)
        .min()
        .map(str::to_string)
}

pub(super) fn later_timestamp(current: Option<&str>, candidate: Option<&str>) -> Option<String> {
    current
        .into_iter()
        .chain(candidate)
        .max()
        .map(str::to_string)
}
