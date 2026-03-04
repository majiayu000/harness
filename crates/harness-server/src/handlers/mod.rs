pub mod classify;
pub mod cross_review;
pub mod exec;
pub mod gc;
pub mod health;
pub mod learn;
pub mod observe;
pub mod preflight;
pub mod rules;
pub mod skills;
pub mod thread;

/// Persist each violation as a `rule_check` event in the event store.
///
/// Severity maps to decision: Critical/High → Block, Medium → Warn, Low → Pass.
/// All violations from a single scan share a session_id so they can be correlated.
pub(crate) fn persist_violations(
    events: &harness_observe::EventStore,
    violations: &[harness_core::Violation],
) {
    if violations.is_empty() {
        return;
    }
    let session_id = harness_core::SessionId::new();
    for violation in violations {
        let decision = match violation.severity {
            harness_core::Severity::Critical | harness_core::Severity::High => {
                harness_core::Decision::Block
            }
            harness_core::Severity::Medium => harness_core::Decision::Warn,
            harness_core::Severity::Low => harness_core::Decision::Pass,
        };
        let mut event = harness_core::Event::new(
            session_id.clone(),
            "rule_check",
            violation.rule_id.as_str(),
            decision,
        );
        event.reason = Some(violation.message.clone());
        event.detail = Some(format!(
            "{}:{}",
            violation.file.display(),
            violation.line.map(|l| l.to_string()).unwrap_or_default()
        ));
        if let Err(e) = events.log(&event) {
            tracing::warn!("failed to log rule violation event: {e}");
        }
    }
}

/// Validate that `file` is an existing path within `project_root` (already canonicalized).
/// Returns the canonicalized file path on success.
pub(crate) fn validate_file_in_root(
    file: &std::path::Path,
    project_root: &std::path::Path,
) -> Result<std::path::PathBuf, String> {
    let canonical = file
        .canonicalize()
        .map_err(|e| format!("invalid file path '{}': {e}", file.display()))?;
    if !canonical.starts_with(project_root) {
        return Err(format!(
            "file path '{}' is outside project root '{}'",
            canonical.display(),
            project_root.display()
        ));
    }
    Ok(canonical)
}

/// Validate that a project root is an existing directory within `$HOME`.
/// Returns the canonicalized path on success.
pub(crate) fn validate_project_root(path: &std::path::Path) -> Result<std::path::PathBuf, String> {
    let home = std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .map_err(|_| "HOME environment variable not set".to_string())?;
    let canonical = path
        .canonicalize()
        .map_err(|e| format!("invalid project root '{}': {e}", path.display()))?;
    if !canonical.is_dir() {
        return Err(format!(
            "project root is not a directory: {}",
            canonical.display()
        ));
    }
    if !canonical.starts_with(&home) {
        return Err(format!(
            "project root must be within HOME: {}",
            canonical.display()
        ));
    }
    Ok(canonical)
}
