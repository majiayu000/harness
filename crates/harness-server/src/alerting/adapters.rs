//! Channel adapters (B-013/B-014): format the alert payload for each
//! channel kind and deliver it through the mockable transport.

use async_trait::async_trait;
use harness_core::alert::AlertPayload;
use harness_core::config::alerting::{AlertChannelConfig, AlertChannelKind};

use crate::feishu_client;

/// Mockable HTTP transport. The real implementation posts JSON and returns
/// the parsed response body; non-2xx statuses are errors.
#[async_trait]
pub trait AlertTransport: Send + Sync {
    async fn post_json(
        &self,
        url: &str,
        bearer: Option<&str>,
        body: &serde_json::Value,
    ) -> anyhow::Result<serde_json::Value>;
}

pub struct HttpTransport {
    http: reqwest::Client,
}

impl HttpTransport {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
        }
    }
}

impl Default for HttpTransport {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl AlertTransport for HttpTransport {
    async fn post_json(
        &self,
        url: &str,
        bearer: Option<&str>,
        body: &serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        let mut request = self.http.post(url).json(body);
        if let Some(token) = bearer {
            request = request.bearer_auth(token);
        }
        let response = request.send().await?;
        let status = response.status();
        if !status.is_success() {
            // Never echo the URL into the error: it may embed a secret
            // token (B-015); the channel name is logged by the caller.
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("alert delivery failed: {status}: {body}");
        }
        Ok(response.json().await.unwrap_or(serde_json::Value::Null))
    }
}

/// Human-readable one-liner shared by the Slack and Feishu formatters.
pub(crate) fn format_text(payload: &AlertPayload) -> String {
    let mut refs = Vec::new();
    if let Some(task) = &payload.subject.task_id {
        refs.push(format!("task={task}"));
    }
    if let Some(workflow) = &payload.subject.workflow_id {
        refs.push(format!("workflow={workflow}"));
    }
    if let Some(issue) = &payload.subject.issue {
        refs.push(format!("issue={issue}"));
    }
    if let Some(pr) = &payload.subject.pr_url {
        refs.push(format!("pr={pr}"));
    }
    if let Some(repo) = &payload.subject.repo {
        refs.push(format!("repo={repo}"));
    }
    if let Some(entity) = &payload.subject.entity {
        refs.push(format!("entity={entity}"));
    }
    let subject = if refs.is_empty() {
        String::new()
    } else {
        format!(" [{}]", refs.join(" "))
    };
    let project = payload
        .project
        .as_deref()
        .map(|p| format!(" ({p})"))
        .unwrap_or_default();
    format!(
        "[harness:{:?}] {}{project}: {}{subject}",
        payload.severity,
        payload.event_class.as_str(),
        payload.message,
    )
}

/// One delivery attempt to `channel`. Formatting failures and transport
/// failures both surface as errors (failed delivery, B-013).
pub(crate) async fn deliver_once(
    channel: &AlertChannelConfig,
    feishu_credentials: Option<&(String, String)>,
    transport: &dyn AlertTransport,
    payload: &AlertPayload,
) -> anyhow::Result<()> {
    match channel.kind {
        AlertChannelKind::Webhook => {
            let url = channel
                .effective_url()
                .ok_or_else(|| anyhow::anyhow!("channel {}: url missing", channel.name))?;
            let body = serde_json::to_value(payload)?;
            transport.post_json(&url, None, &body).await?;
            Ok(())
        }
        AlertChannelKind::Slack => {
            let url = channel
                .effective_url()
                .ok_or_else(|| anyhow::anyhow!("channel {}: url missing", channel.name))?;
            let body = serde_json::json!({ "text": format_text(payload) });
            transport.post_json(&url, None, &body).await?;
            Ok(())
        }
        AlertChannelKind::Feishu => {
            let (app_id, app_secret) = feishu_credentials.ok_or_else(|| {
                anyhow::anyhow!(
                    "channel {}: feishu credentials not configured",
                    channel.name
                )
            })?;
            let receive_id = channel
                .receive_id
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("channel {}: receive_id missing", channel.name))?;
            let token_response = transport
                .post_json(
                    feishu_client::TENANT_TOKEN_URL,
                    None,
                    &feishu_client::tenant_token_request(app_id, app_secret),
                )
                .await?;
            let token = feishu_client::parse_tenant_token(&token_response)?;
            let body = feishu_client::send_text_request(receive_id, &format_text(payload))?;
            transport
                .post_json(feishu_client::SEND_MESSAGE_URL, Some(&token), &body)
                .await?;
            Ok(())
        }
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Mutex;

    /// Mock transport: records calls, optionally fails the first N posts.
    pub struct MockTransport {
        pub calls: Mutex<Vec<(String, Option<String>, serde_json::Value)>>,
        pub fail_first: AtomicU32,
        pub response: serde_json::Value,
    }

    impl MockTransport {
        pub fn ok() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                fail_first: AtomicU32::new(0),
                response: serde_json::Value::Null,
            }
        }

        pub fn failing_first(n: u32) -> Self {
            Self {
                fail_first: AtomicU32::new(n),
                ..Self::ok()
            }
        }

        pub fn always_failing() -> Self {
            Self::failing_first(u32::MAX)
        }

        pub fn with_response(response: serde_json::Value) -> Self {
            Self {
                response,
                ..Self::ok()
            }
        }

        pub fn call_count(&self) -> usize {
            self.calls.lock().unwrap().len()
        }
    }

    #[async_trait]
    impl AlertTransport for MockTransport {
        async fn post_json(
            &self,
            url: &str,
            bearer: Option<&str>,
            body: &serde_json::Value,
        ) -> anyhow::Result<serde_json::Value> {
            self.calls.lock().unwrap().push((
                url.to_string(),
                bearer.map(str::to_string),
                body.clone(),
            ));
            let remaining = self.fail_first.load(Ordering::SeqCst);
            if remaining > 0 {
                self.fail_first.store(remaining - 1, Ordering::SeqCst);
                anyhow::bail!("mock transport failure ({remaining} remaining)");
            }
            Ok(self.response.clone())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::MockTransport;
    use super::*;
    use harness_core::alert::{AlertClass, AlertSeverity, AlertSubject};

    fn channel(kind: AlertChannelKind) -> AlertChannelConfig {
        AlertChannelConfig {
            name: "test".into(),
            kind,
            url: Some("https://example.invalid/hook".into()),
            receive_id: Some("oc_chat".into()),
            max_attempts: 3,
            backoff_base_ms: 1,
        }
    }

    fn payload() -> AlertPayload {
        AlertPayload::new(
            AlertClass::WorkflowBlocked,
            AlertSeverity::Error,
            "workflow wf-1 blocked",
            "workflow_blocked:wf-1",
        )
        .with_subject(AlertSubject {
            workflow_id: Some("wf-1".into()),
            ..AlertSubject::default()
        })
    }

    #[tokio::test]
    async fn webhook_posts_raw_payload_contract() {
        let transport = MockTransport::ok();
        deliver_once(
            &channel(AlertChannelKind::Webhook),
            None,
            &transport,
            &payload(),
        )
        .await
        .unwrap();
        let calls = transport.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "https://example.invalid/hook");
        assert_eq!(calls[0].2["event_class"], "workflow_blocked");
        assert_eq!(calls[0].2["schema_version"], 1);
    }

    #[tokio::test]
    async fn slack_posts_formatted_text() {
        let transport = MockTransport::ok();
        deliver_once(
            &channel(AlertChannelKind::Slack),
            None,
            &transport,
            &payload(),
        )
        .await
        .unwrap();
        let calls = transport.calls.lock().unwrap();
        let text = calls[0].2["text"].as_str().unwrap();
        assert!(text.contains("workflow_blocked"));
        assert!(text.contains("workflow=wf-1"));
    }

    #[tokio::test]
    async fn feishu_fetches_token_then_sends() {
        let transport =
            MockTransport::with_response(serde_json::json!({ "tenant_access_token": "tok-9" }));
        let credentials = ("app-1".to_string(), "secret-1".to_string());
        deliver_once(
            &channel(AlertChannelKind::Feishu),
            Some(&credentials),
            &transport,
            &payload(),
        )
        .await
        .unwrap();
        let calls = transport.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, feishu_client::TENANT_TOKEN_URL);
        assert_eq!(calls[1].0, feishu_client::SEND_MESSAGE_URL);
        assert_eq!(calls[1].1.as_deref(), Some("tok-9"));
    }

    #[tokio::test]
    async fn feishu_without_credentials_is_failed_delivery() {
        let transport = MockTransport::ok();
        let result = deliver_once(
            &channel(AlertChannelKind::Feishu),
            None,
            &transport,
            &payload(),
        )
        .await;
        assert!(result.is_err());
        assert_eq!(transport.call_count(), 0);
    }
}
