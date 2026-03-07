use harness_core::{Decision, Event, OtelConfig, OtelExporter};
use opentelemetry::logs::{AnyValue, LogRecord as _, Logger, LoggerProvider as _, Severity};
use opentelemetry::metrics::{Counter, Histogram, MeterProvider as _};
use opentelemetry::trace::{Span, Tracer, TracerProvider as _};
use opentelemetry::KeyValue;
use opentelemetry_otlp::{Protocol, WithExportConfig};
use opentelemetry_sdk::logs::LoggerProvider;
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};
use opentelemetry_sdk::Resource;
use std::collections::{HashSet, VecDeque};
use std::sync::Mutex;

const SERVICE_NAME: &str = "harness-observe";
const DEFAULT_HTTP_ENDPOINT: &str = "http://127.0.0.1:4318";
const DEFAULT_GRPC_ENDPOINT: &str = "http://127.0.0.1:4317";
const MAX_TRACKED_CONVERSATION_SESSIONS: usize = 10_000;

struct SessionStartDeduper {
    seen: HashSet<String>,
    order: VecDeque<String>,
    capacity: usize,
}

impl SessionStartDeduper {
    fn new(capacity: usize) -> Self {
        Self {
            seen: HashSet::new(),
            order: VecDeque::new(),
            capacity: capacity.max(1),
        }
    }

    fn insert(&mut self, session_id: &str) -> bool {
        if self.seen.contains(session_id) {
            return false;
        }

        let session_id = session_id.to_string();
        self.seen.insert(session_id.clone());
        self.order.push_back(session_id);

        while self.order.len() > self.capacity {
            if let Some(expired) = self.order.pop_front() {
                self.seen.remove(expired.as_str());
            }
        }

        true
    }
}

pub struct OtelPipeline {
    log_user_prompt: bool,
    seen_sessions: Mutex<SessionStartDeduper>,
    tracer_provider: opentelemetry_sdk::trace::TracerProvider,
    tracer: opentelemetry_sdk::trace::Tracer,
    logger_provider: LoggerProvider,
    logger: opentelemetry_sdk::logs::Logger,
    meter_provider: SdkMeterProvider,
    conversation_starts_total: Counter<u64>,
    api_request_total: Counter<u64>,
    tool_decision_total: Counter<u64>,
    tool_execution_duration_ms: Histogram<u64>,
}

impl OtelPipeline {
    pub fn from_config(config: &OtelConfig) -> anyhow::Result<Option<Self>> {
        if config.exporter == OtelExporter::Disabled {
            return Ok(None);
        }

        let resource = Resource::new(vec![
            KeyValue::new("service.name", SERVICE_NAME),
            KeyValue::new("deployment.environment", config.environment.clone()),
        ]);

        let tracer_provider = build_tracer_provider(
            config.exporter,
            config.endpoint.as_deref(),
            resource.clone(),
        )?;
        let tracer = tracer_provider.tracer(SERVICE_NAME);
        let logger_provider = build_logger_provider(
            config.exporter,
            config.endpoint.as_deref(),
            resource.clone(),
        )?;
        let logger = logger_provider.logger(SERVICE_NAME);
        let meter_provider =
            build_meter_provider(config.exporter, config.endpoint.as_deref(), resource)?;
        let meter = meter_provider.meter(SERVICE_NAME);
        let conversation_starts_total = meter
            .u64_counter("harness.conversation_starts.total")
            .with_description("Number of distinct conversation starts by session.")
            .build();
        let api_request_total = meter
            .u64_counter("harness.api_request.total")
            .with_description("Number of API request-like events.")
            .build();
        let tool_decision_total = meter
            .u64_counter("harness.tool_decision.total")
            .with_description("Number of tool decision events.")
            .build();
        let tool_execution_duration_ms = meter
            .u64_histogram("harness.tool_execution.duration_ms")
            .with_description("Tool execution duration in milliseconds.")
            .build();

        Ok(Some(Self {
            log_user_prompt: config.log_user_prompt,
            seen_sessions: Mutex::new(SessionStartDeduper::new(MAX_TRACKED_CONVERSATION_SESSIONS)),
            tracer_provider,
            tracer,
            logger_provider,
            logger,
            meter_provider,
            conversation_starts_total,
            api_request_total,
            tool_decision_total,
            tool_execution_duration_ms,
        }))
    }

    pub fn record_event(&self, event: &Event) {
        let attrs = self.base_attributes(event);
        self.tool_decision_total.add(1, &attrs);
        self.emit_trace("tool_decision", event, &attrs);
        self.emit_log("tool_decision", event, &attrs);

        if self.mark_conversation_started(event) {
            self.conversation_starts_total.add(1, &attrs);
            self.emit_trace("conversation_starts", event, &attrs);
            self.emit_log("conversation_starts", event, &attrs);
        }

        if is_api_request_event(event) {
            self.api_request_total.add(1, &attrs);
            self.emit_trace("api_request", event, &attrs);
            self.emit_log("api_request", event, &attrs);
        }

        if let Some(duration_ms) = event.duration_ms {
            self.tool_execution_duration_ms.record(duration_ms, &attrs);
            let mut execution_attrs = attrs.clone();
            execution_attrs.push(KeyValue::new("duration_ms", duration_ms as i64));
            self.emit_trace("tool_execution", event, &execution_attrs);
            self.emit_log("tool_execution", event, &execution_attrs);
        }
    }

    fn mark_conversation_started(&self, event: &Event) -> bool {
        match self.seen_sessions.lock() {
            Ok(mut sessions) => sessions.insert(event.session_id.as_str()),
            Err(_) => false,
        }
    }

    fn base_attributes(&self, event: &Event) -> Vec<KeyValue> {
        let mut attrs = vec![
            KeyValue::new("event.id", event.id.as_str().to_string()),
            KeyValue::new("event.session_id", event.session_id.as_str().to_string()),
            KeyValue::new("event.hook", event.hook.clone()),
            KeyValue::new("event.tool", event.tool.clone()),
            KeyValue::new("event.decision", decision_label(event.decision)),
        ];

        if self.log_user_prompt || !looks_like_user_prompt_payload(event) {
            if let Some(reason) = &event.reason {
                attrs.push(KeyValue::new("event.reason", reason.clone()));
            }
            if let Some(detail) = &event.detail {
                attrs.push(KeyValue::new("event.detail", detail.clone()));
            }
        }

        attrs
    }

    fn emit_trace(&self, name: &str, event: &Event, attrs: &[KeyValue]) {
        let mut span = self.tracer.start(name.to_string());
        span.set_attribute(KeyValue::new("event.timestamp", event.ts.to_rfc3339()));
        for attr in attrs {
            span.set_attribute(attr.clone());
        }
        span.end();
    }

    fn emit_log(&self, name: &'static str, event: &Event, attrs: &[KeyValue]) {
        let timestamp = std::time::SystemTime::from(event.ts);
        let severity = severity_for_decision(event.decision);
        let mut record = self.logger.create_log_record();
        record.set_event_name(name);
        record.set_body(AnyValue::from(name.to_string()));
        record.set_timestamp(timestamp);
        record.set_observed_timestamp(timestamp);
        record.set_severity_text(severity.name());
        record.set_severity_number(severity);
        for attr in attrs {
            record.add_attribute(attr.key.clone(), attr.value.to_string());
        }
        self.logger.emit(record);
    }
}

impl Drop for OtelPipeline {
    fn drop(&mut self) {
        for result in self.tracer_provider.force_flush() {
            if let Err(err) = result {
                report_pipeline_error("failed to force flush tracer provider", err);
            }
        }
        if let Err(err) = self.tracer_provider.shutdown() {
            report_pipeline_error("failed to shut down tracer provider", err);
        }
        if let Err(err) = self.meter_provider.force_flush() {
            report_pipeline_error("failed to force flush meter provider", err);
        }
        if let Err(err) = self.meter_provider.shutdown() {
            report_pipeline_error("failed to shut down meter provider", err);
        }
        for result in self.logger_provider.force_flush() {
            if let Err(err) = result {
                report_pipeline_error("failed to force flush logger provider", err);
            }
        }
        if let Err(err) = self.logger_provider.shutdown() {
            report_pipeline_error("failed to shut down logger provider", err);
        }
    }
}

fn build_tracer_provider(
    exporter: OtelExporter,
    endpoint: Option<&str>,
    resource: Resource,
) -> anyhow::Result<opentelemetry_sdk::trace::TracerProvider> {
    let span_exporter = match exporter {
        OtelExporter::OtlpHttp => opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_protocol(Protocol::HttpBinary)
            .with_endpoint(resolve_endpoint(exporter, endpoint))
            .build()?,
        OtelExporter::OtlpGrpc => opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .with_endpoint(resolve_endpoint(exporter, endpoint))
            .build()?,
        OtelExporter::Disabled => {
            anyhow::bail!("disabled exporter cannot initialize tracer provider")
        }
    };

    Ok(opentelemetry_sdk::trace::TracerProvider::builder()
        .with_resource(resource)
        .with_batch_exporter(span_exporter, opentelemetry_sdk::runtime::Tokio)
        .build())
}

fn build_logger_provider(
    exporter: OtelExporter,
    endpoint: Option<&str>,
    resource: Resource,
) -> anyhow::Result<LoggerProvider> {
    let log_exporter = match exporter {
        OtelExporter::OtlpHttp => opentelemetry_otlp::LogExporter::builder()
            .with_http()
            .with_protocol(Protocol::HttpBinary)
            .with_endpoint(resolve_endpoint(exporter, endpoint))
            .build()?,
        OtelExporter::OtlpGrpc => opentelemetry_otlp::LogExporter::builder()
            .with_tonic()
            .with_endpoint(resolve_endpoint(exporter, endpoint))
            .build()?,
        OtelExporter::Disabled => {
            anyhow::bail!("disabled exporter cannot initialize logger provider")
        }
    };

    Ok(opentelemetry_sdk::logs::LoggerProvider::builder()
        .with_resource(resource)
        .with_batch_exporter(log_exporter, opentelemetry_sdk::runtime::Tokio)
        .build())
}

fn build_meter_provider(
    exporter: OtelExporter,
    endpoint: Option<&str>,
    resource: Resource,
) -> anyhow::Result<SdkMeterProvider> {
    let metric_exporter = match exporter {
        OtelExporter::OtlpHttp => opentelemetry_otlp::MetricExporter::builder()
            .with_http()
            .with_protocol(Protocol::HttpBinary)
            .with_endpoint(resolve_endpoint(exporter, endpoint))
            .build()?,
        OtelExporter::OtlpGrpc => opentelemetry_otlp::MetricExporter::builder()
            .with_tonic()
            .with_endpoint(resolve_endpoint(exporter, endpoint))
            .build()?,
        OtelExporter::Disabled => {
            anyhow::bail!("disabled exporter cannot initialize meter provider")
        }
    };

    let reader =
        PeriodicReader::builder(metric_exporter, opentelemetry_sdk::runtime::Tokio).build();
    Ok(SdkMeterProvider::builder()
        .with_resource(resource)
        .with_reader(reader)
        .build())
}

fn resolve_endpoint(exporter: OtelExporter, config_endpoint: Option<&str>) -> String {
    if let Some(endpoint) = config_endpoint
        .map(str::trim)
        .filter(|endpoint| !endpoint.is_empty())
    {
        return endpoint.to_string();
    }

    if let Ok(endpoint) = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT") {
        if !endpoint.trim().is_empty() {
            return endpoint;
        }
    }

    match exporter {
        OtelExporter::OtlpHttp => DEFAULT_HTTP_ENDPOINT.to_string(),
        OtelExporter::OtlpGrpc => DEFAULT_GRPC_ENDPOINT.to_string(),
        OtelExporter::Disabled => String::new(),
    }
}

fn is_api_request_event(event: &Event) -> bool {
    [event.hook.as_str(), event.tool.as_str()]
        .iter()
        .any(|value| {
            has_token(value, "api") || has_token(value, "http") || has_token(value, "request")
        })
}

fn has_token(value: &str, needle: &str) -> bool {
    value
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .any(|token| token.eq_ignore_ascii_case(needle))
}

fn looks_like_user_prompt_payload(event: &Event) -> bool {
    let hook = event.hook.to_ascii_lowercase();
    let tool = event.tool.to_ascii_lowercase();
    hook.contains("conversation")
        || hook.contains("prompt")
        || tool.contains("conversation")
        || tool.contains("prompt")
}

fn decision_label(decision: Decision) -> &'static str {
    match decision {
        Decision::Pass => "pass",
        Decision::Warn => "warn",
        Decision::Block => "block",
        Decision::Gate => "gate",
        Decision::Escalate => "escalate",
        Decision::Complete => "complete",
    }
}

fn severity_for_decision(decision: Decision) -> Severity {
    match decision {
        Decision::Pass | Decision::Complete => Severity::Info,
        Decision::Warn => Severity::Warn,
        Decision::Block | Decision::Gate | Decision::Escalate => Severity::Error,
    }
}

fn report_pipeline_error(message: &str, err: impl std::fmt::Display) {
    tracing::warn!("{message}: {err}");
    eprintln!("harness-observe: {message}: {err}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::{Decision, Event, SessionId};

    fn event_with(hook: &str, tool: &str) -> Event {
        Event::new(SessionId::new(), hook, tool, Decision::Pass)
    }

    #[test]
    fn api_request_classifier_matches_common_markers() {
        assert!(is_api_request_event(&event_with("api_call", "tool")));
        assert!(is_api_request_event(&event_with("hook", "http_client")));
        assert!(is_api_request_event(&event_with("request_started", "tool")));
        assert!(!is_api_request_event(&event_with(
            "rule_scan",
            "RuleEngine"
        )));
        assert!(!is_api_request_event(&event_with("therapist", "editor")));
    }

    #[test]
    fn user_prompt_payload_classifier_matches_prompt_markers() {
        assert!(looks_like_user_prompt_payload(&event_with(
            "conversation_start",
            "task_runner"
        )));
        assert!(looks_like_user_prompt_payload(&event_with(
            "hook",
            "user_prompt_tool"
        )));
        assert!(!looks_like_user_prompt_payload(&event_with(
            "rule_check",
            "RuleEngine"
        )));
    }

    #[test]
    fn from_config_returns_none_when_disabled() -> anyhow::Result<()> {
        let config = OtelConfig {
            exporter: OtelExporter::Disabled,
            ..OtelConfig::default()
        };
        assert!(OtelPipeline::from_config(&config)?.is_none());
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn record_event_with_active_pipeline_does_not_panic() -> anyhow::Result<()> {
        let config = OtelConfig {
            exporter: OtelExporter::OtlpHttp,
            endpoint: Some("http://127.0.0.1:1".to_string()),
            ..OtelConfig::default()
        };
        let pipeline = OtelPipeline::from_config(&config)?
            .expect("otlp-http exporter should initialize active pipeline");
        let event = Event::new(
            SessionId::new(),
            "api_request",
            "http_client",
            Decision::Pass,
        );
        pipeline.record_event(&event);
        std::mem::forget(pipeline);
        Ok(())
    }
}
