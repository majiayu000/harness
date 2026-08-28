use harness_core::agent::{AgentDiagnosticSeverity, AgentEvent};
use harness_core::types::TokenUsage;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum TurnSignal {
    Diagnostic {
        severity: AgentDiagnosticSeverity,
        message: String,
    },
    FailureEvidence {
        message: String,
    },
    UsageUpdated {
        usage: TokenUsage,
    },
    Terminal(TurnTerminal),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum TurnTerminal {
    Completed {
        output: String,
        usage: Option<TokenUsage>,
    },
    Failed {
        message: String,
    },
    Cancelled {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) enum AgentTurnOutcome {
    #[default]
    Pending,
    Completed,
    Failed {
        message: String,
    },
    Cancelled {
        message: String,
    },
    Incomplete {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AgentTurnDiagnostic {
    pub(crate) severity: AgentDiagnosticSeverity,
    pub(crate) message: String,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AgentTurnReport {
    pub(crate) outcome: AgentTurnOutcome,
    pub(crate) diagnostics: Vec<AgentTurnDiagnostic>,
    pub(crate) failure_evidence: Vec<String>,
    pub(crate) token_usage: TokenUsage,
}

impl AgentTurnReport {
    pub(crate) fn failure_message(&self) -> Option<&str> {
        match &self.outcome {
            AgentTurnOutcome::Failed { message } | AgentTurnOutcome::Incomplete { message } => {
                Some(message)
            }
            AgentTurnOutcome::Pending
            | AgentTurnOutcome::Completed
            | AgentTurnOutcome::Cancelled { .. } => None,
        }
    }

    pub(crate) fn has_explicit_failure(&self) -> bool {
        matches!(self.outcome, AgentTurnOutcome::Failed { .. }) || !self.failure_evidence.is_empty()
    }
}

#[derive(Debug, Default)]
pub(crate) struct AgentTurnReducer {
    outcome: AgentTurnOutcome,
    diagnostics: Vec<AgentTurnDiagnostic>,
    failure_evidence: Vec<String>,
    token_usage: TokenUsage,
}

impl AgentTurnReducer {
    pub(crate) fn apply(&mut self, signal: TurnSignal) -> Vec<AgentEvent> {
        match signal {
            TurnSignal::Diagnostic { severity, message } => {
                self.diagnostics.push(AgentTurnDiagnostic {
                    severity,
                    message: message.clone(),
                });
                vec![AgentEvent::Diagnostic { severity, message }]
            }
            TurnSignal::FailureEvidence { message } => {
                self.failure_evidence.push(message.clone());
                self.diagnostics.push(AgentTurnDiagnostic {
                    severity: AgentDiagnosticSeverity::Error,
                    message: message.clone(),
                });
                vec![AgentEvent::Diagnostic {
                    severity: AgentDiagnosticSeverity::Error,
                    message,
                }]
            }
            TurnSignal::UsageUpdated { usage } => {
                self.token_usage = usage.clone();
                vec![AgentEvent::TokenUsage { usage }]
            }
            TurnSignal::Terminal(terminal) => self.apply_terminal(terminal),
        }
    }

    pub(crate) fn is_terminal(&self) -> bool {
        !matches!(self.outcome, AgentTurnOutcome::Pending)
    }

    pub(crate) fn finish(mut self) -> AgentTurnReport {
        if matches!(self.outcome, AgentTurnOutcome::Pending) {
            let message = self.failure_evidence.last().cloned().unwrap_or_else(|| {
                "agent stream ended without an authoritative terminal event".to_string()
            });
            self.outcome = AgentTurnOutcome::Incomplete { message };
        }
        AgentTurnReport {
            outcome: self.outcome,
            diagnostics: self.diagnostics,
            failure_evidence: self.failure_evidence,
            token_usage: self.token_usage,
        }
    }

    fn apply_terminal(&mut self, terminal: TurnTerminal) -> Vec<AgentEvent> {
        if self.is_terminal() {
            let message = "agent protocol emitted contradictory terminal events".to_string();
            self.outcome = AgentTurnOutcome::Failed {
                message: message.clone(),
            };
            return vec![AgentEvent::Error { message }];
        }

        match terminal {
            TurnTerminal::Completed { output, usage } => {
                self.outcome = AgentTurnOutcome::Completed;
                let mut events = Vec::with_capacity(2);
                if let Some(usage) = usage {
                    self.token_usage = usage.clone();
                    events.push(AgentEvent::TokenUsage { usage });
                }
                events.push(AgentEvent::TurnCompleted { output });
                events
            }
            TurnTerminal::Failed { message } => {
                self.outcome = AgentTurnOutcome::Failed {
                    message: message.clone(),
                };
                vec![AgentEvent::Error { message }]
            }
            TurnTerminal::Cancelled { message } => {
                self.outcome = AgentTurnOutcome::Cancelled {
                    message: message.clone(),
                };
                vec![AgentEvent::TurnCancelled { message }]
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authoritative_completion_outweighs_failure_evidence() {
        let mut reducer = AgentTurnReducer::default();
        reducer.apply(TurnSignal::FailureEvidence {
            message: "recoverable protocol error".into(),
        });

        let events = reducer.apply(TurnSignal::Terminal(TurnTerminal::Completed {
            output: "done".into(),
            usage: None,
        }));
        let report = reducer.finish();

        assert_eq!(
            events,
            vec![AgentEvent::TurnCompleted {
                output: "done".into()
            }]
        );
        assert_eq!(report.outcome, AgentTurnOutcome::Completed);
        assert_eq!(report.failure_evidence, vec!["recoverable protocol error"]);
    }

    #[test]
    fn missing_terminal_fails_with_latest_failure_evidence() {
        let mut reducer = AgentTurnReducer::default();
        reducer.apply(TurnSignal::FailureEvidence {
            message: "bad config".into(),
        });

        let report = reducer.finish();

        assert_eq!(report.failure_message(), Some("bad config"));
        assert!(matches!(
            report.outcome,
            AgentTurnOutcome::Incomplete { .. }
        ));
    }

    #[test]
    fn contradictory_terminal_events_fail_closed() {
        let mut reducer = AgentTurnReducer::default();
        reducer.apply(TurnSignal::Terminal(TurnTerminal::Completed {
            output: "done".into(),
            usage: None,
        }));

        let events = reducer.apply(TurnSignal::Terminal(TurnTerminal::Failed {
            message: "late failure".into(),
        }));
        let report = reducer.finish();

        assert_eq!(
            events,
            vec![AgentEvent::Error {
                message: "agent protocol emitted contradictory terminal events".into(),
            }]
        );
        assert_eq!(
            report.failure_message(),
            Some("agent protocol emitted contradictory terminal events")
        );
    }
}
