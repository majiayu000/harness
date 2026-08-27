use super::{
    DeclarativeDefinitionPinError, DeclarativeDefinitionResolution, DeclarativeWorkflowDefinition,
    RegisteredWorkflowDefinition, WorkflowDefinitionRegistry, WorkflowStateDefinition,
    GITHUB_ISSUE_PR_DEFINITION_ID,
};
use crate::runtime::declarative::build_builtin_declarative_definition;
use crate::runtime::declarative_pinning::declarative_definition_identity_with_classifier_policies;
use crate::runtime::model::WorkflowInstance;
use crate::runtime::plan_issue::ISSUE_PLAN_ACTIVITY;
use crate::runtime::pr_feedback::LOCAL_REVIEW_ACTIVITY;
use crate::runtime::reducer::{ISSUE_ALREADY_RESOLVED_SIGNAL, ISSUE_CLOSED_SIGNAL};
use crate::runtime::validator::TransitionAllowlist;
use harness_core::config::workflow::{
    DeclaredProgressMode, DeclaredState, WorkflowActivityPolicy, WorkflowDefinitionPolicy,
};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

impl WorkflowDefinitionRegistry {
    pub fn register_declarative_current(
        &mut self,
        definition: DeclarativeWorkflowDefinition,
    ) -> anyhow::Result<()> {
        self.ensure_mutable(&definition.policy().id)?;
        if self.definitions.contains_key(&definition.policy().id) {
            anyhow::bail!(
                "workflow definition '{}' is already registered as current",
                definition.policy().id
            );
        }
        let version_key = (
            definition.policy().id.clone(),
            definition.definition_version(),
        );
        let versioned = self.checked_version_entry(version_key.clone(), definition)?;
        self.validate_declarative_definition(&versioned)?;
        if !self.definition_ids.contains(&version_key.0) {
            self.definition_ids.push(version_key.0.clone());
        }
        self.definitions.insert(
            version_key.0.clone(),
            Arc::new(versioned.registered().clone()),
        );
        self.current_declarative_versions
            .insert(version_key.0.clone(), version_key.1);
        self.declarative_versions.insert(version_key, versioned);
        Ok(())
    }

    pub fn register_declarative_current_batch(
        &mut self,
        definitions: impl IntoIterator<Item = DeclarativeWorkflowDefinition>,
    ) -> anyhow::Result<()> {
        let mut staged = self.clone();
        for definition in definitions {
            staged.register_declarative_current(definition)?;
        }
        *self = staged;
        Ok(())
    }

    pub fn register_declarative_historical(
        &mut self,
        definition: DeclarativeWorkflowDefinition,
    ) -> anyhow::Result<()> {
        self.ensure_mutable(&definition.policy().id)?;
        if self.definitions.contains_key(&definition.policy().id)
            && !self
                .current_declarative_versions
                .contains_key(&definition.policy().id)
        {
            anyhow::bail!(
                "historical declarative definition '{}' collides with a non-declarative current definition",
                definition.policy().id
            );
        }
        let version_key = (
            definition.policy().id.clone(),
            definition.definition_version(),
        );
        let versioned = self.checked_version_entry(version_key.clone(), definition)?;
        self.validate_declarative_definition(&versioned)?;
        if !self.definition_ids.contains(&version_key.0) {
            self.definition_ids.push(version_key.0.clone());
        }
        self.declarative_versions.insert(version_key, versioned);
        Ok(())
    }

    pub fn register_declarative_historical_batch(
        &mut self,
        definitions: impl IntoIterator<Item = DeclarativeWorkflowDefinition>,
    ) -> anyhow::Result<()> {
        let mut staged = self.clone();
        for definition in definitions {
            staged.register_declarative_historical(definition)?;
        }
        *self = staged;
        Ok(())
    }

    fn validate_declarative_definition(
        &self,
        definition: &DeclarativeWorkflowDefinition,
    ) -> anyhow::Result<()> {
        Self::validate_registered_definition(definition.registered())?;
        if definition.registered().id != definition.policy().id {
            anyhow::bail!(
                "declarative workflow registry definition id '{}' does not match policy id '{}'",
                definition.registered().id,
                definition.policy().id
            );
        }
        if !is_builtin_definition_id(&definition.policy().id) {
            let (expected_version, expected_hash) =
                declarative_definition_identity_with_classifier_policies(
                    definition.policy(),
                    definition.classifier_activity_policies(),
                )?;
            if definition.definition_version() != expected_version
                || definition.definition_hash() != expected_hash
            {
                anyhow::bail!(
                    "declarative workflow definition '{}' identity does not match its canonical policy",
                    definition.policy().id
                );
            }
        }
        Ok(())
    }

    fn checked_version_entry(
        &self,
        version_key: (String, u32),
        definition: DeclarativeWorkflowDefinition,
    ) -> anyhow::Result<Arc<DeclarativeWorkflowDefinition>> {
        let Some(existing) = self.declarative_versions.get(&version_key) else {
            return Ok(Arc::new(definition));
        };
        if existing.definition_hash() != definition.definition_hash() {
            anyhow::bail!(
                "declarative workflow definition '{}@{}' version collision between hashes '{}' and '{}'",
                version_key.0,
                version_key.1,
                existing.definition_hash(),
                definition.definition_hash()
            );
        }
        if existing.as_ref() != &definition {
            anyhow::bail!(
                "declarative workflow definition '{}@{}' full hash collision for '{}'",
                version_key.0,
                version_key.1,
                definition.definition_hash()
            );
        }
        Ok(existing.clone())
    }

    pub fn current_declarative_definition(
        &self,
        definition_id: &str,
    ) -> Option<Arc<DeclarativeWorkflowDefinition>> {
        let version = *self.current_declarative_versions.get(definition_id)?;
        self.declarative_definition(definition_id, version)
    }

    pub fn declarative_definition(
        &self,
        definition_id: &str,
        definition_version: u32,
    ) -> Option<Arc<DeclarativeWorkflowDefinition>> {
        self.declarative_versions
            .get(&(definition_id.to_string(), definition_version))
            .cloned()
    }

    pub fn definition_for_version(
        &self,
        definition_id: &str,
        definition_version: u32,
    ) -> Option<Arc<RegisteredWorkflowDefinition>> {
        if let Some(definition) = self.declarative_definition(definition_id, definition_version) {
            return Some(Arc::new(definition.registered().clone()));
        }
        if definition_id == GITHUB_ISSUE_PR_DEFINITION_ID
            && self
                .current_declarative_versions
                .contains_key(definition_id)
        {
            return None;
        }
        is_builtin_definition_id(definition_id).then(|| self.definition(definition_id))?
    }

    pub fn definition_for_instance(
        &self,
        instance: &WorkflowInstance,
    ) -> Option<Arc<RegisteredWorkflowDefinition>> {
        if is_builtin_definition_id(&instance.definition_id)
            && instance.definition_id != GITHUB_ISSUE_PR_DEFINITION_ID
        {
            return self.definition(&instance.definition_id);
        }
        match self.resolve_declarative_definition(instance) {
            DeclarativeDefinitionResolution::Resolved(definition) => {
                Some(Arc::new(definition.registered().clone()))
            }
            DeclarativeDefinitionResolution::PinError(_) => None,
            DeclarativeDefinitionResolution::NotDeclarative => {
                self.definition(&instance.definition_id)
            }
        }
    }

    pub fn resolve_declarative_definition(
        &self,
        instance: &WorkflowInstance,
    ) -> DeclarativeDefinitionResolution {
        if self.definitions.contains_key(&instance.definition_id)
            && !self
                .current_declarative_versions
                .contains_key(&instance.definition_id)
        {
            return DeclarativeDefinitionResolution::NotDeclarative;
        }
        let definition =
            self.declarative_definition(&instance.definition_id, instance.definition_version);
        let is_declarative = definition.is_some()
            || self
                .declarative_versions
                .keys()
                .any(|(definition_id, _)| definition_id == &instance.definition_id);
        if !is_declarative {
            if is_builtin_definition_id(&instance.definition_id) {
                return DeclarativeDefinitionResolution::NotDeclarative;
            }
            if instance.data.get("definition_hash").is_some() {
                return DeclarativeDefinitionResolution::PinError(
                    DeclarativeDefinitionPinError::MissingVersion,
                );
            }
            return DeclarativeDefinitionResolution::NotDeclarative;
        }
        let Some(definition) = definition else {
            return DeclarativeDefinitionResolution::PinError(
                DeclarativeDefinitionPinError::MissingVersion,
            );
        };
        if is_builtin_definition_id(&instance.definition_id) {
            if instance.definition_id != GITHUB_ISSUE_PR_DEFINITION_ID {
                return DeclarativeDefinitionResolution::Resolved(definition);
            }
            // Version 1 predates declarative pinning. Existing workflows may
            // carry a `definition_hash` payload field with unrelated meaning,
            // so version identity alone selects the immutable historical
            // definition. Version 2 and later require the exact content hash.
            if definition.definition_version() == 1 {
                return DeclarativeDefinitionResolution::Resolved(definition);
            }
            let Some(expected_hash) = instance.data.get("definition_hash") else {
                return DeclarativeDefinitionResolution::PinError(
                    DeclarativeDefinitionPinError::MissingHash,
                );
            };
            let Some(expected_hash) = expected_hash.as_str() else {
                return DeclarativeDefinitionResolution::PinError(
                    DeclarativeDefinitionPinError::InvalidHash,
                );
            };
            if definition.definition_hash() != expected_hash {
                return DeclarativeDefinitionResolution::PinError(
                    DeclarativeDefinitionPinError::HashMismatch,
                );
            }
            return DeclarativeDefinitionResolution::Resolved(definition);
        }
        let Some(expected_hash) = instance.data.get("definition_hash") else {
            return DeclarativeDefinitionResolution::PinError(
                DeclarativeDefinitionPinError::MissingHash,
            );
        };
        let Some(expected_hash) = expected_hash.as_str() else {
            return DeclarativeDefinitionResolution::PinError(
                DeclarativeDefinitionPinError::InvalidHash,
            );
        };
        if !is_canonical_definition_hash(expected_hash) {
            return DeclarativeDefinitionResolution::PinError(
                DeclarativeDefinitionPinError::InvalidHash,
            );
        }
        if definition.definition_hash() != expected_hash {
            return DeclarativeDefinitionResolution::PinError(
                DeclarativeDefinitionPinError::HashMismatch,
            );
        }
        DeclarativeDefinitionResolution::Resolved(definition)
    }

    pub fn state_definition_for_version(
        &self,
        definition_id: &str,
        definition_version: u32,
        state: &str,
    ) -> Option<WorkflowStateDefinition> {
        state_definition(
            self.definition_for_version(definition_id, definition_version)?,
            state,
        )
    }

    pub fn state_definition_for_instance(
        &self,
        instance: &WorkflowInstance,
        state: &str,
    ) -> Option<WorkflowStateDefinition> {
        state_definition(self.definition_for_instance(instance)?, state)
    }
}

pub(super) fn github_issue_pr_v1_definition() -> DeclarativeWorkflowDefinition {
    use DeclaredProgressMode::{CommandDriven, ExternalWait, OperatorGate, ParentHandoff};

    let policy = WorkflowDefinitionPolicy {
        id: GITHUB_ISSUE_PR_DEFINITION_ID.to_string(),
        initial: "discovered".to_string(),
        states: BTreeMap::from([
            (
                "discovered".to_string(),
                v1_progress(
                    CommandDriven,
                    [
                        ("DependenciesBlocked", "awaiting_dependencies"),
                        ("IssueScheduled", "scheduled"),
                        ("PlanIssue", "planning"),
                        ("SubmitImplementation", "implementing"),
                    ],
                ),
            ),
            (
                "awaiting_dependencies".to_string(),
                v1_progress(
                    ExternalWait,
                    [
                        ("IssueScheduled", "scheduled"),
                        ("PlanIssue", "planning"),
                        ("SubmitImplementation", "implementing"),
                    ],
                ),
            ),
            (
                "scheduled".to_string(),
                v1_progress(
                    CommandDriven,
                    [
                        ("PlanIssue", "planning"),
                        ("SubmitImplementation", "implementing"),
                        ("ReplanIssue", "replanning"),
                        ("PullRequestReady", "pr_open"),
                    ],
                ),
            ),
            (
                "planning".to_string(),
                v1_activity(ISSUE_PLAN_ACTIVITY, Some("implementing"), []),
            ),
            (
                "implementing".to_string(),
                v1_activity(
                    "implement_issue",
                    Some("pr_open"),
                    [
                        (ISSUE_CLOSED_SIGNAL, "done"),
                        (ISSUE_ALREADY_RESOLVED_SIGNAL, "done"),
                        ("PlanIssue", "replanning"),
                    ],
                ),
            ),
            (
                "replanning".to_string(),
                v1_activity("replan_issue", Some("implementing"), []),
            ),
            (
                "pr_open".to_string(),
                v1_progress(
                    ExternalWait,
                    [
                        ("LocalReviewRequested", "local_review_gate"),
                        ("AwaitFeedback", "awaiting_feedback"),
                        (ISSUE_CLOSED_SIGNAL, "done"),
                    ],
                ),
            ),
            (
                "local_review_gate".to_string(),
                v1_activity(
                    LOCAL_REVIEW_ACTIVITY,
                    Some("awaiting_feedback"),
                    [
                        ("LocalReviewPassed", "awaiting_feedback"),
                        ("LocalReviewChangesRequested", "addressing_feedback"),
                        ("LocalReviewBlocked", "blocked"),
                    ],
                ),
            ),
            (
                "awaiting_feedback".to_string(),
                v1_progress(
                    ExternalWait,
                    [
                        ("FeedbackFound", "addressing_feedback"),
                        ("ChangesRequested", "addressing_feedback"),
                        ("ChecksFailed", "addressing_feedback"),
                        ("PrReadyToMerge", "quality_gate_pending"),
                        (ISSUE_CLOSED_SIGNAL, "done"),
                    ],
                ),
            ),
            (
                "addressing_feedback".to_string(),
                v1_activity("address_pr_feedback", Some("local_review_gate"), []),
            ),
            (
                "quality_gate_pending".to_string(),
                v1_progress(
                    ParentHandoff,
                    [
                        ("QualityPassed", "ready_to_merge"),
                        (ISSUE_CLOSED_SIGNAL, "done"),
                    ],
                ),
            ),
            (
                "ready_to_merge".to_string(),
                v1_progress(
                    OperatorGate,
                    [("MergeRequested", "merging"), (ISSUE_CLOSED_SIGNAL, "done")],
                ),
            ),
            (
                "merging".to_string(),
                v1_activity("merge_pr", Some("done"), []),
            ),
            (
                "blocked".to_string(),
                v1_progress(OperatorGate, std::iter::empty()),
            ),
        ]),
        terminal: BTreeMap::from([
            ("cancelled".to_string(), "cancelled".to_string()),
            ("done".to_string(), "succeeded".to_string()),
            ("failed".to_string(), "failed".to_string()),
        ]),
        evidence_required: BTreeMap::new(),
        recovery_targets: vec![
            "implementing".to_string(),
            "replanning".to_string(),
            "local_review_gate".to_string(),
            "awaiting_feedback".to_string(),
            "addressing_feedback".to_string(),
            "merging".to_string(),
        ],
        intake: None,
    };
    let activity_policies = policy
        .states
        .values()
        .filter_map(|state| state.activity.as_ref())
        .map(|activity| (activity.clone(), WorkflowActivityPolicy::default()))
        .collect();
    build_builtin_declarative_definition(
        &policy,
        &activity_policies,
        TransitionAllowlist::github_issue_pr_v1_defaults(),
        BTreeSet::new(),
        1,
    )
    .unwrap_or_else(|error| panic!("historical github_issue_pr@1 must compile: {error}"))
}

fn v1_activity(
    name: &str,
    on_success: Option<&str>,
    signals: impl IntoIterator<Item = (&'static str, &'static str)>,
) -> DeclaredState {
    DeclaredState {
        activity: Some(name.to_string()),
        on_success: on_success.map(str::to_string),
        on_signal: v1_signal_routes(signals),
        ..DeclaredState::default()
    }
}

fn v1_progress(
    mode: DeclaredProgressMode,
    signals: impl IntoIterator<Item = (&'static str, &'static str)>,
) -> DeclaredState {
    DeclaredState {
        progress: Some(mode),
        on_signal: v1_signal_routes(signals),
        ..DeclaredState::default()
    }
}

fn v1_signal_routes(
    signals: impl IntoIterator<Item = (&'static str, &'static str)>,
) -> BTreeMap<String, String> {
    signals
        .into_iter()
        .map(|(signal, target)| (signal.to_string(), target.to_string()))
        .collect()
}

fn is_canonical_definition_hash(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|digest| {
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

fn state_definition(
    definition: Arc<RegisteredWorkflowDefinition>,
    state: &str,
) -> Option<WorkflowStateDefinition> {
    definition
        .states
        .iter()
        .find(|definition| definition.key.state.as_ref() == state)
        .cloned()
}

fn is_builtin_definition_id(definition_id: &str) -> bool {
    super::is_builtin_workflow_definition_id(definition_id)
}

#[cfg(test)]
#[path = "versioning_tests.rs"]
mod tests;
