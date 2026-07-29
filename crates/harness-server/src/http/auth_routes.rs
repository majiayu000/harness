use axum::{extract::State, http::StatusCode, Json};
use serde_json::json;
use std::sync::Arc;

use super::state::AppState;

#[derive(serde::Deserialize)]
pub(crate) struct PasswordResetRequest {
    pub(crate) email: String,
}

pub(crate) fn prepare_password_reset_request(
    rate_limiter: &crate::http::rate_limit::PasswordResetRateLimiter,
    limit: u32,
    email: &str,
) -> Result<String, (StatusCode, serde_json::Value)> {
    let email = email.trim().to_lowercase();
    if email.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            json!({"error": "email is required"}),
        ));
    }

    if !rate_limiter.check_and_increment(&email) {
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            json!({
                "error": format!(
                    "rate limit exceeded: max {} password reset requests per hour",
                    limit
                )
            }),
        ));
    }

    Ok(email)
}

pub(crate) fn disabled_password_reset_response() -> (StatusCode, serde_json::Value) {
    (
        StatusCode::NOT_IMPLEMENTED,
        json!({"error": "password reset is not yet implemented"}),
    )
}

/// POST /auth/reset-password — temporarily disabled until email delivery exists.
///
/// Requests are still validated, rate-limited, and logged so the auth-exempt
/// endpoint retains its abuse protections while the actual reset flow is
/// unavailable.
pub(crate) async fn password_reset(
    State(state): State<Arc<AppState>>,
    Json(req): Json<PasswordResetRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let limit = state
        .core
        .server
        .config
        .server
        .password_reset_rate_limit_per_hour;
    let email = match prepare_password_reset_request(
        &state.observability.password_reset_rate_limiter,
        limit,
        &req.email,
    ) {
        Ok(email) => email,
        Err((status, body)) => return (status, Json(body)),
    };

    tracing::info!(
        email_hash = %format!("{:x}", {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            email.hash(&mut h);
            h.finish()
        }),
        "password reset requested while endpoint disabled"
    );

    // TODO: wire up SMTP/transactional email before enabling this endpoint.
    let (status, body) = disabled_password_reset_response();
    (status, Json(body))
}
