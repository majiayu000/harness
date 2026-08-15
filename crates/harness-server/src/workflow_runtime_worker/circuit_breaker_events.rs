use crate::http::AppState;
use crate::runtime_circuit_breaker::{CircuitBreakerEvent, CircuitBreakerEventKind, FailureClass};
use harness_core::types::{Decision, Event, SessionId};
use serde_json::json;

pub(crate) async fn emit_circuit_breaker_events(
    state: &AppState,
    events: Vec<CircuitBreakerEvent>,
) {
    for event in events {
        emit_circuit_breaker_event(state, event).await;
    }
}

async fn emit_circuit_breaker_event(state: &AppState, event: CircuitBreakerEvent) {
    let class = event.class.map(FailureClass::as_str);
    let (level, breaker_state, decision, reason) = match event.kind {
        CircuitBreakerEventKind::Opened => (
            "error",
            "open",
            Decision::Block,
            "runtime circuit breaker opened",
        ),
        CircuitBreakerEventKind::Closed => (
            "info",
            "closed",
            Decision::Complete,
            "runtime circuit breaker closed",
        ),
        CircuitBreakerEventKind::Reset => (
            "info",
            "closed",
            Decision::Complete,
            "runtime circuit breaker reset",
        ),
    };
    let detail = json!({
        "level": level,
        "profile": event.profile,
        "state": breaker_state,
        "failure_class": class,
        "consecutive": event.consecutive,
        "cooldown_until": event.cooldown_until,
    });
    match event.kind {
        CircuitBreakerEventKind::Opened => {
            tracing::error!(
                runtime_profile = %detail["profile"],
                failure_class = ?detail["failure_class"],
                consecutive = ?event.consecutive,
                cooldown_until = ?event.cooldown_until,
                "runtime circuit breaker opened"
            );
            state
                .observability
                .alerts
                .raise(crate::alerting::producers::circuit_breaker_open(
                    &event.profile,
                    &format!(
                        "failure_class={:?} consecutive={:?} cooldown_until={:?}",
                        class, event.consecutive, event.cooldown_until
                    ),
                ));
        }
        CircuitBreakerEventKind::Closed | CircuitBreakerEventKind::Reset => tracing::info!(
            runtime_profile = %detail["profile"],
            failure_class = ?detail["failure_class"],
            "runtime circuit breaker recovered"
        ),
    }
    let mut observe_event = Event::new(
        SessionId::new(),
        "runtime_circuit_breaker",
        detail["profile"].as_str().unwrap_or("runtime"),
        decision,
    );
    observe_event.reason = Some(reason.to_string());
    observe_event.detail = Some(detail.to_string());
    if let Err(error) = state.observability.events.log(&observe_event).await {
        tracing::warn!("failed to record runtime circuit breaker event: {error}");
    }
}
