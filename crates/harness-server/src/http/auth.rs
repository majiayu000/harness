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

/// Decode `%XX` percent-encoded sequences in a query-parameter value.
///
/// `encodeURIComponent` in JavaScript encodes all reserved characters, so the
/// raw query string value must be decoded before constant-time comparison with
/// the stored token.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut result: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                result.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        result.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&result).into_owned()
}

/// Bearer token authentication middleware.
///
/// Exempts `/health`, `/webhook`, `/webhook/feishu`, and `/signals`.
/// `/webhook/feishu` relies on Feishu verification-token validation and must
/// fail closed when that token is not configured. All other endpoints require
/// an `Authorization: Bearer <token>` header when `api_token` is configured.
/// When no token is configured the middleware is a no-op (backward compat).
pub(crate) async fn api_auth_middleware(
    State(state): State<Arc<AppState>>,
    req: axum::extract::Request,
    next: Next,
) -> Response {
    let path = req.uri().path();
    // Exempt paths that carry their own authentication or must stay public.
    // /health, /webhook*, and /signals have their own protection or must stay
    // fully public. /favicon.ico is a static asset with no sensitive data.
    // The dashboard HTML (/) is NOT exempt: it embeds the API token as a JS
    // variable, so it must only be served to callers who already know the token.
    // Browsers access the dashboard via /?token=<tok> since they cannot set
    // Authorization headers on a navigation request.
    if matches!(
        path,
        "/health"
            | "/webhook"
            | "/webhook/feishu"
            | "/signals"
            | "/favicon.ico"
            | "/auth/reset-password"
    ) {
        return next.run(req).await;
    }

    // Browser clients cannot set Authorization headers on WebSocket upgrades or
    // on top-level navigation requests (/). Accept a ?token= query parameter
    // as a fallback for these two paths; percent-decode it because the JS
    // client always calls encodeURIComponent() before appending to the URL.
    let query_token: Option<String> = if path == "/ws" || path == "/" {
        req.uri().query().and_then(|q| {
            q.split('&')
                .find_map(|kv| kv.strip_prefix("token=").map(percent_decode))
        })
    } else {
        None
    };

    let Some(expected) = resolve_api_token(&state.core.server.config.server) else {
        // No token configured — skip auth for backward compatibility.
        return next.run(req).await;
    };

    let header_token = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(str::to_string);

    // Accept either the Authorization header token or the query-param token.
    let provided = header_token.or(query_token);

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

#[cfg(test)]
mod tests {
    use super::percent_decode;

    #[test]
    fn percent_decode_plain_text_unchanged() {
        assert_eq!(percent_decode("mytoken123"), "mytoken123");
    }

    #[test]
    fn percent_decode_slash_encoded() {
        assert_eq!(percent_decode("tok%2Fwith%2Fslashes"), "tok/with/slashes");
    }

    #[test]
    fn percent_decode_equals_and_plus() {
        assert_eq!(percent_decode("base64%3D%3D"), "base64==");
        assert_eq!(percent_decode("a%2Bb"), "a+b");
    }

    #[test]
    fn percent_decode_percent_encoded_percent() {
        assert_eq!(percent_decode("100%25"), "100%");
    }

    #[test]
    fn percent_decode_incomplete_sequence_passed_through() {
        assert_eq!(percent_decode("abc%2"), "abc%2");
        assert_eq!(percent_decode("abc%"), "abc%");
    }

    #[test]
    fn percent_decode_invalid_hex_passed_through() {
        assert_eq!(percent_decode("abc%ZZdef"), "abc%ZZdef");
    }
}
