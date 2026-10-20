//! Twitch API 的錯誤型別：對應舊版 `twitch/errors.ts` 逐類別移植。
//!
//! - [`TwitchApiAuthError`]／[`TwitchApiRateLimitError`]／[`TwitchApiTemporaryError`]
//!   對應舊版三個類別；`name()`／`message()` 對應舊版 `Error.name`／
//!   `Error.message`，其餘屬性以同名方法提供（`statusCode`、`retryAt`、
//!   `retryAfterMs`、`reason`）。
//! - [`TwitchApiError`]：舊版抽象基底類 `TwitchApiError` 的 Rust 對應（enum；
//!   `kind()` 對應 `kind` 屬性）。
//! - [`TwitchApiClientInitError`]：舊版 `TwitchApiClient` 建構子直接拋出的
//!   `TypeError`／`RangeError`（選項驗證與 URL 解析失敗）。
//! - 舊版 `retryAt` 存 `Date`；Rust 版存 epoch 毫秒（[`i64`]），ISO 字串由
//!   [`crate::twitch::api`] 的輔助函式換算。
#![allow(dead_code)]

use core::fmt;

/// 舊版 `TwitchApiErrorKind`：`'auth' | 'rate-limit' | 'temporary'`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TwitchApiErrorKind {
    Auth,
    RateLimit,
    Temporary,
}

impl TwitchApiErrorKind {
    /// 舊版字面值（`'auth'` 等）。
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            TwitchApiErrorKind::Auth => "auth",
            TwitchApiErrorKind::RateLimit => "rate-limit",
            TwitchApiErrorKind::Temporary => "temporary",
        }
    }
}

/// 舊版 `TwitchApiTemporaryErrorReason`：
/// `'aborted' | 'http' | 'invalid-response' | 'network' | 'server' | 'timeout'`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TwitchApiTemporaryErrorReason {
    Aborted,
    Http,
    InvalidResponse,
    Network,
    Server,
    Timeout,
}

impl TwitchApiTemporaryErrorReason {
    /// 舊版字面值（`'invalid-response'` 等）。
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            TwitchApiTemporaryErrorReason::Aborted => "aborted",
            TwitchApiTemporaryErrorReason::Http => "http",
            TwitchApiTemporaryErrorReason::InvalidResponse => "invalid-response",
            TwitchApiTemporaryErrorReason::Network => "network",
            TwitchApiTemporaryErrorReason::Server => "server",
            TwitchApiTemporaryErrorReason::Timeout => "timeout",
        }
    }
}

/// 舊版 `TwitchApiRateLimitError` 的訊息（建構子固定字串）。
const RATE_LIMIT_MESSAGE: &str = "Twitch API rate limit exceeded";
/// 舊版 `TwitchApiTemporaryError` 的訊息（建構子固定字串）。
const TEMPORARY_MESSAGE: &str = "Twitch API request failed temporarily";

/// 對應舊版 `TwitchApiAuthError`：認證失敗（401／403）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TwitchApiAuthError {
    /// 對應舊版 `statusCode`（照舊版型別限制，只會是 401 或 403）。
    status_code: u16,
    /// 完整訊息（對應舊版 `Error.message`）。
    message: String,
}

impl TwitchApiAuthError {
    /// 建立錯誤；組出完整訊息（照舊版建構子）。
    pub(crate) fn new(status_code: u16) -> Self {
        Self {
            message: format!("Twitch API authentication failed (HTTP {status_code})"),
            status_code,
        }
    }

    /// 對應舊版 `statusCode` 屬性。
    pub(crate) fn status_code(&self) -> u16 {
        self.status_code
    }

    /// 錯誤類別名稱（對應舊版 `Error.name`）。
    pub(crate) fn name(&self) -> &'static str {
        "TwitchApiAuthError"
    }

    /// 完整訊息（對應舊版 `Error.message`）。
    pub(crate) fn message(&self) -> &str {
        &self.message
    }

    /// 對應舊版 `kind` 屬性。
    pub(crate) fn kind(&self) -> TwitchApiErrorKind {
        TwitchApiErrorKind::Auth
    }
}

impl fmt::Display for TwitchApiAuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for TwitchApiAuthError {}

/// 對應舊版 `TwitchApiRateLimitError`：HTTP 429 的速率限制。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TwitchApiRateLimitError {
    /// 對應舊版 `retryAt`（epoch 毫秒；舊版存 `Date`）。
    retry_at_ms: i64,
    /// 對應舊版 `retryAfterMs`（一定 ≥ 0）。
    retry_after_ms: u64,
}

impl TwitchApiRateLimitError {
    /// 建立錯誤（照舊版建構子；訊息固定）。
    pub(crate) fn new(retry_at_ms: i64, retry_after_ms: u64) -> Self {
        Self {
            retry_at_ms,
            retry_after_ms,
        }
    }

    /// 對應舊版 `retryAt` 屬性（epoch 毫秒）。
    pub(crate) fn retry_at_ms(&self) -> i64 {
        self.retry_at_ms
    }

    /// 對應舊版 `retryAfterMs` 屬性。
    pub(crate) fn retry_after_ms(&self) -> u64 {
        self.retry_after_ms
    }

    /// 錯誤類別名稱（對應舊版 `Error.name`）。
    pub(crate) fn name(&self) -> &'static str {
        "TwitchApiRateLimitError"
    }

    /// 完整訊息（對應舊版 `Error.message`）。
    pub(crate) fn message(&self) -> &str {
        RATE_LIMIT_MESSAGE
    }

    /// 對應舊版 `kind` 屬性。
    pub(crate) fn kind(&self) -> TwitchApiErrorKind {
        TwitchApiErrorKind::RateLimit
    }
}

impl fmt::Display for TwitchApiRateLimitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(RATE_LIMIT_MESSAGE)
    }
}

impl std::error::Error for TwitchApiRateLimitError {}

/// 對應舊版 `TwitchApiTemporaryError`：暫時性失敗（可重試）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TwitchApiTemporaryError {
    /// 對應舊版 `reason` 屬性。
    reason: TwitchApiTemporaryErrorReason,
    /// 對應舊版 `statusCode` 屬性（`undefined` → [`None`]）。
    status_code: Option<u16>,
}

impl TwitchApiTemporaryError {
    /// 建立錯誤（照舊版建構子；訊息固定）。
    pub(crate) fn new(reason: TwitchApiTemporaryErrorReason, status_code: Option<u16>) -> Self {
        Self {
            reason,
            status_code,
        }
    }

    /// 對應舊版 `reason` 屬性。
    pub(crate) fn reason(&self) -> TwitchApiTemporaryErrorReason {
        self.reason
    }

    /// 對應舊版 `statusCode` 屬性。
    pub(crate) fn status_code(&self) -> Option<u16> {
        self.status_code
    }

    /// 錯誤類別名稱（對應舊版 `Error.name`）。
    pub(crate) fn name(&self) -> &'static str {
        "TwitchApiTemporaryError"
    }

    /// 完整訊息（對應舊版 `Error.message`）。
    pub(crate) fn message(&self) -> &str {
        TEMPORARY_MESSAGE
    }

    /// 對應舊版 `kind` 屬性。
    pub(crate) fn kind(&self) -> TwitchApiErrorKind {
        TwitchApiErrorKind::Temporary
    }
}

impl fmt::Display for TwitchApiTemporaryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(TEMPORARY_MESSAGE)
    }
}

impl std::error::Error for TwitchApiTemporaryError {}

/// 對應舊版抽象基底類 `TwitchApiError`（Rust 沒有抽象類別，用 enum 合併三類）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TwitchApiError {
    /// 認證失敗（對應 `kind: 'auth'`）。
    Auth(TwitchApiAuthError),
    /// 速率限制（對應 `kind: 'rate-limit'`）。
    RateLimit(TwitchApiRateLimitError),
    /// 暫時性失敗（對應 `kind: 'temporary'`）。
    Temporary(TwitchApiTemporaryError),
}

impl TwitchApiError {
    /// 對應舊版 `kind` 屬性。
    pub(crate) fn kind(&self) -> TwitchApiErrorKind {
        match self {
            TwitchApiError::Auth(error) => error.kind(),
            TwitchApiError::RateLimit(error) => error.kind(),
            TwitchApiError::Temporary(error) => error.kind(),
        }
    }

    /// 錯誤類別名稱（對應舊版 `Error.name`）。
    pub(crate) fn name(&self) -> &'static str {
        match self {
            TwitchApiError::Auth(error) => error.name(),
            TwitchApiError::RateLimit(error) => error.name(),
            TwitchApiError::Temporary(error) => error.name(),
        }
    }

    /// 完整訊息（對應舊版 `Error.message`）。
    pub(crate) fn message(&self) -> &str {
        match self {
            TwitchApiError::Auth(error) => error.message(),
            TwitchApiError::RateLimit(error) => error.message(),
            TwitchApiError::Temporary(error) => error.message(),
        }
    }
}

impl fmt::Display for TwitchApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for TwitchApiError {}

impl From<TwitchApiAuthError> for TwitchApiError {
    fn from(error: TwitchApiAuthError) -> Self {
        TwitchApiError::Auth(error)
    }
}

impl From<TwitchApiRateLimitError> for TwitchApiError {
    fn from(error: TwitchApiRateLimitError) -> Self {
        TwitchApiError::RateLimit(error)
    }
}

impl From<TwitchApiTemporaryError> for TwitchApiError {
    fn from(error: TwitchApiTemporaryError) -> Self {
        TwitchApiError::Temporary(error)
    }
}

/// 舊版 `TwitchApiClient` 建構子直接拋出的驗證錯誤
/// （Rust 沒有例外，用 enum 合併；`name()`／`message()` 對應
/// `Error.name`／`Error.message`）。
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum TwitchApiClientInitError {
    /// 對應舊版 `TypeError`（欄位不合法或 URL 解析失敗）。
    TypeError(String),
    /// 對應舊版 `RangeError`（requestTimeoutMs 超過 JS 計時器上限）。
    RangeError(String),
}

impl TwitchApiClientInitError {
    /// 錯誤類別名稱（對應舊版 `Error.name`）。
    pub(crate) fn name(&self) -> &'static str {
        match self {
            TwitchApiClientInitError::TypeError(_) => "TypeError",
            TwitchApiClientInitError::RangeError(_) => "RangeError",
        }
    }

    /// 完整訊息（對應舊版 `Error.message`）。
    pub(crate) fn message(&self) -> &str {
        match self {
            TwitchApiClientInitError::TypeError(message)
            | TwitchApiClientInitError::RangeError(message) => message,
        }
    }
}

impl fmt::Display for TwitchApiClientInitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for TwitchApiClientInitError {}
