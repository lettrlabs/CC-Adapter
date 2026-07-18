use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;

use crate::types::anthropic::{ErrorDetail, ErrorResponse};

/// 統一的應用程式錯誤型別，回應格式符合 Anthropic API 的錯誤結構
/// Unified application error type; response format matches Anthropic API error structure
pub struct AppError {
    pub status: StatusCode,
    pub error_type: String,
    pub message: String,
}

impl AppError {
    /// 400 錯誤請求
    /// 400 Bad Request
    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            error_type: "invalid_request_error".to_string(),
            message: msg.into(),
        }
    }

    /// 500 內部伺服器錯誤
    /// 500 Internal Server Error
    pub fn internal(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            error_type: "api_error".to_string(),
            message: msg.into(),
        }
    }

    /// 功能尚未實作（以 400 回應）
    /// Feature not yet implemented (responds with 400)
    #[allow(dead_code)]
    pub fn not_implemented(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            error_type: "invalid_request_error".to_string(),
            message: msg.into(),
        }
    }

    /// 依上游錯誤訊息選擇狀態碼：輸入類錯誤（如超出上下文長度）以 400 回應，
    /// 讓 Claude Code 不會反覆重試一個必然失敗的請求；其餘視為 500。
    /// Choose a status from an upstream error message: input-side failures (e.g.
    /// context length exceeded) map to 400 so Claude Code does not retry a request
    /// that will always fail; everything else stays 500.
    pub fn from_upstream(msg: impl Into<String>) -> Self {
        let message = msg.into();
        if is_non_retryable_upstream(&message) {
            Self {
                status: StatusCode::BAD_REQUEST,
                error_type: "invalid_request_error".to_string(),
                message,
            }
        } else {
            Self {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                error_type: "api_error".to_string(),
                message,
            }
        }
    }
}

/// 判斷上游錯誤是否為「不可重試」的輸入類錯誤（避免 Claude Code 對必敗請求重試 10 次）
/// Whether an upstream error is a non-retryable input-side failure (so Claude Code
/// won't retry a doomed request up to 10 times)
pub fn is_non_retryable_upstream(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    m.contains("context window")
        || m.contains("context_length")
        || m.contains("context length")
        || m.contains("exceeds")
        || m.contains("too large")
        || m.contains("invalid_request_error")
        || m.contains("maximum context")
}

/// 轉換為 Anthropic 格式的 JSON 錯誤回應
/// Convert into an Anthropic-formatted JSON error response
impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let body = ErrorResponse {
            error_type: "error".to_string(),
            error: ErrorDetail {
                error_type: self.error_type,
                message: self.message,
            },
        };
        (self.status, Json(body)).into_response()
    }
}

impl From<anyhow::Error> for AppError {
    fn from(err: anyhow::Error) -> Self {
        AppError::internal(format!("{:#}", err))
    }
}
