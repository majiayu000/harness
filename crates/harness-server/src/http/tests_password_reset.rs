use crate::http::{
    auth,
    misc_routes::{disabled_password_reset_response, prepare_password_reset_request},
    rate_limit::PasswordResetRateLimiter,
};
use axum::http::StatusCode;

#[tokio::test]
async fn password_reset_returns_not_implemented() -> anyhow::Result<()> {
    let limiter = PasswordResetRateLimiter::new(2);
    let email = prepare_password_reset_request(&limiter, 2, "user@example.com")
        .expect("valid email should pass rate limiting");
    assert_eq!(email, "user@example.com");

    let (status, json) = disabled_password_reset_response();
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
    assert_eq!(json["error"], "password reset is not yet implemented");
    Ok(())
}

#[tokio::test]
async fn password_reset_rejects_blank_email() -> anyhow::Result<()> {
    let limiter = PasswordResetRateLimiter::new(2);
    let (status, json) = prepare_password_reset_request(&limiter, 2, "   ").unwrap_err();

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json["error"], "email is required");
    Ok(())
}

#[tokio::test]
async fn password_reset_rate_limit_uses_normalized_email() -> anyhow::Result<()> {
    let limiter = PasswordResetRateLimiter::new(2);

    for _ in 0..2 {
        let email = prepare_password_reset_request(&limiter, 2, "  USER@example.com  ")
            .expect("normalized email should be allowed within the rate limit");
        assert_eq!(email, "user@example.com");
    }

    let (status, json) =
        prepare_password_reset_request(&limiter, 2, "user@example.com").unwrap_err();

    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(
        json["error"]
            .as_str()
            .unwrap_or("")
            .contains("rate limit exceeded"),
        "expected rate limit error body, got: {json}"
    );
    Ok(())
}

#[tokio::test]
async fn password_reset_exempt_from_auth() -> anyhow::Result<()> {
    assert!(
        auth::is_auth_exempt_path("/auth/reset-password"),
        "password reset endpoint must not require auth"
    );
    Ok(())
}
