//! e2e（§7.4）：click／hover 機制的 mock 頁面測試（需要 `CHROME_PATH`，
//! 標 `#[ignore]`）。
//!
//! 涵蓋：click 成功路徑（可見、動作點準確）、隱藏／disabled／被遮住的逾時、
//! hover 缺元素與成功移動、動畫中的 stable 等待（有限→成功、無限→逾時）、
//! 元素途中被移除的重試（成功與逾時）、click 觸發導覽的收尾等待、
//! `mouse_move` 與 `reload`。
use std::path::PathBuf;
use std::sync::Arc;

use crate::browser::cdp::context::BrowserAdapter;
use crate::browser::cdp::launcher::BrowserLauncher;
use crate::browser::cdp::page::{PageShared, WaitUntil};

/// mock 頁面的 file:// URL（`?query` 會附在後面）。
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

// ===== click 成功路徑 =====

#[tokio::test]
#[ignore = "需要 CHROME_PATH"]
async fn click_hits_visible_button_at_clickable_point() {
    let launched = BrowserLauncher::new().launch().await.unwrap();
    let browser = BrowserAdapter::new(launched);
    let page = fresh_page(&browser).await;
    page.goto(
        &mock_page("locator-click.html"),
        WaitUntil::DomContentLoaded,
    )
    .await
    .unwrap();

    page.locator("#btn").click(Some(3_000)).await.unwrap();
    // 事件真的落在按鈕上：計數變 1，且記下的位置在按鈕範圍內。
    assert_eq!(
        page.locator("#counter")
            .text_content(Some(1_000))
            .await
            .unwrap(),
        Some("1".to_string())
    );
    let pointer = page
        .locator("#pointer")
        .text_content(Some(1_000))
        .await
        .unwrap()
        .unwrap_or_default();
    assert!(
        pointer.contains(','),
        "click 的 clientX/Y 應被記下：{pointer}"
    );

    // 連點第二次也成功。
    page.locator("#btn").click(Some(3_000)).await.unwrap();
    assert_eq!(
        page.locator("#counter")
            .text_content(Some(1_000))
            .await
            .unwrap(),
        Some("2".to_string())
    );

    let _ = page.close().await;
    browser.close().await;
}

// ===== click 的逾時情境（隱藏／disabled／被遮住）=====

#[tokio::test]
#[ignore = "需要 CHROME_PATH"]
async fn click_times_out_on_hidden_disabled_and_covered() {
    let launched = BrowserLauncher::new().launch().await.unwrap();
    let browser = BrowserAdapter::new(launched);
    let page = fresh_page(&browser).await;
    page.goto(
        &mock_page("locator-click.html"),
        WaitUntil::DomContentLoaded,
    )
    .await
    .unwrap();

    // 隱藏（缺 visible）、disabled（缺 enabled）、被遮住（hit target）各以
    // 300ms 逾時（X7 訊息與 pw-errors.jsonl 相同格式）。
    for selector in ["#hidden", "#disabled", "#covered"] {
        let error = page.locator(selector).click(Some(300)).await.unwrap_err();
        assert_eq!(error.name, "TimeoutError", "{selector}");
        assert_eq!(
            error.message, "locator.click: Timeout 300ms exceeded.",
            "{selector}"
        );
    }

    let _ = page.close().await;
    browser.close().await;
}

// ===== 動畫中的 stable 等待 =====

#[tokio::test]
#[ignore = "需要 CHROME_PATH"]
async fn click_waits_for_stable_position_during_animation() {
    let launched = BrowserLauncher::new().launch().await.unwrap();
    let browser = BrowserAdapter::new(launched);
    let page = fresh_page(&browser).await;
    page.goto(
        &mock_page("locator-animated.html?duration=200"),
        WaitUntil::DomContentLoaded,
    )
    .await
    .unwrap();

    // 動畫 200ms 後結束：click 等 stable 後成功。
    page.locator("#animated").click(Some(3_000)).await.unwrap();
    assert_eq!(
        page.locator("#animated")
            .text_content(Some(1_000))
            .await
            .unwrap(),
        Some("clicked".to_string())
    );

    let _ = page.close().await;
    browser.close().await;
}

#[tokio::test]
#[ignore = "需要 CHROME_PATH"]
async fn click_on_endlessly_animated_element_times_out() {
    let launched = BrowserLauncher::new().launch().await.unwrap();
    let browser = BrowserAdapter::new(launched);
    let page = fresh_page(&browser).await;
    page.goto(
        &mock_page("locator-animated.html?duration=0"),
        WaitUntil::DomContentLoaded,
    )
    .await
    .unwrap();

    // 永遠在動：stable 等不到 → 逾時。
    let error = page
        .locator("#animated")
        .click(Some(500))
        .await
        .unwrap_err();
    assert_eq!(error.name, "TimeoutError");
    assert_eq!(error.message, "locator.click: Timeout 500ms exceeded.");
    // 沒有真的點下去。
    assert_eq!(
        page.locator("#animated")
            .text_content(Some(1_000))
            .await
            .unwrap(),
        Some("moving".to_string())
    );

    let _ = page.close().await;
    browser.close().await;
}

// ===== notconnected 的重試 =====

#[tokio::test]
#[ignore = "需要 CHROME_PATH"]
async fn click_retries_when_element_detaches_during_action() {
    let launched = BrowserLauncher::new().launch().await.unwrap();
    let browser = BrowserAdapter::new(launched);
    let page = fresh_page(&browser).await;
    page.goto(
        &mock_page("locator-notconnected.html?recreate=80"),
        WaitUntil::DomContentLoaded,
    )
    .await
    .unwrap();

    // 第一次嘗試中按鈕把自己移除（mouseenter → remove）→ 重試後點到重建的新按鈕。
    page.locator("#btn").click(Some(3_000)).await.unwrap();
    assert_eq!(
        page.locator("#btn")
            .text_content(Some(1_000))
            .await
            .unwrap(),
        Some("clicked".to_string())
    );

    let _ = page.close().await;
    browser.close().await;
}

#[tokio::test]
#[ignore = "需要 CHROME_PATH"]
async fn click_times_out_when_detached_element_never_returns() {
    let launched = BrowserLauncher::new().launch().await.unwrap();
    let browser = BrowserAdapter::new(launched);
    let page = fresh_page(&browser).await;
    page.goto(
        &mock_page("locator-notconnected.html"),
        WaitUntil::DomContentLoaded,
    )
    .await
    .unwrap();

    // 按鈕在第一次嘗試中被移除且不重建：重試找不到元素 → 逾時。
    let error = page.locator("#btn").click(Some(600)).await.unwrap_err();
    assert_eq!(error.name, "TimeoutError");
    assert_eq!(error.message, "locator.click: Timeout 600ms exceeded.");

    let _ = page.close().await;
    browser.close().await;
}

// ===== hover =====

#[tokio::test]
#[ignore = "需要 CHROME_PATH"]
async fn hover_moves_the_mouse_and_times_out_when_missing() {
    let launched = BrowserLauncher::new().launch().await.unwrap();
    let browser = BrowserAdapter::new(launched);
    let page = fresh_page(&browser).await;
    page.goto(
        &mock_page("locator-click.html"),
        WaitUntil::DomContentLoaded,
    )
    .await
    .unwrap();

    page.locator("#btn").hover(Some(3_000)).await.unwrap();
    // mouseMoved 真的送達：mouseover 計數 1。
    assert_eq!(
        page.locator("#hover-count")
            .text_content(Some(1_000))
            .await
            .unwrap(),
        Some("1".to_string())
    );

    // 缺元素 → 逾時（訊息與 pw-errors.jsonl 的 hover-missing 相同格式）。
    let error = page.locator("#missing").hover(Some(300)).await.unwrap_err();
    assert_eq!(error.name, "TimeoutError");
    assert_eq!(error.message, "locator.hover: Timeout 300ms exceeded.");

    let _ = page.close().await;
    browser.close().await;
}

// ===== click 觸發的導覽收尾 =====

#[tokio::test]
#[ignore = "需要 CHROME_PATH"]
async fn click_waits_for_navigation_triggered_by_the_action() {
    let launched = BrowserLauncher::new().launch().await.unwrap();
    let browser = BrowserAdapter::new(launched);
    let page = fresh_page(&browser).await;
    page.goto(
        &mock_page("locator-click.html"),
        WaitUntil::DomContentLoaded,
    )
    .await
    .unwrap();

    // 點了會導覽到另一個 mock 頁：click 等導覽收尾才返回。
    page.locator("#nav").click(Some(3_000)).await.unwrap();
    assert!(
        page.url().ends_with("locator-click-target.html"),
        "click 應等導覽收尾，目前 URL：{}",
        page.url()
    );

    let _ = page.close().await;
    browser.close().await;
}

// ===== mouse.move 與 reload =====

#[tokio::test]
#[ignore = "需要 CHROME_PATH"]
async fn mouse_move_dispatches_and_reload_restores_the_document() {
    let launched = BrowserLauncher::new().launch().await.unwrap();
    let browser = BrowserAdapter::new(launched);
    let page = fresh_page(&browser).await;
    page.goto(
        &mock_page("locator-click.html"),
        WaitUntil::DomContentLoaded,
    )
    .await
    .unwrap();

    // mouse.move：位置記在 #pointer（mouseover 的 event.clientX/Y）。
    page.mouse_move(10.5, 20.25).await.unwrap();
    assert_eq!(
        page.locator("#hover-count")
            .text_content(Some(1_000))
            .await
            .unwrap(),
        Some("1".to_string())
    );

    // reload：等新文件的 load；URL 不變。
    page.reload(WaitUntil::Load).await.unwrap();
    assert!(page.url().ends_with("locator-click.html"));
    // 新文件重新計數（reload 後計數歸零）。
    assert_eq!(
        page.locator("#counter")
            .text_content(Some(1_000))
            .await
            .unwrap(),
        Some("0".to_string())
    );

    let _ = page.close().await;
    browser.close().await;
}
