use super::EventStore;
use chrono::Utc;
use harness_core::{
    run_id::RunId,
    types::{AutoFixReport, Decision, Event, EventFilters, Grade, SessionId, Severity, Violation},
};
use std::path::Path;

impl EventStore {
    pub async fn persist_rule_scan(
        &self,
        project_root: &Path,
        violations: &[Violation],
    ) -> SessionId {
        self.persist_rule_scan_with_run_id(project_root, violations, None)
            .await
    }

    pub async fn persist_rule_scan_with_run_id(
        &self,
        project_root: &Path,
        violations: &[Violation],
        run_id: Option<&RunId>,
    ) -> SessionId {
        let session_id = SessionId::new();
        let decision = if violations.is_empty() {
            Decision::Pass
        } else {
            Decision::Warn
        };
        let mut scan_event = Event::new(session_id.clone(), "rule_scan", "RuleEngine", decision);
        apply_explicit_run_id(&mut scan_event, run_id);
        scan_event.reason = Some(format!("violations={}", violations.len()));
        scan_event.detail = Some(project_root.display().to_string());

        let mut events = vec![scan_event];
        events.extend(violation_events(&session_id, violations, run_id));
        if let Err(e) = self.log_many(&events).await {
            tracing::warn!("failed to log rule scan events: {e}");
        }
        session_id
    }

    pub async fn log_quality_grade(&self, grade: Grade, score: f64) {
        let decision = match grade {
            Grade::A | Grade::B => Decision::Pass,
            Grade::C => Decision::Warn,
            Grade::D => Decision::Block,
        };
        let mut event = Event::new(SessionId::new(), "quality_grade", "QualityGrader", decision);
        event.detail = Some(format!("grade={grade:?} score={score:.1}"));
        if let Err(e) = self.log(&event).await {
            tracing::warn!("failed to log quality_grade event: {e}");
        }
    }

    pub async fn log_auto_fix_report(
        &self,
        session_id: &SessionId,
        report: &AutoFixReport,
        project_root: &Path,
    ) {
        let decision = if report.residual_violations.is_empty() {
            Decision::Pass
        } else {
            Decision::Warn
        };
        let mut summary = Event::new(session_id.clone(), "auto_fix", "RuleEngine", decision);
        summary.reason = Some(format!(
            "applied={} residual={}",
            report.fixed_count,
            report.residual_violations.len()
        ));
        summary.detail = Some(project_root.display().to_string());

        let mut events = vec![summary];
        events.extend(report.attempts.iter().map(|attempt| {
            let attempt_decision = if attempt.resolved {
                Decision::Pass
            } else if attempt.applied {
                Decision::Warn
            } else {
                Decision::Block
            };
            let mut event = Event::new(
                session_id.clone(),
                "auto_fix_attempt",
                attempt.rule_id.as_str(),
                attempt_decision,
            );
            event.reason = Some(format!(
                "applied={} resolved={}",
                attempt.applied, attempt.resolved
            ));
            event.detail = Some(if let Some(line) = attempt.line {
                format!("{}:{line}", attempt.file.display())
            } else {
                attempt.file.display().to_string()
            });
            event
        }));
        if let Err(e) = self.log_many(&events).await {
            tracing::warn!("failed to log auto_fix events: {e}");
        }
    }

    pub async fn persist_retry_summary(
        &self,
        checked: u32,
        retried: u32,
        stuck: u32,
        skipped: u32,
    ) {
        let decision = if stuck > 0 {
            Decision::Warn
        } else {
            Decision::Pass
        };
        let mut event = Event::new(
            SessionId::new(),
            "periodic_retry:summary",
            "RetryScheduler",
            decision,
        );
        event.detail = Some(format!(
            r#"{{"checked":{checked},"retried":{retried},"stuck":{stuck},"skipped":{skipped}}}"#
        ));
        if let Err(e) = self.log(&event).await {
            tracing::warn!("periodic_retry: failed to log summary event: {e}");
        }
    }

    pub async fn query_recent(&self, duration: std::time::Duration) -> anyhow::Result<Vec<Event>> {
        let since = Utc::now() - chrono::Duration::from_std(duration)?;
        self.query(&EventFilters {
            since: Some(since),
            ..Default::default()
        })
        .await
    }
}

fn violation_events(
    session_id: &SessionId,
    violations: &[Violation],
    run_id: Option<&RunId>,
) -> Vec<Event> {
    violations
        .iter()
        .map(|violation| {
            let decision = match violation.severity {
                Severity::Critical | Severity::High => Decision::Block,
                Severity::Medium => Decision::Warn,
                Severity::Low => Decision::Pass,
            };
            let mut event = Event::new(
                session_id.clone(),
                "rule_check",
                violation.rule_id.as_str(),
                decision,
            );
            apply_explicit_run_id(&mut event, run_id);
            event.reason = Some(violation.message.clone());
            event.detail = Some(if let Some(line) = violation.line {
                format!("{}:{}", violation.file.display(), line)
            } else {
                violation.file.display().to_string()
            });
            event
        })
        .collect()
}

fn apply_explicit_run_id(event: &mut Event, run_id: Option<&RunId>) {
    if let Some(run_id) = run_id {
        event.run_id = Some(run_id.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn absent_explicit_run_id_preserves_existing_event_identity() -> anyhow::Result<()> {
        let inherited = RunId::from_str("ar-01j1qb3c9r7v5m2k8x4tznq6wd")?;
        let mut event = Event::new(SessionId::new(), "rule_scan", "RuleEngine", Decision::Pass);
        event.run_id = Some(inherited.clone());

        apply_explicit_run_id(&mut event, None);

        assert_eq!(event.run_id.as_ref(), Some(&inherited));
        Ok(())
    }

    #[test]
    fn explicit_run_id_overrides_existing_event_identity() -> anyhow::Result<()> {
        let inherited = RunId::from_str("ar-01j1qb3c9r7v5m2k8x4tznq6wd")?;
        let explicit = RunId::from_str("ar-01j1qb3c9r7v5m2k8x4tznq6we")?;
        let mut event = Event::new(SessionId::new(), "rule_scan", "RuleEngine", Decision::Pass);
        event.run_id = Some(inherited);

        apply_explicit_run_id(&mut event, Some(&explicit));

        assert_eq!(event.run_id.as_ref(), Some(&explicit));
        Ok(())
    }
}
