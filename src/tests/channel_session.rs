//! e2e（S3.4）：ChannelSession 的健康判定與側欄收合接真實 headless shell（需要 `CHROME_PATH`）。
//!
//! 舊版 e2e（`test/e2e/channel-session.spec.ts`）以 file:// 開
//! `testdata/mock-pages` 的六個 mock 頁，另兩個案例用 `page.setContent` 打出
//! 播放器 DOM；Rust 層沒有 setContent，照 §9／任務規則改用測試內建的本機
//! HTTP server 提供**逐字相同**的模板字串。健康判定以正式入口
//! [`evaluate_channel_health`]（對應 `evaluateChannelHealth(page, url)`）、側欄
//! 以既有 [`collapse_side_nav`] 入口驗證；locator 的屬性斷言沿用
//! [`Locator::get_attribute`] 的直接查詢（mock 頁無動畫，屬性會在點擊後同步更新）。
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::browser::cdp::context::BrowserAdapter;
use crate::browser::cdp::launcher::BrowserLauncher;
use crate::browser::cdp::locator::Locator;
use crate::browser::cdp::page::{PageShared, WaitUntil};
use crate::browser::health::{
    CdpHealthPage, ChannelHealthReason, ChannelHealthResult, evaluate_channel_health,
};
use crate::browser::side_nav::{SideNavCollapseResult, collapse_side_nav};
use crate::browser::types::{BrowserPage, BrowserPageAdapter, CdpPageAdapter};

/// 單條連線的處理防線（瀏覽器只會打幾個 GET，不許卡住任務）。
const CONNECTION_DEADLINE: Duration = Duration::from_secs(2);

/// accept 迴圈輪詢停止旗標的間隔。
const IDLE_TICK: Duration = Duration::from_millis(20);

/// mock 頁面的 file:// URL（與其他 e2e 相同的 helper 慣例）。
fn mock_page(name: &str) -> String {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("testdata/mock-pages");
    path.push(name);
    format!("file://{}", path.display())
}

/// 啟動 headless shell（每個測試一個瀏覽器；對應 Playwright 每個測試的
/// 瀏覽器程序）。
async fn launch_browser() -> Arc<BrowserAdapter> {
    let launched = BrowserLauncher::new().launch().await.unwrap();
    BrowserAdapter::new(launched)
}

/// 開一個全新 context 的頁面並導覽到目標 URL（對應舊 e2e 的 page fixture +
/// `page.goto`；回傳（正式碼頁面把手、cdp 把手））。
async fn open_page(
    browser: &BrowserAdapter,
    target: &str,
) -> (Arc<dyn BrowserPage>, Arc<PageShared>) {
    let context = browser.new_context(None, (1280, 720)).await.unwrap();
    let page = context.new_page().await.unwrap();
    page.shared
        .goto(target, WaitUntil::DomContentLoaded)
        .await
        .unwrap();
    let handle = CdpPageAdapter::new(Arc::clone(&page.shared)).page();
    (handle, Arc::clone(&page.shared))
}

/// 本機 HTTP server（舊 `page.setContent` 的替代；任一路徑回同一份 HTML，
/// 其餘回 404）。
async fn spawn_html_server(html: String) -> (u16, Arc<AtomicBool>) {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_for_task = Arc::clone(&stop);
    tokio::spawn(async move {
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else { break };
                    let html = html.clone();
                    tokio::spawn(async move {
                        let _ = tokio::time::timeout(
                            CONNECTION_DEADLINE,
                            handle_connection(stream, html),
                        )
                        .await;
                    });
                }
                _ = tokio::time::sleep(IDLE_TICK) => {
                    if stop_for_task.load(Ordering::SeqCst) {
                        break;
                    }
                }
            }
        }
    });
    (port, stop)
}

/// 讀完請求 headers（收到 `\r\n\r\n` 或斷線為止）。
async fn read_request_head(stream: &mut tokio::net::TcpStream) -> String {
    use tokio::io::AsyncReadExt;
    let mut request = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                request.extend_from_slice(&chunk[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
        }
    }
    String::from_utf8_lossy(&request).into_owned()
}

/// 處理單條連線：`/` 服務 mock 頁，其餘路徑（favicon 等）回 404。
async fn handle_connection(mut stream: tokio::net::TcpStream, html: String) {
    use tokio::io::AsyncWriteExt;
    let request = read_request_head(&mut stream).await;
    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or_default();
    if path != "/" {
        let _ = stream
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await;
        let _ = stream.shutdown().await;
        return;
    }
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        html.len(),
        html
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

/// 舊測試 `page.setContent` 的第一份 DOM（帶可見登入按鈕的播放器頁）。
const LOGIN_BUTTON_HTML: &str = r#"
      <button data-a-target="login-button">Log in</button>
      <div data-a-target="video-player">Live stream</div>
    "#;

/// 舊測試第二步 `page.setContent` 的 DOM（登入按鈕隱藏／`display:none`）。
const HIDDEN_LOGIN_BUTTON_HTML: &str = r#"
      <button data-a-target="login-button" style="display:none">Log in</button>
      <div data-a-target="video-player">Live stream</div>
    "#;

/// `evaluateChannelHealth(page, url)` 的 e2e 對應（cdp 把手 + 頁面 URL）。
async fn evaluate_health(page_shared: &Arc<PageShared>, url: &str) -> ChannelHealthResult {
    evaluate_channel_health(&CdpHealthPage::new(Arc::clone(page_shared)), url).await
}

// 原測試：展開的 Twitch 左側欄會自動收合
#[tokio::test]
#[ignore = "需要 CHROME_PATH"]
async fn expanded_side_nav_collapses_automatically() {
    let browser = launch_browser().await;
    let target = mock_page("side-nav.html");
    let (handle, page_shared) = open_page(&browser, &target).await;

    let first = collapse_side_nav(handle.as_ref()).await;
    assert_eq!(first, SideNavCollapseResult::Collapsed);
    // 舊 e2e 的 `toHaveAttribute('aria-expanded', 'false')`：mock 頁的 click
    // handler 同步改屬性，直接讀值。
    let toggle: Locator = page_shared.locator("[data-a-target=\"side-nav-arrow\"]");
    let expanded = toggle
        .get_attribute("aria-expanded", Some(1_000))
        .await
        .unwrap();
    assert_eq!(expanded.as_deref(), Some("false"));

    let second = collapse_side_nav(handle.as_ref()).await;
    assert_eq!(second, SideNavCollapseResult::AlreadyCollapsed);

    page_shared.close().await.unwrap();
    browser.close().await;
}

// 原測試：live 頁面判定 healthy
#[tokio::test]
#[ignore = "需要 CHROME_PATH"]
async fn live_page_is_healthy() {
    let browser = launch_browser().await;
    let target = mock_page("live.html");
    let (_handle, page_shared) = open_page(&browser, &target).await;

    let result = evaluate_health(&page_shared, &target).await;
    assert_eq!(result, ChannelHealthResult::healthy());

    page_shared.close().await.unwrap();
    browser.close().await;
}

// 原測試：content-warning 頁面會確認後判定 healthy
#[tokio::test]
#[ignore = "需要 CHROME_PATH"]
async fn content_warning_page_accepts_then_is_healthy() {
    let browser = launch_browser().await;
    let target = mock_page("content-warning.html");
    let (_handle, page_shared) = open_page(&browser, &target).await;

    let result = evaluate_health(&page_shared, &target).await;
    assert_eq!(result, ChannelHealthResult::healthy());
    // 舊 e2e 的 `expect(page.locator(...)).toBeVisible()`：點擊後播放器已解除隱藏。
    let player = page_shared.locator("[data-a-target=\"video-player\"]");
    let visible = player.is_visible().await.unwrap();
    assert!(visible, "video player should be visible after accepting");

    page_shared.close().await.unwrap();
    browser.close().await;
}

// 原測試：login-required 頁面回報 login_required
#[tokio::test]
#[ignore = "需要 CHROME_PATH"]
async fn login_required_page_reports_login_required() {
    let browser = launch_browser().await;
    let target = mock_page("login-required.html");
    let (_handle, page_shared) = open_page(&browser, &target).await;

    let result = evaluate_health(&page_shared, &target).await;
    assert_eq!(
        result,
        ChannelHealthResult::failed(ChannelHealthReason::LoginRequired)
    );

    page_shared.close().await.unwrap();
    browser.close().await;
}

// 原測試：一般登入按鈕回報 login_missing，明確登入阻擋才回報 login_required
#[tokio::test]
#[ignore = "需要 CHROME_PATH"]
async fn generic_login_button_reports_login_missing_but_gate_wins() {
    let (port, server_stop) = spawn_html_server(LOGIN_BUTTON_HTML.to_string()).await;
    let browser = launch_browser().await;
    let target = format!("http://127.0.0.1:{port}/");
    let (_handle, page_shared) = open_page(&browser, &target).await;

    let result = evaluate_health(&page_shared, &target).await;
    assert_eq!(
        result,
        ChannelHealthResult::failed(ChannelHealthReason::LoginMissing)
    );

    // 舊 e2e 的 `page.locator(...).evaluate(...)`：把 video-player 標成明確的
    // login-required gate。
    let player = page_shared.locator("[data-a-target=\"video-player\"]");
    player
        .evaluate(
            "element => element.setAttribute('data-test-selector', 'login-required')",
            None,
            None,
        )
        .await
        .unwrap();
    let result = evaluate_health(&page_shared, &target).await;
    assert_eq!(
        result,
        ChannelHealthResult::failed(ChannelHealthReason::LoginRequired)
    );

    page_shared.close().await.unwrap();
    browser.close().await;
    server_stop.store(true, Ordering::SeqCst);
}

// 原測試：隱藏登入按鈕不誤判，未登入也保留播放器錯誤與離線原因
#[tokio::test]
#[ignore = "需要 CHROME_PATH"]
async fn hidden_login_button_never_misdetects() {
    let (port, server_stop) = spawn_html_server(HIDDEN_LOGIN_BUTTON_HTML.to_string()).await;
    let browser = launch_browser().await;
    let target = format!("http://127.0.0.1:{port}/");
    let (_handle, page_shared) = open_page(&browser, &target).await;

    // display:none 的登入按鈕不會讓未登入判定蓋過 healthy（loggedOut 只認可見元素）。
    let result = evaluate_health(&page_shared, &target).await;
    assert_eq!(result, ChannelHealthResult::healthy());

    // 舊 e2e：移除 style 顯示按鈕，再把 div 標成 error → error_page；
    // 換成 offline → offline。
    let button = page_shared.locator("button");
    button
        .evaluate("element => element.removeAttribute('style')", None, None)
        .await
        .unwrap();
    let player = page_shared.locator("div");
    player
        .evaluate(
            "element => element.setAttribute('data-channel-health', 'error')",
            None,
            None,
        )
        .await
        .unwrap();
    let result = evaluate_health(&page_shared, &target).await;
    assert_eq!(
        result,
        ChannelHealthResult::failed(ChannelHealthReason::ErrorPage)
    );

    let player = page_shared.locator("div");
    player
        .evaluate(
            "element => element.setAttribute('data-channel-health', 'offline')",
            None,
            None,
        )
        .await
        .unwrap();
    let result = evaluate_health(&page_shared, &target).await;
    assert_eq!(
        result,
        ChannelHealthResult::failed(ChannelHealthReason::Offline)
    );

    page_shared.close().await.unwrap();
    browser.close().await;
    server_stop.store(true, Ordering::SeqCst);
}

// 原測試：error 頁面回報 error_page
#[tokio::test]
#[ignore = "需要 CHROME_PATH"]
async fn error_page_reports_error_page() {
    let browser = launch_browser().await;
    let target = mock_page("error.html");
    let (_handle, page_shared) = open_page(&browser, &target).await;

    let result = evaluate_health(&page_shared, &target).await;
    assert_eq!(
        result,
        ChannelHealthResult::failed(ChannelHealthReason::ErrorPage)
    );

    page_shared.close().await.unwrap();
    browser.close().await;
}

// 原測試：offline 頁面回報 offline
#[tokio::test]
#[ignore = "需要 CHROME_PATH"]
async fn offline_page_reports_offline() {
    let browser = launch_browser().await;
    let target = mock_page("offline.html");
    let (_handle, page_shared) = open_page(&browser, &target).await;

    let result = evaluate_health(&page_shared, &target).await;
    assert_eq!(
        result,
        ChannelHealthResult::failed(ChannelHealthReason::Offline)
    );

    page_shared.close().await.unwrap();
    browser.close().await;
}
