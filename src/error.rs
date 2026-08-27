use axum::{
    http::StatusCode,
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
    #[error("upstream media service: {0}")]
    Upstream(String),
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("internal error: {0}")]
    Internal(String),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, code, message, log_error) = match &self {
            Self::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "登录状态无效或已过期".to_string(),
                false,
            ),
            Self::Forbidden => (
                StatusCode::FORBIDDEN,
                "forbidden",
                "当前账号没有执行此操作的权限".to_string(),
                false,
            ),
            Self::NotFound(message) => (StatusCode::NOT_FOUND, "not_found", message.clone(), false),
            Self::Validation(message) => (
                StatusCode::BAD_REQUEST,
                "validation_error",
                message.clone(),
                false,
            ),
            Self::Conflict(message) => (StatusCode::CONFLICT, "conflict", message.clone(), false),
            Self::Upstream(_) => (
                StatusCode::BAD_GATEWAY,
                "media_service_error",
                "媒体服务暂时不可用".to_string(),
                true,
            ),
            Self::Database(_) | Self::Internal(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "服务器内部错误".to_string(),
                true,
            ),
        };

        if log_error {
            tracing::error!(error = %self, "request failed");
        }

        (
            status,
            Json(json!({ "error": { "code": code, "message": message } })),
        )
            .into_response()
    }
}
