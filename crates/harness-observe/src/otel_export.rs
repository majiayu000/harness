use harness_core::{Decision, Event, OtelConfig, OtelExporter};
use opentelemetry::logs::{AnyValue, LogRecord, Logger, LoggerProvider as _, Severity};
use opentelemetry::metrics::{Counter, Histogram, MeterProvider as _};
use opentelemetry::trace::{Span, Tracer, TracerProvider as _};
use opentelemetry::KeyValue;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::logs::LoggerProvider;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::Resource;
use std::collections::HashSet;
use std::sync::Mutex;

const SERVICE_NAME: &str = "harness-observe";
const DEFAULT_HTTP_ENDPOINT: &str = "http://127.0.0.1:4318";
const DEFAULT_GRPC_ENDPOINT: &str = "http://127.0.0.1:4317";

pub struct OtelPipeline {
    log_user_prompt: bool,
    seen_sessions: Mutex<HashSet<String>>,
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

        let tracer_provider = build_tracer_provider(config.exporter, resource.clone())?;
        let tracer = tracer_provider.tracer(SERVICE_NAME);
        let logger_provider = build_logger_provider(config.exporter, resource.clone())?;
        let logger = logger_provider.logger(SERVICE_NAME);
        let meter_provider = build_meter_provider(config.exporter, resource)?;
        let meter = meter_provider.meter(SERVICE_NAME);
        let conversation_starts_total = meter
            .u64_counter("harness.conversation_starts.total")
            .with_description("Number of distinct conversation starts by session.")
            .init();
        let api_request_total = meter
            .u64_counter("harness.api_request.total")
            .with_description("Number of API request-like events.")
            .init();
        let tool_decision_total = meter
            .u64_counter("harness.tool_decision.total")
            .with_description("Number of tool decision events.")
            .init();
        let tool_execution_duration_ms = meter
            .u64_histogram("harness.tool_execution.duration_ms")
            .with_description("Tool execution duration in milliseconds.")
            .init();

        Ok(Some(Self {
            log_user_prompt: config.log_user_prompt,
            seen_sessions: Mutex::new(HashSet::new()),
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
            Ok(mut sessions) => sessions.insert(event.session_id.as_str().to_string()),
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

    fn emit_log(&self, name: &str, event: &Event, attrs: &[KeyValue]) {
        let timestamp = std::time::SystemTime::from(event.ts);
        let severity = severity_for_decision(event.decision);
        let mut builder = LogRecord::builder()
            .with_name(name.to_string().into())
            .with_body(AnyValue::from(name.to_string()))
            .with_timestamp(timestamp)
            .with_severity_text(severity.name())
            .with_severity_number(severity);
        for attr in attrs {
            builder = builder.with_attribute(attr.key.clone(), attr.value.clone());
        }
        self.logger.emit(builder.build());
    }
}

impl Drop for OtelPipeline {
    fn drop(&mut self) {
        for result in self.tracer_provider.force_flush() {
            if let Err(err) = result {
                tracing::warn!("failed to force flush tracer provider: {err}");
            }
        }
        if let Err(err) = self.meter_provider.force_flush() {
            tracing::warn!("failed to force flush meter provider: {err}");
        }
        if let Err(err) = self.meter_provider.shutdown() {
            tracing::warn!("failed to shut down meter provider: {err}");
        }
        for result in self.logger_provider.force_flush() {
            if let Err(err) = result {
                tracing::warn!("failed to force flush logger provider: {err}");
            }
        }
        for result in self.logger_provider.shutdown() {
            if let Err(err) = result {
                tracing::warn!("failed to shut down logger provider: {err}");
            }
        }
    }
}

fn build_tracer_provider(
    exporter: OtelExporter,
    resource: Resource,
) -> anyhow::Result<opentelemetry_sdk::trace::TracerProvider> {
    let span_exporter = match exporter {
        OtelExporter::OtlpHttp => opentelemetry_otlp::new_exporter()
            .http()
            .with_endpoint(resolve_endpoint(exporter))
            .build_span_exporter()?,
        OtelExporter::OtlpGrpc => opentelemetry_otlp::new_exporter()
            .tonic()
            .with_endpoint(resolve_endpoint(exporter))
            .build_span_exporter()?,
        OtelExporter::Disabled => {
            unreachable!("disabled exporter should not initialize tracer provider")
        }
    };

    Ok(opentelemetry_sdk::trace::TracerProvider::builder()
        .with_config(opentelemetry_sdk::trace::config().with_resource(resource))
        .with_batch_exporter(span_exporter, opentelemetry_sdk::runtime::Tokio)
        .build())
}

fn build_logger_provider(
    exporter: OtelExporter,
    resource: Resource,
) -> anyhow::Result<LoggerProvider> {
    let log_exporter = match exporter {
        OtelExporter::OtlpHttp => opentelemetry_otlp::new_exporter()
            .http()
            .with_endpoint(resolve_endpoint(exporter))
            .build_log_exporter()?,
        OtelExporter::OtlpGrpc => opentelemetry_otlp::new_exporter()
            .tonic()
            .with_endpoint(resolve_endpoint(exporter))
            .build_log_exporter()?,
        OtelExporter::Disabled => {
            unreachable!("disabled exporter should not initialize logger provider")
        }
    };

    let batch_processor = opentelemetry_sdk::logs::BatchLogProcessor::builder(
        log_exporter,
        opentelemetry_sdk::runtime::Tokio,
    )
    .build();
    Ok(opentelemetry_sdk::logs::LoggerProvider::builder()
        .with_config(opentelemetry_sdk::logs::Config::default().with_resource(resource))
        .with_log_processor(batch_processor)
        .build())
}

fn build_meter_provider(
    exporter: OtelExporter,
    resource: Resource,
) -> anyhow::Result<SdkMeterProvider> {
    let metrics_pipeline = opentelemetry_otlp::new_pipeline()
        .metrics(opentelemetry_sdk::runtime::Tokio)
        .with_resource(resource);

    let meter_provider = match exporter {
        OtelExporter::OtlpHttp => metrics_pipeline
            .with_exporter(
                opentelemetry_otlp::new_exporter()
                    .http()
                    .with_endpoint(resolve_endpoint(exporter)),
            )
            .build()?,
        OtelExporter::OtlpGrpc => metrics_pipeline
            .with_exporter(
                opentelemetry_otlp::new_exporter()
                    .tonic()
                    .with_endpoint(resolve_endpoint(exporter)),
            )
            .build()?,
        OtelExporter::Disabled => {
            unreachable!("disabled exporter should not initialize meter provider")
        }
    };

    Ok(meter_provider)
}

fn resolve_endpoint(exporter: OtelExporter) -> String {
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
    let hook = event.hook.to_ascii_lowercase();
    let tool = event.tool.to_ascii_lowercase();
    hook.contains("api")
        || hook.contains("http")
        || hook.contains("request")
        || tool.contains("api")
        || tool.contains("http")
        || tool.contains("request")
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
        assert!(!is_api_request_event(&event_with(
            "rule_scan",
            "RuleEngine"
        )));
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
}
