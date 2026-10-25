use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use hyakume_core::api::{ApiError, ErrorCode};
use hyakume_storage::StoreError;

/// A failed request. Internal details go to the log, never to the client.
#[derive(Debug)]
pub struct Failure(pub ApiError);

impl Failure {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self(ApiError {
            code,
            message: message.into(),
            latest: None,
        })
    }

    pub fn unauthorized() -> Self {
        Self::new(ErrorCode::Unauthorized, "sign in required")
    }

    /// Used for denied reads too, so a non-member cannot probe which ids exist.
    pub fn not_found(what: &str) -> Self {
        Self::new(ErrorCode::NotFound, format!("{what} not found"))
    }

    pub fn forbidden(action: &str) -> Self {
        Self::new(
            ErrorCode::Forbidden,
            format!("your roles do not allow {action}"),
        )
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Invalid, message)
    }
}

impl From<StoreError> for Failure {
    fn from(err: StoreError) -> Self {
        match err {
            StoreError::NotFound(what) => Self::not_found(what),
            StoreError::Conflict { expected, latest } => Self(ApiError {
                code: ErrorCode::Conflict,
                message: format!(
                    "the task changed since version {expected}; review version {} and retry",
                    latest.version
                ),
                latest: Some(latest),
            }),
            StoreError::Duplicate(message) => Self::new(ErrorCode::Duplicate, message),
            StoreError::Invalid(err) => Self::invalid(err.to_string()),
            err @ (StoreError::Database(_) | StoreError::Migration(_) | StoreError::Corrupt(_)) => {
                tracing::error!(error = %err, "storage failure");
                Self::new(ErrorCode::Internal, "internal server error")
            }
        }
    }
}

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        let status = match self.0.code {
            ErrorCode::Invalid => StatusCode::BAD_REQUEST,
            ErrorCode::Unauthorized => StatusCode::UNAUTHORIZED,
            ErrorCode::Forbidden => StatusCode::FORBIDDEN,
            ErrorCode::NotFound => StatusCode::NOT_FOUND,
            ErrorCode::Conflict | ErrorCode::Duplicate => StatusCode::CONFLICT,
            ErrorCode::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, Json(self.0)).into_response()
    }
}
