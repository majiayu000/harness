use crate::http::AppState;
use harness_core::{AgentRequest, CodeAgent};
use harness_protocol::{RpcResponse, INTERNAL_ERROR};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

const DEFAULT_MAX_ROUNDS: u32 = 3;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrossReviewResult {
    pub primary_review: String,
    pub challenger_review: String,
    pub consensus_issues: Vec<String>,
    pub contested_issues: Vec<String>,
    pub rounds: u32,
    /// "APPROVED" if no consensus issues; "NOT_CONVERGED" if issues remain.
    pub final_verdict: String,
}

pub async fn cross_review(
    state: &AppState,
    id: Option<serde_json::Value>,
    project_root: PathBuf,
    target: String,
    max_rounds: Option<u32>,
) -> RpcResponse {
    let project_root = match crate::handlers::validate_project_root(&project_root) {
        Ok(p) => p,
        Err(e) => return RpcResponse::error(id, INTERNAL_ERROR, e),
    };

    let primary = match state.server.agent_registry.default_agent() {
        Some(a) => a,
        None => return RpcResponse::error(id, INTERNAL_ERROR, "no agent registered"),
    };

    let challenger = state.server.agent_registry.get("codex");
    let rounds = max_rounds.unwrap_or(DEFAULT_MAX_ROUNDS);

    let result =
        match run_cross_review(primary, challenger, project_root, target, rounds).await {
            Ok(r) => r,
            Err(e) => return RpcResponse::error(id, INTERNAL_ERROR, e),
        };

    match serde_json::to_value(&result) {
        Ok(v) => RpcResponse::success(id, v),
        Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
    }
}

/// Core cross-review orchestration logic, exposed for testing.
///
/// Flow:
/// 1. Primary agent reviews `target`.
/// 2. If no challenger, degrade gracefully to single-model result.
/// 3. Challenger iterates up to `max_rounds - 1` times, classifying each issue as
///    CONFIRMED, FALSE-POSITIVE, or MISSED.
/// 4. Returns APPROVED when no consensus issues remain; NOT_CONVERGED after max rounds.
pub async fn run_cross_review(
    primary: Arc<dyn CodeAgent>,
    challenger: Option<Arc<dyn CodeAgent>>,
    project_root: PathBuf,
    target: String,
    max_rounds: u32,
) -> Result<CrossReviewResult, String> {
    let safe_target = harness_core::prompts::wrap_external_data(&target);
    let primary_prompt = format!(
        "Review the following target for issues:\n{safe_target}\n\n\
         List each issue on its own line prefixed with \"ISSUE: \".\n\
         If no issues are found, output: LGTM"
    );

    let primary_resp = primary
        .execute(AgentRequest {
            prompt: primary_prompt,
            project_root: project_root.clone(),
            ..Default::default()
        })
        .await
        .map_err(|e| e.to_string())?;
    let primary_review = primary_resp.output;

    let challenger = match challenger {
        None => {
            // Graceful degradation: treat all ISSUE lines as consensus.
            let consensus_issues = harness_core::prompts::extract_review_issues(&primary_review);
            let verdict = verdict_for(&consensus_issues);
            return Ok(CrossReviewResult {
                primary_review,
                challenger_review: String::new(),
                consensus_issues,
                contested_issues: Vec::new(),
                rounds: 1,
                final_verdict: verdict,
            });
        }
        Some(c) => c,
    };

    let mut challenger_review = String::new();
    let mut rounds_done = 1u32;
    let mut consensus_issues: Vec<String> =
        harness_core::prompts::extract_review_issues(&primary_review);
    let safe_primary = harness_core::prompts::wrap_external_data(&primary_review);

    for _ in 0..max_rounds.saturating_sub(1) {
        rounds_done += 1;

        // Rebuild prompt each round with outstanding issues from previous round
        let outstanding = if consensus_issues.is_empty() {
            String::new()
        } else {
            let items: String = consensus_issues
                .iter()
                .map(|i| format!("- {i}"))
                .collect::<Vec<_>>()
                .join("\n");
            format!("\n\nOutstanding issues from previous round:\n{items}")
        };

        let challenge_prompt = format!(
            "You are a challenger reviewer. Given this primary code review:\n{safe_primary}\n\n\
             For each ISSUE listed, classify it on its own line:\n\
             CONFIRMED: <issue>  — the issue is a real problem\n\
             FALSE-POSITIVE: <issue>  — the issue is wrong or not applicable\n\
             Also list any issues the primary review missed as:\n\
             MISSED: <new issue>{outstanding}"
        );

        let resp = challenger
            .execute(AgentRequest {
                prompt: challenge_prompt,
                project_root: project_root.clone(),
                ..Default::default()
            })
            .await
            .map_err(|e| e.to_string())?;
        challenger_review = resp.output;

        consensus_issues = extract_tagged(&challenger_review, "CONFIRMED")
            .into_iter()
            .chain(extract_tagged(&challenger_review, "MISSED"))
            .collect();

        if consensus_issues.is_empty() {
            let contested = extract_tagged(&challenger_review, "FALSE-POSITIVE");
            return Ok(CrossReviewResult {
                primary_review,
                challenger_review,
                consensus_issues: Vec::new(),
                contested_issues: contested,
                rounds: rounds_done,
                final_verdict: "APPROVED".to_string(),
            });
        }
    }

    let consensus_issues: Vec<String> = extract_tagged(&challenger_review, "CONFIRMED")
        .into_iter()
        .chain(extract_tagged(&challenger_review, "MISSED"))
        .collect();
    let contested_issues = extract_tagged(&challenger_review, "FALSE-POSITIVE");

    Ok(CrossReviewResult {
        primary_review,
        challenger_review,
        consensus_issues,
        contested_issues,
        rounds: rounds_done,
        final_verdict: "NOT_CONVERGED".to_string(),
    })
}

fn verdict_for(issues: &[String]) -> String {
    if issues.is_empty() {
        "APPROVED".to_string()
    } else {
        "NOT_CONVERGED".to_string()
    }
}

fn extract_tagged(output: &str, tag: &str) -> Vec<String> {
    let prefix = format!("{tag}:");
    output
        .lines()
        .filter_map(|l| {
            l.trim()
                .strip_prefix(prefix.as_str())
                .map(|s| s.trim().to_string())
        })
        .filter(|s| !s.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::{
        AgentRequest, AgentResponse, Capability, CodeAgent, Result as HarnessResult,
        StreamItem, TokenUsage,
    };
    use tokio::sync::mpsc::Sender;

    struct PrimaryMock;

    #[async_trait::async_trait]
    impl CodeAgent for PrimaryMock {
        fn name(&self) -> &str {
            "primary"
        }
        fn capabilities(&self) -> Vec<Capability> {
            vec![]
        }
        async fn execute(&self, _req: AgentRequest) -> HarnessResult<AgentResponse> {
            Ok(AgentResponse {
                output: "ISSUE: Missing error handling\nISSUE: Unbounded loop".to_string(),
                stderr: String::new(),
                items: vec![],
                token_usage: TokenUsage::default(),
                model: "mock".to_string(),
                exit_code: Some(0),
            })
        }
        async fn execute_stream(
            &self,
            _req: AgentRequest,
            _tx: Sender<StreamItem>,
        ) -> HarnessResult<()> {
            Ok(())
        }
    }

    struct ChallengerMock;

    #[async_trait::async_trait]
    impl CodeAgent for ChallengerMock {
        fn name(&self) -> &str {
            "challenger"
        }
        fn capabilities(&self) -> Vec<Capability> {
            vec![]
        }
        async fn execute(&self, _req: AgentRequest) -> HarnessResult<AgentResponse> {
            Ok(AgentResponse {
                output: "CONFIRMED: Missing error handling\nFALSE-POSITIVE: Unbounded loop"
                    .to_string(),
                stderr: String::new(),
                items: vec![],
                token_usage: TokenUsage::default(),
                model: "mock".to_string(),
                exit_code: Some(0),
            })
        }
        async fn execute_stream(
            &self,
            _req: AgentRequest,
            _tx: Sender<StreamItem>,
        ) -> HarnessResult<()> {
            Ok(())
        }
    }

    struct LgtmMock;

    #[async_trait::async_trait]
    impl CodeAgent for LgtmMock {
        fn name(&self) -> &str {
            "lgtm"
        }
        fn capabilities(&self) -> Vec<Capability> {
            vec![]
        }
        async fn execute(&self, _req: AgentRequest) -> HarnessResult<AgentResponse> {
            Ok(AgentResponse {
                output: "LGTM".to_string(),
                stderr: String::new(),
                items: vec![],
                token_usage: TokenUsage::default(),
                model: "mock".to_string(),
                exit_code: Some(0),
            })
        }
        async fn execute_stream(
            &self,
            _req: AgentRequest,
            _tx: Sender<StreamItem>,
        ) -> HarnessResult<()> {
            Ok(())
        }
    }

    fn proj_dir() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("cr-test-")
            .tempdir()
            .expect("create temp dir")
    }

    #[tokio::test]
    async fn two_agents_extract_consensus_and_contested() {
        let proj = proj_dir();
        let result = run_cross_review(
            Arc::new(PrimaryMock),
            Some(Arc::new(ChallengerMock)),
            proj.path().to_path_buf(),
            "fn foo() {}".to_string(),
            3,
        )
        .await
        .expect("run_cross_review should succeed");

        assert_eq!(result.consensus_issues, vec!["Missing error handling"]);
        assert_eq!(result.contested_issues, vec!["Unbounded loop"]);
        assert_eq!(result.final_verdict, "NOT_CONVERGED");
        assert!(result.rounds >= 1);
    }

    #[tokio::test]
    async fn single_agent_graceful_degradation() {
        let proj = proj_dir();
        let result = run_cross_review(
            Arc::new(PrimaryMock),
            None,
            proj.path().to_path_buf(),
            "fn foo() {}".to_string(),
            3,
        )
        .await
        .expect("single-agent should succeed");

        assert_eq!(result.rounds, 1);
        assert_eq!(result.challenger_review, "");
        assert_eq!(
            result.consensus_issues,
            vec!["Missing error handling", "Unbounded loop"]
        );
        assert_eq!(result.final_verdict, "NOT_CONVERGED");
    }

    #[tokio::test]
    async fn approved_when_no_issues() {
        let proj = proj_dir();
        let result = run_cross_review(
            Arc::new(LgtmMock),
            None,
            proj.path().to_path_buf(),
            "fn foo() {}".to_string(),
            3,
        )
        .await
        .expect("lgtm path should succeed");

        assert_eq!(result.final_verdict, "APPROVED");
        assert!(result.consensus_issues.is_empty());
    }
}
