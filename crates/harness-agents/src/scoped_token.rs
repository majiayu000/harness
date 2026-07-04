use async_trait::async_trait;
use reqwest::header::{ACCEPT, AUTHORIZATION, USER_AGENT};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::time::{Duration, SystemTime};

pub const SCOPED_GITHUB_TOKEN_ENV: &str = "HARNESS_SCOPED_GITHUB_TOKEN";
pub const CONTAINER_GITHUB_TOKEN_ENV: &str = "GITHUB_TOKEN";
pub const CONTAINER_GH_TOKEN_ENV: &str = "GH_TOKEN";

const DEFAULT_SCOPED_TOKEN_TTL: Duration = Duration::from_secs(3600);
const DEFAULT_GITHUB_API_BASE_URL: &str = "https://api.github.com/";
const GITHUB_API_VERSION: &str = "2022-11-28";
const HARNESS_GITHUB_USER_AGENT: &str = "harness-agents";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopedGitHubTokenRequest {
    pub repo: String,
    pub task_timeout: Duration,
    pub requested_ttl: Option<Duration>,
    pub permissions: ScopedGitHubPermissions,
}

impl ScopedGitHubTokenRequest {
    pub fn new(repo: impl Into<String>, task_timeout: Duration) -> Self {
        Self {
            repo: repo.into(),
            task_timeout,
            requested_ttl: None,
            permissions: ScopedGitHubPermissions::contents_and_pull_requests_write(),
        }
    }

    pub fn with_requested_ttl(mut self, requested_ttl: Duration) -> Self {
        self.requested_ttl = Some(requested_ttl);
        self
    }

    pub fn effective_ttl(&self) -> Result<Duration, ScopedTokenError> {
        validate_scoped_token_repo_slug(&self.repo)?;
        if self.task_timeout.is_zero() {
            return Err(ScopedTokenError::InvalidTimeout);
        }
        let requested = self.requested_ttl.unwrap_or(DEFAULT_SCOPED_TOKEN_TTL);
        let ttl = requested.min(self.task_timeout);
        if ttl.is_zero() {
            return Err(ScopedTokenError::InvalidTimeout);
        }
        Ok(ttl)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScopedGitHubPermissions {
    pub contents: PermissionAccess,
    pub pull_requests: PermissionAccess,
}

impl ScopedGitHubPermissions {
    pub fn contents_and_pull_requests_write() -> Self {
        Self {
            contents: PermissionAccess::Write,
            pull_requests: PermissionAccess::Write,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionAccess {
    Write,
}

#[derive(Clone, PartialEq, Eq)]
pub struct ScopedGitHubTokenLease {
    pub repo: String,
    token: String,
    pub expires_at: SystemTime,
    pub permissions: ScopedGitHubPermissions,
}

impl ScopedGitHubTokenLease {
    pub fn new(
        request: &ScopedGitHubTokenRequest,
        token: impl Into<String>,
        now: SystemTime,
    ) -> Result<Self, ScopedTokenError> {
        let ttl = request.effective_ttl()?;
        let token = token.into();
        if token.trim().is_empty() {
            return Err(ScopedTokenError::EmptyToken);
        }
        Ok(Self {
            repo: request.repo.clone(),
            token,
            expires_at: now + ttl,
            permissions: request.permissions.clone(),
        })
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn container_token_env_pairs(&self) -> [(&'static str, String); 2] {
        [
            (CONTAINER_GITHUB_TOKEN_ENV, self.token.clone()),
            (CONTAINER_GH_TOKEN_ENV, self.token.clone()),
        ]
    }
}

impl fmt::Debug for ScopedGitHubTokenLease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScopedGitHubTokenLease")
            .field("repo", &self.repo)
            .field("token", &"[REDACTED]")
            .field("expires_at", &self.expires_at)
            .field("permissions", &self.permissions)
            .finish()
    }
}

#[async_trait]
pub trait ScopedGitHubTokenIssuer: Send + Sync {
    async fn mint_repo_token(
        &self,
        request: &ScopedGitHubTokenRequest,
        now: SystemTime,
    ) -> Result<ScopedGitHubTokenLease, ScopedTokenError>;

    async fn revoke_repo_token(
        &self,
        lease: &ScopedGitHubTokenLease,
    ) -> Result<(), ScopedTokenError>;
}

#[derive(Clone)]
pub struct GitHubInstallationTokenIssuer {
    client: reqwest::Client,
    api_base_url: reqwest::Url,
    app_jwt: String,
    installation_id: u64,
}

impl GitHubInstallationTokenIssuer {
    pub fn new(app_jwt: impl Into<String>, installation_id: u64) -> Result<Self, ScopedTokenError> {
        Self::with_client(
            reqwest::Client::new(),
            app_jwt,
            installation_id,
            DEFAULT_GITHUB_API_BASE_URL,
        )
    }

    pub fn with_client(
        client: reqwest::Client,
        app_jwt: impl Into<String>,
        installation_id: u64,
        api_base_url: impl AsRef<str>,
    ) -> Result<Self, ScopedTokenError> {
        let app_jwt = app_jwt.into();
        if app_jwt.trim().is_empty() {
            return Err(ScopedTokenError::EmptyAppJwt);
        }
        if installation_id == 0 {
            return Err(ScopedTokenError::InvalidInstallationId);
        }
        Ok(Self {
            client,
            api_base_url: parse_api_base_url(api_base_url.as_ref())?,
            app_jwt,
            installation_id,
        })
    }

    fn endpoint(&self, path: &str) -> Result<reqwest::Url, ScopedTokenError> {
        self.api_base_url
            .join(path)
            .map_err(|error| ScopedTokenError::InvalidApiBaseUrl(error.to_string()))
    }
}

impl fmt::Debug for GitHubInstallationTokenIssuer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GitHubInstallationTokenIssuer")
            .field("api_base_url", &self.api_base_url)
            .field("app_jwt", &"[REDACTED]")
            .field("installation_id", &self.installation_id)
            .finish()
    }
}

#[async_trait]
impl ScopedGitHubTokenIssuer for GitHubInstallationTokenIssuer {
    async fn mint_repo_token(
        &self,
        request: &ScopedGitHubTokenRequest,
        now: SystemTime,
    ) -> Result<ScopedGitHubTokenLease, ScopedTokenError> {
        let repo_name = repository_name(&request.repo)?;
        request.effective_ttl()?;
        let body = CreateInstallationTokenRequest {
            repositories: vec![repo_name.to_string()],
            permissions: request.permissions.clone(),
        };
        let response = self
            .client
            .post(self.endpoint(&format!(
                "app/installations/{}/access_tokens",
                self.installation_id
            ))?)
            .header(ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
            .header(USER_AGENT, HARNESS_GITHUB_USER_AGENT)
            .header(AUTHORIZATION, format!("Bearer {}", self.app_jwt))
            .json(&body)
            .send()
            .await
            .map_err(scoped_token_http_error)?;
        let response_body = checked_response_body(response).await?;
        let token_response: CreateInstallationTokenResponse = serde_json::from_str(&response_body)
            .map_err(|error| {
                ScopedTokenError::GitHubResponse(format!(
                    "{error}; response={}",
                    trim_response_body(&response_body)
                ))
            })?;
        ScopedGitHubTokenLease::new(request, token_response.token, now)
    }

    async fn revoke_repo_token(
        &self,
        lease: &ScopedGitHubTokenLease,
    ) -> Result<(), ScopedTokenError> {
        let response = self
            .client
            .delete(self.endpoint("installation/token")?)
            .header(ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
            .header(USER_AGENT, HARNESS_GITHUB_USER_AGENT)
            .header(AUTHORIZATION, format!("Bearer {}", lease.token()))
            .send()
            .await
            .map_err(scoped_token_http_error)?;
        checked_response_body(response).await?;
        Ok(())
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ScopedTokenError {
    #[error("GitHub repo slug must be in owner/repo form")]
    InvalidRepo,
    #[error("scoped GitHub token task timeout must be greater than zero")]
    InvalidTimeout,
    #[error("scoped GitHub token issuer returned an empty token")]
    EmptyToken,
    #[error("GitHub App JWT must not be empty")]
    EmptyAppJwt,
    #[error("GitHub installation id must be greater than zero")]
    InvalidInstallationId,
    #[error("GitHub API base URL is invalid: {0}")]
    InvalidApiBaseUrl(String),
    #[error("GitHub scoped token request failed with status {status}: {body}")]
    GitHubStatus { status: u16, body: String },
    #[error("GitHub scoped token HTTP request failed: {0}")]
    GitHubHttp(String),
    #[error("GitHub scoped token response was invalid: {0}")]
    GitHubResponse(String),
}

pub fn scoped_token_env_vars(lease: &ScopedGitHubTokenLease) -> Vec<(&'static str, String)> {
    lease.container_token_env_pairs().into_iter().collect()
}

fn validate_scoped_token_repo_slug(repo: &str) -> Result<(), ScopedTokenError> {
    let repo = repo.trim();
    let Some((owner, name)) = repo.split_once('/') else {
        return Err(ScopedTokenError::InvalidRepo);
    };
    if owner.trim().is_empty() || name.trim().is_empty() || name.contains('/') {
        return Err(ScopedTokenError::InvalidRepo);
    }
    Ok(())
}

fn repository_name(repo: &str) -> Result<&str, ScopedTokenError> {
    validate_scoped_token_repo_slug(repo)?;
    let (_, name) = repo.split_once('/').ok_or(ScopedTokenError::InvalidRepo)?;
    Ok(name)
}

fn parse_api_base_url(value: &str) -> Result<reqwest::Url, ScopedTokenError> {
    let mut url = reqwest::Url::parse(value.trim())
        .map_err(|error| ScopedTokenError::InvalidApiBaseUrl(error.to_string()))?;
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    Ok(url)
}

#[derive(Debug, Serialize)]
struct CreateInstallationTokenRequest {
    repositories: Vec<String>,
    permissions: ScopedGitHubPermissions,
}

#[derive(Debug, Deserialize)]
struct CreateInstallationTokenResponse {
    token: String,
}

async fn checked_response_body(response: reqwest::Response) -> Result<String, ScopedTokenError> {
    let status = response.status();
    let body = response.text().await.map_err(scoped_token_http_error)?;
    if !status.is_success() {
        return Err(ScopedTokenError::GitHubStatus {
            status: status.as_u16(),
            body: trim_response_body(&body),
        });
    }
    Ok(body)
}

fn scoped_token_http_error(error: reqwest::Error) -> ScopedTokenError {
    ScopedTokenError::GitHubHttp(error.to_string())
}

fn trim_response_body(body: &str) -> String {
    const MAX_ERROR_BODY: usize = 512;
    let body = body.trim();
    let mut chars = body.chars();
    let truncated: String = chars.by_ref().take(MAX_ERROR_BODY).collect();
    if chars.next().is_some() {
        format!("{truncated}...")
    } else {
        truncated
    }
}

#[cfg(test)]
mod scoped_token_tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Mutex;
    use std::thread;

    #[derive(Default)]
    struct StaticIssuer {
        revoked: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl ScopedGitHubTokenIssuer for StaticIssuer {
        async fn mint_repo_token(
            &self,
            request: &ScopedGitHubTokenRequest,
            now: SystemTime,
        ) -> Result<ScopedGitHubTokenLease, ScopedTokenError> {
            ScopedGitHubTokenLease::new(request, "scoped-token", now)
        }

        async fn revoke_repo_token(
            &self,
            lease: &ScopedGitHubTokenLease,
        ) -> Result<(), ScopedTokenError> {
            self.revoked.lock().unwrap().push(lease.token().to_string());
            Ok(())
        }
    }

    #[tokio::test]
    async fn scoped_token_ttl_is_capped_by_task_timeout() -> anyhow::Result<()> {
        let now = SystemTime::UNIX_EPOCH;
        let request = ScopedGitHubTokenRequest::new("owner/repo", Duration::from_secs(900))
            .with_requested_ttl(Duration::from_secs(3600));
        let lease = StaticIssuer::default()
            .mint_repo_token(&request, now)
            .await?;

        assert_eq!(lease.expires_at, now + Duration::from_secs(900));
        assert_eq!(
            lease.permissions,
            ScopedGitHubPermissions::contents_and_pull_requests_write()
        );
        Ok(())
    }

    #[test]
    fn scoped_token_rejects_invalid_repo() {
        let request = ScopedGitHubTokenRequest::new("owner/repo/extra", Duration::from_secs(900));

        let error = request
            .effective_ttl()
            .expect_err("invalid repo slug should be rejected");

        assert_eq!(error, ScopedTokenError::InvalidRepo);
    }

    #[tokio::test]
    async fn scoped_token_env_uses_container_github_token_names() -> anyhow::Result<()> {
        let now = SystemTime::UNIX_EPOCH;
        let request = ScopedGitHubTokenRequest::new("owner/repo", Duration::from_secs(900));
        let lease = StaticIssuer::default()
            .mint_repo_token(&request, now)
            .await?;

        assert_eq!(
            scoped_token_env_vars(&lease),
            vec![
                (CONTAINER_GITHUB_TOKEN_ENV, "scoped-token".to_string()),
                (CONTAINER_GH_TOKEN_ENV, "scoped-token".to_string()),
            ]
        );
        Ok(())
    }

    #[tokio::test]
    async fn scoped_token_revokes_at_teardown() -> anyhow::Result<()> {
        let issuer = StaticIssuer::default();
        let now = SystemTime::UNIX_EPOCH;
        let request = ScopedGitHubTokenRequest::new("owner/repo", Duration::from_secs(900));
        let lease = issuer.mint_repo_token(&request, now).await?;

        issuer.revoke_repo_token(&lease).await?;

        assert_eq!(issuer.revoked.lock().unwrap().as_slice(), ["scoped-token"]);
        Ok(())
    }

    #[tokio::test]
    async fn scoped_token_debug_redacts_secret() -> anyhow::Result<()> {
        let now = SystemTime::UNIX_EPOCH;
        let request = ScopedGitHubTokenRequest::new("owner/repo", Duration::from_secs(900));
        let lease = StaticIssuer::default()
            .mint_repo_token(&request, now)
            .await?;

        let debug = format!("{lease:?}");

        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("scoped-token"));
        Ok(())
    }

    #[tokio::test]
    async fn github_app_issuer_mints_repo_scoped_token_with_permissions() -> anyhow::Result<()> {
        let (base_url, request_handle) = spawn_one_request_server(
            "201 Created",
            r#"{"token":"scoped-token","expires_at":"2026-07-04T17:00:00Z"}"#,
        )?;
        let issuer = GitHubInstallationTokenIssuer::with_client(
            reqwest::Client::new(),
            "app-jwt",
            42,
            &base_url,
        )?;
        let now = SystemTime::UNIX_EPOCH;
        let request = ScopedGitHubTokenRequest::new("owner/repo", Duration::from_secs(900));

        let lease = issuer.mint_repo_token(&request, now).await?;

        let http_request = join_request(request_handle)?;
        assert_eq!(lease.token(), "scoped-token");
        assert_eq!(lease.expires_at, now + Duration::from_secs(900));
        assert!(http_request.starts_with("post /app/installations/42/access_tokens http/1.1"));
        assert!(http_request.contains("authorization: bearer app-jwt"));
        assert!(http_request.contains(r#""repositories":["repo"]"#));
        assert!(http_request.contains(r#""contents":"write""#));
        assert!(http_request.contains(r#""pull_requests":"write""#));
        Ok(())
    }

    #[tokio::test]
    async fn github_app_issuer_revokes_installation_token() -> anyhow::Result<()> {
        let (base_url, request_handle) = spawn_one_request_server("204 No Content", "")?;
        let issuer = GitHubInstallationTokenIssuer::with_client(
            reqwest::Client::new(),
            "app-jwt",
            42,
            &base_url,
        )?;
        let request = ScopedGitHubTokenRequest::new("owner/repo", Duration::from_secs(900));
        let lease = ScopedGitHubTokenLease::new(&request, "scoped-token", SystemTime::UNIX_EPOCH)?;

        issuer.revoke_repo_token(&lease).await?;

        let http_request = join_request(request_handle)?;
        assert!(http_request.starts_with("delete /installation/token http/1.1"));
        assert!(http_request.contains("authorization: bearer scoped-token"));
        Ok(())
    }

    #[tokio::test]
    async fn github_app_issuer_surfaces_github_status_errors() -> anyhow::Result<()> {
        let (base_url, request_handle) =
            spawn_one_request_server("403 Forbidden", r#"{"message":"denied"}"#)?;
        let issuer = GitHubInstallationTokenIssuer::with_client(
            reqwest::Client::new(),
            "app-jwt",
            42,
            &base_url,
        )?;
        let request = ScopedGitHubTokenRequest::new("owner/repo", Duration::from_secs(900));

        let error = issuer
            .mint_repo_token(&request, SystemTime::UNIX_EPOCH)
            .await
            .expect_err("GitHub status failures should be surfaced");

        join_request(request_handle)?;
        assert_eq!(
            error,
            ScopedTokenError::GitHubStatus {
                status: 403,
                body: r#"{"message":"denied"}"#.to_string()
            }
        );
        Ok(())
    }

    fn spawn_one_request_server(
        status: &'static str,
        body: &'static str,
    ) -> anyhow::Result<(String, thread::JoinHandle<anyhow::Result<String>>)> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept()?;
            stream.set_read_timeout(Some(Duration::from_secs(5)))?;
            let request = read_request(&mut stream)?;
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes())?;
            Ok(request)
        });
        Ok((format!("http://{addr}/"), handle))
    }

    fn read_request(stream: &mut std::net::TcpStream) -> anyhow::Result<String> {
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 1024];
        loop {
            let read = stream.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..read]);
            if has_complete_request(&bytes) {
                break;
            }
        }
        Ok(String::from_utf8_lossy(&bytes).to_ascii_lowercase())
    }

    fn has_complete_request(bytes: &[u8]) -> bool {
        let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
            return false;
        };
        let headers = String::from_utf8_lossy(&bytes[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);
        bytes.len() >= header_end + 4 + content_length
    }

    fn join_request(handle: thread::JoinHandle<anyhow::Result<String>>) -> anyhow::Result<String> {
        handle
            .join()
            .map_err(|_| anyhow::anyhow!("mock GitHub server thread panicked"))?
    }
}
