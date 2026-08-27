use anyhow::Context;
use harness_core::config::workflow::{
    WorkflowActivityPolicy, WorkflowClassifierPolicy, WorkflowConfig,
};
use harness_workflow::runtime::{
    completion_evidence::ARTIFACT_CLASSIFIER_ASSESSMENT, ActivityArtifact, ActivityErrorKind,
    ActivityResult, ActivitySignal, ActivityStatus, DataProvenance,
    DeclarativeDefinitionResolution, DeclarativeWorkflowDefinition, RuntimeJob,
    WorkflowDefinitionRegistry, WorkflowInstance,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::Arc;

use crate::github_pr_snapshot::{fetch_github_pr_snapshot, GitHubPrSnapshotTarget};
use crate::http::AppState;

const CLASSIFIER_OUTPUT_ARTIFACT: &str = "classifier_output";
const CLASSIFIER_ASSESSMENT_SCHEMA: &str = "harness.runtime.classifier_assessment.v1";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClassifierOutput {
    verdict: String,
    rationale: String,
    #[serde(default)]
    evidence_refs: Vec<String>,
}

pub(super) fn policy_for_activity<'a>(
    config: &'a WorkflowConfig,
    activity: &str,
) -> Option<&'a WorkflowClassifierPolicy> {
    config
        .activities
        .get(activity)
        .and_then(|activity| activity.classifier.as_ref())
}

pub(super) fn policy_for_job(
    registry: &WorkflowDefinitionRegistry,
    workflow: Option<&WorkflowInstance>,
    config: &WorkflowConfig,
    activity: &str,
) -> anyhow::Result<Option<WorkflowClassifierPolicy>> {
    let Some(workflow) = workflow else {
        return Ok(policy_for_activity(config, activity).cloned());
    };
    let definition = match registry.resolve_declarative_definition(workflow) {
        DeclarativeDefinitionResolution::NotDeclarative => {
            return Ok(policy_for_activity(config, activity).cloned())
        }
        DeclarativeDefinitionResolution::Resolved(definition) => definition,
        DeclarativeDefinitionResolution::PinError(error) => anyhow::bail!(
            "workflow '{}' has an invalid declarative definition pin while resolving classifier policy: {error:?}",
            workflow.id
        ),
    };
    let expected_activity = definition
        .policy()
        .states
        .get(&workflow.state)
        .and_then(|state| state.activity.as_deref());
    if expected_activity != Some(activity) {
        anyhow::bail!(
            "workflow '{}' state '{}' does not bind runtime activity '{}'",
            workflow.id,
            workflow.state,
            activity
        );
    }
    if !definition.requires_server_classifier_assessment(&workflow.state) {
        return Ok(None);
    }
    let activity_policy = pinned_classifier_activity_policy(&definition, workflow, activity)?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "classifier activity '{}' has no pinned dispatch policy",
                activity
            )
        })?;
    let policy = activity_policy.classifier.as_ref().ok_or_else(|| {
        anyhow::anyhow!(
            "classifier activity '{}' dispatch policy is missing its classifier contract",
            activity
        )
    })?;
    let state_policy = &definition.policy().states[&workflow.state];
    policy.validate_routes(
        activity,
        state_policy.on_success.as_deref(),
        state_policy.on_failure.as_deref(),
        &state_policy.on_signal,
    )?;
    Ok(Some(policy.clone()))
}

pub(super) fn pinned_classifier_activity_policy(
    definition: &DeclarativeWorkflowDefinition,
    workflow: &WorkflowInstance,
    activity: &str,
) -> anyhow::Result<Option<WorkflowActivityPolicy>> {
    if let Some(policy) = definition.classifier_activity_policy(activity) {
        return Ok(Some(policy.clone()));
    }
    if !definition.requires_server_classifier_assessment(&workflow.state)
        || activity != harness_workflow::runtime::CHANGE_SCOPE_REVIEW_ACTIVITY
    {
        return Ok(None);
    }
    let Some(policy) = workflow
        .data
        .get(crate::workflow_runtime_policy::PINNED_CHANGE_SCOPE_CLASSIFIER_POLICY_FIELD)
        .cloned()
    else {
        return Ok(None);
    };
    let pointer = format!(
        "/{}",
        crate::workflow_runtime_policy::PINNED_CHANGE_SCOPE_CLASSIFIER_POLICY_FIELD
    );
    if workflow
        .data_provenance
        .as_ref()
        .and_then(|provenance| provenance.provenance_for(&pointer))
        != Some(DataProvenance::Server)
    {
        anyhow::bail!(
            "workflow '{}' pinned change-scope classifier policy is not server-provenanced",
            workflow.id
        );
    }
    serde_json::from_value(policy).map(Some).map_err(|error| {
        anyhow::anyhow!(
            "workflow '{}' has an invalid pinned change-scope classifier policy: {error}",
            workflow.id
        )
    })
}

pub(super) async fn enrich_scope_facts(
    state: &Arc<AppState>,
    workflow: Option<&harness_workflow::runtime::WorkflowInstance>,
    job: &mut RuntimeJob,
) -> anyhow::Result<()> {
    if job.input.get("activity").and_then(Value::as_str)
        != Some(harness_workflow::runtime::CHANGE_SCOPE_REVIEW_ACTIVITY)
    {
        return Ok(());
    }
    let has_pull_request = job.input.pointer("/scope_facts/pull_request").is_some();
    let requires_pull_request =
        workflow.is_some_and(|workflow| workflow.state == "pr_scope_review");
    if requires_pull_request && !has_pull_request {
        anyhow::bail!("pr_scope_review classifier command is missing required pull request facts");
    }
    let repo = workflow
        .and_then(|workflow| workflow.data.get("repo"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            let url = job
                .input
                .pointer("/scope_facts/pull_request/pr_url")
                .and_then(Value::as_str)?;
            harness_agents::output_parsing::parse_github_pr_url(url)
                .map(|(owner, repo, _)| format!("{owner}/{repo}"))
        });
    let issue_number = workflow
        .and_then(|workflow| workflow.data.get("issue_number"))
        .and_then(Value::as_u64);
    let github_token = state.core.server.config.server.github_token.as_deref();
    let (repo_slug, issue_number) = match (repo.as_deref(), issue_number) {
        (Some(repo), Some(issue_number)) => (repo, issue_number),
        _ => anyhow::bail!("classifier requires a server-verifiable GitHub issue identity"),
    };
    let issue_snapshot =
        crate::reconciliation::fetch_exact_issue_scope_facts(repo_slug, issue_number, github_token)
            .await
            .with_context(|| {
                format!(
            "classifier could not obtain authoritative issue facts for {repo_slug}#{issue_number}"
        )
            })?;
    let issue_snapshot = json!({
        "availability": "available",
        "snapshot": issue_snapshot,
    });
    let pr_snapshot = if has_pull_request {
        let command_pr_number = job
            .input
            .pointer("/scope_facts/pull_request/pr_number")
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow::anyhow!("classifier command PR number is missing"))?;
        let workflow_pr_number = workflow
            .and_then(|workflow| workflow.data.get("pr_number"))
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow::anyhow!("classifier workflow has no bound PR number"))?;
        if command_pr_number != workflow_pr_number {
            anyhow::bail!(
                "classifier command PR #{command_pr_number} does not match workflow-bound PR #{workflow_pr_number}"
            );
        }
        let command_pr_url = job
            .input
            .pointer("/scope_facts/pull_request/pr_url")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("classifier command PR URL is missing"))?;
        let workflow_pr_url = workflow
            .and_then(|workflow| workflow.data.get("pr_url"))
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("classifier workflow has no bound PR URL"))?;
        validate_pr_url_identity(repo_slug, workflow_pr_number, command_pr_url, "command")?;
        validate_pr_url_identity(repo_slug, workflow_pr_number, workflow_pr_url, "workflow")?;
        let pr_number = workflow_pr_number;
        let target = GitHubPrSnapshotTarget::new(repo_slug, pr_number)?;
        let artifacts = fetch_github_pr_snapshot(&target, github_token)
            .await
            .with_context(|| {
                format!(
                    "classifier could not obtain authoritative pull request facts for {repo_slug}#{pr_number}"
                )
            })?;
        let diff = crate::github_pr_snapshot::fetch_complete_pr_diff(
            &target,
            github_token,
            &artifacts.normalized_snapshot,
        )
        .await
        .context("classifier could not obtain complete head-bound pull request diff facts")?;
        let canonical_pr_url = artifacts
            .normalized_snapshot
            .get("pr_url")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("server PR snapshot has no canonical URL"))?;
        if command_pr_url != workflow_pr_url || command_pr_url != canonical_pr_url {
            anyhow::bail!(
                "classifier PR URLs disagree: command='{command_pr_url}', workflow='{workflow_pr_url}', server='{canonical_pr_url}'"
            );
        }
        Some(json!({
            "availability": "available",
            "snapshot": artifacts.normalized_snapshot,
            "diff": diff,
        }))
    } else {
        None
    };
    let input = job
        .input
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("classifier runtime job input must be an object"))?;
    let scope_facts = input
        .entry("scope_facts")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("classifier scope_facts must be an object"))?;
    scope_facts.insert("server_issue_snapshot".to_string(), issue_snapshot);
    if let Some(pr_snapshot) = pr_snapshot {
        scope_facts.insert("server_pr_snapshot".to_string(), pr_snapshot);
    }
    Ok(())
}

fn validate_pr_url_identity(
    repo_slug: &str,
    pr_number: u64,
    pr_url: &str,
    source: &str,
) -> anyhow::Result<()> {
    let (owner, repo, url_number) = harness_agents::output_parsing::parse_github_pr_url(pr_url)
        .ok_or_else(|| anyhow::anyhow!("classifier {source} PR URL is invalid"))?;
    let url_repo = format!("{owner}/{repo}");
    if !url_repo.eq_ignore_ascii_case(repo_slug) || url_number != pr_number {
        anyhow::bail!(
            "classifier {source} PR URL identifies {url_repo}#{url_number}, expected {repo_slug}#{pr_number}"
        );
    }
    Ok(())
}

pub(super) async fn attest_prelaunch_failure_if_configured(
    state: &AppState,
    job: &RuntimeJob,
    result: ActivityResult,
) -> ActivityResult {
    let workflow = match super::job_context::workflow_for_job(state, job).await {
        Ok(workflow) => workflow,
        Err(_) => return result,
    };
    let activity = super::data_helpers::activity_name(job);
    let registry = match state.core.workflow_runtime_store.as_ref() {
        Some(store) => store.definition_registry(),
        None => return result,
    };
    let Some(workflow) = workflow.as_ref() else {
        return result;
    };
    let Some(definition) = registry.declarative_definition_for_instance(workflow) else {
        return result;
    };
    if !definition.requires_server_classifier_assessment(&workflow.state) {
        return result;
    }
    let policy = pinned_classifier_activity_policy(&definition, workflow, &activity)
        .ok()
        .flatten()
        .and_then(|activity_policy| activity_policy.classifier);
    attest_prelaunch_failure(policy.as_ref(), job, result)
}

fn attest_prelaunch_failure(
    policy: Option<&WorkflowClassifierPolicy>,
    job: &RuntimeJob,
    mut result: ActivityResult,
) -> ActivityResult {
    result.signals.clear();
    result.artifacts.retain(|artifact| {
        artifact.artifact_type != CLASSIFIER_OUTPUT_ARTIFACT
            && artifact.artifact_type != ARTIFACT_CLASSIFIER_ASSESSMENT
    });
    result.with_artifact(ActivityArtifact::new(
        ARTIFACT_CLASSIFIER_ASSESSMENT,
        json!({
            "schema": CLASSIFIER_ASSESSMENT_SCHEMA,
            "outcome": "prelaunch_failure",
            "attestation": {
                "runtime_job_id": job.id,
                "runtime_profile": job.runtime_profile,
                "model": Value::Null,
                "prompt_packet_digest": Value::Null,
                "policy_sha256": policy.map(policy_sha256),
            },
        }),
    ))
}

pub(super) fn attest_result(
    policy: &WorkflowClassifierPolicy,
    job: &RuntimeJob,
    requested_model: &str,
    reported_models: &[String],
    prompt_packet: &Value,
    prompt_packet_digest: &str,
    mut result: ActivityResult,
) -> ActivityResult {
    result.signals.clear();
    result
        .artifacts
        .retain(|artifact| artifact.artifact_type != ARTIFACT_CLASSIFIER_ASSESSMENT);
    let attestation = assessment_attestation(
        policy,
        job,
        requested_model,
        reported_models,
        prompt_packet_digest,
    );
    if reported_models.is_empty() {
        result.artifacts.retain(|artifact| {
            artifact.artifact_type != CLASSIFIER_OUTPUT_ARTIFACT
                && artifact.artifact_type != ARTIFACT_CLASSIFIER_ASSESSMENT
        });
        return ActivityResult::failed(
            &result.activity,
            "Classifier model identity was not reported by the backend.",
            "provider-reported model identity is required for classifier attestation",
        )
        .with_error_kind(ActivityErrorKind::Fatal)
        .with_artifact(ActivityArtifact::new(
            ARTIFACT_CLASSIFIER_ASSESSMENT,
            json!({
                "schema": CLASSIFIER_ASSESSMENT_SCHEMA,
                "outcome": "model_identity_unavailable",
                "attestation": attestation,
            }),
        ));
    }
    if reported_models
        .iter()
        .any(|reported_model| reported_model != requested_model)
    {
        result.artifacts.retain(|artifact| {
            artifact.artifact_type != CLASSIFIER_OUTPUT_ARTIFACT
                && artifact.artifact_type != ARTIFACT_CLASSIFIER_ASSESSMENT
        });
        return ActivityResult::failed(
            &result.activity,
            "Classifier model identity did not match the requested model.",
            format!("requested model '{requested_model}', backend reported {reported_models:?}"),
        )
        .with_error_kind(ActivityErrorKind::Fatal)
        .with_artifact(ActivityArtifact::new(
            ARTIFACT_CLASSIFIER_ASSESSMENT,
            json!({
                "schema": CLASSIFIER_ASSESSMENT_SCHEMA,
                "outcome": "model_identity_mismatch",
                "attestation": attestation,
            }),
        ));
    }
    if result.status != ActivityStatus::Succeeded {
        result
            .artifacts
            .retain(|artifact| artifact.artifact_type != CLASSIFIER_OUTPUT_ARTIFACT);
        return result.with_artifact(ActivityArtifact::new(
            ARTIFACT_CLASSIFIER_ASSESSMENT,
            json!({
                "schema": CLASSIFIER_ASSESSMENT_SCHEMA,
                "outcome": "runtime_failure",
                "attestation": attestation,
            }),
        ));
    }

    let outputs = result
        .artifacts
        .iter()
        .filter(|artifact| artifact.artifact_type == CLASSIFIER_OUTPUT_ARTIFACT)
        .collect::<Vec<_>>();
    let output = match outputs.as_slice() {
        [artifact] => serde_json::from_value::<ClassifierOutput>(artifact.artifact.clone())
            .map_err(|error| format!("classifier_output is malformed: {error}")),
        [] => Err("classifier_output artifact is missing".to_string()),
        _ => Err("multiple classifier_output artifacts were returned".to_string()),
    }
    .and_then(|output| validate_output(policy, prompt_packet, output));

    let output = match output {
        Ok(output) => output,
        Err(error) => {
            return ActivityResult::failed(
                &result.activity,
                "Classifier output failed server validation.",
                &error,
            )
            .with_error_kind(ActivityErrorKind::Fatal)
            .with_artifact(ActivityArtifact::new(
                ARTIFACT_CLASSIFIER_ASSESSMENT,
                json!({
                    "schema": CLASSIFIER_ASSESSMENT_SCHEMA,
                    "outcome": "rejected",
                    "error": error,
                    "attestation": attestation,
                }),
            ));
        }
    };
    result
        .artifacts
        .retain(|artifact| artifact.artifact_type != CLASSIFIER_OUTPUT_ARTIFACT);
    let verdict = output.verdict.clone();
    let assessment = json!({
        "schema": CLASSIFIER_ASSESSMENT_SCHEMA,
        "verdict": output.verdict,
        "rationale": output.rationale,
        "evidence_refs": output.evidence_refs,
        "subject_head_oid": prompt_packet
            .pointer("/classifier_facts/facts/server_pr_snapshot/snapshot/head_oid")
            .cloned().unwrap_or(Value::Null),
        "attestation": attestation,
    });
    result
        .with_artifact(ActivityArtifact::new(
            ARTIFACT_CLASSIFIER_ASSESSMENT,
            assessment.clone(),
        ))
        .with_signal(ActivitySignal::new(verdict, assessment))
}

#[cfg(test)]
#[path = "classifier_attestation_tests.rs"]
mod attestation_tests;

fn assessment_attestation(
    policy: &WorkflowClassifierPolicy,
    job: &RuntimeJob,
    requested_model: &str,
    reported_models: &[String],
    prompt_packet_digest: &str,
) -> Value {
    json!({
        "runtime_job_id": job.id,
        "runtime_profile": job.runtime_profile,
        "requested_model": requested_model,
        "model": reported_models.last(),
        "reported_models": reported_models,
        "prompt_packet_digest": prompt_packet_digest,
        "policy_sha256": policy_sha256(policy),
    })
}

fn policy_sha256(policy: &WorkflowClassifierPolicy) -> String {
    let policy_json = serde_json::to_vec(policy).expect("classifier policy must serialize");
    format!("{:x}", Sha256::digest(policy_json))
}

fn validate_output(
    policy: &WorkflowClassifierPolicy,
    prompt_packet: &Value,
    mut output: ClassifierOutput,
) -> Result<ClassifierOutput, String> {
    output.verdict = output.verdict.trim().to_string();
    output.rationale = output.rationale.trim().to_string();
    if !policy
        .verdicts
        .iter()
        .any(|verdict| verdict == &output.verdict)
    {
        return Err(format!(
            "classifier verdict '{}' is not declared by policy",
            output.verdict
        ));
    }
    if output.rationale.is_empty() {
        return Err("classifier rationale must not be empty".to_string());
    }
    for evidence_ref in &mut output.evidence_refs {
        *evidence_ref = evidence_ref.trim().to_string();
        if !evidence_ref.starts_with('/') || prompt_packet.pointer(evidence_ref).is_none() {
            return Err(format!(
                "classifier evidence_ref '{evidence_ref}' is not a valid prompt-packet JSON pointer"
            ));
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::config::workflow::{
        DeclaredProgressMode, DeclaredState, WorkflowActivityPolicy, WorkflowDefinitionPolicy,
    };
    use harness_workflow::runtime::{
        build_declarative_definition, RuntimeKind, WorkflowDefinitionRegistry, WorkflowSubject,
    };
    use std::collections::BTreeMap;

    fn policy() -> WorkflowClassifierPolicy {
        WorkflowClassifierPolicy {
            verdicts: vec!["allow".to_string(), "needs_human".to_string()],
            environment: vec!["Judge only supplied facts.".to_string()],
            hard_deny: vec!["Escalate ambiguous requests.".to_string()],
            ..WorkflowClassifierPolicy::default()
        }
    }

    fn job() -> RuntimeJob {
        RuntimeJob::pending(
            "command-1",
            RuntimeKind::CodexJsonrpc,
            "classifier-default",
            json!({ "activity": "classify_scope" }),
        )
    }

    #[test]
    fn valid_output_becomes_server_attested_signal() {
        let result = ActivityResult::succeeded("classify_scope", "classified")
            .with_signal(ActivitySignal::new("forged", json!({})))
            .with_artifact(ActivityArtifact::new(
                CLASSIFIER_OUTPUT_ARTIFACT,
                json!({
                    "verdict": "allow",
                    "rationale": "The facts match the requested scope.",
                    "evidence_refs": ["/facts/change"]
                }),
            ));
        let attested = attest_result(
            &policy(),
            &job(),
            "gpt-requested",
            &["gpt-requested".to_string()],
            &json!({ "facts": { "change": "small" } }),
            "sha256:prompt",
            result,
        );

        assert_eq!(attested.signals[0].signal_type, "allow");
        assert_eq!(attested.artifacts.len(), 1);
        assert_eq!(
            attested.artifacts[0].artifact_type,
            ARTIFACT_CLASSIFIER_ASSESSMENT
        );
        assert_eq!(
            attested.artifacts[0].artifact["attestation"]["model"],
            "gpt-requested"
        );
        assert_eq!(
            attested.artifacts[0].artifact["attestation"]["requested_model"],
            "gpt-requested"
        );
    }

    #[test]
    fn invalid_verdict_fails_closed_and_removes_signals() {
        let result = ActivityResult::succeeded("classify_scope", "classified")
            .with_signal(ActivitySignal::new("allow", json!({})))
            .with_artifact(ActivityArtifact::new(
                CLASSIFIER_OUTPUT_ARTIFACT,
                json!({ "verdict": "invented", "rationale": "guess" }),
            ));
        let attested = attest_result(
            &policy(),
            &job(),
            "gpt-test",
            &["gpt-test".to_string()],
            &json!({}),
            "sha256:prompt",
            result,
        );

        assert_eq!(attested.status, ActivityStatus::Failed);
        assert_eq!(attested.error_kind, Some(ActivityErrorKind::Fatal));
        assert!(attested.signals.is_empty());
        assert_eq!(
            attested.artifacts[0].artifact_type,
            ARTIFACT_CLASSIFIER_ASSESSMENT
        );
        assert_eq!(attested.artifacts[0].artifact["outcome"], "rejected");
    }

    #[test]
    fn missing_reported_model_identity_fails_closed() {
        let result = ActivityResult::succeeded("classify_scope", "classified").with_artifact(
            ActivityArtifact::new(
                CLASSIFIER_OUTPUT_ARTIFACT,
                json!({ "verdict": "allow", "rationale": "looks coherent" }),
            ),
        );
        let attested = attest_result(
            &policy(),
            &job(),
            "gpt-requested",
            &[],
            &json!({}),
            "sha256:prompt",
            result,
        );

        assert_eq!(attested.status, ActivityStatus::Failed);
        assert!(attested.signals.is_empty());
        assert_eq!(
            attested.artifacts[0].artifact["outcome"],
            "model_identity_unavailable"
        );
        assert!(attested.artifacts[0].artifact["attestation"]["model"].is_null());
    }

    #[test]
    fn substituted_model_identity_fails_closed() {
        let result = ActivityResult::succeeded("classify_scope", "classified").with_artifact(
            ActivityArtifact::new(
                CLASSIFIER_OUTPUT_ARTIFACT,
                json!({ "verdict": "allow", "rationale": "looks coherent" }),
            ),
        );
        let attested = attest_result(
            &policy(),
            &job(),
            "requested-model",
            &["substituted-model".to_string()],
            &json!({}),
            "sha256:prompt",
            result,
        );

        assert_eq!(attested.status, ActivityStatus::Failed);
        assert!(attested.signals.is_empty());
        assert_eq!(
            attested.artifacts[0].artifact["outcome"],
            "model_identity_mismatch"
        );
    }

    #[test]
    fn prelaunch_failure_receives_server_assessment() {
        let result = ActivityResult::failed(
            "classify_scope",
            "Runtime job execution failed before the agent completed.",
            "workspace preparation failed",
        )
        .with_signal(ActivitySignal::new("allow", json!({})));

        let policy = policy();
        let attested = attest_prelaunch_failure(Some(&policy), &job(), result);

        assert!(attested.signals.is_empty());
        assert_eq!(attested.artifacts.len(), 1);
        assert_eq!(
            attested.artifacts[0].artifact["outcome"],
            "prelaunch_failure"
        );
        assert!(attested.artifacts[0].artifact["attestation"]["prompt_packet_digest"].is_null());
    }

    #[test]
    fn classifier_policy_resolves_from_pinned_definition_not_mutable_config() {
        let definition_policy = WorkflowDefinitionPolicy {
            id: "pinned_classifier".to_string(),
            initial: "classifying".to_string(),
            states: BTreeMap::from([
                (
                    "classifying".to_string(),
                    DeclaredState {
                        activity: Some("classify_scope".to_string()),
                        on_failure: Some("blocked".to_string()),
                        on_signal: BTreeMap::from([
                            ("allow".to_string(), "done".to_string()),
                            ("needs_human".to_string(), "failed".to_string()),
                        ]),
                        ..DeclaredState::default()
                    },
                ),
                (
                    "blocked".to_string(),
                    DeclaredState {
                        progress: Some(DeclaredProgressMode::OperatorGate),
                        ..DeclaredState::default()
                    },
                ),
            ]),
            terminal: BTreeMap::from([
                ("cancelled".to_string(), "cancelled".to_string()),
                ("done".to_string(), "succeeded".to_string()),
                ("failed".to_string(), "failed".to_string()),
            ]),
            evidence_required: BTreeMap::new(),
            recovery_targets: vec!["classifying".to_string()],
            intake: None,
        };
        let pinned_policy = policy();
        let definition = build_declarative_definition(
            &definition_policy,
            &BTreeMap::from([(
                "classify_scope".to_string(),
                WorkflowActivityPolicy {
                    classifier: Some(pinned_policy.clone()),
                    ..WorkflowActivityPolicy::default()
                },
            )]),
        )
        .expect("classifier definition should compile");
        let workflow = WorkflowInstance::new(
            definition.policy().id.clone(),
            definition.definition_version(),
            "classifying",
            WorkflowSubject::new("test", "classifier:1"),
        )
        .with_server_data(json!({ "definition_hash": definition.definition_hash() }));
        let mut registry = WorkflowDefinitionRegistry::new();
        registry
            .register_declarative_current(definition)
            .expect("classifier definition should register");
        let mutable_config = WorkflowConfig::default();

        let resolved = policy_for_job(
            &registry,
            Some(&workflow),
            &mutable_config,
            "classify_scope",
        )
        .expect("pinned classifier policy should resolve after checkout policy removal");

        assert_eq!(resolved, Some(pinned_policy));
    }

    #[test]
    fn pr_url_identity_must_match_repo_and_number() {
        validate_pr_url_identity(
            "Owner/Repo",
            7,
            "https://github.com/owner/repo/pull/7",
            "command",
        )
        .expect("GitHub repository identity is case-insensitive");

        let wrong_repo = validate_pr_url_identity(
            "owner/repo",
            7,
            "https://github.com/other/repo/pull/7",
            "command",
        )
        .expect_err("wrong repository must fail closed");
        assert!(wrong_repo.to_string().contains("expected owner/repo#7"));

        let wrong_number = validate_pr_url_identity(
            "owner/repo",
            7,
            "https://github.com/owner/repo/pull/8",
            "workflow",
        )
        .expect_err("wrong PR number must fail closed");
        assert!(wrong_number.to_string().contains("expected owner/repo#7"));
    }
}
