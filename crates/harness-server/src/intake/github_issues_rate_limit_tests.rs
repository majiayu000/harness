use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

fn repo_config_for(repo: &str) -> harness_core::config::intake::GitHubRepoConfig {
    harness_core::config::intake::GitHubRepoConfig {
        repo: repo.to_string(),
        label: "harness".to_string(),
        project_root: None,
        auto_merge: None,
        merge_method: None,
        delete_branch: None,
        require_review_threads_resolved: None,
        require_clean_merge_state: None,
    }
}

#[tokio::test]
async fn poll_shares_rate_limit_throttle_across_repos() -> anyhow::Result<()> {
    async fn rate_limited_issue_page(
        axum::extract::State(requests): axum::extract::State<Arc<AtomicUsize>>,
    ) -> axum::response::Response {
        use axum::response::IntoResponse;

        requests.fetch_add(1, Ordering::SeqCst);
        (
            axum::http::StatusCode::TOO_MANY_REQUESTS,
            [(axum::http::header::RETRY_AFTER, "120")],
            "rate limited",
        )
            .into_response()
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base_url = format!("http://{}", listener.local_addr()?);
    let requests = Arc::new(AtomicUsize::new(0));
    let app = axum::Router::new()
        .route(
            "/repos/owner/repo/issues",
            axum::routing::get(rate_limited_issue_page),
        )
        .route(
            "/repos/owner/other/issues",
            axum::routing::get(rate_limited_issue_page),
        )
        .with_state(Arc::clone(&requests));
    let server = tokio::spawn(async move { axum::serve(listener, app).await });

    let throttle = Arc::new(GitHubRateLimitThrottle::default());
    let first = GitHubIssuesPoller::new_with_token_and_throttle(
        &repo_config_for("owner/repo"),
        None,
        Some("token".to_string()),
        Arc::clone(&throttle),
    );
    let second = GitHubIssuesPoller::new_with_token_and_throttle(
        &repo_config_for("owner/other"),
        None,
        Some("token".to_string()),
        Arc::clone(&throttle),
    );

    let first_issues = first.poll_from_api_base_url(&base_url).await?;
    let second_issues = second.poll_from_api_base_url(&base_url).await?;

    server.abort();
    match server.await {
        Err(join_error) if join_error.is_cancelled() => {}
        Err(join_error) => return Err(join_error.into()),
        Ok(Err(error)) => return Err(error.into()),
        Ok(Ok(())) => {}
    }

    assert!(first_issues.is_empty());
    assert!(second_issues.is_empty());
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    Ok(())
}
