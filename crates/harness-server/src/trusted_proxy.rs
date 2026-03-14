use axum::http::HeaderMap;
use std::net::IpAddr;

/// Extracts the real client IP from request headers, respecting trusted proxy configuration.
///
/// If `peer_addr` is in `trusted_proxies`, the leftmost IP from the `X-Forwarded-For` header
/// is used as the client IP. Otherwise, `peer_addr` is returned as-is.
///
/// Returns `None` when `peer_addr` is `None`.
pub fn extract_client_ip(
    headers: &HeaderMap,
    peer_addr: Option<IpAddr>,
    trusted_proxies: &[String],
) -> Option<IpAddr> {
    let peer = peer_addr?;
    if is_trusted_proxy(peer, trusted_proxies) {
        if let Some(xff) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
            if let Some(first) = xff.split(',').next().map(str::trim) {
                if let Ok(ip) = first.parse::<IpAddr>() {
                    return Some(ip);
                }
            }
        }
    }
    Some(peer)
}

fn is_trusted_proxy(addr: IpAddr, trusted_proxies: &[String]) -> bool {
    trusted_proxies.iter().any(|proxy| {
        proxy
            .parse::<IpAddr>()
            .map(|ip| ip == addr)
            .unwrap_or(false)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;
    use std::net::IpAddr;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn untrusted_proxy_ignores_xff() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "1.2.3.4".parse().unwrap());
        let peer = ip("192.168.1.1");
        let result = extract_client_ip(&headers, Some(peer), &[]);
        assert_eq!(result, Some(peer));
    }

    #[test]
    fn trusted_proxy_uses_xff() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "1.2.3.4".parse().unwrap());
        let peer = ip("10.0.0.1");
        let proxies = vec!["10.0.0.1".to_string()];
        let result = extract_client_ip(&headers, Some(peer), &proxies);
        assert_eq!(result, Some(ip("1.2.3.4")));
    }

    #[test]
    fn trusted_proxy_uses_first_xff_ip() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "1.2.3.4, 5.6.7.8".parse().unwrap());
        let peer = ip("10.0.0.1");
        let proxies = vec!["10.0.0.1".to_string()];
        let result = extract_client_ip(&headers, Some(peer), &proxies);
        assert_eq!(result, Some(ip("1.2.3.4")));
    }

    #[test]
    fn trusted_proxy_no_xff_falls_back_to_peer() {
        let headers = HeaderMap::new();
        let peer = ip("10.0.0.1");
        let proxies = vec!["10.0.0.1".to_string()];
        let result = extract_client_ip(&headers, Some(peer), &proxies);
        assert_eq!(result, Some(peer));
    }

    #[test]
    fn trusted_proxy_invalid_xff_falls_back_to_peer() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "not-an-ip".parse().unwrap());
        let peer = ip("10.0.0.1");
        let proxies = vec!["10.0.0.1".to_string()];
        let result = extract_client_ip(&headers, Some(peer), &proxies);
        assert_eq!(result, Some(peer));
    }

    #[test]
    fn no_peer_returns_none() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "1.2.3.4".parse().unwrap());
        let proxies = vec!["10.0.0.1".to_string()];
        let result = extract_client_ip(&headers, None, &proxies);
        assert_eq!(result, None);
    }

    #[test]
    fn untrusted_peer_is_not_in_proxy_list() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "1.2.3.4".parse().unwrap());
        let peer = ip("172.16.0.5");
        let proxies = vec!["10.0.0.1".to_string(), "10.0.0.2".to_string()];
        let result = extract_client_ip(&headers, Some(peer), &proxies);
        assert_eq!(result, Some(peer));
    }
}
