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
