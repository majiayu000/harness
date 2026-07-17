use super::AppState;
use axum::{
    extract::State,
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use harness_core::config::server::ServerConfig;
use serde_json::json;
use std::sync::Arc;
use subtle::ConstantTimeEq;

/// Percent-decode a query-string value (RFC 3986 §2.1).
///
/// `encodeURIComponent` on the browser side encodes characters such as `+`,
/// `/`, `=`, and `%` as `%2B`, `%2F`, `%3D`, `%25`. Without decoding, the
/// constant-time comparison against the stored token would fail for any token
/// that contains those characters.
fn percent_decode(s: &str) -> String {
    let mut out = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if let (Some(hi), Some(lo)) = (
                hex_nibble(bytes.get(i + 1).copied()),
                hex_nibble(bytes.get(i + 2).copied()),
            ) {
                out.push(hi << 4 | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_nibble(b: Option<u8>) -> Option<u8> {
    match b? {
        c @ b'0'..=b'9' => Some(c - b'0'),
        c @ b'a'..=b'f' => Some(c - b'a' + 10),
        c @ b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decode_plain_token_unchanged() {
        assert_eq!(percent_decode("mytoken"), "mytoken");
    }

    #[test]
    fn percent_decode_plus_sign() {
        // encodeURIComponent("+") → "%2B"; must decode to literal "+"
        assert_eq!(percent_decode("sec%2Bret"), "sec+ret");
    }

    #[test]
    fn percent_decode_slash_equals() {
        // encodeURIComponent("/abc=") → "%2Fabc%3D"
        assert_eq!(percent_decode("%2Fabc%3D"), "/abc=");
    }

    #[test]
    fn percent_decode_percent_sign() {
        // encodeURIComponent("%") → "%25"
        assert_eq!(percent_decode("%25"), "%");
    }

    #[test]
    fn percent_decode_full_base64_style_token() {
        // Simulate encodeURIComponent("sec+ret/abc=") → "sec%2Bret%2Fabc%3D"
        assert_eq!(percent_decode("sec%2Bret%2Fabc%3D"), "sec+ret/abc=");
    }

    #[test]
    fn percent_decode_invalid_escape_left_as_is() {
        // Incomplete %XX should be left as-is, not panic
        assert_eq!(percent_decode("tok%2"), "tok%2");
        assert_eq!(percent_decode("tok%"), "tok%");
    }

    #[test]
    fn usage_page_html_is_auth_exempt() {
        assert!(is_auth_exempt_path("/usage"));
        assert!(!is_auth_exempt_path("/api/usage-monitor"));
    }

    #[test]
    fn resolve_auth_mode_refuses_tokenless_without_opt_in() {
        let err = resolve_api_auth_mode(&ServerConfig::default()).unwrap_err();
        let message = err.to_string();

        assert!(message.contains("api_token"));
        assert!(message.contains("HARNESS_API_TOKEN"));
        assert!(message.contains("allow_unauthenticated"));
    }

    #[test]
    fn resolve_auth_mode_allows_explicit_tokenless_opt_in() {
        let config = ServerConfig {
            allow_unauthenticated: true,
            ..ServerConfig::default()
        };

        assert_eq!(resolve_api_auth_mode(&config).unwrap(), ApiAuthMode::Open);
    }

    #[test]
    fn resolve_auth_mode_token_wins_over_opt_in() {
        let config = ServerConfig {
            api_token: Some(" secret ".to_string()),
            allow_unauthenticated: true,
            ..ServerConfig::default()
        };

        assert_eq!(
            resolve_api_auth_mode(&config).unwrap(),
            ApiAuthMode::Enforced("secret".to_string())
        );
    }
}

/// Resolve the effective API token from server config.
///
/// `HARNESS_API_TOKEN` is applied during config loading; request-time auth must
/// use the startup config snapshot rather than process-global environment state.
pub(crate) fn resolve_api_token(config: &ServerConfig) -> Option<String> {
    config
        .api_token
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ApiAuthMode {
    Enforced(String),
    Open,
}

impl ApiAuthMode {
    pub(crate) fn expected_token(&self) -> Option<&str> {
        match self {
            Self::Enforced(token) => Some(token.as_str()),
            Self::Open => None,
        }
    }
}

pub(crate) fn resolve_api_auth_mode(config: &ServerConfig) -> anyhow::Result<ApiAuthMode> {
    if let Some(token) = resolve_api_token(config) {
        return Ok(ApiAuthMode::Enforced(token));
    }

    if config.allow_unauthenticated {
        return Ok(ApiAuthMode::Open);
    }

    anyhow::bail!(
        "API authentication is not configured: set server.api_token (api_token) in config or HARNESS_API_TOKEN, or explicitly set server.allow_unauthenticated = true for tokenless local development"
    )
}

pub(crate) fn log_api_auth_mode(mode: &ApiAuthMode, config: &ServerConfig) {
    match mode {
        ApiAuthMode::Enforced(_) if config.allow_unauthenticated => {
            tracing::warn!(
                "server.allow_unauthenticated=true is ignored because an API token is configured; API bearer-token authentication is enforced"
            );
        }
        ApiAuthMode::Enforced(_) => {
            tracing::info!("API bearer-token authentication is enforced");
        }
        ApiAuthMode::Open => {
            tracing::warn!(
                "API authentication is disabled because server.allow_unauthenticated=true and no API token is configured"
            );
        }
    }
}

pub(crate) fn is_auth_exempt_path(path: &str) -> bool {
    matches!(
        path,
        "/health"
            | "/webhook"
            | "/webhook/feishu"
            | "/signals"
            | "/favicon.ico"
            | "/auth/reset-password"
            | "/"
            | "/dashboard"
            | "/overview"
            | "/usage"
            | "/worktrees"
            | "/ws"
    ) || path.starts_with("/assets/")
}

/// Bearer token authentication middleware.
///
/// Exempts `/health`, `/webhook`, `/webhook/feishu`, `/signals`, `/favicon.ico`,
/// `/auth/reset-password`, `/` and `/dashboard` (dashboard HTML), `/overview` and `/usage`
/// (React SPA HTML), `/assets/*` (hashed React bundle assets), and `/ws` (WebSocket
/// upgrade).
/// The dashboard HTML no longer embeds the token, so it is safe to serve without
/// auth. `/ws` is exempt from *this middleware* because the WebSocket upgrade
/// cannot carry a body and must be handled before axum reads headers twice;
/// however `ws_handler` performs its own two-layer access control: (1) Origin
/// header validation to prevent CSWH, and (2) Bearer token verification for
/// **all** clients (including those that present a localhost Origin) when a token
/// is configured.  Origin alone is not trusted for auth because it can be forged
/// by non-browser tools.
/// All other endpoints require an `Authorization: Bearer <token>` header in
/// enforced mode. Open mode is only reachable when startup accepted the explicit
/// `allow_unauthenticated = true` opt-in.
pub(crate) async fn api_auth_middleware(
    State(state): State<Arc<AppState>>,
    req: axum::extract::Request,
    next: Next,
) -> Response {
    let path = req.uri().path();
    // Exempt paths: static assets, public health probes, and endpoints that
    // carry no secrets (/ no longer embeds the token; /ws streams only
    // task-status events).
    if is_auth_exempt_path(path) {
        return next.run(req).await;
    }

    let auth_mode = match resolve_api_auth_mode(&state.core.server.config.server) {
        Ok(mode) => mode,
        Err(error) => {
            tracing::error!("API authentication misconfigured after startup: {error}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "error": "authentication_misconfigured",
                    "message": error.to_string(),
                })),
            )
                .into_response();
        }
    };
    let Some(expected) = auth_mode.expected_token() else {
        return next.run(req).await;
    };

    // Bearer header takes priority; fall back to ?token= query param only for
    // SSE stream endpoints — EventSource/browser navigation cannot set custom
    // headers. Leaking bearer tokens through arbitrary query strings is a
    // credential-logging risk, so other endpoints must use the header.
    let provided = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(str::to_string)
        .or_else(|| {
            let is_sse_stream = path.ends_with("/stream")
                && (path.starts_with("/tasks/")
                    || path.starts_with("/api/workflows/runtime/submissions/"));
            if is_sse_stream {
                req.uri().query().and_then(|q| {
                    q.split('&')
                        .find(|p| p.starts_with("token="))
                        .and_then(|p| p.strip_prefix("token="))
                        .map(percent_decode)
                })
            } else {
                None
            }
        });

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
