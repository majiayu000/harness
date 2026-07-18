use super::status;
use anyhow::{bail, Context as _, Result};
use harness_core::config::HarnessConfig;
use harness_server::reconciliation::ReconciliationReport;
use std::path::PathBuf;
use std::time::Duration;

const CONNECT_TIMEOUT_SECS: u64 = 10;

pub async fn run(dry_run: bool, project: Option<PathBuf>, config: &HarnessConfig) -> Result<()> {
    if let Some(project_root) = project {
        bail!(
            "`harness reconcile --project {}` is no longer supported; reconciliation now uses each task's stored project root",
            project_root.display()
        );
    }

    let base_url = status::server_base_url(config, None)?;
    let token = status::resolve_api_token(config);
    let client = reqwest::Client::builder()
        // Reconciliation deliberately sleeps between GitHub rate-limit windows.
        // Bound connection setup, but let the synchronous admin operation return
        // its complete report instead of timing out after partially applying work.
        .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
        .no_proxy()
        .build()
        .context("failed to build HTTP client")?;
    let report = request_report(&client, &base_url, token.as_deref(), dry_run).await?;

    print!("{}", render_report(&report, dry_run));
    Ok(())
}

async fn request_report(
    client: &reqwest::Client,
    base_url: &str,
    token: Option<&str>,
    dry_run: bool,
) -> Result<ReconciliationReport> {
    let path = format!("/reconcile?dry_run={dry_run}");
    let url = format!("{base_url}{path}");
    let mut request = client.post(&url);
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    let response = request
        .send()
        .await
        .with_context(|| format!("failed to connect to harness server at {url}"))?;
    let response_status = response.status();
    if response_status == reqwest::StatusCode::UNAUTHORIZED {
        bail!(
            "server rejected {path} with 401; set server.api_token in config or HARNESS_API_TOKEN"
        );
    }
    if !response_status.is_success() {
        let body = response
            .text()
            .await
            .with_context(|| format!("failed to read error response for {path}"))?;
        bail!("server returned {response_status} for {path}: {body}");
    }
    response
        .json::<ReconciliationReport>()
        .await
        .with_context(|| format!("server returned invalid JSON for {path}"))
}

fn render_report(report: &ReconciliationReport, dry_run: bool) -> String {
    let mut output = String::new();
    if dry_run {
        output.push_str(&format!(
            "Reconciliation dry-run: {} candidate(s), {} terminal skipped, {} task transition(s), {} workflow transition(s), {} workflow alert(s)\n",
            report.candidates,
            report.skipped_terminal,
            report.transitions.len(),
            report.workflow_transitions.len(),
            report.workflow_alerts.len()
        ));
    } else {
        let applied = report.transitions.iter().filter(|t| t.applied).count()
            + report
                .workflow_transitions
                .iter()
                .filter(|t| t.applied)
                .count();
        output.push_str(&format!(
            "Reconciliation: {} candidate(s), {} terminal skipped, {} transition(s) applied, {} workflow alert(s)\n",
            report.candidates,
            report.skipped_terminal,
            applied,
            report.workflow_alerts.len()
        ));
    }

    for transition in &report.transitions {
        let applied = if transition.applied {
            "applied"
        } else {
            "dry-run"
        };
        output.push_str(&format!(
            "  {} → {} ({}) [{}]\n",
            transition.from, transition.to, transition.reason, applied
        ));
    }

    for transition in &report.workflow_transitions {
        let applied = if transition.applied {
            "applied"
        } else {
            "dry-run"
        };
        let repo = transition.repo.as_deref().unwrap_or("<unknown>");
        let target = transition
            .pr_number
            .map(|number| format!("{repo}#{number}"))
            .or_else(|| {
                transition
                    .issue_number
                    .map(|number| format!("{repo} issue #{number}"))
            })
            .unwrap_or_else(|| repo.to_string());
        output.push_str(&format!(
            "  workflow {} {}: {} → {} ({}) [{}]\n",
            transition.workflow_id,
            target,
            transition.from,
            transition.to,
            transition.reason,
            applied
        ));
    }

    for alert in &report.workflow_alerts {
        let repo = alert.repo.as_deref().unwrap_or("<unknown>");
        let issue_number = alert
            .issue_number
            .map(|number| number.to_string())
            .unwrap_or_else(|| "<unknown>".to_string());
        let pr_number = alert
            .pr_number
            .map(|number| number.to_string())
            .unwrap_or_else(|| "<none>".to_string());
        let pr_url = alert.pr_url.as_deref().unwrap_or("<unknown>");
        output.push_str(&format!(
            "  workflow alert {} repo={} issue=#{} pr=#{} state={} age_secs={} ttl_secs={} reason={} url={}\n",
            alert.workflow_id,
            repo,
            issue_number,
            pr_number,
            alert.state,
            alert.age_secs,
            alert.ttl_secs,
            alert.reason,
            pr_url
        ));
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_server::reconciliation::{
        ReconciliationTransition, WorkflowReconciliationAlert, WorkflowReconciliationTransition,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn request_report_posts_dry_run_with_bearer_token() -> Result<()> {
        let body = r#"{"candidates":2,"skipped_terminal":1,"transitions":[],"workflow_transitions":[],"workflow_alerts":[]}"#;
        let (base_url, request) = spawn_response("200 OK", body).await?;
        let client = reqwest::Client::builder().no_proxy().build()?;

        let report = request_report(&client, &base_url, Some("test-token"), true).await?;
        let request = request.await?;

        assert_eq!(report.candidates, 2);
        assert!(request.starts_with("POST /reconcile?dry_run=true HTTP/1.1\r\n"));
        assert!(request
            .to_ascii_lowercase()
            .contains("authorization: bearer test-token\r\n"));
        Ok(())
    }

    #[tokio::test]
    async fn request_report_rejects_unauthorized_response() -> Result<()> {
        let (base_url, request) = spawn_response("401 Unauthorized", "denied").await?;
        let client = reqwest::Client::builder().no_proxy().build()?;

        let error = request_report(&client, &base_url, None, false)
            .await
            .expect_err("401 must fail");
        request.await?;

        assert!(error.to_string().contains("server rejected"));
        assert!(error.to_string().contains("HARNESS_API_TOKEN"));
        Ok(())
    }

    #[tokio::test]
    async fn request_report_rejects_non_success_and_invalid_json() -> Result<()> {
        let client = reqwest::Client::builder().no_proxy().build()?;
        let (base_url, request) = spawn_response("500 Internal Server Error", "broken").await?;
        let error = request_report(&client, &base_url, None, false)
            .await
            .expect_err("500 must fail");
        request.await?;
        assert!(error.to_string().contains("500 Internal Server Error"));
        assert!(error.to_string().contains("broken"));

        let (base_url, request) = spawn_response("200 OK", "not-json").await?;
        let error = request_report(&client, &base_url, None, false)
            .await
            .expect_err("invalid JSON must fail");
        request.await?;
        assert!(error.to_string().contains("invalid JSON"));
        Ok(())
    }

    #[tokio::test]
    async fn deprecated_project_is_rejected_before_connecting() {
        let error = run(
            false,
            Some(PathBuf::from("legacy-project")),
            &HarnessConfig::default(),
        )
        .await
        .expect_err("--project must remain rejected");

        assert!(error.to_string().contains("no longer supported"));
    }

    #[test]
    fn render_report_preserves_apply_and_dry_run_details() {
        let report = sample_report();

        let applied = render_report(&report, false);
        assert!(applied.contains("1 transition(s) applied"));
        assert!(applied.contains("pending → completed (merged) [applied]"));
        assert!(applied.contains("workflow workflow-1 owner/repo#42"));
        assert!(applied.contains("workflow alert workflow-2"));

        let dry_run = render_report(&report, true);
        assert!(dry_run.contains("1 task transition(s), 1 workflow transition(s)"));
    }

    fn sample_report() -> ReconciliationReport {
        ReconciliationReport {
            candidates: 2,
            skipped_terminal: 1,
            transitions: vec![ReconciliationTransition {
                task_id: "task-1".to_string(),
                from: "pending".to_string(),
                to: "completed".to_string(),
                reason: "merged".to_string(),
                applied: true,
            }],
            workflow_transitions: vec![WorkflowReconciliationTransition {
                workflow_id: "workflow-1".to_string(),
                from: "review".to_string(),
                to: "done".to_string(),
                reason: "merged".to_string(),
                applied: false,
                repo: Some("owner/repo".to_string()),
                issue_number: Some(41),
                pr_number: Some(42),
                pr_url: Some("https://example.test/pr/42".to_string()),
            }],
            workflow_alerts: vec![WorkflowReconciliationAlert {
                workflow_id: "workflow-2".to_string(),
                state: "review".to_string(),
                reason: "stale".to_string(),
                age_secs: 120,
                ttl_secs: 60,
                repo: Some("owner/repo".to_string()),
                issue_number: None,
                pr_number: Some(43),
                pr_url: None,
            }],
        }
    }

    async fn spawn_response(
        status: &'static str,
        body: &'static str,
    ) -> Result<(String, tokio::task::JoinHandle<String>)> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("mock request");
            let mut request = Vec::new();
            loop {
                let mut buffer = [0_u8; 1024];
                let read = stream.read(&mut buffer).await.expect("read mock request");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("write mock response");
            String::from_utf8(request).expect("HTTP request is UTF-8")
        });
        Ok((format!("http://{address}"), task))
    }
}
