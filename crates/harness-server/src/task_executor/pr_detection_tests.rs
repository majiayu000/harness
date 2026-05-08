use super::*;
use axum::{
    extract::State,
    http::{header, HeaderMap, HeaderValue, StatusCode, Uri},
    response::IntoResponse,
    routing::get,
    Router,
};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

fn item(title: &str, body: &str) -> GhPrListItem {
    GhPrListItem {
        number: 1,
        head_ref_name: "feat/x".to_string(),
        url: "https://example.test/pr/1".to_string(),
        title: title.to_string(),
        body: body.to_string(),
    }
}

#[test]
fn parse_next_link_extracts_next_relation() {
    let link = r#"<https://api.github.com/repos/o/r/pulls?page=2>; rel="next", <https://api.github.com/repos/o/r/pulls?page=4>; rel="last""#;
    assert_eq!(
        parse_next_link(link),
        Some("https://api.github.com/repos/o/r/pulls?page=2".to_string())
    );
}

#[test]
fn parse_next_link_returns_none_without_next_relation() {
    let link = r#"<https://api.github.com/repos/o/r/pulls?page=4>; rel="last""#;
    assert_eq!(parse_next_link(link), None);
}

#[test]
fn parse_next_link_accepts_whitespace_around_rel_equals() {
    let link = r#"<https://api.github.com/repos/o/r/pulls?page=2>; rel = "next""#;
    assert_eq!(
        parse_next_link(link),
        Some("https://api.github.com/repos/o/r/pulls?page=2".to_string())
    );
}

struct PaginatedPrState {
    base_url: String,
    requests: AtomicUsize,
}

fn page_from_uri(uri: &Uri) -> usize {
    uri.query()
        .and_then(|query| {
            query.split('&').find_map(|part| {
                part.strip_prefix("page=")
                    .and_then(|value| value.parse::<usize>().ok())
            })
        })
        .unwrap_or(1)
}

async fn paginated_prs_handler(
    State(state): State<Arc<PaginatedPrState>>,
    uri: Uri,
) -> impl IntoResponse {
    state.requests.fetch_add(1, Ordering::SeqCst);
    let page = page_from_uri(&uri);

    let body = if page == 1 {
        serde_json::json!([
            {
                "number": 101,
                "html_url": "https://github.com/owner/repo/pull/101",
                "title": "mentions #998",
                "body": "related to #998 but does not close it",
                "head": {"ref": "docs-998"}
            }
        ])
        .to_string()
    } else {
        serde_json::json!([
            {
                "number": 102,
                "html_url": "https://github.com/owner/repo/pull/102",
                "title": "Fix PR dedup pagination",
                "body": "Fixes #998",
                "head": {"ref": "fix-998-pagination"}
            }
        ])
        .to_string()
    };

    let mut headers = HeaderMap::new();
    if page == 1 {
        let next_url = format!(
            "{}/repos/owner/repo/pulls?state=open&per_page=100&page=2",
            state.base_url
        );
        headers.insert(
            header::LINK,
            HeaderValue::from_str(&format!("<{next_url}>; rel=\"next\""))
                .expect("valid link header"),
        );
    }
    (headers, body)
}

async fn endless_prs_handler(
    State(state): State<Arc<PaginatedPrState>>,
    uri: Uri,
) -> impl IntoResponse {
    state.requests.fetch_add(1, Ordering::SeqCst);
    let page = page_from_uri(&uri);
    let body = serde_json::json!([
        {
            "number": page,
            "html_url": format!("https://github.com/owner/repo/pull/{page}"),
            "title": format!("mentions #998 on page {page}"),
            "body": "related to #998 but does not close it",
            "head": {"ref": format!("docs-998-page-{page}")}
        }
    ])
    .to_string();

    let next_url = format!(
        "{}/repos/owner/repo/pulls?state=open&per_page=100&page={}",
        state.base_url,
        page + 1
    );
    let mut headers = HeaderMap::new();
    headers.insert(
        header::LINK,
        HeaderValue::from_str(&format!("<{next_url}>; rel=\"next\"")).expect("valid link header"),
    );
    (headers, body)
}

async fn failing_prs_handler() -> impl IntoResponse {
    (StatusCode::SERVICE_UNAVAILABLE, "temporarily unavailable")
}

async fn rate_limited_prs_handler() -> impl IntoResponse {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::HeaderName::from_static("x-ratelimit-remaining"),
        HeaderValue::from_static("0"),
    );
    headers.insert(header::RETRY_AFTER, HeaderValue::from_static("60"));
    (
        StatusCode::FORBIDDEN,
        headers,
        "API rate limit exceeded for user.",
    )
}

#[tokio::test]
async fn existing_pr_lookup_follows_next_page() -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base_url = format!("http://{}", listener.local_addr()?);
    let state = Arc::new(PaginatedPrState {
        base_url: base_url.clone(),
        requests: AtomicUsize::new(0),
    });
    let app = Router::new()
        .route("/repos/owner/repo/pulls", get(paginated_prs_handler))
        .with_state(state.clone());
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let found = find_existing_pr_for_issue_in_repo(
        &reqwest::Client::new(),
        "owner/repo",
        998,
        None,
        &base_url,
    )
    .await?;

    server.abort();

    assert_eq!(
        found,
        Some((
            102,
            "fix-998-pagination".to_string(),
            "https://github.com/owner/repo/pull/102".to_string()
        ))
    );
    assert_eq!(state.requests.load(Ordering::SeqCst), 2);
    Ok(())
}

#[tokio::test]
async fn existing_pr_lookup_stops_after_page_limit_without_error() -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base_url = format!("http://{}", listener.local_addr()?);
    let state = Arc::new(PaginatedPrState {
        base_url: base_url.clone(),
        requests: AtomicUsize::new(0),
    });
    let app = Router::new()
        .route("/repos/owner/repo/pulls", get(endless_prs_handler))
        .with_state(state.clone());
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let found = find_existing_pr_for_issue_in_repo(
        &reqwest::Client::new(),
        "owner/repo",
        998,
        None,
        &base_url,
    )
    .await?;

    server.abort();

    assert_eq!(found, None);
    assert_eq!(
        state.requests.load(Ordering::SeqCst),
        GITHUB_PR_LOOKUP_MAX_PAGES
    );
    Ok(())
}

#[tokio::test]
async fn existing_pr_lookup_errors_when_github_page_fails() -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base_url = format!("http://{}", listener.local_addr()?);
    let app = Router::new().route("/repos/owner/repo/pulls", get(failing_prs_handler));
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let err = find_existing_pr_for_issue_in_repo(
        &reqwest::Client::new(),
        "owner/repo",
        998,
        None,
        &base_url,
    )
    .await
    .expect_err("GitHub lookup failure should not be treated as no PR found");

    server.abort();

    assert!(err.to_string().contains("returned 503"));
    Ok(())
}

#[tokio::test]
async fn existing_pr_lookup_preserves_rate_limit_context() -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base_url = format!("http://{}", listener.local_addr()?);
    let app = Router::new().route("/repos/owner/repo/pulls", get(rate_limited_prs_handler));
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let err = find_existing_pr_for_issue_in_repo(
        &reqwest::Client::new(),
        "owner/repo",
        998,
        None,
        &base_url,
    )
    .await
    .expect_err("GitHub rate limit failure should be visible to retry classification");

    server.abort();

    let err = err.to_string();
    assert!(err.contains("rate limit"), "unexpected error: {err}");
    assert!(err.contains("retry-after=60"), "unexpected error: {err}");
    assert!(
        err.contains("x-ratelimit-remaining=0"),
        "unexpected error: {err}"
    );
    Ok(())
}

// --- pr_claims_to_close_issue: positive cases ---

#[test]
fn title_suffix_alone_does_not_match() {
    // A PR title ending with `(#N)` but no GitHub closing keyword in title
    // or body is NOT a close signal. Human-authored PRs like
    // "docs: mention scheduler cleanup (#799)" would otherwise be falsely
    // reused as the branch for issue #799.
    let it = item("fix(dedup): guard against closed PRs (#791)", "");
    assert!(!pr_claims_to_close_issue(&it, 791, None));
}

#[test]
fn title_suffix_with_close_keyword_matches() {
    // A PR that has both the suffix AND a closing keyword in the body is fine.
    let it = item("fix(dedup): guard against closed PRs (#791)", "Fixes #791");
    assert!(pr_claims_to_close_issue(&it, 791, None));
}

#[test]
fn closes_keyword_in_body_matches() {
    let it = item("some PR", "This PR closes #791 as agreed.");
    assert!(pr_claims_to_close_issue(&it, 791, None));
}

#[test]
fn fixes_keyword_in_body_matches() {
    let it = item("some PR", "Fixes #791");
    assert!(pr_claims_to_close_issue(&it, 791, None));
}

#[test]
fn resolves_keyword_case_insensitive() {
    let it = item("RESOLVES #791 finally", "");
    assert!(pr_claims_to_close_issue(&it, 791, None));
}

#[test]
fn closes_with_colon_between_keyword_and_number() {
    // "closes: #791" — trim trailing colon before keyword check.
    let it = item("", "closes: #791");
    assert!(pr_claims_to_close_issue(&it, 791, None));
}

// --- pr_claims_to_close_issue: negative cases (the #799 bug) ---

#[test]
fn plain_mention_does_not_match() {
    // The exact scenario that caused #799: PR body merely references
    // another issue number as context, not as a close target.
    let it = item(
        "feat(scheduler): add periodic retry (#794)",
        "depends on #791 being fixed first",
    );
    assert!(!pr_claims_to_close_issue(&it, 791, None));
}

#[test]
fn relates_to_does_not_match() {
    let it = item("some PR", "relates to #791");
    assert!(!pr_claims_to_close_issue(&it, 791, None));
}

#[test]
fn see_also_does_not_match() {
    let it = item("some PR", "See also #791 for context.");
    assert!(!pr_claims_to_close_issue(&it, 791, None));
}

#[test]
fn word_boundary_prefixes_does_not_match_fix() {
    // "prefixes #791" must NOT match the keyword "fixes".
    let it = item("some PR", "prefixes #791 with a label");
    assert!(!pr_claims_to_close_issue(&it, 791, None));
}

#[test]
fn different_issue_number_in_close_keyword_does_not_match() {
    // PR closes #100 but we're asking about #791 — must not match.
    let it = item("", "closes #100, mentions #791");
    assert!(!pr_claims_to_close_issue(&it, 791, None));
}

#[test]
fn title_suffix_for_other_issue_does_not_match() {
    let it = item("fix: something (#100)", "passing reference to #791");
    assert!(!pr_claims_to_close_issue(&it, 791, None));
}

#[test]
fn title_suffix_not_trailing_does_not_match() {
    // "(#791)" appears in the middle of the title but "#812" is the real
    // trailing harness suffix — must NOT match issue 791.
    let it = item("follow-up to prior fix (#791) (#812)", "");
    assert!(!pr_claims_to_close_issue(&it, 791, None));
}

#[test]
fn title_suffix_not_trailing_context_mention_does_not_match() {
    // "docs: mention prior fix (#791)" — no closing keyword, just a
    // mid-title mention that happens to end a sub-phrase.
    let it = item("docs: mention prior fix (#791) and move on (#812)", "");
    assert!(!pr_claims_to_close_issue(&it, 791, None));
}

#[test]
fn prefix_number_does_not_match_longer_issue() {
    // Codex-flagged regression: scanning for `#79` must NOT match `#791`.
    // A PR body "closes #791" claims to close issue 791, not issue 79.
    let it = item("", "closes #791");
    assert!(!pr_claims_to_close_issue(&it, 79, None));
}

#[test]
fn exact_issue_still_matches_even_when_prefix_of_another() {
    // When asked about #79, a PR body that actually says "closes #79"
    // (e.g. followed by space/punctuation, not another digit) must match.
    let it = item("", "closes #79 and also mentions #791");
    assert!(pr_claims_to_close_issue(&it, 79, None));
}

// --- repo-qualified close references (Codex P2) ---

#[test]
fn repo_qualified_fixes_matches() {
    // GitHub allows "Fixes owner/repo#N" as a valid closing reference.
    let it = item("", "Fixes majiayu000/harness#791");
    assert!(pr_claims_to_close_issue(&it, 791, None));
}

#[test]
fn repo_qualified_closes_matches() {
    let it = item("", "closes owner/repo#791 in this body");
    assert!(pr_claims_to_close_issue(&it, 791, None));
}

#[test]
fn repo_qualified_plain_mention_does_not_match() {
    // "related to owner/repo#N" must NOT match — no close keyword.
    let it = item("", "related to majiayu000/harness#791");
    assert!(!pr_claims_to_close_issue(&it, 791, None));
}

#[test]
fn repo_qualified_boundary_guards_longer_number() {
    // "fixes owner/repo#791" must not match when querying issue 79.
    let it = item("", "fixes owner/repo#791");
    assert!(!pr_claims_to_close_issue(&it, 79, None));
}

#[test]
fn repo_qualified_cross_repo_does_not_match() {
    // A PR that closes rust-lang/rust#791 must not match issue #791 in our repo.
    let it = item("", "Fixes rust-lang/rust#791");
    assert!(!pr_claims_to_close_issue(
        &it,
        791,
        Some("majiayu000/harness")
    ));
}

#[test]
fn repo_qualified_same_repo_matches_with_slug() {
    let it = item("", "Fixes majiayu000/harness#791");
    assert!(pr_claims_to_close_issue(
        &it,
        791,
        Some("majiayu000/harness")
    ));
}

#[test]
fn cross_field_keyword_does_not_match() {
    // "fixes" at the end of the title must NOT pair with "#791" at the start
    // of the body. Scanning each field independently prevents this.
    let it = item("CI fixes", "#791 is the main bug");
    assert!(!pr_claims_to_close_issue(&it, 791, None));
}

#[test]
fn trailing_alpha_after_issue_number_does_not_match() {
    // "fixes #791abc" — alphanumeric character after the number must not match.
    let it = item("", "fixes #791abc");
    assert!(!pr_claims_to_close_issue(&it, 791, None));
}

// --- word_boundary_before ---

#[test]
fn word_boundary_before_at_start_is_boundary() {
    assert!(word_boundary_before("fix", 3));
}

#[test]
fn word_boundary_before_space_is_boundary() {
    assert!(word_boundary_before(" fix", 3));
}

#[test]
fn word_boundary_before_alnum_is_not_boundary() {
    // "prefix" — char before last 3 ("fix") is 'e', alphanumeric.
    assert!(!word_boundary_before("prefix", 3));
}

#[test]
fn word_boundary_before_multibyte_unicode_alnum_is_not_boundary() {
    // `é` is a multi-byte UTF-8 character (U+00E9, 0xC3 0xA9). A raw-byte
    // check would see 0xA9, which is not ASCII-alphanumeric, and falsely
    // report a boundary. The char-aware check must return false here.
    assert!(!word_boundary_before("éfix", 3));
}

#[test]
fn unicode_prefix_does_not_match_keyword() {
    // "éfixes #791" — no word boundary before "fixes", must not match.
    let it = item("", "éfixes #791");
    assert!(!pr_claims_to_close_issue(&it, 791, None));
}

#[test]
fn unicode_letter_prefix_does_not_match_keyword() {
    // A Unicode letter before "fixes" is alphanumeric, so there is no
    // word boundary before the keyword.
    let it = item("", "éfixes #791");
    assert!(!pr_claims_to_close_issue(&it, 791, None));
}

// --- parse_harness_mention_command (pre-existing, light coverage) ---

#[test]
fn parses_fix_ci_command() {
    assert_eq!(
        parse_harness_mention_command("@harness fix ci please"),
        Some(HarnessMentionCommand::FixCi)
    );
}

#[test]
fn parses_review_command() {
    assert_eq!(
        parse_harness_mention_command("@harness review"),
        Some(HarnessMentionCommand::Review)
    );
}

#[test]
fn parses_plain_mention() {
    assert_eq!(
        parse_harness_mention_command("hey @harness, take a look"),
        Some(HarnessMentionCommand::Mention)
    );
}

#[test]
fn no_mention_returns_none() {
    assert_eq!(parse_harness_mention_command("nothing here"), None);
}

// --- parse_repo_slug_from_remote_url ---

#[test]
fn parses_ssh_remote() {
    assert_eq!(
        parse_repo_slug_from_remote_url("git@github.com:owner/repo.git"),
        Some("owner/repo".to_string())
    );
}

#[test]
fn parses_https_remote() {
    assert_eq!(
        parse_repo_slug_from_remote_url("https://github.com/owner/repo.git"),
        Some("owner/repo".to_string())
    );
}

#[test]
fn parses_https_remote_without_git_suffix() {
    assert_eq!(
        parse_repo_slug_from_remote_url("https://github.com/owner/repo"),
        Some("owner/repo".to_string())
    );
}

#[test]
fn parses_ssh_scheme_remote() {
    // ssh://git@github.com/owner/repo.git — distinct from SCP-style
    // git@github.com:owner/repo.git; without this branch, detect_repo_slug
    // returns None and the cross-repo guard is silently disabled.
    assert_eq!(
        parse_repo_slug_from_remote_url("ssh://git@github.com/owner/repo.git"),
        Some("owner/repo".to_string())
    );
}

#[test]
fn rejects_unknown_remote() {
    assert_eq!(
        parse_repo_slug_from_remote_url("https://gitlab.com/owner/repo.git"),
        None
    );
}

#[test]
fn parse_git_config_accepts_dotted_remote_section() {
    let config = r#"
        [remote.origin]
            url = https://github.com/owner/repo.git
    "#;
    assert_eq!(
        parse_remote_urls_from_git_config(config),
        vec![(
            "origin".to_string(),
            "https://github.com/owner/repo.git".to_string()
        )]
    );
}

#[test]
fn parse_git_config_accepts_quoted_remote_with_spacing_and_comments() {
    let config = r#"
        # ignored
        [remote "upstream"]
            fetch = +refs/heads/*:refs/remotes/upstream/*
            url=git@github.com:owner/repo.git ; mirror used by tests
        [branch "main"]
            remote = upstream
    "#;
    assert_eq!(
        parse_remote_urls_from_git_config(config),
        vec![(
            "upstream".to_string(),
            "git@github.com:owner/repo.git".to_string()
        )]
    );
}
