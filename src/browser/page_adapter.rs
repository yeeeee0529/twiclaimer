//! Playwright Page adapter 的診斷層：移植舊版
//! `src/browser/adapters/PlaywrightBrowserPageAdapter.ts` 的三個診斷 listener
//! （§7.5）。
//!
//! 分工（照 S1.4 的 G3 驗證界線）：CDP→Playwright 的值翻譯、文字的
//! `redactSensitiveString`＋截斷、GraphQL `operationName` 抽出與 console 位置
//! URL 的消毒都在事件泵（[`crate::browser::cdp::page`]）完成；本層消費
//! [`PageDiagnostic`]，負責訂閱接線、request／response `url` 的
//! `sanitizeBrowserUrl`＋`joinSanitizedUrl` 消毒、文字的消毒與截斷（對泵已處理
//! 過的值為恆等或等價於舊版單次套用），以及選用欄位的省略條件（`failure_text`
//! 有值、`graphql_operation_names` 非空、`source_url` 非空才輸出）。listener
//! 內的錯誤一律吞掉：轉換與 listener 呼叫包在同一個 `catch_unwind`（對應舊版
//! handler 的 try/catch），panic 不會外溢到事件來源。
//!
//! 位置 URL（console `source_url`）不在本層重消毒：事件泵輸出的消毒形狀對
//! `file://` 等位置是 `/…`（舊版單次消毒的結果），再消毒會誤換成 `[REDACTED]`。
//!
//! 以下 API 的正式呼叫端在後續步驟（S3.3 接 `BrowserManager`）；
//! `GraphQL operationName` 抽出的純函式在 [`super::diagnostics`]（照 DEC-006
//! 的重用原則不重複定義），事件泵改接由主會話在合併後統一處理。
#![allow(dead_code)]

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use super::cdp::launcher::CancelListener;
use super::cdp::page::{PageDiagnostic, PageShared};
use super::channel_url::{join_sanitized_url, sanitize_browser_url};
use super::diagnostics::{MAX_FAILURE_TEXT_LENGTH, MAX_MESSAGE_LENGTH, truncate_text};
use crate::logging::redact::redact_sensitive_string;

/// 舊版 `BrowserRequestFailureDiagnostic`：消毒後的 `requestfailed` 診斷
/// （URL 不含 query、fragment、port 與 credentials）。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct BrowserRequestFailureDiagnostic {
    /// 消毒後的 URL（host＋path）。
    pub(crate) url: String,
    pub(crate) method: String,
    pub(crate) resource_type: String,
    /// 有值才輸出（`None` 對應舊版的省略）。
    pub(crate) failure_text: Option<String>,
    /// 非空才輸出（`None` 對應舊版的省略）。
    pub(crate) graphql_operation_names: Option<Vec<String>>,
}

/// 舊版 `BrowserHttpResponseDiagnostic`：消毒後的 `response` 診斷。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct BrowserHttpResponseDiagnostic {
    /// 消毒後的 URL（host＋path）。
    pub(crate) url: String,
    pub(crate) method: String,
    pub(crate) resource_type: String,
    pub(crate) status: u32,
    pub(crate) status_text: String,
    /// 非空才輸出（`None` 對應舊版的省略）。
    pub(crate) graphql_operation_names: Option<Vec<String>>,
}

/// 舊版 `BrowserConsoleDiagnostic`：消毒後的 `console` 診斷（引數永不序列化）。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct BrowserConsoleDiagnostic {
    pub(crate) r#type: String,
    /// 消毒後並截斷到 [`MAX_MESSAGE_LENGTH`] 的文字。
    pub(crate) text: String,
    /// 非空才輸出（`None` 對應舊版的省略）。
    pub(crate) source_url: Option<String>,
    pub(crate) line_number: i64,
    pub(crate) column_number: i64,
}

/// 診斷事件來源（對應舊版 `Page` 的 `on`／`off` 事件介面）：正式碼由
/// [`PageShared`] 提供；單元測試以假來源餵入（照舊測試的 `FakePage`）。
pub(crate) trait DiagnosticEventSource {
    /// 註冊診斷 handler，回傳取消函式（對應舊版 `page.on(...)` 的 off）。
    fn on_diagnostic(&self, handler: Box<dyn Fn(&PageDiagnostic) + Send + Sync>) -> CancelListener;
}

impl DiagnosticEventSource for PageShared {
    fn on_diagnostic(&self, handler: Box<dyn Fn(&PageDiagnostic) + Send + Sync>) -> CancelListener {
        PageShared::on_diagnostic(self, move |diagnostic| handler(diagnostic))
    }
}

/// 舊版 `PlaywrightBrowserPageAdapter` 的診斷部分：把事件泵的
/// [`PageDiagnostic`] 消毒、整形後轉發給 listener。
pub(crate) struct PlaywrightBrowserPageAdapter {
    source: Arc<dyn DiagnosticEventSource>,
}

impl PlaywrightBrowserPageAdapter {
    /// 以診斷事件來源建立（正式碼傳入 `Arc<PageShared>`）。
    pub(crate) fn new(source: Arc<dyn DiagnosticEventSource>) -> Self {
        Self { source }
    }

    /// 舊版 `onRequestFailed`：註冊 requestfailed 的消毒轉發 listener。
    pub(crate) fn on_request_failed(
        &self,
        listener: impl Fn(&BrowserRequestFailureDiagnostic) + Send + Sync + 'static,
    ) -> CancelListener {
        self.source.on_diagnostic(Box::new(move |diagnostic| {
            // 舊版 handler 的 try/catch：轉換與呼叫 listener 的任何錯誤一律吞掉
            // （診斷不得影響頁面生命週期 listener）。轉換在 listener 呼叫之前
            // 完成，失敗即不會呼叫 listener。
            let _ = catch_unwind(AssertUnwindSafe(|| {
                if let PageDiagnostic::RequestFailed {
                    url,
                    method,
                    resource_type,
                    failure_text,
                    graphql_operation_names,
                } = diagnostic
                {
                    listener(&BrowserRequestFailureDiagnostic {
                        url: sanitize_url_for_diagnostic(url),
                        method: method.clone(),
                        resource_type: resource_type.clone(),
                        failure_text: failure_text.as_deref().and_then(truncate_failure_text),
                        graphql_operation_names: optional_graphql_operation_names(
                            graphql_operation_names,
                        ),
                    });
                }
            }));
        }))
    }

    /// 舊版 `onResponse`：註冊 response 的消毒轉發 listener。
    pub(crate) fn on_response(
        &self,
        listener: impl Fn(&BrowserHttpResponseDiagnostic) + Send + Sync + 'static,
    ) -> CancelListener {
        self.source.on_diagnostic(Box::new(move |diagnostic| {
            let _ = catch_unwind(AssertUnwindSafe(|| {
                if let PageDiagnostic::Response {
                    url,
                    method,
                    resource_type,
                    status,
                    status_text,
                    graphql_operation_names,
                } = diagnostic
                {
                    listener(&BrowserHttpResponseDiagnostic {
                        url: sanitize_url_for_diagnostic(url),
                        method: method.clone(),
                        resource_type: resource_type.clone(),
                        status: *status,
                        status_text: status_text.clone(),
                        graphql_operation_names: optional_graphql_operation_names(
                            graphql_operation_names,
                        ),
                    });
                }
            }));
        }))
    }

    /// 舊版 `onConsole`：註冊 console 的消毒轉發 listener。
    pub(crate) fn on_console(
        &self,
        listener: impl Fn(&BrowserConsoleDiagnostic) + Send + Sync + 'static,
    ) -> CancelListener {
        self.source.on_diagnostic(Box::new(move |diagnostic| {
            let _ = catch_unwind(AssertUnwindSafe(|| {
                if let PageDiagnostic::Console {
                    r#type,
                    text,
                    source_url,
                    line_number,
                    column_number,
                } = diagnostic
                {
                    listener(&BrowserConsoleDiagnostic {
                        r#type: r#type.clone(),
                        // 舊版：先消毒再截斷到 `MAX_MESSAGE_LENGTH`。
                        text: truncate_text(&redact_sensitive_string(text), MAX_MESSAGE_LENGTH),
                        // 位置 URL 已在事件泵消毒（`optional_source_url`）；這裡只
                        // 套省略條件（非空才輸出），重消毒會把 `file://` 位置的
                        // `/…` 形狀誤換成 `[REDACTED]`。
                        source_url: source_url.clone().filter(|url| !url.is_empty()),
                        line_number: *line_number,
                        column_number: *column_number,
                    });
                }
            }));
        }))
    }
}

/// 舊版 `sanitizeUrlForDiagnostic`：`joinSanitizedUrl(sanitizeBrowserUrl(rawUrl))`。
fn sanitize_url_for_diagnostic(raw_url: &str) -> String {
    join_sanitized_url(&sanitize_browser_url(Some(raw_url)))
}

/// 舊版 `truncateFailureText`：缺值與空字串回傳 `None`（欄位省略），其餘先
/// 消毒再截斷到 [`MAX_FAILURE_TEXT_LENGTH`]。
fn truncate_failure_text(error_text: &str) -> Option<String> {
    if error_text.is_empty() {
        return None;
    }
    Some(truncate_text(
        &redact_sensitive_string(error_text),
        MAX_FAILURE_TEXT_LENGTH,
    ))
}

/// 舊版的省略條件：`graphQlOperationNames` 非空才輸出。名單由事件泵抽出
/// （照舊版 `extractGraphQlOperationNames`，已含消毒、截斷與去重）。
fn optional_graphql_operation_names(names: &[String]) -> Option<Vec<String>> {
    if names.is_empty() {
        None
    } else {
        Some(names.to_vec())
    }
}

#[cfg(test)]
mod tests;
