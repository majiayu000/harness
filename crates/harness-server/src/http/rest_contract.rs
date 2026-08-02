//! Compiler-enforced adapters for the REST DTO ownership boundary.
//!
//! New request and response DTOs must be declared in `harness-protocol::rest`
//! and use the `Contract*` adapters. Existing server-local DTOs can only pass
//! through the closed `Legacy*` registry in this module.

#![allow(clippy::disallowed_types)]

use axum::{
    extract::{FromRequest, FromRequestParts, Path as AxumPath, Query as AxumQuery, Request},
    response::{IntoResponse, Response},
    Json as AxumJson,
};
use harness_protocol::rest::{RestDto, RestRequest, RestResponse};
use serde::{de::DeserializeOwned, Serialize};

/// JSON extractor/response for protocol-owned DTOs.
///
/// A server-local DTO cannot be used here because `RestDto` is sealed:
///
/// ```compile_fail
/// use harness_server::http::rest_contract::ContractJson;
///
/// struct ServerLocalDto;
///
/// let _: Option<ContractJson<ServerLocalDto>> = None;
/// ```
#[derive(Debug, Clone, Copy, Default)]
pub struct ContractJson<T: RestDto>(pub T);

impl<S, T> FromRequest<S> for ContractJson<T>
where
    S: Send + Sync,
    T: RestRequest,
{
    type Rejection = axum::extract::rejection::JsonRejection;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        AxumJson::<T>::from_request(request, state)
            .await
            .map(|value| Self(value.0))
    }
}

impl<T> IntoResponse for ContractJson<T>
where
    T: RestResponse,
{
    fn into_response(self) -> Response {
        AxumJson(self.0).into_response()
    }
}

/// Query extractor for protocol-owned DTOs.
#[derive(Debug, Clone, Copy, Default)]
pub struct ContractQuery<T: RestDto>(pub T);

impl<S, T> FromRequestParts<S> for ContractQuery<T>
where
    S: Send + Sync,
    T: RestRequest,
{
    type Rejection = axum::extract::rejection::QueryRejection;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        AxumQuery::<T>::from_request_parts(parts, state)
            .await
            .map(|value| Self(value.0))
    }
}

/// Path extractor for protocol-owned named DTOs.
#[derive(Debug, Clone, Copy, Default)]
pub struct ContractPath<T: RestDto>(pub T);

impl<S, T> FromRequestParts<S> for ContractPath<T>
where
    S: Send + Sync,
    T: RestRequest,
{
    type Rejection = axum::extract::rejection::PathRejection;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        AxumPath::<T>::from_request_parts(parts, state)
            .await
            .map(|value| Self(value.0))
    }
}

mod legacy_private {
    pub trait Sealed {}
}

pub(crate) trait LegacyRestDto: legacy_private::Sealed {}

/// JSON adapter for the exact server-local DTOs that predate the protocol
/// boundary. Its sealed registry must only shrink as DTOs are migrated.
#[derive(Debug, Clone, Copy, Default)]
pub struct LegacyJson<T>(pub T);

impl<S, T> FromRequest<S> for LegacyJson<T>
where
    S: Send + Sync,
    T: LegacyRestDto + DeserializeOwned,
{
    type Rejection = axum::extract::rejection::JsonRejection;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        AxumJson::<T>::from_request(request, state)
            .await
            .map(|value| Self(value.0))
    }
}

impl<T> IntoResponse for LegacyJson<T>
where
    T: LegacyRestDto + Serialize,
{
    fn into_response(self) -> Response {
        AxumJson(self.0).into_response()
    }
}

/// Query adapter for the exact server-local DTOs that predate the protocol
/// boundary.
#[derive(Debug, Clone, Copy, Default)]
pub struct LegacyQuery<T>(pub T);

impl<S, T> FromRequestParts<S> for LegacyQuery<T>
where
    S: Send + Sync,
    T: LegacyRestDto + DeserializeOwned,
{
    type Rejection = axum::extract::rejection::QueryRejection;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        AxumQuery::<T>::from_request_parts(parts, state)
            .await
            .map(|value| Self(value.0))
    }
}

mod primitive_path_private {
    pub trait Sealed {}
}

pub(crate) trait PrimitivePathValue:
    primitive_path_private::Sealed + DeserializeOwned + Send + 'static
{
}

/// Path adapter restricted to the primitive path shapes used by legacy
/// routes. New named path DTOs must use `ContractPath`.
#[derive(Debug, Clone, Copy, Default)]
pub struct PrimitivePath<T>(pub T);

impl<S, T> FromRequestParts<S> for PrimitivePath<T>
where
    S: Send + Sync,
    T: PrimitivePathValue,
{
    type Rejection = axum::extract::rejection::PathRejection;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        AxumPath::<T>::from_request_parts(parts, state)
            .await
            .map(|value| Self(value.0))
    }
}

macro_rules! register_legacy_dtos {
    ($($dto:ty),+ $(,)?) => {
        $(
            impl legacy_private::Sealed for $dto {}
            impl LegacyRestDto for $dto {}
        )+
    };
}

register_legacy_dtos!(
    serde_json::Value,
    crate::handlers::projects::RegisterProjectRequest,
    crate::handlers::reconcile::ReconcileParams,
    crate::handlers::runtime_hosts::CompleteRuntimeJobRequest,
    crate::handlers::runtime_hosts::RegisterRuntimeHostRequest,
    crate::handlers::runtime_hosts::lease::ClaimRuntimeJobRequest,
    crate::handlers::runtime_hosts::lease::RenewRuntimeJobLeaseRequest,
    crate::handlers::runtime_project_cache::SyncWatchedProjectsRequest,
    crate::handlers::usage_monitor::UsageMonitorQuery,
    crate::handlers::usage_monitor::UsageMonitorResponse,
    crate::http::auth_routes::PasswordResetRequest,
    crate::http::workflow_routes::runtime_tree::WorkflowRuntimeTreeQuery,
    crate::http::workflow_routes::runtime_tree::WorkflowRuntimeTreeResponse,
    crate::http::runtime_submission_routes::ApprovalResponse,
    Vec<crate::http::runtime_submission_routes::RuntimeSubmissionArtifact>,
    Vec<crate::http::runtime_submission_routes::RuntimeSubmissionPrompt>,
    crate::http::task_mutation_routes::RuntimeTranscriptReconstructionRequest,
    crate::http::task_mutation_routes::WorkflowRuntimeCancelRequest,
    crate::http::task_mutation_routes::WorkflowRuntimeMergeRequest,
    crate::http::task_mutation_routes::WorkflowRuntimeRecoveryRouteRequest,
    crate::http::task_query_routes::RuntimeSubmissionListParams,
    crate::http::task_query_routes::RuntimeSubmissionListResponse,
    crate::http::task_query_routes::detail::RuntimeTaskResponse,
    crate::http::workflow_routes::IssueWorkflowByIssueQuery,
    crate::http::workflow_routes::IssueWorkflowByPrQuery,
    crate::http::workflow_routes::ProjectWorkflowByProjectQuery,
    crate::reconciliation::ReconciliationReport,
    crate::workflow_runtime_submission::CreateTaskRequest,
    harness_core::agent::ApprovalDecision,
    harness_core::proof_of_work::ProofOfWork,
);

impl primitive_path_private::Sealed for String {}
impl PrimitivePathValue for String {}

impl primitive_path_private::Sealed for (String, String) {}
impl PrimitivePathValue for (String, String) {}

impl primitive_path_private::Sealed for (String, String, u64) {}
impl PrimitivePathValue for (String, String, u64) {}

#[cfg(test)]
mod tests {
    use super::ContractJson;
    use axum::{
        body::Body,
        http::{Request, StatusCode},
        routing::post,
        Router,
    };
    use harness_protocol::methods::{Method, RpcRequest, RpcResponse};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    async fn protocol_echo(
        ContractJson(request): ContractJson<RpcRequest>,
    ) -> ContractJson<RpcResponse> {
        ContractJson(RpcResponse::success(
            request.id,
            serde_json::json!({"ok": true}),
        ))
    }

    #[tokio::test]
    async fn protocol_json_preserves_the_wire_shape() -> anyhow::Result<()> {
        let app = Router::new().route("/rpc", post(protocol_echo));
        let request = RpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(7)),
            method: Method::GcStatus,
        };
        let response = app
            .oneshot(
                Request::post("/rpc")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&request)?))?,
            )
            .await?;

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await?.to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&body)?;
        assert_eq!(
            body,
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 7,
                "result": {"ok": true}
            })
        );
        Ok(())
    }
}
