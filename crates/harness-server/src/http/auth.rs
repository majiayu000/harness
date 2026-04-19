use super::AppState;
use axum::{
    extract::State,
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use std::sync::Arc;
use subtle::ConstantTimeEq;

/// Resolve the effective API token from server config or `HARNESS_API_TOKEN` env var.
///
/// Filters empty strings *before* the env-var fallback so that an explicit
/// `api_token = ""` in server.toml does not shadow `HARNESS_API_TOKEN`.
pub(crate) fn resolve_api_token(
    config: &harness_core::config::server::ServerConfig,
) -> Option<String> {
    config
        .api_token
        .as_deref()
        .filter(|t| !t.is_empty())
        .map(|t| t.to_owned())
        .or_else(|| {
            std::env::var("HARNESS_API_TOKEN")
                .ok()
                .filter(|t| !t.is_empty())
        })
}

/// Bearer token authentication middleware.
///
/// Exempts `/health`, `/webhook`, `/webhook/feishu`, `/signals`, `/favicon.ico`,
/// `/auth/reset-password`, `/` (dashboard HTML), `/overview` (system overview
/// HTML), `/assets/*` (hashed React bundle assets), and `/ws` (WebSocket
/// upgrade).
/// The dashboard HTML no longer embeds the token, so it is safe to serve without
/// auth. `/ws` is exempt from *this middleware* because the WebSocket upgrade
/// cannot carry a body and must be handled before axum reads headers twice;
/// however `ws_handler` performs its own two-layer access control: (1) Origin
/// header validation to prevent CSWH, and (2) Bearer token verification for
/// **all** clients (including those that present a localhost Origin) when a token
/// is configured.  Origin alone is not trusted for auth because it can be forged
/// by non-browser tools.
/// All other endpoints require an `Authorization: Bearer <token>` header when
/// `api_token` is configured. When no token is configured the middleware is a
/// no-op (backward compat).
pub(crate) async fn api_auth_middleware(
    State(state): State<Arc<AppState>>,
    req: axum::extract::Request,
    next: Next,
) -> Response {
    let path = req.uri().path();
    // Exempt paths: static assets, public health probes, and endpoints that
    // carry no secrets (/ no longer embeds the token; /ws streams only
    // task-status events).
    if matches!(
        path,
        "/health"
            | "/webhook"
            | "/webhook/feishu"
            | "/signals"
            | "/favicon.ico"
            | "/auth/reset-password"
            | "/"
            | "/overview"
            | "/ws"
    ) || path.starts_with("/assets/")
    {
        return next.run(req).await;
    }

    let Some(expected) = resolve_api_token(&state.core.server.config.server) else {
        // No token configured — skip auth for backward compatibility.
        return next.run(req).await;
    };

    let provided = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(str::to_string);

    let authorized = provided
        .as_deref()
        .map(|tok| tok.as_bytes().ct_eq(expected.as_bytes()).into())
        .unwrap_or(false);

    if authorized {
        next.run(req).await
    } else {
        (
            axum::http::StatusCode::UNAUTHORIZED,
            Json(json!({"error": "unauthorized"})),
        )
            .into_response()
    }
}
