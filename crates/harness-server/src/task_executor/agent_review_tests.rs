use super::agent_review::{jaccard_word_similarity, normalize_issues, run_agent_review};
use crate::task_runner::{TaskId, TaskState, TaskStatus, TaskStore};
use harness_core::agent::{AgentRequest, AgentResponse, CodeAgent, StreamItem};
use harness_core::config::agents::SandboxMode;
use harness_core::interceptor::{InterceptResult, PostExecuteResult, TurnInterceptor};
use harness_core::types::{Capability, ExecutionPhase, TokenUsage};
use harness_observe::event_store::EventStore;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use tokio::time::Duration;

const AGENT_REVIEW_TEST_TIMEOUT: Duration = Duration::from_secs(30);

struct SequenceAgent {
    name: &'static str,
    responses: Mutex<Vec<String>>,
    requests: Mutex<Vec<AgentRequest>>,
}

impl SequenceAgent {
    fn new(name: &'static str, responses: Vec<&str>) -> Self {
        Self {
            name,
            responses: Mutex::new(
                responses
                    .into_iter()
                    .map(std::string::ToString::to_string)
                    .collect(),
            ),
            requests: Mutex::new(Vec::new()),
        }
    }

    async fn next_response(&self) -> String {
        let mut responses = self.responses.lock().await;
        if responses.is_empty() {
            String::new()
        } else {
            responses.remove(0)
        }
    }

    async fn recorded_requests(&self) -> Vec<AgentRequest> {
        self.requests.lock().await.clone()
    }
}

#[async_trait::async_trait]
impl CodeAgent for SequenceAgent {
    fn name(&self) -> &str {
        self.name
    }

    fn capabilities(&self) -> Vec<Capability> {
        Vec::new()
    }

    async fn execute(&self, req: AgentRequest) -> harness_core::error::Result<AgentResponse> {
        self.requests.lock().await.push(req);
        Ok(AgentResponse {
            output: self.next_response().await,
            stderr: String::new(),
            items: Vec::new(),
            token_usage: TokenUsage::default(),
            model: self.name.to_string(),
            exit_code: Some(0),
        })
    }

    async fn execute_stream(
        &self,
        req: AgentRequest,
        tx: tokio::sync::mpsc::Sender<StreamItem>,
    ) -> harness_core::error::Result<()> {
        self.requests.lock().await.push(req);
        let output = self.next_response().await;
        let _ = tx.send(StreamItem::MessageDelta { text: output }).await;
        let _ = tx.send(StreamItem::Done).await;
        Ok(())
    }
}

struct MutatingAgent {
    name: &'static str,
    response: &'static str,
    path: PathBuf,
}

#[async_trait::async_trait]
impl CodeAgent for MutatingAgent {
    fn name(&self) -> &str {
        self.name
    }

    fn capabilities(&self) -> Vec<Capability> {
        Vec::new()
    }

    async fn execute(&self, _req: AgentRequest) -> harness_core::error::Result<AgentResponse> {
        std::fs::write(&self.path, "changed").map_err(|error| {
            harness_core::error::HarnessError::AgentExecution(error.to_string())
        })?;
        Ok(AgentResponse {
            output: self.response.to_string(),
            stderr: String::new(),
            items: Vec::new(),
            token_usage: TokenUsage::default(),
            model: self.name.to_string(),
            exit_code: Some(0),
        })
    }

    async fn execute_stream(
        &self,
        _req: AgentRequest,
        tx: tokio::sync::mpsc::Sender<StreamItem>,
    ) -> harness_core::error::Result<()> {
        std::fs::write(&self.path, "changed").map_err(|error| {
            harness_core::error::HarnessError::AgentExecution(error.to_string())
        })?;
        let _ = tx
            .send(StreamItem::MessageDelta {
                text: self.response.to_string(),
            })
            .await;
        let _ = tx.send(StreamItem::Done).await;
        Ok(())
    }
}

struct CacheWritingPostInterceptor {
    path: PathBuf,
    execution_only: bool,
}

#[async_trait::async_trait]
impl TurnInterceptor for CacheWritingPostInterceptor {
    fn name(&self) -> &str {
        "cache_writing_post"
    }

    async fn pre_execute(&self, _req: &AgentRequest) -> InterceptResult {
        InterceptResult::pass()
    }

    async fn post_execute(&self, req: &AgentRequest, _resp: &AgentResponse) -> PostExecuteResult {
        if self.execution_only && req.execution_phase != Some(ExecutionPhase::Execution) {
            return PostExecuteResult::pass();
        }
        if let Some(parent) = self.path.parent() {
            if let Err(error) = std::fs::create_dir_all(parent) {
                return PostExecuteResult::fail(error.to_string());
            }
        }
        match std::fs::write(&self.path, "cache") {
            Ok(()) => PostExecuteResult::pass(),
            Err(error) => PostExecuteResult::fail(error.to_string()),
        }
    }
}

fn step_tracker(tracker: &mut Option<(Vec<String>, u32)>, issues: &[String]) -> (u32, bool, bool) {
    let normalized = normalize_issues(issues);
    let count = match tracker.as_ref() {
        Some((prev, c)) if *prev == normalized => c + 1,
        _ => 1,
    };
    *tracker = Some((normalized, count));
    let intervention = count >= 3;
    let fatal = count >= 5;
    (count, intervention, fatal)
}

#[tokio::test]
async fn reviewer_request_uses_read_only_network_sandbox() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
    let task_id = TaskId::new();
    store.insert(&TaskState::new(task_id.clone())).await;
    let events = EventStore::new(dir.path()).await?;
    let skills = RwLock::new(harness_skills::store::SkillStore::new());
    let config = harness_core::config::agents::AgentReviewConfig {
        enabled: true,
        max_rounds: 1,
        ..harness_core::config::agents::AgentReviewConfig::default()
    };
    let implementor = SequenceAgent::new("implementor", vec![]);
    let reviewer = SequenceAgent::new("reviewer", vec!["APPROVED"]);
    let mut turns_used = 0;

    let (approved, pushed, approved_head, provider_report) = run_agent_review(
        &store,
        &task_id,
        &implementor,
        &reviewer,
        &config,
        &[],
        dir.path(),
        &[],
        AGENT_REVIEW_TEST_TIMEOUT,
        "https://github.com/owner/repo/pull/1",
        "standard",
        &events,
        &skills,
        &HashMap::new(),
        None,
        Some(super::agent_review::ReviewHeadProbe::Static(Ok(
            "reviewed-sha",
        ))),
        &mut turns_used,
    )
    .await?;

    let reviewer_requests = reviewer.recorded_requests().await;
    assert!(approved);
    assert!(!pushed);
    assert_eq!(
        provider_report
            .as_ref()
            .map(|report| report.report.provider_id.as_str()),
        Some("codex_agent_review")
    );
    assert_eq!(
        approved_head
            .as_ref()
            .and_then(|head| head.as_ref().ok())
            .map(String::as_str),
        Some("reviewed-sha")
    );
    assert_eq!(reviewer_requests.len(), 1);
    assert_eq!(
        reviewer_requests[0].sandbox_mode,
        Some(SandboxMode::ReadOnlyWithNetwork)
    );
    Ok(())
}

#[tokio::test]
async fn claude_reviewer_request_uses_configured_sandbox() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
    let task_id = TaskId::new();
    store.insert(&TaskState::new(task_id.clone())).await;
    let events = EventStore::new(dir.path()).await?;
    let skills = RwLock::new(harness_skills::store::SkillStore::new());
    let config = harness_core::config::agents::AgentReviewConfig {
        enabled: true,
        max_rounds: 1,
        ..harness_core::config::agents::AgentReviewConfig::default()
    };
    let implementor = SequenceAgent::new("implementor", vec![]);
    let reviewer = SequenceAgent::new("claude", vec!["APPROVED"]);
    let mut turns_used = 0;

    let (approved, pushed, _, _) = run_agent_review(
        &store,
        &task_id,
        &implementor,
        &reviewer,
        &config,
        &[],
        dir.path(),
        &[],
        AGENT_REVIEW_TEST_TIMEOUT,
        "https://github.com/owner/repo/pull/1",
        "standard",
        &events,
        &skills,
        &HashMap::new(),
        None,
        None,
        &mut turns_used,
    )
    .await?;

    let reviewer_requests = reviewer.recorded_requests().await;
    assert!(approved);
    assert!(!pushed);
    assert_eq!(reviewer_requests.len(), 1);
    assert_eq!(reviewer_requests[0].sandbox_mode, None);
    Ok(())
}

#[tokio::test]
async fn unresolved_issues_after_max_rounds_fail_local_review() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
    let task_id = TaskId::new();
    store.insert(&TaskState::new(task_id.clone())).await;
    let events = EventStore::new(dir.path()).await?;
    let skills = RwLock::new(harness_skills::store::SkillStore::new());
    let config = harness_core::config::agents::AgentReviewConfig {
        enabled: true,
        max_rounds: 1,
        ..harness_core::config::agents::AgentReviewConfig::default()
    };
    let implementor = SequenceAgent::new("implementor", vec![]);
    let reviewer = SequenceAgent::new("reviewer", vec!["ISSUE: unresolved defect"]);
    let mut turns_used = 0;

    let (approved, pushed, _, _) = run_agent_review(
        &store,
        &task_id,
        &implementor,
        &reviewer,
        &config,
        &[],
        dir.path(),
        &[],
        AGENT_REVIEW_TEST_TIMEOUT,
        "https://github.com/owner/repo/pull/1",
        "standard",
        &events,
        &skills,
        &HashMap::new(),
        None,
        None,
        &mut turns_used,
    )
    .await?;

    let state = store.get(&task_id).expect("task state should be present");
    assert!(!approved);
    assert!(!pushed);
    assert_eq!(state.status, TaskStatus::Failed);
    assert!(
        state
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("exhausted 1 rounds without approval"),
        "unexpected error: {:?}",
        state.error
    );
    Ok(())
}

#[tokio::test]
async fn final_impasse_round_does_not_push_unreviewed_fix() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
    let task_id = TaskId::new();
    store.insert(&TaskState::new(task_id.clone())).await;
    let events = EventStore::new(dir.path()).await?;
    let skills = RwLock::new(harness_skills::store::SkillStore::new());
    let config = harness_core::config::agents::AgentReviewConfig {
        enabled: true,
        max_rounds: 3,
        ..harness_core::config::agents::AgentReviewConfig::default()
    };
    let implementor = SequenceAgent::new(
        "implementor",
        vec![
            "fixed once\nPR_URL=https://github.com/owner/repo/pull/1\nPUSHED_COMMIT=true",
            "fixed twice\nPR_URL=https://github.com/owner/repo/pull/1\nPUSHED_COMMIT=true",
        ],
    );
    let reviewer = SequenceAgent::new(
        "reviewer",
        vec![
            "ISSUE: repeated defect",
            "ISSUE: repeated defect",
            "ISSUE: repeated defect",
        ],
    );
    let mut turns_used = 0;

    let (approved, pushed, _, _) = run_agent_review(
        &store,
        &task_id,
        &implementor,
        &reviewer,
        &config,
        &[],
        dir.path(),
        &[],
        AGENT_REVIEW_TEST_TIMEOUT,
        "https://github.com/owner/repo/pull/1",
        "standard",
        &events,
        &skills,
        &HashMap::new(),
        None,
        None,
        &mut turns_used,
    )
    .await?;

    let state = store.get(&task_id).expect("task state should be present");
    let fix_rounds = state
        .rounds
        .iter()
        .filter(|round| round.action == "agent_review_fix")
        .count();
    assert!(!approved);
    assert!(
        pushed,
        "earlier reviewed fix rounds should still be reported"
    );
    assert_eq!(turns_used, 5, "expected review/fix/review/fix/review");
    assert_eq!(fix_rounds, 2, "final review round must not push a new fix");
    assert_eq!(state.status, TaskStatus::Failed);
    assert!(
        state
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("exhausted 3 rounds without approval"),
        "unexpected error: {:?}",
        state.error
    );
    Ok(())
}

#[tokio::test]
async fn changed_fix_round_requires_pr_head_advance() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let project = dir.path().join("project");
    std::fs::create_dir(&project)?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
    let task_id = TaskId::new();
    store.insert(&TaskState::new(task_id.clone())).await;
    let events = EventStore::new(dir.path()).await?;
    let skills = RwLock::new(harness_skills::store::SkillStore::new());
    let config = harness_core::config::agents::AgentReviewConfig {
        enabled: true,
        max_rounds: 2,
        ..harness_core::config::agents::AgentReviewConfig::default()
    };
    let implementor = MutatingAgent {
        name: "implementor",
        response: "fixed\nPR_URL=https://github.com/owner/repo/pull/1\nPUSHED_COMMIT=true",
        path: project.join("changed.txt"),
    };
    let reviewer = SequenceAgent::new("reviewer", vec!["ISSUE: defect", "APPROVED"]);
    let mut turns_used = 0;

    let (approved, requires_head_advance, _, _) = run_agent_review(
        &store,
        &task_id,
        &implementor,
        &reviewer,
        &config,
        &[],
        &project,
        &[],
        AGENT_REVIEW_TEST_TIMEOUT,
        "https://github.com/owner/repo/pull/1",
        "standard",
        &events,
        &skills,
        &HashMap::new(),
        None,
        None,
        &mut turns_used,
    )
    .await?;

    assert!(approved);
    assert!(
        requires_head_advance,
        "changed local fix must require a new reviewed PR head"
    );
    Ok(())
}

#[tokio::test]
async fn changed_fix_round_rejects_false_noop_marker() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let project = dir.path().join("project");
    std::fs::create_dir(&project)?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
    let task_id = TaskId::new();
    store.insert(&TaskState::new(task_id.clone())).await;
    let events = EventStore::new(dir.path()).await?;
    let skills = RwLock::new(harness_skills::store::SkillStore::new());
    let config = harness_core::config::agents::AgentReviewConfig {
        enabled: true,
        max_rounds: 2,
        ..harness_core::config::agents::AgentReviewConfig::default()
    };
    let implementor = MutatingAgent {
        name: "implementor",
        response: "fixed\nPR_URL=https://github.com/owner/repo/pull/1\nPUSHED_COMMIT=false",
        path: project.join("changed.txt"),
    };
    let reviewer = SequenceAgent::new("reviewer", vec!["ISSUE: defect", "APPROVED"]);
    let mut turns_used = 0;

    let (approved, requires_head_advance, _, _) = run_agent_review(
        &store,
        &task_id,
        &implementor,
        &reviewer,
        &config,
        &[],
        &project,
        &[],
        AGENT_REVIEW_TEST_TIMEOUT,
        "https://github.com/owner/repo/pull/1",
        "standard",
        &events,
        &skills,
        &HashMap::new(),
        None,
        None,
        &mut turns_used,
    )
    .await?;

    let state = store.get(&task_id).expect("task state should be present");
    assert!(!approved);
    assert!(!requires_head_advance);
    assert_eq!(state.status, TaskStatus::Failed);
    assert!(
        state
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("changed the workspace but reported PUSHED_COMMIT=false"),
        "unexpected error: {:?}",
        state.error
    );
    Ok(())
}

#[tokio::test]
async fn noop_fix_round_ignores_validation_cache_artifacts() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let project = dir.path().join("project");
    std::fs::create_dir(&project)?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
    let task_id = TaskId::new();
    store.insert(&TaskState::new(task_id.clone())).await;
    let events = EventStore::new(dir.path()).await?;
    let skills = RwLock::new(harness_skills::store::SkillStore::new());
    let config = harness_core::config::agents::AgentReviewConfig {
        enabled: true,
        max_rounds: 2,
        ..harness_core::config::agents::AgentReviewConfig::default()
    };
    let implementor = SequenceAgent::new(
        "implementor",
        vec!["no source changes\nPR_URL=https://github.com/owner/repo/pull/1\nPUSHED_COMMIT=false"],
    );
    let reviewer = SequenceAgent::new("reviewer", vec!["ISSUE: defect", "APPROVED"]);
    let interceptors: Vec<Arc<dyn TurnInterceptor>> = vec![Arc::new(CacheWritingPostInterceptor {
        path: project.join(".pytest_cache").join("nodeids"),
        execution_only: true,
    })];
    let mut turns_used = 0;

    let (approved, requires_head_advance, _, _) = run_agent_review(
        &store,
        &task_id,
        &implementor,
        &reviewer,
        &config,
        &[],
        &project,
        &interceptors,
        AGENT_REVIEW_TEST_TIMEOUT,
        "https://github.com/owner/repo/pull/1",
        "standard",
        &events,
        &skills,
        &HashMap::new(),
        None,
        None,
        &mut turns_used,
    )
    .await?;

    assert!(approved);
    assert!(
        !requires_head_advance,
        "post-execute cache artifacts must not force PR-head advancement for no-op fixes"
    );
    Ok(())
}

#[tokio::test]
async fn noop_fix_round_rejects_validation_source_artifacts() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let project = dir.path().join("project");
    std::fs::create_dir(&project)?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
    let task_id = TaskId::new();
    store.insert(&TaskState::new(task_id.clone())).await;
    let events = EventStore::new(dir.path()).await?;
    let skills = RwLock::new(harness_skills::store::SkillStore::new());
    let config = harness_core::config::agents::AgentReviewConfig {
        enabled: true,
        max_rounds: 2,
        ..harness_core::config::agents::AgentReviewConfig::default()
    };
    let implementor = SequenceAgent::new(
        "implementor",
        vec!["no source changes\nPR_URL=https://github.com/owner/repo/pull/1\nPUSHED_COMMIT=false"],
    );
    let reviewer = SequenceAgent::new("reviewer", vec!["ISSUE: defect", "APPROVED"]);
    let interceptors: Vec<Arc<dyn TurnInterceptor>> = vec![Arc::new(CacheWritingPostInterceptor {
        path: project.join("src").join("formatted.rs"),
        execution_only: true,
    })];
    let mut turns_used = 0;

    let (approved, requires_head_advance, _, _) = run_agent_review(
        &store,
        &task_id,
        &implementor,
        &reviewer,
        &config,
        &[],
        &project,
        &interceptors,
        AGENT_REVIEW_TEST_TIMEOUT,
        "https://github.com/owner/repo/pull/1",
        "standard",
        &events,
        &skills,
        &HashMap::new(),
        None,
        None,
        &mut turns_used,
    )
    .await?;

    let state = store.get(&task_id).expect("task state should be present");
    assert!(!approved);
    assert!(!requires_head_advance);
    assert_eq!(state.status, TaskStatus::Failed);
    assert!(
        state
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("changed the workspace but reported PUSHED_COMMIT=false"),
        "unexpected error: {:?}",
        state.error
    );
    Ok(())
}

#[tokio::test]
async fn malformed_reviewer_output_fails_local_review() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
    let task_id = TaskId::new();
    store.insert(&TaskState::new(task_id.clone())).await;
    let events = EventStore::new(dir.path()).await?;
    let skills = RwLock::new(harness_skills::store::SkillStore::new());
    let config = harness_core::config::agents::AgentReviewConfig {
        enabled: true,
        max_rounds: 1,
        ..harness_core::config::agents::AgentReviewConfig::default()
    };
    let implementor = SequenceAgent::new("implementor", vec![]);
    let reviewer = SequenceAgent::new("reviewer", vec!["looks fine to me"]);
    let mut turns_used = 0;

    let (approved, _, _, _) = run_agent_review(
        &store,
        &task_id,
        &implementor,
        &reviewer,
        &config,
        &[],
        dir.path(),
        &[],
        AGENT_REVIEW_TEST_TIMEOUT,
        "https://github.com/owner/repo/pull/1",
        "standard",
        &events,
        &skills,
        &HashMap::new(),
        None,
        None,
        &mut turns_used,
    )
    .await?;

    let state = store.get(&task_id).expect("task state should be present");
    assert!(!approved);
    assert_eq!(state.status, TaskStatus::Failed);
    assert!(
        state
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("neither APPROVED nor ISSUE"),
        "unexpected error: {:?}",
        state.error
    );
    Ok(())
}

#[test]
fn normalize_issues_is_order_invariant() {
    let ordered = vec!["issue A".to_string(), "issue B".to_string()];
    let reversed = vec!["issue B".to_string(), "issue A".to_string()];
    assert_eq!(normalize_issues(&ordered), normalize_issues(&reversed));
}

#[test]
fn impasse_no_intervention_for_first_two_rounds() {
    let issues = vec!["null pointer".to_string()];
    let mut tracker: Option<(Vec<String>, u32)> = None;
    let (c1, i1, f1) = step_tracker(&mut tracker, &issues);
    let (c2, i2, f2) = step_tracker(&mut tracker, &issues);
    assert_eq!(c1, 1);
    assert!(!i1 && !f1, "no action on first occurrence");
    assert_eq!(c2, 2);
    assert!(!i2 && !f2, "no action on second occurrence");
}

#[test]
fn impasse_intervention_at_third_consecutive_round() {
    let issues = vec!["null pointer".to_string()];
    let mut tracker: Option<(Vec<String>, u32)> = None;
    step_tracker(&mut tracker, &issues);
    step_tracker(&mut tracker, &issues);
    let (c3, i3, f3) = step_tracker(&mut tracker, &issues);
    assert_eq!(c3, 3);
    assert!(i3, "intervention at 3rd consecutive round");
    assert!(!f3, "not yet fatal at round 3");
}

#[test]
fn impasse_fatal_at_fifth_consecutive_round() {
    let issues = vec!["null pointer".to_string()];
    let mut tracker: Option<(Vec<String>, u32)> = None;
    for _ in 0..4 {
        step_tracker(&mut tracker, &issues);
    }
    let (c5, i5, f5) = step_tracker(&mut tracker, &issues);
    assert_eq!(c5, 5);
    assert!(i5, "intervention still active at round 5");
    assert!(f5, "fatal at 5th consecutive round");
}

#[test]
fn impasse_counter_resets_when_issues_change() {
    let issues = vec!["null pointer".to_string()];
    let other = vec!["different bug".to_string()];
    let mut tracker: Option<(Vec<String>, u32)> = None;
    step_tracker(&mut tracker, &issues);
    step_tracker(&mut tracker, &issues);
    step_tracker(&mut tracker, &issues);
    let (c_reset, i_reset, _) = step_tracker(&mut tracker, &other);
    assert_eq!(c_reset, 1, "counter resets on different issues");
    assert!(!i_reset, "no intervention after reset");
}

#[test]
fn jaccard_identical_strings() {
    assert_eq!(jaccard_word_similarity("hello world", "hello world"), 1.0);
}

#[test]
fn jaccard_disjoint_strings() {
    assert_eq!(jaccard_word_similarity("foo bar", "baz qux"), 0.0);
}

#[test]
fn jaccard_partial_overlap() {
    let score = jaccard_word_similarity("a b", "b c");
    let expected = 1.0_f64 / 3.0_f64;
    assert!(
        (score - expected).abs() < 1e-10,
        "expected ~{expected}, got {score}"
    );
}

#[test]
fn jaccard_one_empty() {
    assert_eq!(jaccard_word_similarity("", "hello world"), 0.0);
    assert_eq!(jaccard_word_similarity("hello world", ""), 0.0);
}

#[test]
fn jaccard_both_empty() {
    assert_eq!(jaccard_word_similarity("", ""), 1.0);
}
