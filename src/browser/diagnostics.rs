//! 瀏覽器診斷的純函式：`browser-diagnostics.ts` 的剩餘匯出（§4.2，行為依據舊版
//! `src/browser/browser-diagnostics.ts`）。
//!
//! `sanitizeBrowserUrl`／`joinSanitizedUrl`／`classifyHostname` 已依 DEC-006 移植到
//! [`super::channel_url`]，此處直接重用其 `pub(crate)` 型別，不重複定義。
#![allow(dead_code)]

use serde_json::Value;

use super::channel_url::EndpointCategory;
use crate::logging::redact::redact_sensitive_string;

/// Console 訊息輸出上限，超過即裁切（對應舊版 `MAX_MESSAGE_LENGTH`）。
pub(crate) const MAX_MESSAGE_LENGTH: usize = 500;

/// Request failure 文字輸出上限，超過即裁切（對應舊版 `MAX_FAILURE_TEXT_LENGTH`）。
pub(crate) const MAX_FAILURE_TEXT_LENGTH: usize = 300;

/// 單一診斷事件最多記錄的 GraphQL operation 名稱數（對應舊版
/// `MAX_GRAPHQL_OPERATION_NAMES`）。
pub(crate) const MAX_GRAPHQL_OPERATION_NAMES: usize = 5;

/// 單一 operation 名稱的字串上限（對應舊版 `MAX_GRAPHQL_OPERATION_NAME_LENGTH`）。
pub(crate) const MAX_GRAPHQL_OPERATION_NAME_LENGTH: usize = 64;

/// 回應與 console 診斷共用的日誌層級判定（對應舊版 `DiagnosticLogLevel`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiagnosticLogLevel {
    Warn,
    Debug,
    /// 不記錄。
    None,
}

impl DiagnosticLogLevel {
    /// 層級的 snake_case 字串，與舊版字面值一致。
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Warn => "warn",
            Self::Debug => "debug",
            Self::None => "none",
        }
    }
}

/// 從 GraphQL POST body 抽出字串值的 `operationName` 欄位（對應舊版
/// `extractGraphQlOperationNames`）。
///
/// 接受單一 operation 物件或批次陣列；variables、extensions、hash 與原始 body
/// 一律不保留也不記錄。格式錯誤的 JSON 回傳空陣列且不 panic。
/// `None` 對應舊版的 `null`／`undefined`。
pub(crate) fn extract_graph_ql_operation_names(post_data: Option<&str>) -> Vec<String> {
    let Some(post_data) = post_data else {
        return Vec::new();
    };
    if post_data.is_empty() {
        return Vec::new();
    }

    // 舊版 `JSON.parse`：解析失敗回傳空陣列。
    let parsed: Value = match serde_json::from_str(post_data) {
        Ok(parsed) => parsed,
        Err(_) => return Vec::new(),
    };

    // 舊版 `Array.isArray(parsed) ? parsed : [parsed]`。
    let candidates: &[Value] = match &parsed {
        Value::Array(items) => items,
        _ => std::slice::from_ref(&parsed),
    };

    let mut names: Vec<String> = Vec::new();
    for candidate in candidates {
        // 舊版：非物件、或 `operationName` 非字串（或空白）的一律忽略。
        let Some(operation_name) = candidate.get("operationName").and_then(Value::as_str) else {
            continue;
        };
        if operation_name.trim().is_empty() {
            continue;
        }
        // 舊版順序：先消毒、後裁切到單一名稱上限。
        let bounded = truncate_text(
            &redact_sensitive_string(operation_name),
            MAX_GRAPHQL_OPERATION_NAME_LENGTH,
        );
        if names.contains(&bounded) {
            continue;
        }
        names.push(bounded);
        if names.len() >= MAX_GRAPHQL_OPERATION_NAMES {
            break;
        }
    }
    names
}

/// 判定一個 HTTP 回應是否值得記錄、以及記錄層級（對應舊版
/// `decideHttpResponseLevel`）。例行的媒體分段、圖片、字型與成功的第三方
/// 分析回應一律排除。
pub(crate) fn decide_http_response_level(
    status: u16,
    resource_type: &str,
    endpoint_category: EndpointCategory,
) -> DiagnosticLogLevel {
    if endpoint_category == EndpointCategory::TwitchMedia
        || matches!(resource_type, "image" | "font" | "media")
    {
        return DiagnosticLogLevel::None;
    }

    if status >= 500 {
        return DiagnosticLogLevel::Warn;
    }

    if status >= 400 {
        if endpoint_category == EndpointCategory::TwitchGraphql
            || endpoint_category == EndpointCategory::TwitchAntiAbuse
            || is_relevant_resource_type(resource_type)
        {
            return DiagnosticLogLevel::Warn;
        }
        return DiagnosticLogLevel::None;
    }

    if status >= 200
        && matches!(
            endpoint_category,
            EndpointCategory::TwitchDocument
                | EndpointCategory::TwitchJavascript
                | EndpointCategory::TwitchGraphql
                | EndpointCategory::TwitchAntiAbuse
                | EndpointCategory::TwitchApi
        )
        && is_relevant_resource_type(resource_type)
    {
        return DiagnosticLogLevel::Debug;
    }

    DiagnosticLogLevel::None
}

/// 在 4xx 失敗或成功 bootstrap 回應時值得記錄的 resource types（對應舊版
/// `RELEVANT_RESOURCE_TYPES`）。
fn is_relevant_resource_type(resource_type: &str) -> bool {
    matches!(resource_type, "document" | "script" | "xhr" | "fetch")
}

/// 判定失敗請求的記錄層級（對應舊版 `decideRequestFailureLevel`）。被本機
/// 網路封鎖的追蹤／分析 host、未知 host，以及中斷的請求（`net::ERR_ABORTED`，
/// 例如切換畫質時取消的媒體分段）屬於例行噪音，對應 `debug`；Twitch 核心端點
/// 的失敗維持 `warn`。`None` 對應舊版未提供 failureText 的情形。
pub(crate) fn decide_request_failure_level(
    endpoint_category: EndpointCategory,
    failure_text: Option<&str>,
) -> DiagnosticLogLevel {
    if matches!(
        endpoint_category,
        EndpointCategory::ThirdParty | EndpointCategory::TwitchOther | EndpointCategory::Unknown
    ) {
        return DiagnosticLogLevel::Debug;
    }
    if failure_text == Some("net::ERR_ABORTED") {
        return DiagnosticLogLevel::Debug;
    }
    DiagnosticLogLevel::Warn
}

/// 對應舊版 `CONSOLE_ERROR_DEBUG_PATTERNS`：與降級請求失敗互相呼應的 console
/// 錯誤訊息樣式——由同樣被封鎖的追蹤 host 或 Twitch 端 integrity check 造成的
/// 例行頁面錯誤，而非 watchdog 本身。樣式已小寫化，比對前先把訊息小寫化。
const CONSOLE_ERROR_DEBUG_PATTERNS: [&str; 2] =
    ["spadeclient send error", "failed integrity check"];

/// 把 console 訊息類型對應到記錄層級（對應舊版 `decideConsoleLevel`）：`error`
/// 對應 `warn`，除非訊息來源是例行噪音 host（`third_party`、`twitch_other`、
/// `unknown`，對應舊版未提供 category 時以 `None` 表示）或訊息符合已知噪音樣式；
/// `warning`／`warn` 對應 `debug`；一般的 `log`、`info`、`debug`、table、timing
/// 與 trace 訊息一律忽略。
pub(crate) fn decide_console_level(
    console_type: &str,
    message: &str,
    endpoint_category: Option<EndpointCategory>,
) -> DiagnosticLogLevel {
    let normalized = console_type.trim().to_lowercase();
    if normalized == "error" {
        if matches!(
            endpoint_category,
            Some(
                EndpointCategory::ThirdParty
                    | EndpointCategory::TwitchOther
                    | EndpointCategory::Unknown
            )
        ) {
            return DiagnosticLogLevel::Debug;
        }
        let normalized_message = message.trim().to_lowercase();
        if CONSOLE_ERROR_DEBUG_PATTERNS
            .iter()
            .any(|pattern| normalized_message.contains(pattern))
        {
            return DiagnosticLogLevel::Debug;
        }
        return DiagnosticLogLevel::Warn;
    }
    if normalized == "warning" || normalized == "warn" {
        return DiagnosticLogLevel::Debug;
    }
    DiagnosticLogLevel::None
}

/// 將自由文字裁切至最多 `max` 個字元（對應舊版 `truncateText`）。
///
/// 舊版以 UTF-16 code unit 計數（`String.prototype.slice` 的語意），此處以
/// char 累加 UTF-16 長度並在字元邊界切斷：BMP 內字元與舊版逐字相同，僅代理
/// 對字元（astral）有差異。輸入恆為 UTF-8 合法字串，不會有 lone surrogate。
pub(crate) fn truncate_text(value: &str, max: usize) -> String {
    let mut utf16_len = 0usize;
    for (index, ch) in value.char_indices() {
        let char_utf16_len = ch.len_utf16();
        if utf16_len + char_utf16_len > max {
            return value[..index].to_string();
        }
        utf16_len += char_utf16_len;
    }
    value.to_string()
}

#[cfg(test)]
mod tests;
