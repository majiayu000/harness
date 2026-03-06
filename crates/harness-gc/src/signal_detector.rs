use harness_core::{
    Decision, Event, ProjectId, RemediationType, Signal, SignalType, Violation,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignalThresholds {
    pub repeated_warn_min: usize,
    pub chronic_block_min: usize,
    pub hot_file_edits_min: usize,
    pub slow_op_threshold_ms: u64,
    pub slow_op_count_min: usize,
    pub escalation_ratio: f64,
    pub violation_min: usize,
}

impl Default for SignalThresholds {
    fn default() -> Self {
        Self {
            repeated_warn_min: 10,
            chronic_block_min: 5,
            hot_file_edits_min: 20,
            slow_op_threshold_ms: 5000,
            slow_op_count_min: 10,
            escalation_ratio: 1.5,
            violation_min: 5,
        }
    }
}

impl From<harness_core::config::SignalThresholds> for SignalThresholds {
    fn from(t: harness_core::config::SignalThresholds) -> Self {
        Self {
            repeated_warn_min: t.repeated_warn_min,
            chronic_block_min: t.chronic_block_min,
            hot_file_edits_min: t.hot_file_edits_min,
            slow_op_threshold_ms: t.slow_op_threshold_ms,
            slow_op_count_min: t.slow_op_count_min,
            escalation_ratio: t.escalation_ratio,
            violation_min: t.violation_min,
        }
    }
}

pub struct SignalDetector {
    thresholds: SignalThresholds,
    project_id: ProjectId,
}

impl SignalDetector {
    pub fn new(thresholds: SignalThresholds, project_id: ProjectId) -> Self {
        Self {
            thresholds,
            project_id,
        }
    }

    pub fn detect(&self, events: &[Event]) -> Vec<Signal> {
        let mut signals = Vec::new();
        signals.extend(self.detect_repeated_warns(events));
        signals.extend(self.detect_chronic_blocks(events));
        signals.extend(self.detect_hot_files(events));
        signals.extend(self.detect_slow_sessions(events));
        signals.extend(self.detect_warn_escalation(events));
        signals.extend(self.detect_linter_violations(events));
        signals
    }

    pub fn from_violations(&self, violations: &[Violation]) -> Vec<Signal> {
        let mut by_rule: HashMap<String, Vec<&Violation>> = HashMap::new();
        for v in violations {
            by_rule.entry(v.rule_id.as_str().to_string()).or_default().push(v);
        }

        by_rule
            .into_iter()
            .filter(|(_, vs)| vs.len() >= self.thresholds.violation_min)
            .map(|(rule_id, vs)| {
                Signal::new(
                    SignalType::LinterViolations,
                    self.project_id.clone(),
                    serde_json::json!({
                        "rule_id": rule_id,
                        "count": vs.len(),
                        "files": vs.iter().map(|v| v.file.display().to_string()).collect::<Vec<_>>(),
                    }),
                    RemediationType::Guard,
                )
            })
            .collect()
    }

    fn detect_repeated_warns(&self, events: &[Event]) -> Vec<Signal> {
        let mut by_reason: HashMap<String, usize> = HashMap::new();
        for e in events {
            if matches!(e.decision, Decision::Warn) {
                let reason = e.reason.clone().unwrap_or_else(|| e.hook.clone());
                *by_reason.entry(reason).or_default() += 1;
            }
        }

        by_reason
            .into_iter()
            .filter(|(_, count)| *count >= self.thresholds.repeated_warn_min)
            .map(|(reason, count)| {
                Signal::new(
                    SignalType::RepeatedWarn,
                    self.project_id.clone(),
                    serde_json::json!({ "reason": reason, "count": count }),
                    RemediationType::Guard,
                )
            })
            .collect()
    }

    fn detect_chronic_blocks(&self, events: &[Event]) -> Vec<Signal> {
        let mut by_hook: HashMap<String, usize> = HashMap::new();
        for e in events {
            if matches!(e.decision, Decision::Block) {
                *by_hook.entry(e.hook.clone()).or_default() += 1;
            }
        }

        by_hook
            .into_iter()
            .filter(|(_, count)| *count >= self.thresholds.chronic_block_min)
            .map(|(hook, count)| {
                Signal::new(
                    SignalType::ChronicBlock,
                    self.project_id.clone(),
                    serde_json::json!({ "hook": hook, "count": count }),
                    RemediationType::Rule,
                )
            })
            .collect()
    }

    fn detect_hot_files(&self, events: &[Event]) -> Vec<Signal> {
        let mut by_file: HashMap<String, usize> = HashMap::new();
        for e in events {
            if let Some(ref detail) = e.detail {
                // Assume detail contains file path for edit events
                if e.hook.contains("edit") || e.tool.contains("Edit") || e.tool.contains("Write") {
                    *by_file.entry(detail.clone()).or_default() += 1;
                }
            }
        }

        by_file
            .into_iter()
            .filter(|(_, count)| *count >= self.thresholds.hot_file_edits_min)
            .map(|(file, count)| {
                Signal::new(
                    SignalType::HotFiles,
                    self.project_id.clone(),
                    serde_json::json!({ "file": file, "count": count }),
                    RemediationType::Skill,
                )
            })
            .collect()
    }

    fn detect_slow_sessions(&self, events: &[Event]) -> Vec<Signal> {
        let slow_count = events
            .iter()
            .filter(|e| {
                e.duration_ms
                    .map(|d| d > self.thresholds.slow_op_threshold_ms)
                    .unwrap_or(false)
            })
            .count();

        if slow_count >= self.thresholds.slow_op_count_min {
            vec![Signal::new(
                SignalType::SlowSessions,
                self.project_id.clone(),
                serde_json::json!({
                    "slow_ops": slow_count,
                    "threshold_ms": self.thresholds.slow_op_threshold_ms,
                }),
                RemediationType::Skill,
            )]
        } else {
            Vec::new()
        }
    }

    fn detect_linter_violations(&self, events: &[Event]) -> Vec<Signal> {
        let mut by_rule: HashMap<String, (usize, HashSet<String>)> = HashMap::new();
        for e in events.iter().filter(|e| e.hook == "rule_check") {
            let entry = by_rule.entry(e.tool.clone()).or_default();
            entry.0 += 1;
            if let Some(detail) = &e.detail {
                // `EventStore::log_violations` stores detail as "file:line";
                // use rsplit_once so paths containing colons (e.g. Windows) are handled correctly.
                let file = match detail.rsplit_once(':') {
                    Some((path, line)) if line.chars().all(|c| c.is_ascii_digit()) => path,
                    _ => detail.as_str(),
                }
                .to_string();
                if !file.is_empty() {
                    entry.1.insert(file);
                }
            }
        }

        by_rule
            .into_iter()
            .filter(|(_, (count, _))| *count >= self.thresholds.violation_min)
            .map(|(rule_id, (count, files))| {
                let mut files: Vec<String> = files.into_iter().collect();
                files.sort();
                Signal::new(
                    SignalType::LinterViolations,
                    self.project_id.clone(),
                    serde_json::json!({
                        "rule_id": rule_id,
                        "count": count,
                        "files": files,
                    }),
                    RemediationType::Guard,
                )
            })
            .collect()
    }

    fn detect_warn_escalation(&self, events: &[Event]) -> Vec<Signal> {
        if events.len() < 20 {
            return Vec::new();
        }

        let mid = events.len() / 2;
        let first_half_warns = events[..mid]
            .iter()
            .filter(|e| matches!(e.decision, Decision::Warn))
            .count() as f64;
        let second_half_warns = events[mid..]
            .iter()
            .filter(|e| matches!(e.decision, Decision::Warn))
            .count() as f64;

        let first_rate = first_half_warns / mid as f64;
        let second_rate = second_half_warns / (events.len() - mid) as f64;

        if first_rate > 0.0 && second_rate / first_rate >= self.thresholds.escalation_ratio {
            vec![Signal::new(
                SignalType::WarnEscalation,
                self.project_id.clone(),
                serde_json::json!({
                    "first_half_rate": first_rate,
                    "second_half_rate": second_rate,
                    "ratio": second_rate / first_rate,
                }),
                RemediationType::Rule,
            )]
        } else {
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::{Decision, Event, ProjectId, SessionId, SignalType, Violation, Severity};
    use std::path::PathBuf;

    fn detector() -> SignalDetector {
        SignalDetector::new(SignalThresholds::default(), ProjectId::new())
    }

    fn warn_event(reason: &str) -> Event {
        let mut e = Event::new(SessionId::new(), "hook", "tool", Decision::Warn);
        e.reason = Some(reason.to_string());
        e
    }

    fn block_event(hook: &str) -> Event {
        Event::new(SessionId::new(), hook, "tool", Decision::Block)
    }

    fn edit_event(file: &str) -> Event {
        let mut e = Event::new(SessionId::new(), "edit_hook", "Edit", Decision::Pass);
        e.detail = Some(file.to_string());
        e
    }

    fn slow_event(ms: u64) -> Event {
        let mut e = Event::new(SessionId::new(), "hook", "tool", Decision::Pass);
        e.duration_ms = Some(ms);
        e
    }

    #[test]
    fn detects_repeated_warn() {
        let det = detector();
        let events: Vec<Event> = (0..10).map(|_| warn_event("unwrap usage")).collect();
        let signals = det.detect(&events);
        assert!(signals.iter().any(|s| s.signal_type == SignalType::RepeatedWarn));
    }

    #[test]
    fn no_signal_below_repeated_warn_threshold() {
        let det = detector();
        let events: Vec<Event> = (0..9).map(|_| warn_event("test")).collect();
        let signals = det.detect(&events);
        assert!(!signals.iter().any(|s| s.signal_type == SignalType::RepeatedWarn));
    }

    #[test]
    fn detects_chronic_block() {
        let det = detector();
        let events: Vec<Event> = (0..5).map(|_| block_event("security")).collect();
        let signals = det.detect(&events);
        assert!(signals.iter().any(|s| s.signal_type == SignalType::ChronicBlock));
    }

    #[test]
    fn detects_hot_files() {
        let det = detector();
        let events: Vec<Event> = (0..20).map(|_| edit_event("/src/main.rs")).collect();
        let signals = det.detect(&events);
        assert!(signals.iter().any(|s| s.signal_type == SignalType::HotFiles));
    }

    #[test]
    fn detects_slow_sessions() {
        let det = detector();
        let events: Vec<Event> = (0..10).map(|_| slow_event(6000)).collect();
        let signals = det.detect(&events);
        assert!(signals.iter().any(|s| s.signal_type == SignalType::SlowSessions));
    }

    #[test]
    fn detects_warn_escalation() {
        let det = SignalDetector::new(
            SignalThresholds { escalation_ratio: 1.5, ..Default::default() },
            ProjectId::new(),
        );
        // First half: 2 warns out of 10 → rate 0.2
        // Second half: 8 warns out of 10 → rate 0.8 (ratio 4.0 > 1.5)
        let mut events: Vec<Event> = Vec::new();
        for _ in 0..8 {
            events.push(Event::new(SessionId::new(), "h", "t", Decision::Pass));
        }
        for _ in 0..2 {
            events.push(warn_event("reason"));
        }
        for _ in 0..2 {
            events.push(Event::new(SessionId::new(), "h", "t", Decision::Pass));
        }
        for _ in 0..8 {
            events.push(warn_event("reason"));
        }
        let signals = det.detect(&events);
        assert!(signals.iter().any(|s| s.signal_type == SignalType::WarnEscalation));
    }

    #[test]
    fn detects_linter_violations_signal() {
        let det = detector();
        let violations: Vec<Violation> = (0..5).map(|_| Violation {
            rule_id: harness_core::RuleId::from_str("SEC-01"),
            file: PathBuf::from("/src/lib.rs"),
            line: Some(1),
            message: "issue".to_string(),
            severity: Severity::High,
        }).collect();
        let signals = det.from_violations(&violations);
        assert!(signals.iter().any(|s| s.signal_type == SignalType::LinterViolations));
    }

    #[test]
    fn detects_linter_violations_from_events() {
        let det = detector();
        let sid = SessionId::new();
        let mut events: Vec<Event> = Vec::new();
        for i in 0..5 {
            let mut e = Event::new(sid.clone(), "rule_check", "SEC-01", Decision::Block);
            e.detail = Some(format!("/src/lib.rs:{i}"));
            events.push(e);
        }
        let signals = det.detect(&events);
        assert!(signals.iter().any(|s| s.signal_type == SignalType::LinterViolations));
    }
}
