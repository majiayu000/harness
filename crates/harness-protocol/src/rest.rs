//! HTTP REST DTO boundary for the Harness operator/control-plane API.
//!
//! New REST request and response types belong in `harness-protocol` so CLI,
//! server, dashboard, and automation callers share one wire contract. Existing
//! server-local DTOs are legacy migration targets and must not be added to the
//! server's closed legacy registry.

use serde::{de::DeserializeOwned, Serialize};

/// Signed signal payload accepted by `POST /signals`.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct IngestSignalRequest {
    pub source: String,
    #[serde(default)]
    pub severity: Option<harness_core::types::Severity>,
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct WorkflowEvidenceQuery {
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub commit_sha: Option<String>,
    #[serde(default)]
    pub suite: Option<String>,
    #[serde(default)]
    pub decision: Option<String>,
    #[serde(default)]
    pub created_after: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    pub created_before: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    pub include_payload: Option<bool>,
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkflowEvidenceArtifact {
    pub id: String,
    pub workflow_id: String,
    pub command_id: Option<String>,
    pub runtime_job_id: Option<String>,
    pub project_id: String,
    pub commit_sha: Option<String>,
    pub stack: String,
    pub suite: String,
    pub baseline: Option<String>,
    pub decision: String,
    pub schema: String,
    pub digest: String,
    pub trust: String,
    pub location: serde_json::Value,
    pub retention_class: String,
    pub payload: Option<serde_json::Value>,
    pub payload_expires_at: Option<chrono::DateTime<chrono::Utc>>,
    pub payload_expired_at: Option<chrono::DateTime<chrono::Utc>>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkflowEvidenceExportResponse {
    pub schema: String,
    pub generated_at: chrono::DateTime<chrono::Utc>,
    pub limit: i64,
    pub count: usize,
    pub records: Vec<WorkflowEvidenceArtifact>,
}

mod private {
    pub trait Sealed {}
}

/// Marks a wire type as owned by the shared REST protocol.
///
/// The trait is sealed so downstream crates cannot make a server-local type
/// look protocol-owned:
///
/// ```compile_fail
/// use harness_protocol::rest::RestDto;
///
/// struct ServerLocalDto;
///
/// impl RestDto for ServerLocalDto {}
/// ```
pub trait RestDto: private::Sealed + Send + Sync + 'static {}

/// A protocol-owned DTO that can be extracted from an HTTP request.
pub trait RestRequest: RestDto + DeserializeOwned {}

impl<T> RestRequest for T where T: RestDto + DeserializeOwned {}

/// A protocol-owned DTO that can be serialized into an HTTP response.
pub trait RestResponse: RestDto + Serialize {}

impl<T> RestResponse for T where T: RestDto + Serialize {}

impl private::Sealed for crate::methods::RpcRequest {}
impl RestDto for crate::methods::RpcRequest {}

impl private::Sealed for crate::methods::RpcResponse {}
impl RestDto for crate::methods::RpcResponse {}

impl private::Sealed for IngestSignalRequest {}
impl RestDto for IngestSignalRequest {}

impl private::Sealed for WorkflowEvidenceQuery {}
impl RestDto for WorkflowEvidenceQuery {}

impl private::Sealed for WorkflowEvidenceArtifact {}
impl RestDto for WorkflowEvidenceArtifact {}

impl private::Sealed for WorkflowEvidenceExportResponse {}
impl RestDto for WorkflowEvidenceExportResponse {}
