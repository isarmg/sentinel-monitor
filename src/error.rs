use axum::{
    http::{header::RETRY_AFTER, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

pub type Result<T> = std::result::Result<T, AppError>;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("authentication required")]
    Unauthorized,
    #[error("permission denied")]
    Forbidden,
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Validation(String),
    #[error("{0}")]
    Conflict(String),
    #[error("rate limited; retry after {retry_after} seconds")]
    RateLimited { retry_after: u64 },
    #[error("upstream media service: {0}")]
    Upstream(String),
    #[error("upstream media service outcome is unknown: {0}")]
    UpstreamUnknown(String),
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("internal error: {0}")]
    Internal(String),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, code, message, log_error, retry_after) = match &self {
            Self::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "登录状态无效或已过期".to_string(),
                false,
                None,
            ),
            Self::Forbidden => (
                StatusCode::FORBIDDEN,
                "forbidden",
                "当前账号没有执行此操作的权限".to_string(),
                false,
                None,
            ),
            Self::NotFound(message) => (
                StatusCode::NOT_FOUND,
                "not_found",
                message.clone(),
                false,
                None,
            ),
            Self::Validation(message) => (
                StatusCode::BAD_REQUEST,
                "validation_error",
                message.clone(),
                false,
                None,
            ),
            Self::Conflict(message) => (
                StatusCode::CONFLICT,
                "conflict",
                message.clone(),
                false,
                None,
            ),
            Self::RateLimited { retry_after } => (
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limited",
                "登录尝试过多，请稍后重试".to_string(),
                false,
                Some(*retry_after),
            ),
            Self::Upstream(_) | Self::UpstreamUnknown(_) => (
                StatusCode::BAD_GATEWAY,
                "media_service_error",
                "媒体服务暂时不可用".to_string(),
                true,
                None,
            ),
            Self::Database(_) | Self::Internal(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "服务器内部错误".to_string(),
                true,
                None,
            ),
        };

        if log_error {
            tracing::error!(error = %self, "request failed");
        }

        let mut response = (
            status,
            Json(json!({ "error": { "code": code, "message": message } })),
        )
            .into_response();
        if let Some(retry_after) = retry_after {
            if let Ok(value) = HeaderValue::from_str(&retry_after.to_string()) {
                response.headers_mut().insert(RETRY_AFTER, value);
            }
        }
        response
    }
}
