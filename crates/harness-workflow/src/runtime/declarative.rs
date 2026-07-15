use super::{
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclarativeWorkflowDefinition {
    pub registered: RegisteredWorkflowDefinition,
    pub policy: WorkflowDefinitionPolicy,
}

impl DeclarativeWorkflowDefinition {
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

    Ok(DeclarativeWorkflowDefinition {
        registered: RegisteredWorkflowDefinition::new(&policy.id, states, allowlist),
        policy: policy.clone(),
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

#[cfg(test)]
mod validation {
    use super::*;

    fn activity_state(on_success: impl Into<String>) -> DeclaredState {
        DeclaredState {
            activity: Some("implement".to_string()),
            on_success: Some(on_success.into()),
            on_failure: Some("failed".to_string()),
            on_signal: BTreeMap::from([("cancel".to_string(), "cancelled".to_string())]),
            ..DeclaredState::default()
        }
    }

    fn valid_policy() -> WorkflowDefinitionPolicy {
        WorkflowDefinitionPolicy {
            id: "docs_review".to_string(),
            initial: "reviewing".to_string(),
            states: BTreeMap::from([
                (
                    "blocked".to_string(),
                    DeclaredState {
                        progress: Some(DeclaredProgressMode::OperatorGate),
                        ..DeclaredState::default()
                    },
                ),
                ("reviewing".to_string(), activity_state("done")),
            ]),
            terminal: BTreeMap::from([
                ("cancelled".to_string(), "cancelled".to_string()),
                ("done".to_string(), "succeeded".to_string()),
                ("failed".to_string(), "failed".to_string()),
            ]),
            evidence_required: BTreeMap::from([(
                "done".to_string(),
                vec!["review_report".to_string()],
            )]),
            recovery_targets: vec!["reviewing".to_string()],
        }
    }

    fn activity_policies() -> BTreeMap<String, WorkflowActivityPolicy> {
        BTreeMap::from([("implement".to_string(), WorkflowActivityPolicy::default())])
    }

    fn error_for(policy: &WorkflowDefinitionPolicy) -> String {
        build_declarative_definition(policy, &activity_policies())
            .expect_err("declaration should be invalid")
            .to_string()
    }

    #[test]
    fn compiles_valid_definition_without_registration_side_effects() {
        let policy = valid_policy();
        let compiled = build_declarative_definition(&policy, &activity_policies())
            .expect("valid declaration should compile");

        assert_eq!(compiled.registered.id, "docs_review");
        assert_eq!(compiled.policy, policy);
        assert_eq!(compiled.registered.states.len(), 5);
        assert_eq!(
            compiled
                .registered
                .allowlist
                .rule_for("reviewing", "done")
                .expect("declared transition should be compiled")
                .allowed_commands,
            BTreeSet::from([WorkflowCommandType::MarkDone])
        );
        assert_eq!(
            compiled
                .registered
                .allowlist
                .rule_for("reviewing", "blocked")
                .expect("blocked fallback should be compiled")
                .allowed_commands,
            BTreeSet::from([
                WorkflowCommandType::MarkBlocked,
                WorkflowCommandType::Wait,
                WorkflowCommandType::RequestOperatorAttention,
            ])
        );
        assert!(super::super::state_registry::workflow_definition("docs_review").is_none());
    }

    #[test]
    fn rejects_builtin_id_collision() {
        let mut policy = valid_policy();
        policy.id = PROMPT_TASK_DEFINITION_ID.to_string();
        assert!(error_for(&policy).contains("collides with a built-in definition"));
    }

    #[test]
    fn rejects_empty_declarations_and_invalid_initial_state() {
        let mut empty_active = valid_policy();
        empty_active.states.clear();
        assert!(error_for(&empty_active).contains("at least one active state"));

        let mut empty_terminal = valid_policy();
        empty_terminal.terminal.clear();
        assert!(error_for(&empty_terminal).contains("must declare terminal states"));

        let mut missing_initial = valid_policy();
        missing_initial.initial = "missing".to_string();
        assert!(error_for(&missing_initial).contains("is not declared as an active state"));

        let mut terminal_initial = valid_policy();
        terminal_initial.initial = "done".to_string();
        assert!(error_for(&terminal_initial).contains("must be active, not terminal"));
    }

    #[test]
    fn rejects_namespace_overlap_and_invalid_blocked_state() {
        let mut overlap = valid_policy();
        overlap
            .terminal
            .insert("blocked".to_string(), "succeeded".to_string());
        overlap
            .terminal
            .insert("done".to_string(), "failed".to_string());
        overlap
            .terminal
            .insert("failed".to_string(), "cancelled".to_string());
        overlap.terminal.remove("cancelled");
        assert!(error_for(&overlap).contains("both active and terminal namespaces"));

        let mut missing_blocked = valid_policy();
        missing_blocked.states.remove("blocked");
        assert!(error_for(&missing_blocked).contains("must declare active state 'blocked'"));

        let mut wrong_blocked = valid_policy();
        wrong_blocked.states.insert(
            "blocked".to_string(),
            DeclaredState {
                progress: Some(DeclaredProgressMode::ExternalWait),
                ..DeclaredState::default()
            },
        );
        assert!(error_for(&wrong_blocked).contains("progress 'operator_gate'"));
    }

    #[test]
    fn rejects_incomplete_or_duplicate_terminal_mapping() {
        let mut missing = valid_policy();
        missing.terminal.remove("failed");
        assert!(error_for(&missing).contains("missing terminal class 'failed'"));

        let mut duplicate = valid_policy();
        duplicate
            .terminal
            .insert("also_done".to_string(), "succeeded".to_string());
        assert!(error_for(&duplicate).contains("terminal class 'succeeded' is assigned to both"));

        let mut unknown = valid_policy();
        unknown
            .terminal
            .insert("failed".to_string(), "aborted".to_string());
        assert!(error_for(&unknown).contains("unknown class 'aborted'"));
    }

    #[test]
    fn rejects_missing_or_dual_progress_mode_and_unknown_activity() {
        let mut missing = valid_policy();
        missing
            .states
            .insert("reviewing".to_string(), DeclaredState::default());
        assert!(error_for(&missing).contains("exactly one of activity or progress"));

        let mut dual = valid_policy();
        dual.states.get_mut("reviewing").unwrap().progress =
            Some(DeclaredProgressMode::ExternalWait);
        assert!(error_for(&dual).contains("exactly one of activity or progress"));

        let mut unknown_activity = valid_policy();
        unknown_activity
            .states
            .get_mut("reviewing")
            .unwrap()
            .activity = Some("missing_policy".to_string());
        assert!(
            error_for(&unknown_activity).contains("absent from the workflow activities policy map")
        );
    }

    #[test]
    fn rejects_undeclared_transition_and_evidence_targets() {
        let mut transition = valid_policy();
        transition.states.get_mut("reviewing").unwrap().on_failure = Some("missing".to_string());
        let error = error_for(&transition);
        assert!(
            error.contains("active state 'reviewing' on_failure target 'missing' is undeclared")
        );

        let mut evidence = valid_policy();
        evidence
            .evidence_required
            .insert("missing".to_string(), vec!["report".to_string()]);
        assert!(
            error_for(&evidence).contains("evidence requirement target 'missing' is undeclared")
        );
    }

    #[test]
    fn rejects_invalid_recovery_targets() {
        let mut terminal = valid_policy();
        terminal.recovery_targets = vec!["done".to_string()];
        assert!(error_for(&terminal).contains("must name an active state, but it is terminal"));

        let mut missing = valid_policy();
        missing.recovery_targets = vec!["missing".to_string()];
        assert!(error_for(&missing).contains("must name an active state, but it is undeclared"));

        let mut duplicate = valid_policy();
        duplicate.recovery_targets = vec!["reviewing".to_string(), "reviewing".to_string()];
        assert!(error_for(&duplicate).contains("is declared more than once"));
    }

    #[test]
    fn rejects_unreachable_active_and_terminal_states() {
        let mut active = valid_policy();
        active.states.insert(
            "orphan".to_string(),
            DeclaredState {
                progress: Some(DeclaredProgressMode::ExternalWait),
                ..DeclaredState::default()
            },
        );
        assert!(error_for(&active)
            .contains("states unreachable from initial state 'reviewing': orphan"));

        let mut terminal = valid_policy();
        terminal
            .terminal
            .insert("unused".to_string(), "succeeded".to_string());
        terminal.terminal.remove("done");
        terminal.states.get_mut("reviewing").unwrap().on_success = Some("failed".to_string());
        terminal.evidence_required =
            BTreeMap::from([("unused".to_string(), vec!["review_report".to_string()])]);
        let error = error_for(&terminal);
        assert!(
            error.contains("states unreachable from initial state 'reviewing': unused"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn compiles_deterministic_signal_edges_and_target_driver_commands() {
        let mut policy = valid_policy();
        policy.states.insert(
            "waiting".to_string(),
            DeclaredState {
                progress: Some(DeclaredProgressMode::ExternalWait),
                on_success: Some("failed".to_string()),
                ..DeclaredState::default()
            },
        );
        let reviewing = policy.states.get_mut("reviewing").unwrap();
        reviewing.on_success = Some("waiting".to_string());
        reviewing.on_blocked = Some("blocked".to_string());
        reviewing
            .on_signal
            .insert("approved".to_string(), "done".to_string());

        let compiled = build_declarative_definition(&policy, &activity_policies()).unwrap();
        let allowlist = &compiled.registered.allowlist;
        assert_eq!(
            allowlist
                .rule_for("reviewing", "waiting")
                .unwrap()
                .allowed_commands,
            BTreeSet::from([WorkflowCommandType::Wait])
        );
        assert_eq!(
            allowlist
                .rule_for("reviewing", "done")
                .unwrap()
                .allowed_commands,
            BTreeSet::from([WorkflowCommandType::MarkDone])
        );
        assert_eq!(
            allowlist
                .rule_for("reviewing", "blocked")
                .unwrap()
                .allowed_commands,
            BTreeSet::from([
                WorkflowCommandType::MarkBlocked,
                WorkflowCommandType::RequestOperatorAttention,
                WorkflowCommandType::Wait,
            ])
        );
    }
}
