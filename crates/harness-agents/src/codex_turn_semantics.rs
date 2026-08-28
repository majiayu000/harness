use crate::turn_reducer::{AgentTurnReducer, TurnSignal, TurnTerminal};
use harness_core::agent::{AgentDiagnosticSeverity, AgentEvent};
use harness_core::types::TokenUsage;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CodexTurnFact {
    Diagnostic {
        severity: AgentDiagnosticSeverity,
        message: String,
    },
    FailureObserved {
        message: String,
    },
    UsageUpdated {
        usage: TokenUsage,
    },
    Terminal(CodexTurnTerminal),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CodexTurnTerminal {
    Completed {
        output: String,
        usage: Option<TokenUsage>,
    },
    Failed {
        message: String,
    },
    Interrupted {
        message: String,
    },
}

#[derive(Debug, Default)]
pub(crate) struct CodexTurnSemantics;

impl CodexTurnSemantics {
    pub(crate) fn interpret(&mut self, fact: CodexTurnFact) -> Vec<TurnSignal> {
        let signal = match fact {
            CodexTurnFact::Diagnostic { severity, message } => {
                TurnSignal::Diagnostic { severity, message }
            }
            CodexTurnFact::FailureObserved { message } => TurnSignal::FailureEvidence { message },
            CodexTurnFact::UsageUpdated { usage } => TurnSignal::UsageUpdated { usage },
            CodexTurnFact::Terminal(CodexTurnTerminal::Completed { output, usage }) => {
                TurnSignal::Terminal(TurnTerminal::Completed { output, usage })
            }
            CodexTurnFact::Terminal(CodexTurnTerminal::Failed { message }) => {
                TurnSignal::Terminal(TurnTerminal::Failed { message })
            }
            CodexTurnFact::Terminal(CodexTurnTerminal::Interrupted { message }) => {
                TurnSignal::Terminal(TurnTerminal::Cancelled { message })
            }
        };
        vec![signal]
    }
}

#[derive(Debug, Default)]
pub(crate) struct CodexTurnSession {
    semantics: CodexTurnSemantics,
    reducer: AgentTurnReducer,
}

impl CodexTurnSession {
    pub(crate) fn project_app_server_event(
        &mut self,
        event: AgentEvent,
    ) -> (Vec<AgentEvent>, bool) {
        let fact = match event {
            AgentEvent::Diagnostic {
                severity: AgentDiagnosticSeverity::Error,
                message,
            } => CodexTurnFact::FailureObserved { message },
            AgentEvent::Diagnostic { severity, message } => {
                CodexTurnFact::Diagnostic { severity, message }
            }
            AgentEvent::TokenUsage { usage } => CodexTurnFact::UsageUpdated { usage },
            AgentEvent::TurnCompleted { output } => {
                CodexTurnFact::Terminal(CodexTurnTerminal::Completed {
                    output,
                    usage: None,
                })
            }
            AgentEvent::TurnCancelled { message } => {
                CodexTurnFact::Terminal(CodexTurnTerminal::Interrupted { message })
            }
            AgentEvent::Error { message } => {
                CodexTurnFact::Terminal(CodexTurnTerminal::Failed { message })
            }
            event => return (vec![event], self.reducer.is_terminal()),
        };
        let events = self
            .semantics
            .interpret(fact)
            .into_iter()
            .flat_map(|signal| self.reducer.apply(signal))
            .collect();
        (events, self.reducer.is_terminal())
    }
}
