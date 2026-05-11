use super::super::*;
use async_trait::async_trait;
use harness_core::agent::{AgentRequest, AgentResponse, StreamItem};
use harness_core::types::{Capability, ContextItem, ExecutionPhase, TokenUsage};
use tokio::time::{sleep, Duration, Instant};

pub(super) struct CapturingAgent {
    pub(super) captured: tokio::sync::Mutex<Vec<ContextItem>>,
}

impl CapturingAgent {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            captured: tokio::sync::Mutex::new(Vec::new()),
        })
    }
}

pub(super) async fn wait_until(
    timeout: Duration,
    mut predicate: impl FnMut() -> bool,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if predicate() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            anyhow::bail!("condition not met within {:?}", timeout);
        }
        sleep(Duration::from_millis(25)).await;
    }
}

#[async_trait]
impl harness_core::agent::CodeAgent for CapturingAgent {
    fn name(&self) -> &str {
        "capturing-mock"
    }

    fn capabilities(&self) -> Vec<Capability> {
        vec![]
    }

    async fn execute(&self, req: AgentRequest) -> harness_core::error::Result<AgentResponse> {
        let mut guard = self.captured.lock().await;
        if guard.is_empty() {
            *guard = req.context.clone();
        }
        Ok(AgentResponse {
            output: String::new(),
            stderr: String::new(),
            items: vec![],
            token_usage: TokenUsage {
                input_tokens: 0,
                output_tokens: 0,
                total_tokens: 0,
                cost_usd: 0.0,
            },
            model: "mock".into(),
            exit_code: Some(0),
        })
    }

    async fn execute_stream(
        &self,
        req: AgentRequest,
        _tx: tokio::sync::mpsc::Sender<StreamItem>,
    ) -> harness_core::error::Result<()> {
        // Mirror execute(): capture context on first call so tests that
        // verify skill injection work whether execute or execute_stream is called.
        let mut guard = self.captured.lock().await;
        if guard.is_empty() {
            *guard = req.context.clone();
        }
        Ok(())
    }
}

pub(super) struct BlockingInterceptor;

#[async_trait]
impl harness_core::interceptor::TurnInterceptor for BlockingInterceptor {
    fn name(&self) -> &str {
        "blocking-test"
    }

    async fn pre_execute(&self, _req: &AgentRequest) -> harness_core::interceptor::InterceptResult {
        harness_core::interceptor::InterceptResult::block("test block")
    }
}

/// Mock agent that records the `execution_phase` from every call and
/// returns pre-configured responses in order.
pub(super) struct PhaseCapturingAgent {
    phases: tokio::sync::Mutex<Vec<Option<ExecutionPhase>>>,
    prompts: tokio::sync::Mutex<Vec<String>>,
    responses: tokio::sync::Mutex<Vec<String>>,
}

impl PhaseCapturingAgent {
    pub(super) fn new(responses: Vec<String>) -> Arc<Self> {
        Arc::new(Self {
            phases: tokio::sync::Mutex::new(Vec::new()),
            prompts: tokio::sync::Mutex::new(Vec::new()),
            responses: tokio::sync::Mutex::new(responses),
        })
    }

    pub(super) async fn captured_phases(&self) -> Vec<Option<ExecutionPhase>> {
        self.phases.lock().await.clone()
    }

    pub(super) async fn captured_prompts(&self) -> Vec<String> {
        self.prompts.lock().await.clone()
    }

    async fn next_response(&self) -> String {
        let mut guard = self.responses.lock().await;
        if guard.is_empty() {
            String::new()
        } else {
            guard.remove(0)
        }
    }
}

pub(super) async fn wait_for_captured_phases(
    agent: &PhaseCapturingAgent,
    min_count: usize,
) -> Vec<Option<ExecutionPhase>> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let phases = agent.captured_phases().await;
        if phases.len() >= min_count || Instant::now() >= deadline {
            return phases;
        }
        sleep(Duration::from_millis(50)).await;
    }
}

pub(super) async fn wait_for_captured_prompts(
    agent: &PhaseCapturingAgent,
    min_count: usize,
) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let prompts = agent.captured_prompts().await;
        if prompts.len() >= min_count || Instant::now() >= deadline {
            return prompts;
        }
        sleep(Duration::from_millis(50)).await;
    }
}

#[async_trait]
impl harness_core::agent::CodeAgent for PhaseCapturingAgent {
    fn name(&self) -> &str {
        "phase-capturing-mock"
    }

    fn capabilities(&self) -> Vec<Capability> {
        vec![]
    }

    async fn execute(&self, req: AgentRequest) -> harness_core::error::Result<AgentResponse> {
        self.phases.lock().await.push(req.execution_phase);
        self.prompts.lock().await.push(req.prompt.clone());
        let output = self.next_response().await;
        Ok(AgentResponse {
            output,
            stderr: String::new(),
            items: vec![],
            token_usage: TokenUsage::default(),
            model: "mock".into(),
            exit_code: Some(0),
        })
    }

    async fn execute_stream(
        &self,
        req: AgentRequest,
        tx: tokio::sync::mpsc::Sender<StreamItem>,
    ) -> harness_core::error::Result<()> {
        self.phases.lock().await.push(req.execution_phase);
        self.prompts.lock().await.push(req.prompt.clone());
        let output = self.next_response().await;
        if !output.is_empty() {
            if let Err(e) = tx.send(StreamItem::MessageDelta { text: output }).await {
                tracing::warn!("PhaseCapturingAgent: failed to send MessageDelta: {e}");
            }
        }
        if let Err(e) = tx.send(StreamItem::Done).await {
            tracing::warn!("PhaseCapturingAgent: failed to send Done: {e}");
        }
        Ok(())
    }
}
