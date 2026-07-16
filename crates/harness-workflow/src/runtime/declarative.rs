use super::{
    declarative_pinning::declarative_definition_identity,
    model::{
        ActivityArtifact, WorkflowCommand, WorkflowCommandType, WorkflowDecision, WorkflowEvidence,
        WorkflowInstance,
    },
    pr_feedback::PR_FEEDBACK_DEFINITION_ID,
    prompt_task::PROMPT_TASK_DEFINITION_ID,
    quality_gate::QUALITY_GATE_DEFINITION_ID,
    reducer::GITHUB_ISSUE_PR_DEFINITION_ID,
    state_registry::{
        RegisteredWorkflowDefinition, WorkflowProgressMode, WorkflowStateDefinition,
        WorkflowTerminalState,
    },
    validator::{TransitionAllowlist, TransitionRule},
};
use harness_core::config::workflow::{
    DeclaredProgressMode, DeclaredState, WorkflowActivityPolicy, WorkflowDefinitionPolicy,
};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde_json::json;

/// A structurally validated declaration and the registry definition compiled from it.
///
/// The policy remains attached because the generic interpreter needs routing, evidence,
/// activity, and recovery metadata that `RegisteredWorkflowDefinition` does not retain.
///
/// ```compile_fail
/// use harness_workflow::runtime::DeclarativeWorkflowDefinition;
///
/// fn mutate_compiled_states(definition: &mut DeclarativeWorkflowDefinition) {
///     definition.registered.states.clear();
/// }
/// ```
///
/// ```compile_fail
/// use harness_workflow::runtime::DeclarativeWorkflowDefinition;
///
/// fn forge_identity(definition: &mut DeclarativeWorkflowDefinition) {
///     definition.definition_hash = "sha256:forged".to_string();
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclarativeWorkflowDefinition {
    registered: RegisteredWorkflowDefinition,
    policy: WorkflowDefinitionPolicy,
    definition_version: u32,
    definition_hash: String,
}

impl DeclarativeWorkflowDefinition {
    pub fn registered(&self) -> &RegisteredWorkflowDefinition {
        &self.registered
    }

    pub fn policy(&self) -> &WorkflowDefinitionPolicy {
        &self.policy
    }

    pub fn definition_version(&self) -> u32 {
        self.definition_version
    }

    pub fn definition_hash(&self) -> &str {
        &self.definition_hash
    }

    pub fn into_registered(self) -> RegisteredWorkflowDefinition {
        self.registered
    }
}

/// Validates and compiles a declarative workflow without registering it.
pub fn build_declarative_definition(
    policy: &WorkflowDefinitionPolicy,
    activity_policies: &BTreeMap<String, WorkflowActivityPolicy>,
) -> anyhow::Result<DeclarativeWorkflowDefinition> {
    validate_top_level(policy)?;

    let terminal_states = parse_terminal_states(policy)?;
    validate_active_states(policy, activity_policies)?;
    validate_targets(policy, &terminal_states)?;
    validate_reachability(policy, &terminal_states)?;

    let states = compile_states(policy, &terminal_states);
    let allowlist = compile_allowlist(policy, &terminal_states);
    let (definition_version, definition_hash) = declarative_definition_identity(policy)?;

    Ok(DeclarativeWorkflowDefinition {
        registered: RegisteredWorkflowDefinition::new(&policy.id, states, allowlist),
        policy: policy.clone(),
        definition_version,
        definition_hash,
    })
}

/// Builds the audited self-transition that starts a declarative workflow.
///
/// The instance must already be pinned to this exact declaration. The command
/// uses a submission-scoped dedupe key because no activity completion event
/// exists yet.
pub fn build_declarative_submission_decision(
    definition: &DeclarativeWorkflowDefinition,
    instance: &WorkflowInstance,
) -> anyhow::Result<WorkflowDecision> {
    let policy = definition.policy();
    if instance.definition_id != policy.id
        || instance.definition_version != definition.definition_version()
        || instance
            .data
            .get("definition_hash")
            .and_then(serde_json::Value::as_str)
            != Some(definition.definition_hash())
    {
        anyhow::bail!(
            "declarative workflow instance '{}' is not pinned to definition '{}@{}'",
            instance.id,
            policy.id,
            definition.definition_version()
        );
    }
    if instance.state != policy.initial {
        anyhow::bail!(
            "declarative workflow instance '{}' must start in initial state '{}', got '{}'",
            instance.id,
            policy.initial,
            instance.state
        );
    }
    let initial = policy.states.get(&policy.initial).ok_or_else(|| {
        anyhow::anyhow!(
            "declarative workflow definition '{}' has no active initial state '{}'",
            policy.id,
            policy.initial
        )
    })?;
    let dedupe_key = format!("{}:{}:submit", instance.id, policy.initial);
    let command = if let Some(activity) = initial.activity.as_deref() {
        WorkflowCommand::enqueue_activity(activity, dedupe_key)
    } else {
        match initial.progress {
            Some(DeclaredProgressMode::ExternalWait) => WorkflowCommand::wait(
                format!(
                    "declarative workflow '{}' entered initial external wait state '{}'",
                    policy.id, policy.initial
                ),
                dedupe_key,
            ),
            Some(DeclaredProgressMode::OperatorGate) => WorkflowCommand::new(
                WorkflowCommandType::RequestOperatorAttention,
                dedupe_key,
                json!({
                    "reason": "declarative workflow entered its initial operator gate",
                    "definition_id": policy.id,
                    "target_state": policy.initial,
                }),
            ),
            None => anyhow::bail!(
                "declarative workflow definition '{}' initial state '{}' has no progress driver",
                policy.id,
                policy.initial
            ),
        }
    };

    Ok(WorkflowDecision::new(
        &instance.id,
        &instance.state,
        "submit_declarative_workflow",
        &instance.state,
        format!(
            "submitted declarative workflow '{}' in initial state '{}'",
            policy.id, policy.initial
        ),
    )
    .with_command(command)
    .high_confidence())
}

/// Converts completed activity artifacts into exact, case-sensitive evidence kinds.
///
/// Signals, summaries, and the agent-authored workflow decision artifact are not evidence.
pub fn workflow_evidence_from_activity_artifacts(
    artifacts: &[ActivityArtifact],
) -> anyhow::Result<Vec<WorkflowEvidence>> {
    let mut evidence = Vec::new();
    for artifact in artifacts {
        if artifact.artifact_type.trim().is_empty() {
            anyhow::bail!(
                "activity artifact_type must not be empty when deriving workflow evidence"
            );
        }
        if artifact.artifact_type == "workflow_decision" {
            continue;
        }
        evidence.push(WorkflowEvidence::new(
            artifact.artifact_type.clone(),
            format!("activity produced '{}' artifact", artifact.artifact_type),
        ));
    }
    Ok(evidence)
}

fn validate_top_level(policy: &WorkflowDefinitionPolicy) -> anyhow::Result<()> {
    if policy.id.trim().is_empty() {
        anyhow::bail!("declarative workflow definition id must not be empty");
    }
    if [
        GITHUB_ISSUE_PR_DEFINITION_ID,
        PROMPT_TASK_DEFINITION_ID,
        QUALITY_GATE_DEFINITION_ID,
        PR_FEEDBACK_DEFINITION_ID,
    ]
    .contains(&policy.id.as_str())
    {
        anyhow::bail!(
            "declarative workflow definition id '{}' collides with a built-in definition",
            policy.id
        );
    }
    if policy.states.is_empty() {
        anyhow::bail!(
            "declarative workflow definition '{}' must declare at least one active state",
            policy.id
        );
    }
    if policy.terminal.is_empty() {
        anyhow::bail!(
            "declarative workflow definition '{}' must declare terminal states",
            policy.id
        );
    }
    if policy.initial.trim().is_empty() {
        anyhow::bail!(
            "declarative workflow definition '{}' initial state must not be empty",
            policy.id
        );
    }
    if policy.terminal.contains_key(&policy.initial) {
        anyhow::bail!(
            "declarative workflow definition '{}' initial state '{}' must be active, not terminal",
            policy.id,
            policy.initial
        );
    }
    if !policy.states.contains_key(&policy.initial) {
        anyhow::bail!(
            "declarative workflow definition '{}' initial state '{}' is not declared as an active state",
            policy.id,
            policy.initial
        );
    }

    if let Some(overlap) = policy
        .states
        .keys()
        .find(|state| policy.terminal.contains_key(*state))
    {
        anyhow::bail!(
            "declarative workflow definition '{}' state '{}' is declared in both active and terminal namespaces",
            policy.id,
            overlap
        );
    }

    let blocked = policy.states.get("blocked").ok_or_else(|| {
        anyhow::anyhow!(
            "declarative workflow definition '{}' must declare active state 'blocked' with progress 'operator_gate' for runtime fallbacks",
            policy.id
        )
    })?;
    if blocked.activity.is_some() || blocked.progress != Some(DeclaredProgressMode::OperatorGate) {
        anyhow::bail!(
            "declarative workflow definition '{}' state 'blocked' must declare exactly progress 'operator_gate' and no activity",
            policy.id
        );
    }
    if !transition_targets(blocked).is_empty() {
        anyhow::bail!(
            "declarative workflow definition '{}' state 'blocked' must not declare outgoing routes; recovery_targets are operator-authorized separately",
            policy.id
        );
    }

    Ok(())
}

fn parse_terminal_states(
    policy: &WorkflowDefinitionPolicy,
) -> anyhow::Result<BTreeMap<String, WorkflowTerminalState>> {
    let mut parsed = BTreeMap::new();
    let mut owners = BTreeMap::<&str, String>::new();

    for (state, class) in &policy.terminal {
        if state.trim().is_empty() {
            anyhow::bail!(
                "declarative workflow definition '{}' has an empty terminal state name",
                policy.id
            );
        }
        let (class_name, terminal_state) = match class.as_str() {
            "succeeded" => ("succeeded", WorkflowTerminalState::Succeeded),
            "failed" => ("failed", WorkflowTerminalState::Failed),
            "cancelled" => ("cancelled", WorkflowTerminalState::Cancelled),
            _ => {
                anyhow::bail!(
                    "declarative workflow definition '{}' terminal state '{}' has unknown class '{}'; expected succeeded, failed, or cancelled",
                    policy.id,
                    state,
                    class
                )
            }
        };
        if let Some(existing) = owners.insert(class_name, state.clone()) {
            anyhow::bail!(
                "declarative workflow definition '{}' terminal class '{}' is assigned to both '{}' and '{}'; exactly one state is required",
                policy.id,
                class,
                existing,
                state
            );
        }
        parsed.insert(state.clone(), terminal_state);
    }

    for class in ["succeeded", "failed", "cancelled"] {
        if !owners.contains_key(class) {
            anyhow::bail!(
                "declarative workflow definition '{}' is missing terminal class '{}'; exactly one state is required",
                policy.id,
                class
            );
        }
    }

    Ok(parsed)
}

fn validate_active_states(
    policy: &WorkflowDefinitionPolicy,
    activity_policies: &BTreeMap<String, WorkflowActivityPolicy>,
) -> anyhow::Result<()> {
    for (state_name, state) in &policy.states {
        if state_name.trim().is_empty() {
            anyhow::bail!(
                "declarative workflow definition '{}' has an empty active state name",
                policy.id
            );
        }
        if state.activity.is_some() == state.progress.is_some() {
            anyhow::bail!(
                "declarative workflow definition '{}' active state '{}' must declare exactly one of activity or progress",
                policy.id,
                state_name
            );
        }
        if let Some(activity) = state.activity.as_deref() {
            if activity.trim().is_empty() {
                anyhow::bail!(
                    "declarative workflow definition '{}' active state '{}' has an empty activity name",
                    policy.id,
                    state_name
                );
            }
            if !activity_policies.contains_key(activity) {
                anyhow::bail!(
                    "declarative workflow definition '{}' active state '{}' references activity '{}' absent from the workflow activities policy map",
                    policy.id,
                    state_name,
                    activity
                );
            }
        }
        if let Some(signal_type) = state
            .on_signal
            .keys()
            .find(|signal_type| signal_type.trim().is_empty())
        {
            anyhow::bail!(
                "declarative workflow definition '{}' active state '{}' has an empty signal type '{}', which is not deterministic",
                policy.id,
                state_name,
                signal_type
            );
        }
    }
    Ok(())
}

fn validate_targets(
    policy: &WorkflowDefinitionPolicy,
    terminal_states: &BTreeMap<String, WorkflowTerminalState>,
) -> anyhow::Result<()> {
    let is_declared =
        |state: &str| policy.states.contains_key(state) || terminal_states.contains_key(state);

    for (state_name, state) in &policy.states {
        for (route, target) in transition_targets(state) {
            if !is_declared(target) {
                anyhow::bail!(
                    "declarative workflow definition '{}' active state '{}' {} target '{}' is undeclared",
                    policy.id,
                    state_name,
                    route,
                    target
                );
            }
        }
    }

    for evidence_target in policy.evidence_required.keys() {
        if evidence_target == "blocked" {
            anyhow::bail!(
                "declarative workflow definition '{}' must not require evidence for safety fallback state 'blocked'",
                policy.id
            );
        }
        if !is_declared(evidence_target) {
            anyhow::bail!(
                "declarative workflow definition '{}' evidence requirement target '{}' is undeclared",
                policy.id,
                evidence_target
            );
        }
    }
    for (target, evidence_kinds) in &policy.evidence_required {
        let mut seen = BTreeSet::new();
        for evidence_kind in evidence_kinds {
            if evidence_kind.trim().is_empty() {
                anyhow::bail!(
                    "declarative workflow definition '{}' evidence requirement target '{}' contains an empty evidence kind",
                    policy.id,
                    target
                );
            }
            if !seen.insert(evidence_kind) {
                anyhow::bail!(
                    "declarative workflow definition '{}' evidence requirement target '{}' repeats evidence kind '{}'",
                    policy.id,
                    target,
                    evidence_kind
                );
            }
        }
    }

    let mut recovery_targets = BTreeSet::new();
    for target in &policy.recovery_targets {
        if !recovery_targets.insert(target) {
            anyhow::bail!(
                "declarative workflow definition '{}' recovery target '{}' is declared more than once",
                policy.id,
                target
            );
        }
        if !policy.states.contains_key(target) {
            let kind = if terminal_states.contains_key(target) {
                "terminal"
            } else {
                "undeclared"
            };
            anyhow::bail!(
                "declarative workflow definition '{}' recovery target '{}' must name an active state, but it is {}",
                policy.id,
                target,
                kind
            );
        }
        if target == "blocked" {
            anyhow::bail!(
                "declarative workflow definition '{}' recovery target '{}' must differ from 'blocked'",
                policy.id,
                target
            );
        }
    }

    Ok(())
}

fn validate_reachability(
    policy: &WorkflowDefinitionPolicy,
    terminal_states: &BTreeMap<String, WorkflowTerminalState>,
) -> anyhow::Result<()> {
    let mut reachable = BTreeSet::new();
    let mut pending = VecDeque::from([policy.initial.as_str()]);

    while let Some(state_name) = pending.pop_front() {
        if !reachable.insert(state_name) {
            continue;
        }
        let Some(state) = policy.states.get(state_name) else {
            continue;
        };
        for (_, target) in transition_targets(state) {
            pending.push_back(target);
        }
        // Any active state can enter the runtime's invalid-output blocked fallback.
        pending.push_back("blocked");
        if state_name == "blocked" {
            pending.extend(policy.recovery_targets.iter().map(String::as_str));
        }
    }

    let unreachable = policy
        .states
        .keys()
        .chain(terminal_states.keys())
        .filter(|state| !reachable.contains(state.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    if !unreachable.is_empty() {
        anyhow::bail!(
            "declarative workflow definition '{}' has states unreachable from initial state '{}': {}",
            policy.id,
            policy.initial,
            unreachable.join(", ")
        );
    }

    Ok(())
}

fn compile_states(
    policy: &WorkflowDefinitionPolicy,
    terminal_states: &BTreeMap<String, WorkflowTerminalState>,
) -> Vec<WorkflowStateDefinition> {
    let mut states = policy
        .states
        .iter()
        .map(|(name, state)| {
            let progress_mode = match state.progress {
                Some(DeclaredProgressMode::ExternalWait) => WorkflowProgressMode::ExternalWait,
                Some(DeclaredProgressMode::OperatorGate) => WorkflowProgressMode::OperatorGate,
                None => WorkflowProgressMode::CommandDriven,
            };
            WorkflowStateDefinition::active(policy.id.as_str(), name.as_str(), progress_mode)
        })
        .collect::<Vec<_>>();
    states.extend(terminal_states.iter().map(|(name, terminal_state)| {
        WorkflowStateDefinition::terminal(policy.id.as_str(), name.as_str(), *terminal_state)
    }));
    states
}

fn compile_allowlist(
    policy: &WorkflowDefinitionPolicy,
    terminal_states: &BTreeMap<String, WorkflowTerminalState>,
) -> TransitionAllowlist {
    let mut edges = BTreeSet::new();
    for (source, state) in &policy.states {
        for (_, target) in transition_targets(state) {
            edges.insert((source.as_str(), target));
        }
        // Submission starts in the declared initial state and emits that
        // state's driver as an audited self-transition. Activity states also
        // retain their existing validator-controlled retry self-transition.
        if source == &policy.initial || state.activity.is_some() {
            edges.insert((source.as_str(), source.as_str()));
        }
    }
    edges.extend(
        policy
            .recovery_targets
            .iter()
            .map(|target| ("blocked", target.as_str())),
    );

    let mut rules = edges
        .into_iter()
        .map(|(source, target)| {
            let mut rule = TransitionRule::new(
                source,
                target,
                allowed_commands_for_target(policy, terminal_states, target),
            );
            rule.required_command =
                Some(required_command_for_target(policy, terminal_states, target));
            if target != "blocked" {
                rule.required_evidence = policy
                    .evidence_required
                    .get(target)
                    .into_iter()
                    .flatten()
                    .cloned()
                    .collect();
            }
            rule.operator_recovery_only = source == "blocked"
                && policy
                    .recovery_targets
                    .iter()
                    .any(|recovery_target| recovery_target == target);
            rule
        })
        .collect::<Vec<_>>();
    rules.push(TransitionRule::from_any(
        "blocked",
        [
            WorkflowCommandType::MarkBlocked,
            WorkflowCommandType::RequestOperatorAttention,
            WorkflowCommandType::Wait,
        ],
    ));
    TransitionAllowlist::new(rules)
}

fn required_command_for_target(
    policy: &WorkflowDefinitionPolicy,
    terminal_states: &BTreeMap<String, WorkflowTerminalState>,
    target: &str,
) -> WorkflowCommandType {
    if let Some(terminal_state) = terminal_states.get(target) {
        return match terminal_state {
            WorkflowTerminalState::Succeeded => WorkflowCommandType::MarkDone,
            WorkflowTerminalState::Failed => WorkflowCommandType::MarkFailed,
            WorkflowTerminalState::Cancelled => WorkflowCommandType::MarkCancelled,
        };
    }
    let state = &policy.states[target];
    if state.activity.is_some() {
        WorkflowCommandType::EnqueueActivity
    } else {
        match state.progress {
            Some(DeclaredProgressMode::ExternalWait) => WorkflowCommandType::Wait,
            Some(DeclaredProgressMode::OperatorGate) => {
                WorkflowCommandType::RequestOperatorAttention
            }
            None => unreachable!("validated active state must declare a progress driver"),
        }
    }
}

fn allowed_commands_for_target(
    policy: &WorkflowDefinitionPolicy,
    terminal_states: &BTreeMap<String, WorkflowTerminalState>,
    target: &str,
) -> Vec<WorkflowCommandType> {
    if target == "blocked" {
        return vec![
            WorkflowCommandType::MarkBlocked,
            WorkflowCommandType::RequestOperatorAttention,
            WorkflowCommandType::Wait,
        ];
    }
    if let Some(terminal_state) = terminal_states.get(target) {
        return vec![match terminal_state {
            WorkflowTerminalState::Succeeded => WorkflowCommandType::MarkDone,
            WorkflowTerminalState::Failed => WorkflowCommandType::MarkFailed,
            WorkflowTerminalState::Cancelled => WorkflowCommandType::MarkCancelled,
        }];
    }

    let state = &policy.states[target];
    if state.activity.is_some() {
        vec![WorkflowCommandType::EnqueueActivity]
    } else {
        match state.progress {
            Some(DeclaredProgressMode::ExternalWait) => vec![WorkflowCommandType::Wait],
            Some(DeclaredProgressMode::OperatorGate) => {
                vec![WorkflowCommandType::RequestOperatorAttention]
            }
            None => unreachable!("active-state progress contract was validated"),
        }
    }
}

fn transition_targets(state: &DeclaredState) -> Vec<(&str, &str)> {
    let mut targets = Vec::new();
    if let Some(target) = state.on_success.as_deref() {
        targets.push(("on_success", target));
    }
    if let Some(target) = state.on_failure.as_deref() {
        targets.push(("on_failure", target));
    }
    if let Some(target) = state.on_blocked.as_deref() {
        targets.push(("on_blocked", target));
    }
    targets.extend(
        state
            .on_signal
            .iter()
            .map(|(signal, target)| (signal.as_str(), target.as_str())),
    );
    targets
}
