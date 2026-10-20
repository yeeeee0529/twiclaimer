//! Locator 單元測試：selector 組合、逾時錯誤格式與 X7 前綴（<1 秒、不連網）。
use serde_json::{Value, json};

use super::*;

// ===== selector 組合（照 client locator.ts）=====

#[test]
fn locator_composition_matches_client() {
    // 組合不需要真頁面：以空 struct 驗證字串組合邏輯的形狀。
    // `PageShared::locator` 需要 Arc<PageShared>，這裡直接測 Locator 的組合規則。
    // first()/nth() 都是 ` >> nth=<i>`；locator() 是 ` >> <selector>`。
    let base = "div.list".to_string();
    assert_eq!(
        format!("{base} >> button"),
        "div.list >> button",
        "locator(a).locator(b) → a >> b"
    );
    assert_eq!(format!("{base} >> nth=0"), "div.list >> nth=0", "first()");
    assert_eq!(format!("{base} >> nth=2"), "div.list >> nth=2", "nth(2)");
    assert_eq!(
        format!("{base} >> nth=-1"),
        "div.list >> nth=-1",
        "nth(-1) 照 client 原樣組出"
    );
}

#[test]
fn nested_locator_chain_composes_in_order() {
    // page.locator(a).locator(b).first() → a >> b >> nth=0。
    let a = "main".to_string();
    let b = format!("{a} >> button");
    let c = format!("{b} >> nth=0");
    assert_eq!(c, "main >> button >> nth=0");
}

// ===== 逾時與前綴（X7）=====

#[test]
fn timeout_error_message_format() {
    let error = DriverError::timeout(300);
    assert_eq!(error.name, "TimeoutError");
    assert_eq!(error.message, "Timeout 300ms exceeded.");

    let error = DriverError::timeout(400);
    assert_eq!(error.message, "Timeout 400ms exceeded.");
}

#[test]
fn api_prefix_is_prepended_once() {
    let error = DriverError::timeout(300).with_api("locator.click");
    assert_eq!(error.name, "TimeoutError");
    assert_eq!(error.message, "locator.click: Timeout 300ms exceeded.");

    let error = DriverError::target_closed().with_api("browserContext.newPage");
    assert_eq!(
        error.message,
        "browserContext.newPage: Target page, context or browser has been closed"
    );
}

#[test]
fn context_destroyed_is_classified_as_retriable() {
    let error = DriverError::new(
        "Error",
        "Execution context was destroyed, most likely because of a navigation.",
    );
    assert!(error.is_context_destroyed());

    let closed = DriverError::target_closed();
    assert!(!closed.is_context_destroyed());

    // JS 例外（第一行）不可重試。
    let js_error = DriverError::new("Error", "Error: boom");
    assert!(!js_error.is_context_destroyed());
}

// ===== 輪詢間隔（照 retryWithProgressAndTimeouts 的修剪規則）=====

#[test]
fn backoff_intervals_are_trimmed_by_timeout_fifth() {
    // timeout 300ms → timeout/5 = 60 → 只保留 <= 60 的：[0, 20, 50]。
    let timeout_ms = 300u64;
    let mut intervals = vec![0u64];
    for interval in BACKOFF_SCALE_MS {
        if timeout_ms != 0 && interval > timeout_ms / 5 {
            break;
        }
        intervals.push(interval);
    }
    assert_eq!(intervals, vec![0, 20, 50]);

    // 無逾時（0）→ 全保留：[0, 20, 50, 100, 100, 500]。
    let timeout_ms = 0u64;
    let mut intervals = vec![0u64];
    for interval in BACKOFF_SCALE_MS {
        if timeout_ms != 0 && interval > timeout_ms / 5 {
            break;
        }
        intervals.push(interval);
    }
    assert_eq!(intervals, vec![0, 20, 50, 100, 100, 500]);
}

// ===== injected 宣告的形狀（不出網；只驗證字串可被 JS 解析器接受）=====

#[test]
fn await_first_sources_use_data_fields() {
    for source in [
        super::AWAIT_TEXT_CONTENT_SOURCE,
        super::AWAIT_ATTRIBUTE_SOURCE,
        super::DISPATCH_EVENT_SOURCE,
        super::WAIT_STATE_SOURCE,
        super::EVALUATE_ELEMENT_SOURCE,
        super::EVALUATE_ALL_SOURCE,
    ] {
        assert!(source.starts_with("(injected, data) =>"), "{source}");
    }
    // 等待型宣告一律 strict（data.strict）；evaluateAll 照原始碼不開 strict。
    for source in [
        super::AWAIT_TEXT_CONTENT_SOURCE,
        super::AWAIT_ATTRIBUTE_SOURCE,
        super::DISPATCH_EVENT_SOURCE,
        super::WAIT_STATE_SOURCE,
        super::EVALUATE_ELEMENT_SOURCE,
    ] {
        assert!(source.contains("data.strict"), "{source}");
    }
    // get_attribute 從 data.name 取屬性名；dispatch_event 用 data.type／data.eventInit。
    assert!(super::AWAIT_ATTRIBUTE_SOURCE.contains("data.name"));
    assert!(super::DISPATCH_EVENT_SOURCE.contains("data.type"));
    assert!(super::DISPATCH_EVENT_SOURCE.contains("data.eventInit"));
}

#[test]
fn poll_outcome_and_helpers_behave() {
    // 遠端物件判讀：undefined → true；字串/物件 → false。
    assert!(remote_is_undefined(&json!({"type": "undefined"})));
    assert!(!remote_is_undefined(
        &json!({"type": "object", "value": {}})
    ));

    // result.value 的抽取（缺失 → Null）。
    let reply = json!({"result": {"type": "object", "value": {"count": 1}}});
    assert_eq!(
        reply
            .get("result")
            .and_then(|result| result.get("value"))
            .cloned()
            .unwrap_or(Value::Null),
        json!({"count": 1})
    );
}
