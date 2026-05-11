//! Prompts for GC-driven remediation drafts, one per `SignalType`.
//!
//! The `SignalType` enum lives in `harness-gc`; matching on a variant remains
//! the caller's responsibility so this crate does not depend on `harness-gc`.
//! Callers fan out a `match SignalType` to the appropriate function below.

/// Remediation prompt for the `RepeatedWarn` signal.
pub fn repeated_warn(project_name: &str, details: &str) -> String {
    format!(
        "Analyze repeated warnings in project {project_name} and generate a guard script to detect this pattern.\n\
         Details: {details}"
    )
}

/// Remediation prompt for the `ChronicBlock` signal.
pub fn chronic_block(project_name: &str, details: &str) -> String {
    format!(
        "Analyze chronic block events in project {project_name} and suggest rule improvements to reduce false blocks.\n\
         Details: {details}"
    )
}

/// Remediation prompt for the `HotFiles` signal.
pub fn hot_files(project_name: &str, details: &str) -> String {
    format!(
        "Analyze frequently edited files in project {project_name} and create a SKILL.md with editing strategies.\n\
         Details: {details}"
    )
}

/// Remediation prompt for the `SlowSessions` signal.
pub fn slow_sessions(project_name: &str, details: &str) -> String {
    format!(
        "Analyze slow operations in project {project_name} and create a performance optimization SKILL.md.\n\
         Details: {details}"
    )
}

/// Remediation prompt for the `WarnEscalation` signal.
pub fn warn_escalation(project_name: &str, details: &str) -> String {
    format!(
        "Warning trends are escalating in project {project_name}. Suggest rule upgrades (warn → block).\n\
         Details: {details}"
    )
}

/// Remediation prompt for the `LinterViolations` signal.
pub fn linter_violations(project_name: &str, details: &str) -> String {
    format!(
        "Code scan found violations in project {project_name}. Generate a guard script to detect and prevent these.\n\
         Details: {details}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    type SignalRenderer = fn(&str, &str) -> String;

    #[test]
    fn each_prompt_includes_project_name_and_details() {
        let cases: &[(&str, SignalRenderer)] = &[
            ("guard script", repeated_warn),
            ("rule improvements", chronic_block),
            ("SKILL.md", hot_files),
            ("performance optimization", slow_sessions),
            ("rule upgrades", warn_escalation),
            ("guard script", linter_violations),
        ];
        for (marker, render) in cases {
            let prompt = render("demo-project", "raw details");
            assert!(
                prompt.contains("demo-project"),
                "missing project name in `{marker}` prompt"
            );
            assert!(
                prompt.contains("Details: raw details"),
                "missing details in `{marker}` prompt"
            );
        }
    }
}
