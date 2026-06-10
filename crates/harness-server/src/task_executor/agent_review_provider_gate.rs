use crate::task_runner::{mutate_and_persist, RoundResult, TaskId, TaskStore};
use chrono::{DateTime, Utc};
use harness_core::agent::{AgentRequest, CodeAgent};
use harness_core::config::agents::{AgentReviewConfig, SandboxMode};
use harness_core::review::{
    evaluate_review_gate, parse_review_report, ReviewDecision, ReviewGateProviderReport,
    ReviewGateResult, ReviewProviderKind, ReviewProviderRole, ReviewReport,
};
use harness_core::types::{ContextItem, ExecutionPhase};
use std::collections::HashMap;
use std::path::Path;
use tokio::time::Duration;

pub(crate) type AgentReviewGateReport = Option<ReviewGateProviderReport>;

const CODEX_CLI_REVIEW_PROVIDER_ID: &str = "codex_cli_review";
const CODEX_AGENT_REVIEW_PROVIDER_ID: &str = "codex_agent_review";

pub(crate) struct CodexCliReviewProviderRequest<'a> {
    pub(crate) review_config: &'a AgentReviewConfig,
    pub(crate) context_items: &'a [ContextItem],
    pub(crate) project: &'a Path,
    pub(crate) pr_url: &'a str,
    pub(crate) project_type: &'a str,
    pub(crate) cargo_env: &'a HashMap<String, String>,
}

pub(crate) fn should_run_codex_cli_review(review_config: &AgentReviewConfig) -> bool {
    review_config.codex_cli_review.enabled
        && provider_policy_references(review_config, CODEX_CLI_REVIEW_PROVIDER_ID)
}

pub(crate) fn should_run_codex_agent_review(review_config: &AgentReviewConfig) -> bool {
    review_config.codex_agent_review.enabled
        || provider_policy_references(review_config, CODEX_AGENT_REVIEW_PROVIDER_ID)
}

pub(crate) fn requires_hosted_review_loop(review_config: &AgentReviewConfig) -> bool {
    review_config.review_bot_auto_trigger
}

pub(crate) fn evaluate_configured_review_gate(
    reports: &[ReviewGateProviderReport],
    review_config: &AgentReviewConfig,
) -> ReviewGateResult {
    evaluate_review_gate(
        reports,
        &review_config.required_providers,
        &review_config.advisory_providers,
        review_config.external_required,
    )
}

pub(crate) fn codex_agent_review_config(review_config: &AgentReviewConfig) -> AgentReviewConfig {
    let mut config = review_config.clone();
    config.max_rounds = review_config.codex_agent_review.max_rounds;
    config
}

pub(crate) fn local_review_provider_report(
    review_config: &AgentReviewConfig,
    raw_output: &str,
    started_at: DateTime<Utc>,
    completed_at: DateTime<Utc>,
) -> ReviewGateProviderReport {
    let provider_id = configured_local_review_provider_id(review_config);
    ReviewGateProviderReport {
        role: configured_review_provider_role(review_config, provider_id),
        report: parse_review_report(
            provider_id,
            configured_local_review_provider_kind(provider_id),
            raw_output,
            started_at,
            completed_at,
        ),
    }
}

pub(crate) async fn run_codex_cli_review_provider(
    store: &TaskStore,
    task_id: &TaskId,
    request: CodexCliReviewProviderRequest<'_>,
    turns_used: &mut u32,
) -> anyhow::Result<ReviewGateProviderReport> {
    let mut provider = harness_agents::codex::CodexAgent::new(
        request.review_config.codex_cli_review.cli_path.clone(),
        SandboxMode::ReadOnlyWithNetwork,
    )
    .with_stream_timeout(Some(request.review_config.codex_cli_review.timeout_secs));
    provider.default_model = request.review_config.codex_cli_review.model.clone();
    provider.reasoning_effort = request
        .review_config
        .codex_cli_review
        .reasoning_effort
        .clone();

    run_codex_cli_review_provider_with_agent(store, task_id, &provider, request, turns_used).await
}

pub(crate) async fn run_codex_cli_review_provider_with_agent(
    store: &TaskStore,
    task_id: &TaskId,
    provider: &dyn CodeAgent,
    request: CodexCliReviewProviderRequest<'_>,
    turns_used: &mut u32,
) -> anyhow::Result<ReviewGateProviderReport> {
    let started_at = Utc::now();
    let agent_request = AgentRequest {
        prompt: codex_cli_review_prompt(
            request.pr_url,
            request.project_type,
            &request.review_config.codex_cli_review.base_ref,
            &request.review_config.codex_cli_review.output_format,
        ),
        project_root: request.project.to_path_buf(),
        context: request.context_items.to_vec(),
        execution_phase: Some(ExecutionPhase::SimpleReview),
        sandbox_mode: Some(SandboxMode::ReadOnlyWithNetwork),
        approval_policy: Some("never".to_string()),
        model: Some(request.review_config.codex_cli_review.model.clone()),
        reasoning_effort: Some(
            request
                .review_config
                .codex_cli_review
                .reasoning_effort
                .clone(),
        ),
        env_vars: request.cargo_env.clone(),
        ..Default::default()
    };

    let timeout_secs = request.review_config.codex_cli_review.timeout_secs.max(1);
    let response = tokio::time::timeout(
        Duration::from_secs(timeout_secs),
        provider.execute(agent_request),
    )
    .await;
    *turns_used = turns_used.saturating_add(1);
    let completed_at = Utc::now();
    let report = match response {
        Ok(Ok(response)) => ReviewGateProviderReport {
            role: configured_review_provider_role(
                request.review_config,
                CODEX_CLI_REVIEW_PROVIDER_ID,
            ),
            report: parse_review_report(
                CODEX_CLI_REVIEW_PROVIDER_ID,
                ReviewProviderKind::LocalCli,
                &response.output,
                started_at,
                completed_at,
            ),
        },
        Ok(Err(error)) => provider_failure_report(
            request.review_config,
            ReviewDecision::Failed,
            started_at,
            completed_at,
            format!("Codex CLI review provider failed: {error}"),
        ),
        Err(_) => provider_failure_report(
            request.review_config,
            ReviewDecision::TimedOut,
            started_at,
            completed_at,
            format!("Codex CLI review provider timed out after {timeout_secs}s."),
        ),
    };

    mutate_and_persist(store, task_id, |state| {
        state.rounds.push(RoundResult::new(
            *turns_used,
            CODEX_CLI_REVIEW_PROVIDER_ID,
            review_decision_label(report.report.decision),
            Some(report.report.summary.clone()),
            None,
            None,
        ));
    })
    .await?;

    Ok(report)
}

fn codex_cli_review_prompt(
    pr_url: &str,
    project_type: &str,
    base_ref: &str,
    output_format: &str,
) -> String {
    let json_format = if output_format.eq_ignore_ascii_case("json") {
        "\nReturn exactly one fenced `harness-review-report` JSON block with this shape:\n\
         ```harness-review-report\n\
         {\"decision\":\"approved|changes_requested|failed|timed_out|skipped\",\
         \"summary\":\"concise summary\",\
         \"findings\":[{\"severity\":\"critical|high|medium|low\",\
         \"category\":\"security|correctness|data_integrity|concurrency|performance|test_gap|maintainability|other\",\
         \"path\":\"optional path or null\",\
         \"line\":123,\
         \"message\":\"finding\",\
         \"evidence\":\"optional evidence or null\",\
         \"recommendation\":\"optional recommendation or null\",\
         \"blocking\":true,\
         \"confidence\":0.9}]}\n\
         ```"
    } else {
        "\nIf everything is safe, put APPROVED on the last line. Otherwise list each blocking issue on its own line prefixed with ISSUE:."
    };

    format!(
        "You are the configured codex_cli_review provider for PR {pr_url}.\n\
         Review the local workspace against base ref `{base_ref}` as a read-only provider.\n\
         Project type: {project_type}.\n\n\
         Requirements:\n\
         - Inspect the PR intent, diff, and changed files before deciding.\n\
         - Focus on security, logic, data integrity, error handling, and missing tests.\n\
         - Do not modify files, commit, push, or mark review threads resolved.\n\
         - Treat unparseable or incomplete evidence as a failed provider review.\n\
         {json_format}"
    )
}

fn provider_failure_report(
    review_config: &AgentReviewConfig,
    decision: ReviewDecision,
    started_at: DateTime<Utc>,
    completed_at: DateTime<Utc>,
    summary: String,
) -> ReviewGateProviderReport {
    ReviewGateProviderReport {
        role: configured_review_provider_role(review_config, CODEX_CLI_REVIEW_PROVIDER_ID),
        report: ReviewReport {
            provider_id: CODEX_CLI_REVIEW_PROVIDER_ID.to_string(),
            provider_kind: ReviewProviderKind::LocalCli,
            decision,
            summary: summary.clone(),
            findings: Vec::new(),
            raw_output: Some(summary),
            started_at,
            completed_at,
            elapsed_ms: completed_at
                .signed_duration_since(started_at)
                .num_milliseconds()
                .max(0) as u64,
        },
    }
}

fn review_decision_label(decision: ReviewDecision) -> String {
    match decision {
        ReviewDecision::Approved => "approved",
        ReviewDecision::ChangesRequested => "changes_requested",
        ReviewDecision::Failed => "failed",
        ReviewDecision::TimedOut => "timed_out",
        ReviewDecision::Skipped => "skipped",
    }
    .to_string()
}

fn configured_local_review_provider_id(_review_config: &AgentReviewConfig) -> &'static str {
    CODEX_AGENT_REVIEW_PROVIDER_ID
}

fn configured_review_provider_role(
    review_config: &AgentReviewConfig,
    provider_id: &str,
) -> ReviewProviderRole {
    if review_config
        .required_providers
        .iter()
        .any(|provider| provider == provider_id)
    {
        ReviewProviderRole::Required
    } else {
        ReviewProviderRole::Advisory
    }
}

fn configured_local_review_provider_kind(provider_id: &str) -> ReviewProviderKind {
    match provider_id {
        CODEX_CLI_REVIEW_PROVIDER_ID => ReviewProviderKind::LocalCli,
        _ => ReviewProviderKind::LocalAgent,
    }
}

fn provider_policy_references(review_config: &AgentReviewConfig, provider_id: &str) -> bool {
    review_config
        .required_providers
        .iter()
        .chain(review_config.advisory_providers.iter())
        .any(|provider| provider == provider_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::agent::{AgentResponse, StreamItem};
    use harness_core::types::{Capability, TokenUsage};
    use tokio::sync::Mutex;

    struct RecordingProvider {
        output: String,
        requests: Mutex<Vec<AgentRequest>>,
    }

    #[async_trait::async_trait]
    impl CodeAgent for RecordingProvider {
        fn name(&self) -> &str {
            "recording_codex"
        }

        fn capabilities(&self) -> Vec<Capability> {
            Vec::new()
        }

        async fn execute(&self, req: AgentRequest) -> harness_core::error::Result<AgentResponse> {
            self.requests.lock().await.push(req);
            Ok(AgentResponse {
                output: self.output.clone(),
                stderr: String::new(),
                items: Vec::new(),
                token_usage: TokenUsage::default(),
                model: "recording_codex".to_string(),
                exit_code: Some(0),
            })
        }

        async fn execute_stream(
            &self,
            req: AgentRequest,
            tx: tokio::sync::mpsc::Sender<StreamItem>,
        ) -> harness_core::error::Result<()> {
            self.requests.lock().await.push(req);
            if tx
                .send(StreamItem::MessageDelta {
                    text: self.output.clone(),
                })
                .await
                .is_err()
            {
                return Ok(());
            }
            if tx.send(StreamItem::Done).await.is_err() {
                return Ok(());
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn codex_cli_review_provider_uses_configured_request() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join(["tasks", "db"].join("."))).await?;
        let task_id = TaskId::new();
        store
            .insert(&crate::task_runner::TaskState::new(task_id.clone()))
            .await;
        let provider = RecordingProvider {
            output: "APPROVED".to_string(),
            requests: Mutex::new(Vec::new()),
        };
        let mut config = AgentReviewConfig::default();
        config.codex_cli_review.model = "gpt-test".to_string();
        config.codex_cli_review.reasoning_effort = "xhigh".to_string();
        config.codex_cli_review.base_ref = "origin/main".to_string();
        config.codex_cli_review.timeout_secs = 5;
        let mut turns_used = 0;

        let report = run_codex_cli_review_provider_with_agent(
            &store,
            &task_id,
            &provider,
            CodexCliReviewProviderRequest {
                review_config: &config,
                context_items: &[],
                project: dir.path(),
                pr_url: "https://github.com/owner/repo/pull/1",
                project_type: "rust",
                cargo_env: &HashMap::new(),
            },
            &mut turns_used,
        )
        .await?;

        let requests = provider.requests.lock().await;
        assert_eq!(turns_used, 1);
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].model.as_deref(), Some("gpt-test"));
        assert_eq!(requests[0].reasoning_effort.as_deref(), Some("xhigh"));
        assert_eq!(
            requests[0].sandbox_mode,
            Some(SandboxMode::ReadOnlyWithNetwork)
        );
        assert_eq!(requests[0].approval_policy.as_deref(), Some("never"));
        assert!(requests[0].prompt.contains("origin/main"));
        assert_eq!(report.role, ReviewProviderRole::Required);
        assert_eq!(report.report.provider_id, CODEX_CLI_REVIEW_PROVIDER_ID);
        assert_eq!(report.report.decision, ReviewDecision::Approved);
        let Some(state) = store.get(&task_id) else {
            panic!("task state should be present");
        };
        assert!(state.rounds.iter().any(|round| {
            round.action == CODEX_CLI_REVIEW_PROVIDER_ID && round.result == "approved"
        }));
        Ok(())
    }

    #[test]
    fn codex_cli_review_prompt_uses_parseable_report_fence() {
        let prompt = codex_cli_review_prompt(
            "https://github.com/owner/repo/pull/1",
            "rust",
            "origin/main",
            "json",
        );

        assert!(prompt.contains("```harness-review-report"));
        assert!(!prompt.contains("```review_report"));
    }

    #[test]
    fn codex_agent_review_runs_only_when_configured_or_referenced() {
        let mut config = AgentReviewConfig::default();
        assert!(!should_run_codex_agent_review(&config));
        config.codex_agent_review.enabled = true;
        assert!(should_run_codex_agent_review(&config));
    }

    #[test]
    fn external_required_does_not_enable_legacy_hosted_review_loop() {
        let mut config = AgentReviewConfig {
            external_required: true,
            ..AgentReviewConfig::default()
        };

        assert!(!requires_hosted_review_loop(&config));
        config.review_bot_auto_trigger = true;
        assert!(requires_hosted_review_loop(&config));
    }

    #[test]
    fn configured_gate_blocks_missing_external_required_report() {
        let mut config = AgentReviewConfig {
            external_required: true,
            ..AgentReviewConfig::default()
        };
        config.required_providers = vec![CODEX_CLI_REVIEW_PROVIDER_ID.to_string()];
        config.advisory_providers = vec!["gemini_github_bot".to_string()];
        let now = Utc::now();
        let local_report = ReviewGateProviderReport {
            role: ReviewProviderRole::Required,
            report: parse_review_report(
                CODEX_CLI_REVIEW_PROVIDER_ID,
                ReviewProviderKind::LocalCli,
                "APPROVED",
                now,
                now,
            ),
        };

        let result = evaluate_configured_review_gate(&[local_report], &config);

        assert_eq!(
            result.decision,
            harness_core::review::ReviewGateDecision::Blocked
        );
        assert_eq!(
            result.blocking_provider_id.as_deref(),
            Some("gemini_github_bot")
        );
    }

    #[test]
    fn codex_agent_review_config_uses_provider_round_limit() {
        let mut config = AgentReviewConfig {
            max_rounds: 8,
            ..AgentReviewConfig::default()
        };
        config.codex_agent_review.max_rounds = 2;

        let provider_config = codex_agent_review_config(&config);

        assert_eq!(provider_config.max_rounds, 2);
        assert_eq!(
            provider_config.required_providers,
            config.required_providers
        );
    }
}
