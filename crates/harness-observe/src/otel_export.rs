use harness_core::config::misc::{OtelConfig, OtelExporter};
use harness_core::types::{Decision, Event};
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
use std::time::Duration;

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
    pub async fn from_config(config: &OtelConfig) -> anyhow::Result<Option<Self>> {
        if config.exporter == OtelExporter::Disabled {
            return Ok(None);
        }

        let endpoint = resolve_endpoint(config.exporter, config.endpoint.as_deref());
        ensure_endpoint_reachable(config.exporter, endpoint.as_str()).await?;

        let resource = Resource::new(vec![
            KeyValue::new("service.name", SERVICE_NAME),
            KeyValue::new("deployment.environment", config.environment.clone()),
        ]);

        let tracer_provider =
            build_tracer_provider(config.exporter, endpoint.as_str(), resource.clone())?;
        let tracer = tracer_provider.tracer(SERVICE_NAME);
        let logger_provider =
            build_logger_provider(config.exporter, endpoint.as_str(), resource.clone())?;
        let logger = logger_provider.logger(SERVICE_NAME);
        let meter_provider = build_meter_provider(config.exporter, endpoint.as_str(), resource)?;
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
            Err(poisoned) => {
                tracing::error!("SessionStartDeduper mutex poisoned; recovering state");
                poisoned.into_inner().insert(event.session_id.as_str())
            }
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

    pub async fn shutdown(self) {
        let Self {
            tracer_provider,
            logger_provider,
            meter_provider,
            ..
        } = self;
        let shutdown_result = tokio::task::spawn_blocking(move || {
            for result in tracer_provider.force_flush() {
                if let Err(err) = result {
                    report_pipeline_error("failed to force flush tracer provider", err);
                }
            }
            if let Err(err) = tracer_provider.shutdown() {
                report_pipeline_error("failed to shut down tracer provider", err);
            }
            if let Err(err) = meter_provider.force_flush() {
                report_pipeline_error("failed to force flush meter provider", err);
            }
            if let Err(err) = meter_provider.shutdown() {
                report_pipeline_error("failed to shut down meter provider", err);
            }
            for result in logger_provider.force_flush() {
                if let Err(err) = result {
                    report_pipeline_error("failed to force flush logger provider", err);
                }
            }
            if let Err(err) = logger_provider.shutdown() {
                report_pipeline_error("failed to shut down logger provider", err);
            }
        })
        .await;
        if let Err(err) = shutdown_result {
            tracing::warn!("failed to join OpenTelemetry shutdown task: {err}");
        }
    }
}

fn build_tracer_provider(
    exporter: OtelExporter,
    endpoint: &str,
    resource: Resource,
) -> anyhow::Result<opentelemetry_sdk::trace::TracerProvider> {
    let span_exporter = match exporter {
        OtelExporter::OtlpHttp => opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_protocol(Protocol::HttpBinary)
            .with_endpoint(endpoint.to_string())
            .build()?,
        OtelExporter::OtlpGrpc => opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .with_endpoint(endpoint.to_string())
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
    endpoint: &str,
    resource: Resource,
) -> anyhow::Result<LoggerProvider> {
    let log_exporter = match exporter {
        OtelExporter::OtlpHttp => opentelemetry_otlp::LogExporter::builder()
            .with_http()
            .with_protocol(Protocol::HttpBinary)
            .with_endpoint(endpoint.to_string())
            .build()?,
        OtelExporter::OtlpGrpc => opentelemetry_otlp::LogExporter::builder()
            .with_tonic()
            .with_endpoint(endpoint.to_string())
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
    endpoint: &str,
    resource: Resource,
) -> anyhow::Result<SdkMeterProvider> {
    let metric_exporter = match exporter {
        OtelExporter::OtlpHttp => opentelemetry_otlp::MetricExporter::builder()
            .with_http()
            .with_protocol(Protocol::HttpBinary)
            .with_endpoint(endpoint.to_string())
            .build()?,
        OtelExporter::OtlpGrpc => opentelemetry_otlp::MetricExporter::builder()
            .with_tonic()
            .with_endpoint(endpoint.to_string())
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

async fn ensure_endpoint_reachable(exporter: OtelExporter, endpoint: &str) -> anyhow::Result<()> {
    let (host, port) = endpoint_host_and_port(exporter, endpoint)?;
    let mut attempted = false;
    let mut connection_errors: Vec<String> = Vec::new();

    let lookup_target = format!("{host}:{port}");
    let addrs = tokio::net::lookup_host(lookup_target)
        .await
        .map_err(|err| anyhow::anyhow!("failed to resolve OTLP endpoint `{endpoint}`: {err}"))?;
    for addr in addrs {
        attempted = true;
        match tokio::time::timeout(Duration::from_secs(1), tokio::net::TcpStream::connect(addr))
            .await
        {
            Ok(Ok(stream)) => {
                drop(stream);
                return Ok(());
            }
            Ok(Err(err)) => connection_errors.push(format!("{addr}: {err}")),
            Err(_) => connection_errors.push(format!("{addr}: connection timed out")),
        }
    }

    if !attempted {
        anyhow::bail!("OTLP endpoint `{endpoint}` did not resolve to any socket address");
    }

    anyhow::bail!(
        "OTLP endpoint `{endpoint}` is unreachable ({})",
        connection_errors.join("; ")
    )
}

fn endpoint_host_and_port(exporter: OtelExporter, endpoint: &str) -> anyhow::Result<(String, u16)> {
    let trimmed = endpoint.trim();
    if trimmed.is_empty() {
        anyhow::bail!("OTLP endpoint cannot be empty");
    }

    let without_scheme = trimmed
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(trimmed);
    let authority = without_scheme.split('/').next().unwrap_or_default();
    let authority = authority.rsplit('@').next().unwrap_or(authority);

    if authority.is_empty() {
        anyhow::bail!("OTLP endpoint `{endpoint}` is missing authority");
    }

    if authority.starts_with('[') {
        let close = authority.find(']').ok_or_else(|| {
            anyhow::anyhow!("OTLP endpoint `{endpoint}` has invalid IPv6 authority")
        })?;
        let host = authority[1..close].to_string();
        let tail = &authority[(close + 1)..];
        let port = if let Some(port_str) = tail.strip_prefix(':') {
            port_str
                .parse::<u16>()
                .map_err(|err| anyhow::anyhow!("invalid OTLP endpoint port `{port_str}`: {err}"))?
        } else {
            default_endpoint_port(exporter)
        };
        return Ok((host, port));
    }

    if let Some((host, port_str)) = authority.rsplit_once(':') {
        if !host.contains(':') {
            let port = port_str
                .parse::<u16>()
                .map_err(|err| anyhow::anyhow!("invalid OTLP endpoint port `{port_str}`: {err}"))?;
            return Ok((host.to_string(), port));
        }
    }

    Ok((authority.to_string(), default_endpoint_port(exporter)))
}

fn default_endpoint_port(exporter: OtelExporter) -> u16 {
    match exporter {
        OtelExporter::OtlpHttp => 4318,
        OtelExporter::OtlpGrpc => 4317,
        OtelExporter::Disabled => 0,
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
    use harness_core::{types::Decision, types::Event, types::SessionId};

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

    #[tokio::test]
    async fn from_config_returns_none_when_disabled() -> anyhow::Result<()> {
        let config = OtelConfig {
            exporter: OtelExporter::Disabled,
            ..OtelConfig::default()
        };
        assert!(OtelPipeline::from_config(&config).await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn from_config_reports_error_when_endpoint_unreachable() {
        let config = OtelConfig {
            exporter: OtelExporter::OtlpHttp,
            endpoint: Some("http://127.0.0.1:1".to_string()),
            ..OtelConfig::default()
        };
        let result = OtelPipeline::from_config(&config).await;
        assert!(result.is_err());
    }
}
