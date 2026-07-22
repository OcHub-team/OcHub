//! HTTP error envelope.
//!
//! `ochub-core` returns `AppError`; we map it to a JSON body the UI/clients can
//! parse, preserving the localized-error shape where present.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use ochub_core::AppError;
use serde_json::json;

/// Wraps an `AppError` for use as an axum handler error type.
pub struct ApiError(pub AppError);

impl From<AppError> for ApiError {
    fn from(err: AppError) -> Self {
        ApiError(err)
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(err: anyhow::Error) -> Self {
        ApiError(AppError::Message(err.to_string()))
    }
}

impl From<String> for ApiError {
    fn from(err: String) -> Self {
        ApiError(AppError::Message(err))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match &self.0 {
            AppError::InvalidInput(_) | AppError::McpValidation(_) => StatusCode::BAD_REQUEST,
            AppError::AppDisabled(_) => StatusCode::FORBIDDEN,
            AppError::Config(_) => StatusCode::BAD_REQUEST,
            AppError::HttpStatus { status, .. } => {
                StatusCode::from_u16(*status).unwrap_or(StatusCode::BAD_GATEWAY)
            }
            AppError::NoProvidersConfigured
            | AppError::OmoConfigNotFound
            | AppError::AllProvidersCircuitOpen => StatusCode::CONFLICT,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };

        // Preserve the localized key + bilingual messages so the UI can render
        // a friendly, translated error rather than a flat string.
        let body = match &self.0 {
            AppError::Localized { key, zh, en } => json!({
                "error": self.0.to_string(),
                "key": key,
                "zh": zh,
                "en": en,
            }),
            other => json!({ "error": other.to_string() }),
        };

        (status, Json(body)).into_response()
    }
}

pub type ApiResult<T> = Result<T, ApiError>;
