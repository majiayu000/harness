use crate::otel_attributes::{
    gen_ai_turn_attributes, GenAiTurnAttributes, HARNESS_ACTIVITY_KIND, HARNESS_OUTCOME,
    HARNESS_RETRY_ATTEMPT, HARNESS_RUNTIME_JOB_ID, HARNESS_WORKFLOW_ID,
};
use chrono::{DateTime, Utc};
use opentelemetry::trace::{
    Span, SpanBuilder, SpanContext, SpanId, SpanKind, Status, TraceContextExt, TraceFlags, TraceId,
    TraceState, Tracer,
};
use opentelemetry::{Context, KeyValue};
use sha2::{Digest, Sha256};
use std::time::SystemTime;

const WORKFLOW_SPAN_NAME: &str = "harness.workflow";
const ACTIVITY_SPAN_NAME: &str = "harness.activity";
const AGENT_TURN_SPAN_NAME: &str = "gen_ai.client.operation";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrajectoryTraceContext {
    pub trace_id: String,
    pub root_span_id: String,
}

impl TrajectoryTraceContext {
    pub fn new(trace_id: impl Into<String>, root_span_id: impl Into<String>) -> Option<Self> {
        let context = Self {
            trace_id: trace_id.into(),
            root_span_id: root_span_id.into(),
        };
        context.is_valid().then_some(context)
    }

    pub fn is_valid(&self) -> bool {
        parse_trace_id(&self.trace_id).is_some() && parse_span_id(&self.root_span_id).is_some()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowRootSpan {
    pub trace_context: TrajectoryTraceContext,
    pub workflow_id: String,
    pub definition_id: String,
    pub subject_type: String,
    pub subject_key: String,
    pub outcome: String,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ActivitySpan {
    pub trace_context: TrajectoryTraceContext,
    pub workflow_id: String,
    pub runtime_job_id: String,
    pub activity_kind: String,
    pub outcome: String,
    pub retry_attempt: Option<u64>,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentTurnSpan {
    pub trace_context: TrajectoryTraceContext,
    pub workflow_id: String,
    pub runtime_job_id: String,
    pub activity_kind: String,
    pub outcome: String,
    pub system: String,
    pub model: Option<String>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cost_usd: Option<f64>,
    pub thread_id: String,
    pub turn_id: String,
    pub retry_attempt: Option<u64>,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
}

pub(crate) fn record_workflow_root_span<T>(tracer: &T, input: WorkflowRootSpan)
where
    T: Tracer,
{
    let Some(trace_id) = parse_trace_id(&input.trace_context.trace_id) else {
        return;
    };
    let Some(root_span_id) = parse_span_id(&input.trace_context.root_span_id) else {
        return;
    };
    let attrs = vec![
        KeyValue::new(HARNESS_WORKFLOW_ID, input.workflow_id),
        KeyValue::new("harness.workflow.definition_id", input.definition_id),
        KeyValue::new("harness.workflow.subject_type", input.subject_type),
        KeyValue::new("harness.workflow.subject_key", input.subject_key),
        KeyValue::new(HARNESS_OUTCOME, input.outcome),
    ];
    let mut span = span_builder(WORKFLOW_SPAN_NAME, attrs, input.started_at, input.ended_at)
        .with_trace_id(trace_id)
        .with_span_id(root_span_id)
        .start(tracer);
    span.end();
}

pub(crate) fn record_activity_span<T>(tracer: &T, input: ActivitySpan)
where
    T: Tracer,
{
    let Some(activity_span_id) = activity_span_id(&input.runtime_job_id) else {
        return;
    };
    let Some(parent) = root_parent_context(&input.trace_context) else {
        return;
    };
    let mut attrs = vec![
        KeyValue::new(HARNESS_WORKFLOW_ID, input.workflow_id),
        KeyValue::new(HARNESS_RUNTIME_JOB_ID, input.runtime_job_id.clone()),
        KeyValue::new(HARNESS_ACTIVITY_KIND, input.activity_kind),
        KeyValue::new(HARNESS_OUTCOME, input.outcome),
    ];
    if let Some(retry_attempt) = input
        .retry_attempt
        .and_then(|value| i64::try_from(value).ok())
    {
        attrs.push(KeyValue::new(HARNESS_RETRY_ATTEMPT, retry_attempt));
    }
    let mut span = span_builder(ACTIVITY_SPAN_NAME, attrs, input.started_at, input.ended_at)
        .with_span_id(activity_span_id)
        .start_with_context(tracer, &parent);
    span.end();
}

pub(crate) fn record_agent_turn_span<T>(tracer: &T, input: AgentTurnSpan)
where
    T: Tracer,
{
    let Some(parent) = activity_parent_context(&input.trace_context, &input.runtime_job_id) else {
        return;
    };
    let attrs = gen_ai_turn_attributes(GenAiTurnAttributes {
        system: Some(input.system),
        model: input.model,
        input_tokens: input.input_tokens,
        output_tokens: input.output_tokens,
        cost_usd: input.cost_usd,
        workflow_id: Some(input.workflow_id),
        activity_kind: Some(input.activity_kind),
        outcome: Some(input.outcome),
        runtime_job_id: Some(input.runtime_job_id),
        thread_id: Some(input.thread_id.clone()),
        turn_id: Some(input.turn_id.clone()),
        retry_attempt: input.retry_attempt,
    });
    let Some(turn_span_id) = span_id_from_seed(format!("turn:{}", input.turn_id)) else {
        return;
    };
    let mut span = span_builder(
        AGENT_TURN_SPAN_NAME,
        attrs,
        input.started_at,
        input.ended_at,
    )
    .with_span_id(turn_span_id)
    .start_with_context(tracer, &parent);
    span.end();
}

fn span_builder(
    name: &'static str,
    attrs: Vec<KeyValue>,
    started_at: Option<DateTime<Utc>>,
    ended_at: Option<DateTime<Utc>>,
) -> SpanBuilder {
    let mut builder = SpanBuilder::from_name(name)
        .with_kind(SpanKind::Internal)
        .with_attributes(attrs)
        .with_status(Status::Unset);
    if let Some(started_at) = started_at {
        builder = builder.with_start_time(SystemTime::from(started_at));
    }
    if let Some(ended_at) = ended_at {
        builder = builder.with_end_time(SystemTime::from(ended_at));
    }
    builder
}

fn root_parent_context(trace_context: &TrajectoryTraceContext) -> Option<Context> {
    Some(remote_parent_context(
        parse_trace_id(&trace_context.trace_id)?,
        parse_span_id(&trace_context.root_span_id)?,
    ))
}

fn activity_parent_context(
    trace_context: &TrajectoryTraceContext,
    runtime_job_id: &str,
) -> Option<Context> {
    Some(remote_parent_context(
        parse_trace_id(&trace_context.trace_id)?,
        activity_span_id(runtime_job_id)?,
    ))
}

fn remote_parent_context(trace_id: TraceId, span_id: SpanId) -> Context {
    Context::new().with_remote_span_context(SpanContext::new(
        trace_id,
        span_id,
        TraceFlags::SAMPLED,
        true,
        TraceState::default(),
    ))
}

fn activity_span_id(runtime_job_id: &str) -> Option<SpanId> {
    span_id_from_seed(format!("activity:{runtime_job_id}"))
}

fn span_id_from_seed(seed: impl AsRef<[u8]>) -> Option<SpanId> {
    let digest = Sha256::digest(seed.as_ref());
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    if bytes.iter().all(|byte| *byte == 0) {
        bytes[7] = 1;
    }
    Some(SpanId::from_bytes(bytes))
}

fn parse_trace_id(value: &str) -> Option<TraceId> {
    TraceId::from_hex(value)
        .ok()
        .filter(|trace_id| *trace_id != TraceId::INVALID)
}

fn parse_span_id(value: &str) -> Option<SpanId> {
    SpanId::from_hex(value)
        .ok()
        .filter(|span_id| *span_id != SpanId::INVALID)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trajectory_trace_context_validates_w3c_ids() {
        assert!(TrajectoryTraceContext::new(
            "58406520a006649127e371903a2de979",
            "5f467fe7bf42676c"
        )
        .is_some());
        assert!(TrajectoryTraceContext::new("0", "5f467fe7bf42676c").is_none());
        assert!(TrajectoryTraceContext::new("58406520a006649127e371903a2de979", "0").is_none());
    }

    #[test]
    fn trajectory_activity_span_id_is_stable_per_runtime_job() {
        let first = activity_span_id("job-1");
        let second = activity_span_id("job-1");
        let other = activity_span_id("job-2");

        assert_eq!(first, second);
        assert_ne!(first, other);
    }
}
