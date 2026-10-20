//! e2e（§7.4）：locator 核心 API 的 mock 頁面測試（每個 API 至少一個情境；
//! 需要 `CHROME_PATH`，標 `#[ignore]`）。
//!
//! 涵蓋：`count`（可見／隱藏／缺失）、`isVisible`、`waitFor`（visible 出現、
//! hidden 消失）、`getAttribute`（有值／空字串／缺屬性）、`textContent`、
//! `evaluate`（元素＋參數）、`evaluateAll`、`dispatchEvent`（事件真的派送）。
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{Value, json};

use crate::browser::cdp::context::BrowserAdapter;
use crate::browser::cdp::launcher::BrowserLauncher;
use crate::browser::cdp::locator::WaitForState;
use crate::browser::cdp::page::{PageShared, WaitUntil};

/// mock 頁面的 file:// URL。
fn mock_page(name: &str) -> String {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("testdata/mock-pages");
    path.push(name);
    format!("file://{}", path.display())
}

/// 建一個 page（全新 context；viewport 固定錄製值）。
async fn fresh_page(browser: &BrowserAdapter) -> Arc<PageShared> {
    let context = browser.new_context(None, (1280, 720)).await.unwrap();
    let page = context.new_page().await.unwrap();
    Arc::clone(&page.shared)
}

// ===== count =====

#[tokio::test]
#[ignore = "需要 CHROME_PATH"]
async fn count_returns_matched_elements_without_waiting() {
    let launched = BrowserLauncher::new().launch().await.unwrap();
    let browser = BrowserAdapter::new(launched);
    let page = fresh_page(&browser).await;
    page.goto(
        &mock_page("locator-count.html"),
        WaitUntil::DomContentLoaded,
    )
    .await
    .unwrap();

    let locator = page.locator("button");
    assert_eq!(locator.count().await.unwrap(), 3, "三個可見按鈕");
    assert_eq!(
        page.locator("#missing").count().await.unwrap(),
        0,
        "沒有元素時回 0"
    );
    // 隱藏元素也算（count 不管可見性）。
    assert_eq!(page.locator("#hidden").count().await.unwrap(), 1);

    let _ = page.close().await;
    browser.close().await;
}

// ===== isVisible =====

#[tokio::test]
#[ignore = "需要 CHROME_PATH"]
async fn is_visible_reflects_visibility_without_waiting() {
    let launched = BrowserLauncher::new().launch().await.unwrap();
    let browser = BrowserAdapter::new(launched);
    let page = fresh_page(&browser).await;
    page.goto(
        &mock_page("locator-count.html"),
        WaitUntil::DomContentLoaded,
    )
    .await
    .unwrap();

    assert!(page.locator("button").first().is_visible().await.unwrap());
    assert!(
        !page.locator("#hidden").is_visible().await.unwrap(),
        "display:none 不可見"
    );
    assert!(
        !page.locator("#missing").is_visible().await.unwrap(),
        "沒有元素回 false"
    );

    let _ = page.close().await;
    browser.close().await;
}

// ===== waitFor =====

#[tokio::test]
#[ignore = "需要 CHROME_PATH"]
async fn wait_for_polls_until_state_is_satisfied() {
    let launched = BrowserLauncher::new().launch().await.unwrap();
    let browser = BrowserAdapter::new(launched);
    let page = fresh_page(&browser).await;
    page.goto(&mock_page("locator-wait.html"), WaitUntil::DomContentLoaded)
        .await
        .unwrap();

    // visible：元素 150ms 後出現。
    let later = page.locator("#later");
    later
        .wait_for(WaitForState::Visible, Some(2_000))
        .await
        .unwrap();
    assert!(later.is_visible().await.unwrap());

    // hidden：元素 150ms 後移除。
    let leaving = page.locator("#leaving");
    leaving
        .wait_for(WaitForState::Hidden, Some(2_000))
        .await
        .unwrap();
    assert_eq!(leaving.count().await.unwrap(), 0);

    // 已經是 hidden 的元素立即成功。
    page.locator("#gone")
        .wait_for(WaitForState::Hidden, Some(300))
        .await
        .unwrap();

    let _ = page.close().await;
    browser.close().await;
}

#[tokio::test]
#[ignore = "需要 CHROME_PATH"]
async fn wait_for_times_out_with_playwright_message() {
    let launched = BrowserLauncher::new().launch().await.unwrap();
    let browser = BrowserAdapter::new(launched);
    let page = fresh_page(&browser).await;
    page.goto(
        &mock_page("locator-count.html"),
        WaitUntil::DomContentLoaded,
    )
    .await
    .unwrap();

    let error = page
        .locator("#missing")
        .wait_for(WaitForState::Visible, Some(300))
        .await
        .unwrap_err();
    assert_eq!(error.name, "TimeoutError");
    assert_eq!(error.message, "locator.waitFor: Timeout 300ms exceeded.");

    let _ = page.close().await;
    browser.close().await;
}

// ===== getAttribute／textContent =====

#[tokio::test]
#[ignore = "需要 CHROME_PATH"]
async fn attribute_and_text_content_are_returned_verbatim() {
    let launched = BrowserLauncher::new().launch().await.unwrap();
    let browser = BrowserAdapter::new(launched);
    let page = fresh_page(&browser).await;
    page.goto(
        &mock_page("locator-attrs.html"),
        WaitUntil::DomContentLoaded,
    )
    .await
    .unwrap();

    let target = page.locator("#target");
    assert_eq!(
        target
            .get_attribute("data-role", Some(1_000))
            .await
            .unwrap(),
        Some("main".to_string())
    );
    // 空字串屬性：有值（空字串）而不是 None。
    assert_eq!(
        target
            .get_attribute("data-empty", Some(1_000))
            .await
            .unwrap(),
        Some(String::new())
    );
    // 缺屬性 → None。
    assert_eq!(
        target
            .get_attribute("data-absent", Some(1_000))
            .await
            .unwrap(),
        None
    );
    // textContent 含子節點文字。
    assert_eq!(
        target.text_content(Some(1_000)).await.unwrap(),
        Some("hellochild".to_string())
    );

    // 沒有元素 → 逾時訊息（frames.ts 的 waitForFunctionOnSelector）。
    let error = page
        .locator("#missing")
        .text_content(Some(300))
        .await
        .unwrap_err();
    assert_eq!(
        error.message,
        "locator.textContent: Timeout 300ms exceeded."
    );

    let _ = page.close().await;
    browser.close().await;
}

// ===== evaluate／evaluateAll =====

#[tokio::test]
#[ignore = "需要 CHROME_PATH"]
async fn evaluate_passes_element_and_arg_in_main_world() {
    let launched = BrowserLauncher::new().launch().await.unwrap();
    let browser = BrowserAdapter::new(launched);
    let page = fresh_page(&browser).await;
    page.goto(
        &mock_page("locator-evaluate.html"),
        WaitUntil::DomContentLoaded,
    )
    .await
    .unwrap();

    let target = page.locator("#target");
    // 元素第一參數 + arg。
    let result = target
        .evaluate(
            "(element, arg) => ({ id: element.id, x: Number(element.dataset.x), arg })",
            Some(json!({"n": 3})),
            Some(1_000),
        )
        .await
        .unwrap();
    eprintln!("DEBUG evaluate result = {result:?}");
    assert_eq!(result.get("id"), Some(&json!("target")));
    assert_eq!(result.get("x"), Some(&json!(7)));
    assert_eq!(result.get("arg"), Some(&json!({"n": 3})));

    // 頁面級 evaluate（fn, arg）。
    let sum = page
        .evaluate_function("(...args) => args.length", Some(json!(null)))
        .await
        .unwrap();
    assert_eq!(sum, json!(1));

    // evaluateAll：所有元素的陣列。
    let ids = page
        .locator("div")
        .evaluate_all("(elements) => elements.map((element) => element.id)")
        .await
        .unwrap();
    assert_eq!(ids, json!(["target"]));

    // evaluateAll 對沒有元素的情況回空陣列的長度（ok 語意）。
    let count = page
        .locator("#missing")
        .evaluate_all("(elements) => elements.length")
        .await
        .unwrap();
    assert_eq!(count, json!(0));

    // 元素級 evaluate 的逾時（frames.ts：等到 attached）。
    let error = page
        .locator("#missing")
        .evaluate("(element) => element.id", None, Some(300))
        .await
        .unwrap_err();
    assert_eq!(error.message, "locator.evaluate: Timeout 300ms exceeded.");

    // evaluate 內拋錯 → 例外描述第一行。
    let error = page
        .locator("#target")
        .evaluate("() => { throw new TypeError('boom'); }", None, Some(1_000))
        .await
        .unwrap_err();
    assert_eq!(error.name, "Error");
    assert_eq!(error.message, "locator.evaluate: TypeError: boom");

    let _ = page.close().await;
    browser.close().await;
}

// ===== dispatchEvent =====

#[tokio::test]
#[ignore = "需要 CHROME_PATH"]
async fn dispatch_event_reaches_page_handlers() {
    let launched = BrowserLauncher::new().launch().await.unwrap();
    let browser = BrowserAdapter::new(launched);
    let page = fresh_page(&browser).await;
    page.goto(
        &mock_page("locator-dispatch.html"),
        WaitUntil::DomContentLoaded,
    )
    .await
    .unwrap();

    let button = page.locator("#btn");
    button
        .dispatch_event("click", Value::Null, Some(1_000))
        .await
        .unwrap();
    // 事件真的派送：計數從 0 變 1。
    let counter = page.locator("#counter");
    assert_eq!(
        counter.text_content(Some(1_000)).await.unwrap(),
        Some("1".to_string())
    );

    // 再派一次（含 eventInit）。
    button
        .dispatch_event("click", json!({"detail": 5}), Some(1_000))
        .await
        .unwrap();
    assert_eq!(
        counter.text_content(Some(1_000)).await.unwrap(),
        Some("2".to_string())
    );

    // 沒有元素 → 逾時。
    let error = page
        .locator("#missing")
        .dispatch_event("click", Value::Null, Some(300))
        .await
        .unwrap_err();
    assert_eq!(
        error.message,
        "locator.dispatchEvent: Timeout 300ms exceeded."
    );

    let _ = page.close().await;
    browser.close().await;
}

// ===== strict 違規訊息 =====

#[tokio::test]
#[ignore = "需要 CHROME_PATH"]
async fn strict_violation_keeps_injected_first_line() {
    let launched = BrowserLauncher::new().launch().await.unwrap();
    let browser = BrowserAdapter::new(launched);
    let page = fresh_page(&browser).await;
    page.goto(
        &mock_page("locator-count.html"),
        WaitUntil::DomContentLoaded,
    )
    .await
    .unwrap();

    // isVisible 的 strict 違規（frames.ts：non-retriable → 上拋）。
    let error = page.locator("button").is_visible().await.unwrap_err();
    assert_eq!(error.name, "Error");
    assert_eq!(
        error.message,
        "locator.isVisible: Error: strict mode violation: locator('button') resolved to 3 elements:",
        "X7：多行訊息只保留第一行（數量由頁面內容決定）"
    );

    let _ = page.close().await;
    browser.close().await;
}
