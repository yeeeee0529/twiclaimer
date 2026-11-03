//! e2e：`ChannelPointsReader` 讀真實 community points DOM（S1.7；需要 `CHROME_PATH`）。
//!
//! 舊版以 file:// 開 `testdata/mock-pages` 的 `channel-points-available.html`／
//! `channel-points-unavailable.html`，再用 locator.evaluate 動過 DOM 後重讀。
//! Rust 端的 `read_channel_points_balance` 依移植契約回傳 `u64`，不含舊版
//! `displayValue`／`status`／`reason` 的包裝（模組開頭已註明），因此斷言對應：
//! available → `Ok(餘額)`、not_found → `Err(NotFound)`、parse_failed →
//! `Err(ParseFailed)`。
use std::path::Path;
use std::sync::Arc;

use crate::browser::cdp::context::BrowserAdapter;
use crate::browser::cdp::launcher::BrowserLauncher;
use crate::browser::cdp::page::{PageShared, WaitUntil};
use crate::browser::channel_points_reader::{BalanceError, read_channel_points_balance};
use crate::browser::types::{BrowserPageAdapter, CdpPageAdapter};

/// mock 頁的 file:// URL（`testdata/mock-pages` 內的檔名；同 multi_page 的 helper）。
fn mock_file_url(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("testdata/mock-pages")
        .join(name);
    url::Url::from_file_path(&path)
        .unwrap()
        .as_str()
        .to_string()
}

/// launch → 新 context → 新頁 → 導覽到 mock 頁。
/// 回傳（browser handle、頁面把手）；context 由 browser 收尾時一併關閉。
async fn open_mock_page(name: &str) -> (Arc<BrowserAdapter>, Arc<PageShared>) {
    let launched = BrowserLauncher::new().launch().await.unwrap();
    let browser = BrowserAdapter::new(launched);
    let context = browser.new_context(None, (1280, 720)).await.unwrap();
    let page = context.new_page().await.unwrap();
    page.shared
        .goto(&mock_file_url(name), WaitUntil::DomContentLoaded)
        .await
        .unwrap();
    (browser, Arc::clone(&page.shared))
}

// 原測試：從真實 community points DOM 讀取千分位餘額
#[tokio::test]
#[ignore = "需要 CHROME_PATH"]
async fn reads_thousands_balance_from_real_community_points_dom() {
    let (browser, page) = open_mock_page("channel-points-available.html").await;

    // 正式碼的頁面把手（BrowserPage trait）：manager 途徑的 CdpPage。
    let browser_page = CdpPageAdapter::new(Arc::clone(&page)).page();
    let balance = read_channel_points_balance(browser_page.as_ref()).await;

    assert_eq!(
        balance,
        Ok(12_345),
        "aria-label 的摘要與千分位餘額應解析成功"
    );

    page.close().await.unwrap();
    browser.close().await;
}

// 原測試：支援本地化縮寫
#[tokio::test]
#[ignore = "需要 CHROME_PATH"]
async fn supports_localized_abbreviations() {
    let (browser, page) = open_mock_page("channel-points-available.html").await;

    // 舊版的 locator.evaluate：把餘額換成『2,5萬』（逗號小數 + 萬縮寫）。
    page.locator("[data-test-selector=\"balance-string\"]")
        .evaluate(
            "(element) => { element.textContent = '2,5萬'; }",
            None,
            None,
        )
        .await
        .unwrap();

    let browser_page = CdpPageAdapter::new(Arc::clone(&page)).page();
    let balance = read_channel_points_balance(browser_page.as_ref()).await;

    assert_eq!(balance, Ok(25_000), "2,5萬 應解析為 25,000");

    page.close().await.unwrap();
    browser.close().await;
}

// 原測試：community points summary 不存在時回傳 not_found
#[tokio::test]
#[ignore = "需要 CHROME_PATH"]
async fn returns_not_found_when_community_points_summary_is_missing() {
    let (browser, page) = open_mock_page("channel-points-unavailable.html").await;

    let browser_page = CdpPageAdapter::new(Arc::clone(&page)).page();
    let balance = read_channel_points_balance(browser_page.as_ref()).await;

    // 舊版：{ status: 'unavailable', reason: 'not_found' }。
    assert_eq!(
        balance,
        Err(BalanceError::NotFound),
        "沒有摘要元素時應回 not_found"
    );

    page.close().await.unwrap();
    browser.close().await;
}

// 原測試：summary 存在但無數值時回傳 parse_failed
#[tokio::test]
#[ignore = "需要 CHROME_PATH"]
async fn returns_parse_failed_when_summary_has_no_numeric_value() {
    let (browser, page) = open_mock_page("channel-points-available.html").await;

    // 舊版的 locator.evaluate：摘要拿掉 aria-label，並把內文換成
    // 沒有數字的文字（textContent 覆寫會一起移除內部的 balance-string span）。
    page
        .locator("[data-test-selector=\"community-points-summary\"]")
        .evaluate(
            "(element) => { element.removeAttribute('aria-label'); element.textContent = 'Channel Points unavailable'; }",
            None,
            None,
        )
        .await
        .unwrap();

    let browser_page = CdpPageAdapter::new(Arc::clone(&page)).page();
    let balance = read_channel_points_balance(browser_page.as_ref()).await;

    // 舊版：{ status: 'unavailable', reason: 'parse_failed' }。
    assert_eq!(
        balance,
        Err(BalanceError::ParseFailed),
        "摘要存在但無數值時應回 parse_failed"
    );

    page.close().await.unwrap();
    browser.close().await;
}
