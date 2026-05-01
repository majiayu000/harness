use super::model::{WorkflowCommand, WorkflowCommandType, WorkflowDecision, WorkflowInstance};
use chrono::{DateTime, Utc};
use std::collections::BTreeSet;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransitionRule {
    pub from_state: Option<String>,
    pub to_state: String,
    pub allowed_commands: BTreeSet<WorkflowCommandType>,
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
        }
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

    pub fn github_issue_pr_defaults() -> Self {
        use WorkflowCommandType::{
            BindPr, EnqueueActivity, MarkBlocked, MarkCancelled, MarkDone, MarkFailed,
            RecordPlanConcern, RequestOperatorAttention, StartChildWorkflow, Wait,
        };

        Self::default()
            .allow("discovered", "scheduled", [EnqueueActivity, Wait])
            .allow("scheduled", "planning", [EnqueueActivity, Wait])
            .allow("scheduled", "implementing", [EnqueueActivity, Wait])
            .allow("planning", "implementing", [EnqueueActivity, MarkBlocked])
            .allow(
                "implementing",
                "implementing",
                [EnqueueActivity, RecordPlanConcern, Wait],
            )
            .allow(
                "implementing",
                "replanning",
                [EnqueueActivity, RecordPlanConcern, MarkBlocked, Wait],
            )
            .allow(
                "replanning",
                "implementing",
                [EnqueueActivity, RecordPlanConcern, MarkBlocked, Wait],
            )
            .allow(
                "implementing",
                "pr_open",
                [BindPr, EnqueueActivity, StartChildWorkflow, Wait],
            )
            .allow("pr_open", "pr_open", [BindPr, Wait])
            .allow(
                "pr_open",
                "awaiting_feedback",
                [EnqueueActivity, StartChildWorkflow, Wait],
            )
            .allow(
                "pr_open",
                "addressing_feedback",
                [EnqueueActivity, StartChildWorkflow, MarkBlocked, Wait],
            )
            .allow(
                "pr_open",
                "ready_to_merge",
                [EnqueueActivity, StartChildWorkflow, Wait],
            )
            .allow(
                "awaiting_feedback",
                "awaiting_feedback",
                [EnqueueActivity, StartChildWorkflow, Wait],
            )
            .allow(
                "awaiting_feedback",
                "addressing_feedback",
                [EnqueueActivity, StartChildWorkflow, MarkBlocked, Wait],
            )
            .allow(
                "addressing_feedback",
                "addressing_feedback",
                [EnqueueActivity, StartChildWorkflow, MarkBlocked, Wait],
            )
            .allow(
                "addressing_feedback",
                "awaiting_feedback",
                [EnqueueActivity, StartChildWorkflow, Wait],
            )
            .allow(
                "addressing_feedback",
                "ready_to_merge",
                [EnqueueActivity, StartChildWorkflow, Wait],
            )
            .allow(
                "awaiting_feedback",
                "ready_to_merge",
                [EnqueueActivity, StartChildWorkflow, Wait],
            )
            .allow("ready_to_merge", "ready_to_merge", [Wait])
            .allow("ready_to_merge", "done", [MarkDone])
            .allow_from_any("blocked", [MarkBlocked, RequestOperatorAttention, Wait])
            .allow_from_any("failed", [MarkFailed])
            .allow_from_any("cancelled", [MarkCancelled])
    }

    pub fn repo_backlog_defaults() -> Self {
        use WorkflowCommandType::{
            EnqueueActivity, MarkBlocked, MarkCancelled, MarkDone, MarkFailed,
            RequestOperatorAttention, StartChildWorkflow, Wait,
        };

        Self::default()
            .allow(
                "idle",
                "dispatching",
                [StartChildWorkflow, EnqueueActivity, Wait],
            )
            .allow("idle", "reconciling", [EnqueueActivity, Wait])
            .allow(
                "scanning",
                "dispatching",
                [StartChildWorkflow, EnqueueActivity, Wait],
            )
            .allow("scanning", "reconciling", [EnqueueActivity, Wait])
            .allow(
                "dispatching",
                "dispatching",
                [StartChildWorkflow, EnqueueActivity, Wait],
            )
            .allow("dispatching", "reconciling", [EnqueueActivity, Wait])
            .allow("reconciling", "reconciling", [EnqueueActivity, Wait])
            .allow(
                "reconciling",
                "dispatching",
                [StartChildWorkflow, EnqueueActivity, Wait],
            )
            .allow("dispatching", "idle", [Wait])
            .allow("reconciling", "idle", [Wait])
            .allow_from_any("blocked", [MarkBlocked, RequestOperatorAttention, Wait])
            .allow_from_any("done", [MarkDone])
            .allow_from_any("failed", [MarkFailed])
            .allow_from_any("cancelled", [MarkCancelled])
    }
}

#[derive(Debug, Clone)]
pub struct ValidationContext {
    pub actor: String,
    pub now: DateTime<Utc>,
    pub resource_budget_available: bool,
    pub replan_available: bool,
    pub wait_available: bool,
    pub allow_terminal_reopen: bool,
    pub active_dedupe_keys: BTreeSet<String>,
}

impl ValidationContext {
    pub fn new(actor: impl Into<String>, now: DateTime<Utc>) -> Self {
        Self {
            actor: actor.into(),
            now,
            resource_budget_available: true,
            replan_available: true,
            wait_available: true,
            allow_terminal_reopen: false,
            active_dedupe_keys: BTreeSet::new(),
        }
    }

    pub fn without_resource_budget(mut self) -> Self {
        self.resource_budget_available = false;
        self
    }

    pub fn with_replan_exhausted(mut self) -> Self {
        self.replan_available = false;
        self
    }

    pub fn with_wait_exhausted(mut self) -> Self {
        self.wait_available = false;
        self
    }

    pub fn with_active_dedupe_key(mut self, dedupe_key: impl Into<String>) -> Self {
        self.active_dedupe_keys.insert(dedupe_key.into());
        self
    }

    pub fn allow_terminal_reopen(mut self) -> Self {
        self.allow_terminal_reopen = true;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowDecisionRejectionKind {
    WorkflowIdMismatch,
    StateMismatch,
    TransitionNotAllowed,
    CommandNotAllowed,
    MissingDedupeKey,
    DuplicateCommandDedupeKey,
    ActiveDuplicateCommand,
    LeaseOwnerMismatch,
    LeaseExpired,
    ResourceBudgetUnavailable,
    ReplanLimitExhausted,
    WaitLimitExhausted,
    TerminalReopenDenied,
    RequiredCommandMissing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowDecisionRejection {
    pub kind: WorkflowDecisionRejectionKind,
    pub message: String,
}

impl WorkflowDecisionRejection {
    fn new(kind: WorkflowDecisionRejectionKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl fmt::Display for WorkflowDecisionRejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for WorkflowDecisionRejection {}

#[derive(Debug, Clone)]
pub struct DecisionValidator {
    allowlist: TransitionAllowlist,
}

impl DecisionValidator {
    pub fn new(allowlist: TransitionAllowlist) -> Self {
        Self { allowlist }
    }

    pub fn github_issue_pr() -> Self {
        Self::new(TransitionAllowlist::github_issue_pr_defaults())
    }

    pub fn repo_backlog() -> Self {
        Self::new(TransitionAllowlist::repo_backlog_defaults())
    }

    pub fn validate(
        &self,
        instance: &WorkflowInstance,
        decision: &WorkflowDecision,
        context: &ValidationContext,
    ) -> Result<(), WorkflowDecisionRejection> {
        if decision.workflow_id != instance.id {
            return Err(WorkflowDecisionRejection::new(
                WorkflowDecisionRejectionKind::WorkflowIdMismatch,
                format!(
                    "decision workflow_id '{}' does not match instance '{}'",
                    decision.workflow_id, instance.id
                ),
            ));
        }

        if decision.observed_state != instance.state {
            return Err(WorkflowDecisionRejection::new(
                WorkflowDecisionRejectionKind::StateMismatch,
                format!(
                    "decision observed state '{}' does not match persisted state '{}'",
                    decision.observed_state, instance.state
                ),
            ));
        }

        if instance.is_terminal()
            && decision.next_state != instance.state
            && !context.allow_terminal_reopen
        {
            return Err(WorkflowDecisionRejection::new(
                WorkflowDecisionRejectionKind::TerminalReopenDenied,
                "terminal workflows cannot transition without an explicit recovery override",
            ));
        }

        if let Some(lease) = instance.lease.as_ref() {
            if lease.owner != context.actor {
                return Err(WorkflowDecisionRejection::new(
                    WorkflowDecisionRejectionKind::LeaseOwnerMismatch,
                    format!(
                        "workflow lease owner '{}' does not match actor '{}'",
                        lease.owner, context.actor
                    ),
                ));
            }

            if lease.expires_at <= context.now {
                return Err(WorkflowDecisionRejection::new(
                    WorkflowDecisionRejectionKind::LeaseExpired,
                    "workflow lease expired before decision commit",
                ));
            }
        }

        let Some(rule) = self
            .allowlist
            .rule_for(&decision.observed_state, &decision.next_state)
        else {
            return Err(WorkflowDecisionRejection::new(
                WorkflowDecisionRejectionKind::TransitionNotAllowed,
                format!(
                    "transition '{}' -> '{}' is not allowed",
                    decision.observed_state, decision.next_state
                ),
            ));
        };

        self.validate_commands(rule, decision, context)
    }

    pub fn transition_rules_from<'a>(
        &'a self,
        from_state: &'a str,
    ) -> impl Iterator<Item = &'a TransitionRule> + 'a {
        self.allowlist.rules_from(from_state)
    }

    fn validate_commands(
        &self,
        rule: &TransitionRule,
        decision: &WorkflowDecision,
        context: &ValidationContext,
    ) -> Result<(), WorkflowDecisionRejection> {
        let mut seen_dedupe_keys = BTreeSet::new();

        for command in &decision.commands {
            if !rule.allowed_commands.contains(&command.command_type) {
                return Err(WorkflowDecisionRejection::new(
                    WorkflowDecisionRejectionKind::CommandNotAllowed,
                    format!(
                        "command {:?} is not allowed for this transition",
                        command.command_type
                    ),
                ));
            }

            self.validate_dedupe(command, &mut seen_dedupe_keys, context)?;

            if command.requires_runtime_job() && !context.resource_budget_available {
                return Err(WorkflowDecisionRejection::new(
                    WorkflowDecisionRejectionKind::ResourceBudgetUnavailable,
                    "runtime command requested while resource budget is unavailable",
                ));
            }

            if is_replan_command(command) && !context.replan_available {
                return Err(WorkflowDecisionRejection::new(
                    WorkflowDecisionRejectionKind::ReplanLimitExhausted,
                    "replan command requested after replan budget was exhausted",
                ));
            }

            if command.command_type == WorkflowCommandType::Wait && !context.wait_available {
                return Err(WorkflowDecisionRejection::new(
                    WorkflowDecisionRejectionKind::WaitLimitExhausted,
                    "wait command requested after wait budget was exhausted",
                ));
            }
        }

        if let Some(required_command) =
            required_command_for_transition(&decision.observed_state, &decision.next_state)
        {
            if !decision
                .commands
                .iter()
                .any(|command| command.command_type == required_command)
            {
                return Err(WorkflowDecisionRejection::new(
                    WorkflowDecisionRejectionKind::RequiredCommandMissing,
                    format!(
                        "transition '{}' -> '{}' requires command {:?}",
                        decision.observed_state, decision.next_state, required_command
                    ),
                ));
            }
        }

        if decision.decision == "run_replan" && !context.replan_available {
            return Err(WorkflowDecisionRejection::new(
                WorkflowDecisionRejectionKind::ReplanLimitExhausted,
                "run_replan decision requested after replan budget was exhausted",
            ));
        }

        Ok(())
    }

    fn validate_dedupe(
        &self,
        command: &WorkflowCommand,
        seen_dedupe_keys: &mut BTreeSet<String>,
        context: &ValidationContext,
    ) -> Result<(), WorkflowDecisionRejection> {
        if command.dedupe_key.trim().is_empty() {
            return Err(WorkflowDecisionRejection::new(
                WorkflowDecisionRejectionKind::MissingDedupeKey,
                "workflow commands must include a non-empty dedupe key",
            ));
        }

        if !seen_dedupe_keys.insert(command.dedupe_key.clone()) {
            return Err(WorkflowDecisionRejection::new(
                WorkflowDecisionRejectionKind::DuplicateCommandDedupeKey,
                format!(
                    "decision contains duplicate command dedupe key '{}'",
                    command.dedupe_key
                ),
            ));
        }

        if context.active_dedupe_keys.contains(&command.dedupe_key) {
            return Err(WorkflowDecisionRejection::new(
                WorkflowDecisionRejectionKind::ActiveDuplicateCommand,
                format!(
                    "an active command already owns dedupe key '{}'",
                    command.dedupe_key
                ),
            ));
        }

        Ok(())
    }
}

fn is_replan_command(command: &WorkflowCommand) -> bool {
    command.activity_name() == Some("replan_issue")
}

fn required_command_for_transition(
    from_state: &str,
    to_state: &str,
) -> Option<WorkflowCommandType> {
    match (from_state, to_state) {
        ("implementing", "pr_open") => Some(WorkflowCommandType::BindPr),
        (_, "done") => Some(WorkflowCommandType::MarkDone),
        (_, "blocked") => Some(WorkflowCommandType::MarkBlocked),
        (_, "failed") => Some(WorkflowCommandType::MarkFailed),
        (_, "cancelled") => Some(WorkflowCommandType::MarkCancelled),
        _ => None,
    }
}
