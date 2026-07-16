use crate::runtime::model::WorkflowCommandType;
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransitionRule {
    pub from_state: Option<String>,
    pub to_state: String,
    pub allowed_commands: BTreeSet<WorkflowCommandType>,
    pub required_evidence: BTreeSet<String>,
}

impl TransitionRule {
    pub fn new(
        from_state: impl Into<String>,
        to_state: impl Into<String>,
        allowed_commands: impl IntoIterator<Item = WorkflowCommandType>,
    ) -> Self {
        Self {
            from_state: Some(from_state.into()),
            to_state: to_state.into(),
            allowed_commands: allowed_commands.into_iter().collect(),
            required_evidence: BTreeSet::new(),
        }
    }

    pub fn from_any(
        to_state: impl Into<String>,
        allowed_commands: impl IntoIterator<Item = WorkflowCommandType>,
    ) -> Self {
        Self {
            from_state: None,
            to_state: to_state.into(),
            allowed_commands: allowed_commands.into_iter().collect(),
            required_evidence: BTreeSet::new(),
        }
    }

    pub fn require_evidence(mut self, kind: impl Into<String>) -> Self {
        self.required_evidence.insert(kind.into());
        self
    }

    fn matches(&self, from_state: &str, to_state: &str) -> bool {
        self.to_state == to_state
            && self
                .from_state
                .as_deref()
                .is_none_or(|rule_from| rule_from == from_state)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TransitionAllowlist {
    rules: Vec<TransitionRule>,
}

impl TransitionAllowlist {
    pub fn new(rules: Vec<TransitionRule>) -> Self {
        Self { rules }
    }

    pub fn allow(
        mut self,
        from_state: impl Into<String>,
        to_state: impl Into<String>,
        allowed_commands: impl IntoIterator<Item = WorkflowCommandType>,
    ) -> Self {
        self.rules
            .push(TransitionRule::new(from_state, to_state, allowed_commands));
        self
    }

    pub fn allow_from_any(
        mut self,
        to_state: impl Into<String>,
        allowed_commands: impl IntoIterator<Item = WorkflowCommandType>,
    ) -> Self {
        self.rules
            .push(TransitionRule::from_any(to_state, allowed_commands));
        self
    }

    pub fn rule_for(&self, from_state: &str, to_state: &str) -> Option<&TransitionRule> {
        self.rules
            .iter()
            .find(|rule| rule.matches(from_state, to_state))
    }

    pub fn rules(&self) -> impl Iterator<Item = &TransitionRule> {
        self.rules.iter()
    }

    pub fn rules_from<'a>(
        &'a self,
        from_state: &'a str,
    ) -> impl Iterator<Item = &'a TransitionRule> + 'a {
        self.rules.iter().filter(move |rule| {
            rule.from_state
                .as_deref()
                .is_none_or(|rule_from| rule_from == from_state)
        })
    }
}
