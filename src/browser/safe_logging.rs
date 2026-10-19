//! 安全日誌輔助：`safeErrorMessage`、`safeLog` 逐函式移植（§4.2，行為依據舊版
//! `src/browser/safe-logging.ts`）。
//!
//! 依賴的消毒函式在 [`crate::logging::redact`]、Logger 在 [`crate::logging::logger`]，
//! 皆直接重用。舊版的 `unknown` 錯誤動態（`Error` instance、字串、`String(error)`、
//! try/catch）在 Rust 由呼叫端以 `Display`（`to_string()`）產生訊息字串後傳入，
//! 詳見 [`safe_error_message`]。
#![allow(dead_code)]

use crate::logging::logger::{JsonLineLogger, LogLevel};
use crate::logging::redact::{SanitizedValue, redact_sensitive_string};

/// 舊版 `safeErrorMessage` 在 `error` 為 `undefined`／`null` 時的預設訊息。
pub(crate) const UNKNOWN_CHANNEL_SESSION_FAILURE: &str = "Unknown channel session failure";

/// 舊版 try/catch 回退的訊息；Rust 的 `Display` 不會丟例外，正常不會走到，
/// 常數照舊保留以利對照。
pub(crate) const UNSERIALIZABLE_CHANNEL_SESSION_FAILURE: &str =
    "Unserializable channel session failure";

/// 把 caught error 轉成消毒後的訊息字串（對應舊版 `safeErrorMessage`）。
///
/// - `Some(message)`：對應舊版的 `Error.message`／字串／`String(error)` 分支，
///   由呼叫端先以 `Display` 取得訊息。
/// - `None`：對應 `undefined`／`null`，回傳 [`UNKNOWN_CHANNEL_SESSION_FAILURE`]。
///
/// 回傳值一律經過 [`redact_sensitive_string`]，絕不會帶出敏感內容。
pub(crate) fn safe_error_message(error: Option<&str>) -> String {
    match error {
        Some(message) => redact_sensitive_string(message),
        None => UNKNOWN_CHANNEL_SESSION_FAILURE.to_string(),
    }
}

/// 安全轉發日誌（對應舊版 `safeLog`）：日誌的任何失敗不得中斷 session 清理或
/// timer callback。舊版以 try/catch 達成；Rust 的 `JsonLineLogger` 契約本來就
/// 不回傳失敗（寫入錯誤照舊版忽略），此處保留同樣的「不丟錯」語意。
pub(crate) fn safe_log(
    logger: &JsonLineLogger,
    level: LogLevel,
    event: &str,
    fields: &SanitizedValue,
) {
    match level {
        LogLevel::Debug => logger.debug(event, Some(fields)),
        LogLevel::Info => logger.info(event, Some(fields)),
        LogLevel::Warn => logger.warn(event, Some(fields)),
        LogLevel::Error => logger.error(event, Some(fields)),
    }
}

#[cfg(test)]
mod tests;
