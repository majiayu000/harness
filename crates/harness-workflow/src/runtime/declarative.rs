use super::{
    declarative_pinning::declarative_definition_identity,
    model::WorkflowCommandType,
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
        if !is_declared(evidence_target) {
            anyhow::bail!(
                "declarative workflow definition '{}' evidence requirement target '{}' is undeclared",
                policy.id,
                evidence_target
            );
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
    }

    let mut rules = edges
        .into_iter()
        .map(|(source, target)| {
            TransitionRule::new(
                source,
                target,
                allowed_commands_for_target(policy, terminal_states, target),
            )
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
