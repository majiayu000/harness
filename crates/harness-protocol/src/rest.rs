//! HTTP REST DTO boundary for the Harness operator/control-plane API.
//!
//! New REST request and response types belong in `harness-protocol` so CLI,
//! server, dashboard, and automation callers share one wire contract. Existing
//! server-local DTOs are legacy migration targets and must not be added to the
//! server's closed legacy registry.

use serde::{de::DeserializeOwned, Serialize};

/// Runtime-host capability required before the server issues proof-bearing leases.
pub const RUNTIME_JOB_LEASE_PROOF_V1_CAPABILITY: &str = "runtime_job_lease_proof_v1";

/// Signed signal payload accepted by `POST /signals`.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct IngestSignalRequest {
    pub source: String,
    #[serde(default)]
    pub severity: Option<harness_core::types::Severity>,
    pub payload: serde_json::Value,
}

/// Host-observed execution facts that are outside the agent-authored result.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct RuntimeHostExecutionEvidence {
    pub checked_out_commit: String,
    pub resource_limit_report: serde_json::Value,
    pub usage: RuntimeHostUsageEvidence,
    pub isolation_cleanup_status: String,
    #[serde(default)]
    pub validation: Vec<RuntimeHostValidationEvidence>,
}

#[derive(Debug, Clone, serde::Deserialize, Serialize)]
pub struct RuntimeHostValidationEvidence {
    pub argv: Vec<String>,
    pub exit_code: i32,
    pub output_sha256: String,
    pub duration_ms: u64,
}

/// Host-observed token usage for a remotely executed activity.
#[derive(Debug, Clone, serde::Deserialize, Serialize)]
pub struct RuntimeHostUsageEvidence {
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub cached_input_tokens: u64,
    pub total_tokens: u64,
    #[serde(default)]
    pub cost_usd_micros: Option<u64>,
}

/// Completion request for a remotely leased workflow runtime job.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct CompleteRuntimeJobRequest {
    pub lease_expires_at: chrono::DateTime<chrono::Utc>,
    #[serde(default)]
    pub lease_generation: Option<u64>,
    #[serde(default)]
    pub lease_proof: Option<uuid::Uuid>,
    pub result: serde_json::Value,
    #[serde(default)]
    pub execution_evidence: Option<RuntimeHostExecutionEvidence>,
}

#[derive(Debug, Clone, Default)]
pub struct OptionalLeaseSeconds(Option<u64>);

impl OptionalLeaseSeconds {
    pub fn value(&self) -> Option<u64> {
        self.0
    }
}

impl<'de> serde::Deserialize<'de> for OptionalLeaseSeconds {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        u64::deserialize(deserializer).map(|value| Self(Some(value)))
    }
}

/// Lease renewal request for a remotely leased workflow runtime job.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct RenewRuntimeJobLeaseRequest {
    pub lease_generation: u64,
    pub lease_expires_at: chrono::DateTime<chrono::Utc>,
    #[serde(default)]
    pub lease_proof: Option<uuid::Uuid>,
    pub renewal_id: uuid::Uuid,
    #[serde(default)]
    pub lease_secs: OptionalLeaseSeconds,
}

/// Claim request for the next compatible remotely leased workflow runtime job.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ClaimRuntimeJobRequest {
    #[serde(default)]
    pub lease_secs: OptionalLeaseSeconds,
    #[serde(default)]
    pub project: Option<String>,
}

/// JSON response envelope for runtime-host job completion outcomes.
#[derive(Debug, Clone, Serialize)]
#[serde(transparent)]
pub struct RuntimeHostCompletionResponse(pub serde_json::Value);

impl std::ops::Deref for RuntimeHostCompletionResponse {
    type Target = serde_json::Value;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// JSON response envelope for runtime-host lease renewal outcomes.
#[derive(Debug, Clone, Serialize)]
#[serde(transparent)]
pub struct RuntimeHostLeaseResponse(pub serde_json::Value);

/// JSON response envelope for runtime-host job claim outcomes.
#[derive(Debug, Clone, Serialize)]
#[serde(transparent)]
pub struct RuntimeHostClaimResponse(pub serde_json::Value);

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

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
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

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
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

impl private::Sealed for CompleteRuntimeJobRequest {}
impl RestDto for CompleteRuntimeJobRequest {}

impl private::Sealed for RenewRuntimeJobLeaseRequest {}
impl RestDto for RenewRuntimeJobLeaseRequest {}

impl private::Sealed for ClaimRuntimeJobRequest {}
impl RestDto for ClaimRuntimeJobRequest {}

impl private::Sealed for RuntimeHostCompletionResponse {}
impl RestDto for RuntimeHostCompletionResponse {}

impl private::Sealed for RuntimeHostLeaseResponse {}
impl RestDto for RuntimeHostLeaseResponse {}

impl private::Sealed for RuntimeHostClaimResponse {}
impl RestDto for RuntimeHostClaimResponse {}

impl private::Sealed for WorkflowEvidenceQuery {}
impl RestDto for WorkflowEvidenceQuery {}

impl private::Sealed for WorkflowEvidenceArtifact {}
impl RestDto for WorkflowEvidenceArtifact {}

impl private::Sealed for WorkflowEvidenceExportResponse {}
impl RestDto for WorkflowEvidenceExportResponse {}
