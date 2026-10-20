//! `PlaywrightBrowserPageAdapter` 診斷層的單元測試：移植舊版
//! `test/unit/playwright-browser-page-adapter.test.ts` 的 9 個案例。
//!
//! 舊測試以假 playwright Request／Response／ConsoleMessage 物件餵入；新架構的
//! 對應輸入是事件泵翻譯出的 [`PageDiagnostic`]（假來源照舊測試的 `FakePage`
//! 登記與派發）。值翻譯與文字消毒在事件泵完成（G3 驗證），因此測試輸入的
//! 文字／位置欄位攜帶泵的輸出形狀，`url` 攜帶泵保留的原始值、由本層消毒。
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex};

use serde_json::json;

use crate::browser::cdp::launcher::CancelListener;
use crate::browser::cdp::page::PageDiagnostic;
use crate::browser::diagnostics::extract_graph_ql_operation_names;

use super::{
    BrowserConsoleDiagnostic, BrowserHttpResponseDiagnostic, BrowserRequestFailureDiagnostic,
    DiagnosticEventSource, PlaywrightBrowserPageAdapter,
};

/// 假來源的 handler 清單（對應舊測試 FakePage 的 listener 集合）。
type SharedHandlers = Arc<Mutex<Vec<Arc<dyn Fn(&PageDiagnostic) + Send + Sync>>>>;

/// 對應舊測試的 `FakePage`：登記 handler、可查數量、手動派發事件。
struct FakeDiagnosticSource {
    handlers: SharedHandlers,
}

impl FakeDiagnosticSource {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            handlers: Arc::new(Mutex::new(Vec::new())),
        })
    }

    /// 已登記的 handler 數（對應舊測試的 `page.listenerCount(event)`）。
    fn listener_count(&self) -> usize {
        self.handlers.lock().unwrap().len()
    }

    /// 派發一個診斷事件（對應舊測試的 `page.emit(event, payload)`；不在來源層
    /// 攔錯——吞錯是 adapter handler 的職責）。
    fn emit(&self, diagnostic: &PageDiagnostic) {
        let handlers = self.handlers.lock().unwrap().clone();
        for handler in handlers {
            handler(diagnostic);
        }
    }
}

impl DiagnosticEventSource for FakeDiagnosticSource {
    fn on_diagnostic(&self, handler: Box<dyn Fn(&PageDiagnostic) + Send + Sync>) -> CancelListener {
        let handler: Arc<dyn Fn(&PageDiagnostic) + Send + Sync> = Arc::from(handler);
        self.handlers.lock().unwrap().push(Arc::clone(&handler));
        let handlers = Arc::clone(&self.handlers);
        Box::new(move || {
            handlers
                .lock()
                .unwrap()
                .retain(|registered| !Arc::ptr_eq(registered, &handler));
        })
    }
}

/// 以假來源建立 adapter（對應舊測試的 `new PlaywrightBrowserPageAdapter(page)`）。
fn adapter_with_source() -> (Arc<FakeDiagnosticSource>, PlaywrightBrowserPageAdapter) {
    let source = FakeDiagnosticSource::new();
    let adapter =
        PlaywrightBrowserPageAdapter::new(Arc::clone(&source) as Arc<dyn DiagnosticEventSource>);
    (source, adapter)
}

/// 建立收集 requestfailed 診斷的捕手。
fn capture_request_failures(
    adapter: &PlaywrightBrowserPageAdapter,
) -> Arc<Mutex<Vec<BrowserRequestFailureDiagnostic>>> {
    let captured: Arc<Mutex<Vec<BrowserRequestFailureDiagnostic>>> =
        Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&captured);
    // 明確取消制：drop 取消函式不會解除註冊，捕手持續有效。
    let _cancel = adapter.on_request_failed(move |diagnostic| {
        sink.lock().unwrap().push(diagnostic.clone());
    });
    captured
}

/// 建立收集 response 診斷的捕手。
fn capture_responses(
    adapter: &PlaywrightBrowserPageAdapter,
) -> Arc<Mutex<Vec<BrowserHttpResponseDiagnostic>>> {
    let captured: Arc<Mutex<Vec<BrowserHttpResponseDiagnostic>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&captured);
    let _cancel = adapter.on_response(move |diagnostic| {
        sink.lock().unwrap().push(diagnostic.clone());
    });
    captured
}

/// 建立收集 console 診斷的捕手。
fn capture_console_messages(
    adapter: &PlaywrightBrowserPageAdapter,
) -> Arc<Mutex<Vec<BrowserConsoleDiagnostic>>> {
    let captured: Arc<Mutex<Vec<BrowserConsoleDiagnostic>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&captured);
    let _cancel = adapter.on_console(move |diagnostic| {
        sink.lock().unwrap().push(diagnostic.clone());
    });
    captured
}

#[test]
// 原測試：訂閱與取消訂閱 requestfailed
fn subscribe_and_unsubscribe_request_failed() {
    let (source, adapter) = adapter_with_source();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&captured);
    let unsubscribe = adapter.on_request_failed(move |diagnostic| {
        sink.lock().unwrap().push(diagnostic.clone());
    });

    assert_eq!(source.listener_count(), 1);
    unsubscribe();
    assert_eq!(source.listener_count(), 0);
}

#[test]
// 原測試：將 requestfailed 轉為消毒後的結構化診斷
fn converts_request_failed_into_sanitized_structured_diagnostic() {
    let (source, adapter) = adapter_with_source();
    let captured = capture_request_failures(&adapter);

    // `url` 由本層消毒：query 全丟；`failureText` 經消毒與截斷（與泵的處理
    // 疊加後等同舊版單次套用）。
    source.emit(&PageDiagnostic::RequestFailed {
        url: "https://static.twitchcdn.net/assets/main.js?token=secret".to_string(),
        method: "GET".to_string(),
        resource_type: "script".to_string(),
        failure_text: Some("net::ERR_ABORTED".to_string()),
        graphql_operation_names: Vec::new(),
    });

    assert_eq!(
        captured.lock().unwrap().clone(),
        vec![BrowserRequestFailureDiagnostic {
            url: "static.twitchcdn.net/assets/main.js".to_string(),
            method: "GET".to_string(),
            resource_type: "script".to_string(),
            failure_text: Some("net::ERR_ABORTED".to_string()),
            graphql_operation_names: None,
        }]
    );
}

#[test]
// 原測試：requestfailed 抽出 GraphQL operationName 但不含 variables
fn extracts_graphql_operation_name_without_variables_on_request_failed() {
    let (source, adapter) = adapter_with_source();
    let captured = capture_request_failures(&adapter);

    // 舊版在 handler 內從 `request.postData()` 抽出；抽出已收斂成純函式
    // （`diagnostics::extract_graph_ql_operation_names`，事件泵改接由主會話
    // 統一處理），這裡照同一順序先抽出再餵入 adapter。
    let post_data = json!({
        "operationName": "PlaybackAccessToken",
        "variables": { "login": "private_channel" },
    })
    .to_string();
    source.emit(&PageDiagnostic::RequestFailed {
        url: "https://gql.twitch.tv/gql".to_string(),
        method: "POST".to_string(),
        resource_type: "fetch".to_string(),
        failure_text: Some("net::ERR_FAILED".to_string()),
        graphql_operation_names: extract_graph_ql_operation_names(Some(&post_data)),
    });

    let captured = captured.lock().unwrap().clone();
    assert_eq!(captured.len(), 1);
    assert_eq!(
        captured[0].graphql_operation_names,
        Some(vec!["PlaybackAccessToken".to_string()]),
    );
    let serialized = format!("{captured:?}");
    assert!(!serialized.contains("private_channel"), "{serialized}");
    assert!(!serialized.contains("variables"), "{serialized}");
}

#[test]
// 原測試：GraphQL body 解析失敗不傳播例外
fn graphql_body_parse_failure_does_not_propagate() {
    // 純函式層：解析失敗回傳空清單且不 panic（對應舊版不拋例外）。
    assert!(extract_graph_ql_operation_names(Some("{invalid-json")).is_empty());

    let (source, adapter) = adapter_with_source();
    let captured = capture_request_failures(&adapter);

    // adapter 端：空清單省略欄位，listener 仍被呼叫一次。
    source.emit(&PageDiagnostic::RequestFailed {
        url: "https://gql.twitch.tv/gql".to_string(),
        method: "POST".to_string(),
        resource_type: "fetch".to_string(),
        failure_text: None,
        graphql_operation_names: extract_graph_ql_operation_names(Some("{invalid-json")),
    });

    let captured = captured.lock().unwrap().clone();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].graphql_operation_names, None);
    assert_eq!(captured[0].failure_text, None);
}

#[test]
// 原測試：訂閱與取消訂閱 response
fn subscribe_and_unsubscribe_response() {
    let (source, adapter) = adapter_with_source();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&captured);
    let unsubscribe = adapter.on_response(move |diagnostic| {
        sink.lock().unwrap().push(diagnostic.clone());
    });

    assert_eq!(source.listener_count(), 1);
    unsubscribe();
    assert_eq!(source.listener_count(), 0);
}

#[test]
// 原測試：將 response 轉為含 status 的結構化診斷
fn converts_response_into_structured_diagnostic_with_status() {
    let (source, adapter) = adapter_with_source();
    let captured = capture_responses(&adapter);

    source.emit(&PageDiagnostic::Response {
        url: "https://static.twitchcdn.net/assets/loader.js?x=1".to_string(),
        method: "GET".to_string(),
        resource_type: "script".to_string(),
        status: 503,
        status_text: "Service Unavailable".to_string(),
        graphql_operation_names: Vec::new(),
    });

    assert_eq!(
        captured.lock().unwrap().clone(),
        vec![BrowserHttpResponseDiagnostic {
            url: "static.twitchcdn.net/assets/loader.js".to_string(),
            method: "GET".to_string(),
            resource_type: "script".to_string(),
            status: 503,
            status_text: "Service Unavailable".to_string(),
            graphql_operation_names: None,
        }]
    );
}

#[test]
// 原測試：訂閱與取消訂閱 console
fn subscribe_and_unsubscribe_console() {
    let (source, adapter) = adapter_with_source();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&captured);
    let unsubscribe = adapter.on_console(move |diagnostic| {
        sink.lock().unwrap().push(diagnostic.clone());
    });

    assert_eq!(source.listener_count(), 1);
    unsubscribe();
    assert_eq!(source.listener_count(), 0);
}

#[test]
// 原測試：將 console message 轉為消毒後的結構化診斷
fn converts_console_message_into_sanitized_structured_diagnostic() {
    let (source, adapter) = adapter_with_source();
    let captured = capture_console_messages(&adapter);

    // 文字由本層消毒與截斷；位置 URL 已在事件泵消毒（§7.5 的
    // `optional_source_url`），`PageDiagnostic` 攜帶消毒後的值。
    source.emit(&PageDiagnostic::Console {
        r#type: "error".to_string(),
        text: "Failed with token=Bearer abc123".to_string(),
        source_url: Some("static.twitchcdn.net/app.js".to_string()),
        line_number: 12,
        column_number: 4,
    });

    assert_eq!(
        captured.lock().unwrap().clone(),
        vec![BrowserConsoleDiagnostic {
            r#type: "error".to_string(),
            text: "Failed with token=[REDACTED] [REDACTED]".to_string(),
            source_url: Some("static.twitchcdn.net/app.js".to_string()),
            line_number: 12,
            column_number: 4,
        }]
    );
}

#[test]
// 原測試：console text 拋錯時吞掉例外且不呼叫 listener
fn console_diagnostic_panics_are_swallowed_and_the_source_keeps_working() {
    let (source, adapter) = adapter_with_source();

    // 舊測試：handler 的 try/catch 圍住「取值＋呼叫 listener」，text 拋錯時
    // 吞掉例外且 listener 未被呼叫。Rust 的取值在事件泵完成（序列化不會失敗，
    // 泵的攔截已涵蓋該層）；本層的等價防線是 catch_unwind 圍住轉換與 listener
    // 呼叫——轉換先於呼叫完成，轉換或 listener 的 panic 一律吞掉、不外溢到
    // 來源。此處以 listener panic 驗證吞錯路徑與生命週期不受影響。
    let _cancel = adapter.on_console(move |_diagnostic| {
        panic!("text serialization failed");
    });
    let emit_result = catch_unwind(AssertUnwindSafe(|| {
        source.emit(&PageDiagnostic::Console {
            r#type: "error".to_string(),
            text: "irrelevant".to_string(),
            source_url: None,
            line_number: 0,
            column_number: 0,
        });
    }));
    assert!(emit_result.is_ok(), "listener 的 panic 不得外溢到事件來源");

    // 生命週期不受影響：之後的事件照常送達健康的 listener（舊測試的
    // 「listener 未被呼叫」由「轉換先於呼叫、失敗即不呼叫」的結構保證）。
    let captured = capture_console_messages(&adapter);
    source.emit(&PageDiagnostic::Console {
        r#type: "error".to_string(),
        text: "second event".to_string(),
        source_url: None,
        line_number: 0,
        column_number: 0,
    });
    assert_eq!(captured.lock().unwrap().len(), 1);
}

/// 額外測試（§7.5 的截斷長度）：`failureText` 超過上限依上限裁切、空字串省略。
/// 舊測試 2 只覆蓋短文字；adapter 的截斷路徑在此補上。
#[test]
fn request_failed_failure_text_is_truncated_and_empty_is_omitted() {
    let (source, adapter) = adapter_with_source();
    let captured = capture_request_failures(&adapter);

    let long_failure = "x".repeat(400);
    source.emit(&PageDiagnostic::RequestFailed {
        url: "https://example.com/a".to_string(),
        method: "GET".to_string(),
        resource_type: "other".to_string(),
        failure_text: Some(long_failure),
        graphql_operation_names: Vec::new(),
    });
    source.emit(&PageDiagnostic::RequestFailed {
        url: "https://example.com/b".to_string(),
        method: "GET".to_string(),
        resource_type: "other".to_string(),
        failure_text: Some(String::new()),
        graphql_operation_names: Vec::new(),
    });

    let captured = captured.lock().unwrap().clone();
    assert_eq!(captured.len(), 2);
    assert_eq!(
        captured[0].failure_text,
        Some("x".repeat(super::MAX_FAILURE_TEXT_LENGTH)),
        "超過 300 字元應依上限裁切"
    );
    assert_eq!(captured[1].failure_text, None, "空字串應省略欄位");
}
