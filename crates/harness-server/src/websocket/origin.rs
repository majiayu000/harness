//! WebSocket origin validation: CSWH prevention for the `/ws` endpoint.

use axum::http::HeaderMap;

/// Returns true if the origin is a localhost origin (safe for local dev tools).
///
/// Parses the host from the origin to prevent bypass via domains like
/// `http://localhost.evil.com`.
///
/// `"null"` is intentionally NOT treated as local: browsers send `Origin: null`
/// for `file:` URLs and sandboxed iframes, which are untrusted contexts.
fn is_local_origin(origin: &str) -> bool {
    // Origin format: scheme://host or scheme://host:port
    // Extract the host by stripping scheme and optional port.
    let host = origin
        .split_once("://")
        .map(|(_, rest)| {
            rest.split(':')
                .next()
                .unwrap_or("")
                .split('/')
                .next()
                .unwrap_or("")
        })
        .unwrap_or("");
    host == "localhost" || host == "127.0.0.1"
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum OriginValidationError {
    InvalidUtf8,
    NonLocal(String),
}

pub(super) fn validate_origin_header(headers: &HeaderMap) -> Result<(), OriginValidationError> {
    let Some(origin) = headers.get("Origin") else {
        return Ok(());
    };

    let origin_str = origin
        .to_str()
        .map_err(|_| OriginValidationError::InvalidUtf8)?;

    if is_local_origin(origin_str) {
        Ok(())
    } else {
        Err(OriginValidationError::NonLocal(origin_str.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::{is_local_origin, validate_origin_header, OriginValidationError};
    use axum::http::{HeaderMap, HeaderValue};

    #[test]
    fn is_local_origin_accepts_localhost_variants() {
        assert!(is_local_origin("http://localhost"));
        assert!(is_local_origin("http://localhost:3000"));
        assert!(is_local_origin("https://localhost:9800"));
        assert!(is_local_origin("http://127.0.0.1"));
        assert!(is_local_origin("http://127.0.0.1:8080"));
    }

    #[test]
    fn is_local_origin_rejects_non_local() {
        assert!(!is_local_origin("http://example.com"));
        assert!(!is_local_origin("http://localhost.evil.com"));
        assert!(!is_local_origin("http://192.168.1.1"));
        assert!(!is_local_origin("http://0.0.0.0"));
        // "null" is sent by browsers for file: URLs and sandboxed iframes —
        // these are untrusted contexts and must NOT be treated as local.
        assert!(!is_local_origin("null"));
    }

    #[test]
    fn validate_origin_header_allows_missing_origin() {
        let headers = HeaderMap::new();
        assert!(validate_origin_header(&headers).is_ok());
    }

    #[test]
    fn validate_origin_header_allows_local_origin() {
        let mut headers = HeaderMap::new();
        headers.insert("Origin", HeaderValue::from_static("http://localhost:9800"));
        assert!(validate_origin_header(&headers).is_ok());
    }

    #[test]
    fn validate_origin_header_rejects_remote_origin() {
        let mut headers = HeaderMap::new();
        headers.insert("Origin", HeaderValue::from_static("http://evil.com"));
        assert!(matches!(
            validate_origin_header(&headers),
            Err(OriginValidationError::NonLocal(_))
        ));
    }

    #[test]
    fn validate_origin_header_rejects_non_utf8_origin() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "Origin",
            HeaderValue::from_bytes(b"http://localhost\xff").expect("valid raw header value"),
        );

        assert!(matches!(
            validate_origin_header(&headers),
            Err(OriginValidationError::InvalidUtf8)
        ));
    }
}
