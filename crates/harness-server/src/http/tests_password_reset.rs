use std::sync::Arc;

use axum::{body::Body, http::Request, http::StatusCode, middleware, routing::post, Router};
use tower::ServiceExt;

use crate::http::{auth, misc_routes::password_reset, AppState};
use crate::test_helpers::make_test_state_with_config;

fn password_reset_app(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/auth/reset-password", post(password_reset))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::api_auth_middleware,
        ))
        .with_state(state)
}

async fn response_json(response: axum::response::Response) -> anyhow::Result<serde_json::Value> {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
    Ok(serde_json::from_slice(&bytes)?)
}

#[tokio::test]
async fn password_reset_returns_501_not_implemented() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state =
        make_test_state_with_config(dir.path(), harness_core::config::HarnessConfig::default())
            .await?;
    let app = password_reset_app(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/reset-password")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"email": "user@example.com"}).to_string(),
                ))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
    let json = response_json(response).await?;
    assert!(
        json["error"]
            .as_str()
            .unwrap_or("")
            .contains("not yet implemented"),
        "expected 'not yet implemented' in error body, got: {json}"
    );
    Ok(())
}

#[tokio::test]
async fn password_reset_exempt_from_auth() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut config = harness_core::config::HarnessConfig::default();
    config.server.api_token = Some("secret123".to_string());
    let state = make_test_state_with_config(dir.path(), config).await?;
    let app = password_reset_app(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/reset-password")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"email": "user@example.com"}).to_string(),
                ))?,
        )
        .await?;

    assert_ne!(
        response.status(),
        StatusCode::UNAUTHORIZED,
        "password reset endpoint must not require auth"
    );
    assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
    Ok(())
}
