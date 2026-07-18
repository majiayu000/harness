use super::model::{WorkflowCommand, WorkflowCommandType, WorkflowDecision, WorkflowInstance};
use super::validator_progress;
use chrono::{DateTime, Utc};
use std::collections::BTreeSet;
use std::fmt;

#[path = "validator_github_issue_pr.rs"]
mod github_issue_pr_validation;
#[path = "validator_prompt_task.rs"]
mod prompt_task_validation;
#[path = "validator_context.rs"]
mod validation_context;

#[cfg(test)]
#[path = "validator_tests.rs"]
mod tests;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransitionRule {
    pub from_state: Option<String>,
    pub to_state: String,
    pub allowed_commands: BTreeSet<WorkflowCommandType>,
    pub required_command: Option<WorkflowCommandType>,
    pub required_evidence: BTreeSet<String>,
    pub operator_recovery_only: bool,
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
            required_command: None,
            required_evidence: BTreeSet::new(),
            operator_recovery_only: false,
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
            required_command: None,
            required_evidence: BTreeSet::new(),
            operator_recovery_only: false,
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

    pub fn github_issue_pr_defaults() -> Self {
        use WorkflowCommandType::{
            BindPr, EnqueueActivity, MarkBlocked, MarkCancelled, MarkDone, MarkFailed,
            RecordPlanConcern, RequestOperatorAttention, StartChildWorkflow, Wait,
        };

        Self::default()
            .allow("discovered", "awaiting_dependencies", [Wait])
            .allow("failed", "awaiting_dependencies", [Wait])
            .allow("cancelled", "awaiting_dependencies", [Wait])
            .allow("awaiting_dependencies", "awaiting_dependencies", [Wait])
            .allow(
                "awaiting_dependencies",
                "scheduled",
                [EnqueueActivity, Wait],
            )
            .allow("awaiting_dependencies", "planning", [EnqueueActivity, Wait])
            .allow(
                "awaiting_dependencies",
                "implementing",
                [EnqueueActivity, Wait],
            )
            .allow("discovered", "scheduled", [EnqueueActivity, Wait])
            .allow("discovered", "planning", [EnqueueActivity, Wait])
            .allow("discovered", "implementing", [EnqueueActivity, Wait])
            .allow("scheduled", "scheduled", [EnqueueActivity, Wait])
            .allow("failed", "scheduled", [EnqueueActivity, Wait])
            .allow("failed", "planning", [EnqueueActivity, Wait])
            .allow("failed", "implementing", [EnqueueActivity, Wait])
            .allow("failed", "replanning", [EnqueueActivity, Wait])
            .allow("failed", "local_review_gate", [EnqueueActivity, Wait])
            .allow(
                "failed",
                "awaiting_feedback",
                [EnqueueActivity, StartChildWorkflow, Wait],
            )
            .allow(
                "failed",
                "addressing_feedback",
                [EnqueueActivity, StartChildWorkflow, Wait],
            )
            .allow("failed", "merging", [EnqueueActivity])
            .allow("blocked", "implementing", [EnqueueActivity, Wait])
            .allow("blocked", "replanning", [EnqueueActivity, Wait])
            .allow("blocked", "local_review_gate", [EnqueueActivity, Wait])
            .allow(
                "blocked",
                "awaiting_feedback",
                [EnqueueActivity, StartChildWorkflow, Wait],
            )
            .allow(
                "blocked",
                "addressing_feedback",
                [EnqueueActivity, StartChildWorkflow, Wait],
            )
            .allow("blocked", "merging", [EnqueueActivity])
            .allow("cancelled", "scheduled", [EnqueueActivity, Wait])
            .allow("cancelled", "planning", [EnqueueActivity, Wait])
            .allow("cancelled", "implementing", [EnqueueActivity, Wait])
            .allow("scheduled", "planning", [EnqueueActivity, Wait])
            .allow(
                "scheduled",
                "implementing",
                [EnqueueActivity, RecordPlanConcern, Wait],
            )
            .allow(
                "scheduled",
                "replanning",
                [EnqueueActivity, RecordPlanConcern, MarkBlocked, Wait],
            )
            .allow("planning", "implementing", [EnqueueActivity, MarkBlocked])
            .allow("planning", "planning", [EnqueueActivity, Wait])
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
            .allow("implementing", "done", [MarkDone])
            .allow(
                "scheduled",
                "pr_open",
                [BindPr, EnqueueActivity, StartChildWorkflow, Wait],
            )
            .allow("pr_open", "pr_open", [BindPr, Wait])
            .allow("pr_open", "local_review_gate", [EnqueueActivity, Wait])
            .allow("pr_open", "awaiting_feedback", [Wait])
            .allow(
                "local_review_gate",
                "local_review_gate",
                [EnqueueActivity, Wait],
            )
            .allow("local_review_gate", "awaiting_feedback", [Wait])
            .allow(
                "local_review_gate",
                "addressing_feedback",
                [EnqueueActivity, MarkBlocked, Wait],
            )
            .allow("pr_open", "done", [MarkDone])
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
                "local_review_gate",
                [EnqueueActivity, StartChildWorkflow, Wait],
            )
            .allow(
                "awaiting_feedback",
                "quality_gate_pending",
                [StartChildWorkflow, Wait],
            )
            .allow(
                "quality_gate_pending",
                "ready_to_merge",
                std::iter::empty::<WorkflowCommandType>(),
            )
            .allow("awaiting_feedback", "done", [MarkDone])
            .allow("addressing_feedback", "done", [MarkDone])
            .allow("quality_gate_pending", "done", [MarkDone])
            .allow("quality_gate_pending", "quality_gate_pending", [Wait])
            .allow("ready_to_merge", "ready_to_merge", [Wait])
            .allow("ready_to_merge", "merging", [EnqueueActivity])
            .allow("merging", "done", [MarkDone])
            .allow("ready_to_merge", "done", [MarkDone])
            .allow_from_any("blocked", [MarkBlocked, RequestOperatorAttention, Wait])
            .allow_from_any("failed", [MarkFailed])
            .allow_from_any("cancelled", [MarkCancelled])
    }

    pub fn quality_gate_defaults() -> Self {
        use WorkflowCommandType::{
            EnqueueActivity, MarkBlocked, MarkCancelled, MarkFailed, RequestOperatorAttention, Wait,
        };

        Self::default()
            .allow("pending", "checking", [EnqueueActivity, Wait])
            .allow("checking", "checking", [EnqueueActivity, Wait])
            .allow(
                "checking",
                "passed",
                std::iter::empty::<WorkflowCommandType>(),
            )
            .allow_from_any("blocked", [MarkBlocked, RequestOperatorAttention, Wait])
            .allow_from_any("failed", [MarkFailed])
            .allow_from_any("cancelled", [MarkCancelled])
    }

    pub fn pr_feedback_defaults() -> Self {
        use WorkflowCommandType::{
            EnqueueActivity, MarkBlocked, MarkCancelled, MarkFailed, RequestOperatorAttention, Wait,
        };

        Self::default()
            .allow("pending", "inspecting", [EnqueueActivity, Wait])
            .allow("inspecting", "inspecting", [EnqueueActivity, Wait])
            .allow("inspecting", "feedback_found", std::iter::empty())
            .allow("inspecting", "no_actionable_feedback", std::iter::empty())
            .allow("inspecting", "ready_to_merge", std::iter::empty())
            .allow("feedback_found", "done", [Wait])
            .allow("no_actionable_feedback", "done", [Wait])
            .allow("ready_to_merge", "done", [Wait])
            .allow_from_any("blocked", [MarkBlocked, RequestOperatorAttention, Wait])
            .allow_from_any("failed", [MarkFailed])
            .allow_from_any("cancelled", [MarkCancelled])
    }

    pub fn prompt_task_defaults() -> Self {
        use WorkflowCommandType::{
            EnqueueActivity, MarkBlocked, MarkCancelled, MarkDone, MarkFailed,
            RequestOperatorAttention, Wait,
        };

        Self::default()
            .allow("submitted", "awaiting_dependencies", [Wait])
            .allow("failed", "awaiting_dependencies", [Wait])
            .allow("cancelled", "awaiting_dependencies", [Wait])
            .allow("awaiting_dependencies", "awaiting_dependencies", [Wait])
            .allow(
                "awaiting_dependencies",
                "implementing",
                [EnqueueActivity, Wait],
            )
            .allow("submitted", "implementing", [EnqueueActivity, Wait])
            .allow("failed", "implementing", [EnqueueActivity, Wait])
            .allow("cancelled", "implementing", [EnqueueActivity, Wait])
            .allow("implementing", "implementing", [EnqueueActivity])
            .allow("blocked", "awaiting_dependencies", [Wait])
            .allow("blocked", "implementing", [EnqueueActivity, Wait])
            .allow("implementing", "done", [MarkDone])
            .allow_from_any("blocked", [MarkBlocked, RequestOperatorAttention, Wait])
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
    pub allow_missing_pinned_cancel: bool,
    pub active_dedupe_keys: BTreeSet<String>,
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
    InvalidCommandPayload,
    InvalidDecisionContract,
    ProgressDriverMissing,
    MissingTerminalEvidence,
    MissingRequiredEvidence,
    OperatorRecoveryDenied,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowDecisionRejection {
    pub kind: WorkflowDecisionRejectionKind,
    pub message: String,
}

impl WorkflowDecisionRejection {
    pub(super) fn new(kind: WorkflowDecisionRejectionKind, message: impl Into<String>) -> Self {
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
    kind: DecisionValidatorKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DecisionValidatorKind {
    Generic,
    GithubIssuePr,
    PromptTask,
}

impl DecisionValidator {
    pub fn new(allowlist: TransitionAllowlist) -> Self {
        Self {
            allowlist,
            kind: DecisionValidatorKind::Generic,
        }
    }

    pub fn github_issue_pr() -> Self {
        super::state_registry::decision_validator_for_definition(
            super::reducer::GITHUB_ISSUE_PR_DEFINITION_ID,
        )
        .expect("built-in github_issue_pr workflow definition must be registered")
    }

    pub fn quality_gate() -> Self {
        super::state_registry::decision_validator_for_definition(
            super::quality_gate::QUALITY_GATE_DEFINITION_ID,
        )
        .expect("built-in quality_gate workflow definition must be registered")
    }

    pub fn pr_feedback() -> Self {
        super::state_registry::decision_validator_for_definition(
            super::pr_feedback::PR_FEEDBACK_DEFINITION_ID,
        )
        .expect("built-in pr_feedback workflow definition must be registered")
    }

    pub fn prompt_task() -> Self {
        super::state_registry::decision_validator_for_definition(
            super::prompt_task::PROMPT_TASK_DEFINITION_ID,
        )
        .expect("built-in prompt_task workflow definition must be registered")
    }

    pub(crate) fn for_definition(definition_id: &str, allowlist: TransitionAllowlist) -> Self {
        let kind = match definition_id {
            super::reducer::GITHUB_ISSUE_PR_DEFINITION_ID => DecisionValidatorKind::GithubIssuePr,
            super::prompt_task::PROMPT_TASK_DEFINITION_ID => DecisionValidatorKind::PromptTask,
            _ => DecisionValidatorKind::Generic,
        };
        Self { allowlist, kind }
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

        if self.validate_hidden_workflow_transition(instance, decision, context)? {
            return Ok(());
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

        validator_progress::validate_declarative_transition_metadata(rule, decision, context)?;

        if self.kind == DecisionValidatorKind::PromptTask
            && decision.observed_state == "implementing"
            && decision.next_state == "implementing"
        {
            prompt_task_validation::validate_decision(decision)?;
        }
        self.validate_commands(rule, decision, context)?;
        validator_progress::validate_target_progress_contract_with_override(
            instance,
            decision,
            context.allow_missing_pinned_cancel,
        )?;
        self.validate_workflow_specific_rules(decision, context)
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
            let Some(payload) = command.command.as_object() else {
                return Err(WorkflowDecisionRejection::new(
                    WorkflowDecisionRejectionKind::InvalidCommandPayload,
                    "workflow command payload must be a JSON object",
                ));
            };

            if !rule.allowed_commands.contains(&command.command_type) {
                return Err(WorkflowDecisionRejection::new(
                    WorkflowDecisionRejectionKind::CommandNotAllowed,
                    format!(
                        "command {:?} is not allowed for this transition",
                        command.command_type
                    ),
                ));
            }

            self.validate_command_payload(command, payload)?;
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

    fn validate_workflow_specific_rules(
        &self,
        decision: &WorkflowDecision,
        context: &ValidationContext,
    ) -> Result<(), WorkflowDecisionRejection> {
        if self.kind == DecisionValidatorKind::GithubIssuePr {
            github_issue_pr_validation::validate_decision(decision, context)?;
        }
        if self.kind == DecisionValidatorKind::PromptTask
            && !(decision.observed_state == "implementing" && decision.next_state == "implementing")
        {
            prompt_task_validation::validate_decision(decision)?;
        }
        Ok(())
    }

    fn validate_hidden_workflow_transition(
        &self,
        instance: &WorkflowInstance,
        decision: &WorkflowDecision,
        context: &ValidationContext,
    ) -> Result<bool, WorkflowDecisionRejection> {
        if self.kind != DecisionValidatorKind::GithubIssuePr
            || !github_issue_pr_validation::is_reconciliation_only_done_transition(decision)
        {
            return Ok(false);
        }

        let rule = TransitionRule::new(
            decision.observed_state.as_str(),
            "done",
            [WorkflowCommandType::MarkDone],
        );
        self.validate_commands(&rule, decision, context)?;
        validator_progress::validate_target_progress_contract(instance, decision)?;
        github_issue_pr_validation::validate_reconciliation_only_done(decision, context)?;
        Ok(true)
    }

    fn validate_command_payload(
        &self,
        command: &WorkflowCommand,
        payload: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<(), WorkflowDecisionRejection> {
        if command.command_type != WorkflowCommandType::BindPr {
            return Ok(());
        }

        if payload
            .get("pr_number")
            .and_then(serde_json::Value::as_u64)
            .is_none()
        {
            return Err(WorkflowDecisionRejection::new(
                WorkflowDecisionRejectionKind::InvalidCommandPayload,
                "BindPr command must include numeric pr_number",
            ));
        }

        if payload
            .get("pr_url")
            .and_then(serde_json::Value::as_str)
            .is_none_or(|value| value.trim().is_empty())
        {
            return Err(WorkflowDecisionRejection::new(
                WorkflowDecisionRejectionKind::InvalidCommandPayload,
                "BindPr command must include non-empty pr_url",
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
        (from_state, "pr_open") if from_state != "pr_open" => Some(WorkflowCommandType::BindPr),
        ("idle", "scanning") => Some(WorkflowCommandType::EnqueueActivity),
        ("scanning", "planning_batch") => Some(WorkflowCommandType::EnqueueActivity),
        ("planning_batch", "dispatching") => Some(WorkflowCommandType::StartChildWorkflow),
        (_, "done") => Some(WorkflowCommandType::MarkDone),
        (_, "blocked") => Some(WorkflowCommandType::MarkBlocked),
        (_, "failed") => Some(WorkflowCommandType::MarkFailed),
        (_, "cancelled") => Some(WorkflowCommandType::MarkCancelled),
        _ => None,
    }
}
