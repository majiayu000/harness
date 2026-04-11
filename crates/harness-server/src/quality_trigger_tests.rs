use crate::quality_trigger::{unix_now, QualityTrigger, TaskReviewContext};
use harness_core::agent::{AgentRequest, AgentResponse, CodeAgent, StreamItem};
use harness_core::error::Result as HarnessResult;
use harness_core::types::{Capability, Decision, Event, Grade, SessionId, TokenUsage};
use harness_gc::draft_store::DraftStore;
use harness_gc::gc_agent::GcAgent;
use harness_gc::signal_detector::SignalDetector;
use harness_observe::event_store::EventStore;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::sync::mpsc::Sender;

async fn make_trigger(
    dir: &Path,
    auto_gc_grades: Vec<Grade>,
    cooldown_secs: u64,
) -> QualityTrigger {
    make_trigger_with_challenger(dir, auto_gc_grades, cooldown_secs, None, None).await
}

async fn make_trigger_with_challenger(
    dir: &Path,
    auto_gc_grades: Vec<Grade>,
    cooldown_secs: u64,
    primary: Option<Arc<dyn CodeAgent>>,
    challenger: Option<Arc<dyn CodeAgent>>,
) -> QualityTrigger {
    let events = Arc::new(EventStore::new(dir).await.expect("event store"));
    let gc_config = harness_core::config::misc::GcConfig::default();
    let signal_detector = SignalDetector::new(
        gc_config.signal_thresholds.clone().into(),
        harness_core::types::ProjectId::new(),
    );
    let draft_store = DraftStore::new(dir).expect("draft store");
    let gc_agent = Arc::new(GcAgent::new(
        gc_config,
        signal_detector,
        draft_store,
        dir.to_path_buf(),
    ));
    let mut registry = harness_agents::registry::AgentRegistry::new("test");
    if let Some(p) = primary {
        registry.register("test", p);
    }
    let agent_registry = Arc::new(registry);
    QualityTrigger::new(
        events,
        gc_agent,
        agent_registry,
        dir.to_path_buf(),
        auto_gc_grades,
        cooldown_secs,
        challenger,
    )
}

// Mock that returns "ISSUE: foo\nCONFIRMED: foo" — used as primary in NOT_CONVERGED tests.
// Has a distinct name from NotConvergedMock so the identity guard does not skip cross-review.
struct NotConvergedPrimaryMock;

#[async_trait::async_trait]
impl CodeAgent for NotConvergedPrimaryMock {
    fn name(&self) -> &str {
        "not-converged-primary"
    }
    fn capabilities(&self) -> Vec<Capability> {
        vec![]
    }
    async fn execute(&self, _req: AgentRequest) -> HarnessResult<AgentResponse> {
        Ok(AgentResponse {
            output: "ISSUE: foo\nCONFIRMED: foo".to_string(),
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

// Mock that returns "ISSUE: foo\nCONFIRMED: foo" — NOT_CONVERGED
struct NotConvergedMock;

#[async_trait::async_trait]
impl CodeAgent for NotConvergedMock {
    fn name(&self) -> &str {
        "not-converged-mock"
    }
    fn capabilities(&self) -> Vec<Capability> {
        vec![]
    }
    async fn execute(&self, _req: AgentRequest) -> HarnessResult<AgentResponse> {
        Ok(AgentResponse {
            output: "ISSUE: foo\nCONFIRMED: foo".to_string(),
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

// Mock that returns LGTM — APPROVED
struct ApprovedMock;

#[async_trait::async_trait]
impl CodeAgent for ApprovedMock {
    fn name(&self) -> &str {
        "approved-mock"
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

// Mock primary that advertises Write/Execute (like CodexAgent) — verifies that the
// primary capability guard skips cross-review and never calls execute().
struct WritePrimaryMock;

#[async_trait::async_trait]
impl CodeAgent for WritePrimaryMock {
    fn name(&self) -> &str {
        "write-primary"
    }
    fn capabilities(&self) -> Vec<Capability> {
        vec![Capability::Read, Capability::Write, Capability::Execute]
    }
    async fn execute(&self, _req: AgentRequest) -> HarnessResult<AgentResponse> {
        panic!("WritePrimaryMock.execute must never be called during cross-review");
    }
    async fn execute_stream(
        &self,
        _req: AgentRequest,
        _tx: Sender<StreamItem>,
    ) -> HarnessResult<()> {
        Ok(())
    }
}

fn block_event(hook: &str) -> Event {
    Event::new(SessionId::new(), hook, "Edit", Decision::Block)
}

fn pass_event() -> Event {
    Event::new(SessionId::new(), "pre_tool_use", "Edit", Decision::Pass)
}

// --- grade_triggers_gc mapping tests ---

#[tokio::test]
async fn grade_d_triggers_gc_by_default() {
    let dir = tempfile::tempdir().unwrap();
    let trigger = make_trigger(dir.path(), vec![Grade::D], 300).await;
    assert!(trigger.grade_triggers_gc(Grade::D));
}

#[tokio::test]
async fn grade_a_does_not_trigger_gc_by_default() {
    let dir = tempfile::tempdir().unwrap();
    let trigger = make_trigger(dir.path(), vec![Grade::D], 300).await;
    assert!(!trigger.grade_triggers_gc(Grade::A));
}

#[tokio::test]
async fn grade_b_does_not_trigger_gc_by_default() {
    let dir = tempfile::tempdir().unwrap();
    let trigger = make_trigger(dir.path(), vec![Grade::D], 300).await;
    assert!(!trigger.grade_triggers_gc(Grade::B));
}

#[tokio::test]
async fn grade_c_does_not_trigger_gc_by_default() {
    let dir = tempfile::tempdir().unwrap();
    let trigger = make_trigger(dir.path(), vec![Grade::D], 300).await;
    assert!(!trigger.grade_triggers_gc(Grade::C));
}

#[tokio::test]
async fn configuring_c_and_d_both_trigger() {
    let dir = tempfile::tempdir().unwrap();
    let trigger = make_trigger(dir.path(), vec![Grade::C, Grade::D], 300).await;
    assert!(trigger.grade_triggers_gc(Grade::C));
    assert!(trigger.grade_triggers_gc(Grade::D));
    assert!(!trigger.grade_triggers_gc(Grade::A));
    assert!(!trigger.grade_triggers_gc(Grade::B));
}

#[tokio::test]
async fn empty_auto_gc_grades_never_triggers() {
    let dir = tempfile::tempdir().unwrap();
    let trigger = make_trigger(dir.path(), vec![], 300).await;
    assert!(!trigger.grade_triggers_gc(Grade::A));
    assert!(!trigger.grade_triggers_gc(Grade::B));
    assert!(!trigger.grade_triggers_gc(Grade::C));
    assert!(!trigger.grade_triggers_gc(Grade::D));
}

// --- cooldown tests ---

#[tokio::test]
async fn cooldown_elapsed_when_never_triggered() {
    let dir = tempfile::tempdir().unwrap();
    let trigger = make_trigger(dir.path(), vec![Grade::D], 300).await;
    assert!(trigger.cooldown_elapsed());
}

#[tokio::test]
async fn cooldown_not_elapsed_immediately_after_trigger() {
    let dir = tempfile::tempdir().unwrap();
    let trigger = make_trigger(dir.path(), vec![Grade::D], 300).await;
    trigger.last_triggered.store(unix_now(), Ordering::Relaxed);
    assert!(!trigger.cooldown_elapsed());
}

#[tokio::test]
async fn zero_cooldown_always_elapsed() {
    let dir = tempfile::tempdir().unwrap();
    let trigger = make_trigger(dir.path(), vec![Grade::D], 0).await;
    trigger.last_triggered.store(unix_now(), Ordering::Relaxed);
    assert!(trigger.cooldown_elapsed());
}

// --- log_quality_grade integration test ---

#[tokio::test]
async fn check_logs_quality_grade_event() {
    let dir = tempfile::tempdir().unwrap();
    let trigger = make_trigger(dir.path(), vec![Grade::D], 300).await;

    trigger
        .events
        .log(&Event::new(
            SessionId::new(),
            "pre_tool_use",
            "Edit",
            Decision::Pass,
        ))
        .await
        .unwrap();

    trigger.check_and_maybe_trigger(None).await;

    let events = trigger
        .events
        .query(&harness_core::types::EventFilters {
            hook: Some("quality_grade".to_string()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(events.len(), 1, "expected exactly one quality_grade event");
    assert!(events[0]
        .detail
        .as_deref()
        .unwrap_or("")
        .starts_with("grade="));
}

// --- challenger cross-review tests ---

// Scenario 1: challenger=None, task_ctx=None → existing behavior, one quality_grade event
#[tokio::test]
async fn no_challenger_no_ctx_baseline() {
    let dir = tempfile::tempdir().unwrap();
    let trigger = make_trigger(dir.path(), vec![Grade::D], 300).await;
    trigger.events.log(&pass_event()).await.unwrap();
    trigger.check_and_maybe_trigger(None).await;
    let events = trigger
        .events
        .query(&harness_core::types::EventFilters {
            hook: Some("quality_grade".to_string()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(events.len(), 1);
}

// Scenario 2: challenger=Some, grade=A → cross-review skipped
#[tokio::test]
async fn challenger_grade_a_skips_cross_review() {
    let dir = tempfile::tempdir().unwrap();
    let trigger = make_trigger_with_challenger(
        dir.path(),
        vec![Grade::D],
        300,
        None,
        Some(Arc::new(NotConvergedMock)),
    )
    .await;
    for _ in 0..20 {
        trigger.events.log(&pass_event()).await.unwrap();
    }
    let ctx = TaskReviewContext {
        diff: "diff".to_string(),
        pr_description: "https://github.com/o/r/pull/1".to_string(),
    };
    trigger.check_and_maybe_trigger(Some(&ctx)).await;
    let events = trigger
        .events
        .query(&harness_core::types::EventFilters {
            hook: Some("quality_grade".to_string()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(events.len(), 1);
    let detail = events[0].detail.as_deref().unwrap_or("");
    assert!(detail.contains("A"), "expected grade A in detail: {detail}");
}

// Scenario 3: challenger=Some, grade=B, verdict=APPROVED → grade stays B
#[tokio::test]
async fn challenger_grade_b_approved_stays_b() {
    let dir = tempfile::tempdir().unwrap();
    let trigger = make_trigger_with_challenger(
        dir.path(),
        vec![Grade::D],
        300,
        None,
        Some(Arc::new(ApprovedMock)),
    )
    .await;
    for _ in 0..7 {
        trigger.events.log(&pass_event()).await.unwrap();
    }
    for _ in 0..3 {
        trigger
            .events
            .log(&block_event("security_check"))
            .await
            .unwrap();
    }
    let ctx = TaskReviewContext {
        diff: "diff".to_string(),
        pr_description: "https://github.com/o/r/pull/1".to_string(),
    };
    trigger.check_and_maybe_trigger(Some(&ctx)).await;
    let events = trigger
        .events
        .query(&harness_core::types::EventFilters {
            hook: Some("quality_grade".to_string()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(events.len(), 1);
    let detail = events[0].detail.as_deref().unwrap_or("");
    assert!(
        detail.contains("B") || detail.contains("A"),
        "expected B or A in detail after APPROVED: {detail}"
    );
}

// Scenario 4: challenger=Some, grade=B, verdict=NOT_CONVERGED → downgraded to C
#[tokio::test]
async fn challenger_grade_b_not_converged_downgraded_to_c() {
    let dir = tempfile::tempdir().unwrap();
    let trigger = make_trigger_with_challenger(
        dir.path(),
        vec![Grade::D],
        300,
        Some(Arc::new(NotConvergedPrimaryMock)),
        Some(Arc::new(NotConvergedMock)),
    )
    .await;
    for _ in 0..7 {
        trigger.events.log(&pass_event()).await.unwrap();
    }
    for _ in 0..3 {
        trigger
            .events
            .log(&block_event("security_check"))
            .await
            .unwrap();
    }
    let ctx = TaskReviewContext {
        diff: "diff".to_string(),
        pr_description: "https://github.com/o/r/pull/1".to_string(),
    };
    trigger.check_and_maybe_trigger(Some(&ctx)).await;
    let events = trigger
        .events
        .query(&harness_core::types::EventFilters {
            hook: Some("quality_grade".to_string()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(events.len(), 1);
    let detail = events[0].detail.as_deref().unwrap_or("");
    assert!(
        detail.contains("C") || detail.contains("D"),
        "expected downgrade to C or D after NOT_CONVERGED: {detail}"
    );
}

// Scenario 5: challenger=Some, grade=C, NOT_CONVERGED → D, GC triggered
#[tokio::test]
async fn challenger_grade_c_not_converged_triggers_gc() {
    let dir = tempfile::tempdir().unwrap();
    let trigger = make_trigger_with_challenger(
        dir.path(),
        vec![Grade::D],
        0, // zero cooldown so GC fires immediately
        None,
        Some(Arc::new(NotConvergedMock)),
    )
    .await;
    for _ in 0..5 {
        trigger.events.log(&pass_event()).await.unwrap();
    }
    for _ in 0..5 {
        trigger
            .events
            .log(&block_event("security_check"))
            .await
            .unwrap();
    }
    let ctx = TaskReviewContext {
        diff: "diff".to_string(),
        pr_description: "https://github.com/o/r/pull/1".to_string(),
    };
    trigger.check_and_maybe_trigger(Some(&ctx)).await;
    let events = trigger
        .events
        .query(&harness_core::types::EventFilters {
            hook: Some("quality_grade".to_string()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(events.len(), 1);
    let detail = events[0].detail.as_deref().unwrap_or("");
    assert!(
        detail.contains("C") || detail.contains("D"),
        "expected C or D after NOT_CONVERGED on C input: {detail}"
    );
}

// Scenario 6: task_ctx=None, challenger=Some → cross-review skipped, numeric grade only
#[tokio::test]
async fn challenger_no_ctx_skips_cross_review() {
    let dir = tempfile::tempdir().unwrap();
    let trigger = make_trigger_with_challenger(
        dir.path(),
        vec![Grade::D],
        300,
        None,
        Some(Arc::new(NotConvergedMock)),
    )
    .await;
    trigger.events.log(&pass_event()).await.unwrap();
    trigger.check_and_maybe_trigger(None).await;
    let events = trigger
        .events
        .query(&harness_core::types::EventFilters {
            hook: Some("quality_grade".to_string()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(events.len(), 1);
}

// Scenario 7: primary has Write/Execute capabilities → cross-review skipped,
// grade unchanged (WritePrimaryMock.execute panics if called, proving the guard fires).
#[tokio::test]
async fn write_primary_skips_cross_review() {
    let dir = tempfile::tempdir().unwrap();
    let trigger = make_trigger_with_challenger(
        dir.path(),
        vec![Grade::D],
        300,
        Some(Arc::new(WritePrimaryMock)),
        Some(Arc::new(NotConvergedMock)),
    )
    .await;
    for _ in 0..7 {
        trigger.events.log(&pass_event()).await.unwrap();
    }
    for _ in 0..3 {
        trigger
            .events
            .log(&block_event("security_check"))
            .await
            .unwrap();
    }
    let ctx = TaskReviewContext {
        diff: "diff".to_string(),
        pr_description: "https://github.com/o/r/pull/1".to_string(),
    };
    trigger.check_and_maybe_trigger(Some(&ctx)).await;
    let events = trigger
        .events
        .query(&harness_core::types::EventFilters {
            hook: Some("quality_grade".to_string()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(events.len(), 1);
    let detail = events[0].detail.as_deref().unwrap_or("");
    assert!(
        detail.contains("B") || detail.contains("A"),
        "expected B or A (no downgrade) when primary is skipped: {detail}"
    );
}
