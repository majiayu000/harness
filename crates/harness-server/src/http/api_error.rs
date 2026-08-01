use crate::services::execution::EnqueueTaskError;
use axum::{
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};

#[derive(Debug)]
pub(crate) enum ApiError {
    BadRequest(String),
    Internal(String),
    StoreUnavailable(&'static str),
    MaintenanceWindow { retry_after_secs: u64 },
}

impl ApiError {
    pub(crate) fn internal(message: impl Into<String>) -> Self {
        Self::Internal(message.into())
    }

    pub(crate) fn store_unavailable(store_name: &'static str) -> Self {
        Self::StoreUnavailable(store_name)
    }

    pub(crate) fn status(&self) -> StatusCode {
        match self {
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Self::StoreUnavailable(_) | Self::MaintenanceWindow { .. } => {
                StatusCode::SERVICE_UNAVAILABLE
            }
        }
    }

    pub(crate) fn body(&self) -> Value {
        match self {
            Self::BadRequest(message) | Self::Internal(message) => json!({ "error": message }),
            Self::StoreUnavailable(store_name) => {
                json!({ "error": format!("{store_name} unavailable") })
            }
            Self::MaintenanceWindow { retry_after_secs } => {
                json!({ "error": "maintenance_window", "retry_after": retry_after_secs })
            }
        }
    }

    pub(crate) fn into_status_json(self) -> (StatusCode, Json<Value>) {
        (self.status(), Json(self.body()))
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadRequest(message) => write!(f, "bad request: {message}"),
            Self::Internal(message) => write!(f, "internal error: {message}"),
            Self::StoreUnavailable(store_name) => write!(f, "{store_name} unavailable"),
            Self::MaintenanceWindow { retry_after_secs } => {
                write!(
                    f,
                    "maintenance window active; retry after {retry_after_secs}s"
                )
            }
        }
    }
}

impl std::error::Error for ApiError {}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            Self::MaintenanceWindow { retry_after_secs } => (
                StatusCode::SERVICE_UNAVAILABLE,
                [(header::RETRY_AFTER, retry_after_secs.to_string())],
                Json(json!({
                    "error": "maintenance_window",
                    "retry_after": retry_after_secs,
                })),
            )
                .into_response(),
            error => error.into_status_json().into_response(),
        }
    }
}

impl From<EnqueueTaskError> for ApiError {
    fn from(error: EnqueueTaskError) -> Self {
        match error {
            EnqueueTaskError::BadRequest(message) => Self::BadRequest(message),
            EnqueueTaskError::Internal(message) => Self::Internal(message),
            EnqueueTaskError::StoreUnavailable(store_name) => Self::StoreUnavailable(store_name),
            EnqueueTaskError::MaintenanceWindow { retry_after_secs } => {
                Self::MaintenanceWindow { retry_after_secs }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_unavailable_uses_service_unavailable_contract() {
        let error = ApiError::store_unavailable("workflow runtime store");

        assert_eq!(error.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            error.body(),
            json!({ "error": "workflow runtime store unavailable" })
        );
    }

    #[test]
    fn enqueue_task_error_maps_to_api_error_once() {
        assert_eq!(
            ApiError::from(EnqueueTaskError::BadRequest("bad input".to_string())).status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            ApiError::from(EnqueueTaskError::Internal("failed".to_string())).status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(
            ApiError::from(EnqueueTaskError::StoreUnavailable("workflow runtime store")).status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            ApiError::from(EnqueueTaskError::MaintenanceWindow {
                retry_after_secs: 42
            })
            .status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }
}
