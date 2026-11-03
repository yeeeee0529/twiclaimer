//! e2e：聊天室封鎖 init script 套在 context 的真實頁面（S1.7；需要 `CHROME_PATH`）。
//!
//! 舊版以 `context.addInitScript(CHAT_WEBSOCKET_BLOCK_SCRIPT)` 注入後導覽
//! about:blank 驗證 WebSocket 換成靜默 fake socket；Rust 端對應
//! `context.configure_chat_blocking(true)`（§7.3.6；腳本內容為 DEC-004 的
//! `chat-block-init.js`，包裝方式同 Playwright 的 InitScript）。
//! 兩個 evaluate 函式逐字照抄 $OLD `test/e2e/chat-blocking.spec.ts`
//! （僅移除 TS 的型別註記；promise 以 `awaitPromise` 語意解開）。
use serde_json::{Value, json};

use crate::browser::cdp::context::BrowserAdapter;
use crate::browser::cdp::launcher::BrowserLauncher;
use crate::browser::cdp::page::WaitUntil;
use crate::browser::chat_disabler::CHAT_WEBSOCKET_HOSTNAME;

/// 舊版第一個 evaluate 的函式（`page.evaluate(fn, CHAT_WEBSOCKET_HOSTNAME)`）。
const CHAT_SOCKET_PROBE: &str = r#"(host) => {
  const socket = new WebSocket(`wss://${host}/`);
  return new Promise((resolve) => {
    socket.onopen = () => {
      socket.send('PING');
      socket.close();
      resolve({ opened: true, readyState: socket.readyState });
    };
    setTimeout(() => resolve({ opened: false, readyState: -1 }), 2000);
  });
}"#;

/// 舊版第二個 evaluate 的函式（零參數；URL 寫死在函式內）。
const NON_CHAT_SOCKET_PROBE: &str = r#"() => {
  const socket = new WebSocket('wss://example.invalid/');
  return socket instanceof WebSocket && socket.url === 'wss://example.invalid/';
}"#;

// 原測試：chat WebSocket is replaced by a silent fake socket
#[tokio::test]
#[ignore = "需要 CHROME_PATH"]
async fn chat_websocket_is_replaced_by_a_silent_fake_socket() {
    let launched = BrowserLauncher::new().launch().await.unwrap();
    let browser = BrowserAdapter::new(launched);
    let context = browser.new_context(None, (1280, 720)).await.unwrap();
    context.configure_chat_blocking(true).await.unwrap();
    let page = context.new_page().await.unwrap();
    page.shared
        .goto("about:blank", WaitUntil::DomContentLoaded)
        .await
        .unwrap();

    let result = page
        .shared
        .evaluate_function(CHAT_SOCKET_PROBE, Some(json!(CHAT_WEBSOCKET_HOSTNAME)))
        .await
        .unwrap();
    assert_eq!(
        result.get("opened").and_then(Value::as_bool),
        Some(true),
        "fake chat socket 應在 ~2 秒內開啟：{result}"
    );

    page.shared.close().await.unwrap();
    context.close().await.unwrap();
    browser.close().await;
}

// 原測試：non-chat WebSocket URLs use the real constructor
#[tokio::test]
#[ignore = "需要 CHROME_PATH"]
async fn non_chat_websocket_urls_use_the_real_constructor() {
    let launched = BrowserLauncher::new().launch().await.unwrap();
    let browser = BrowserAdapter::new(launched);
    let context = browser.new_context(None, (1280, 720)).await.unwrap();
    context.configure_chat_blocking(true).await.unwrap();
    let page = context.new_page().await.unwrap();
    page.shared
        .goto("about:blank", WaitUntil::DomContentLoaded)
        .await
        .unwrap();

    let is_patched_for_other_host = page
        .shared
        .evaluate_function(NON_CHAT_SOCKET_PROBE, None)
        .await
        .unwrap();
    assert_eq!(
        is_patched_for_other_host,
        Value::Bool(true),
        "非聊天室 host 應走真正的 WebSocket：{is_patched_for_other_host}"
    );

    page.shared.close().await.unwrap();
    context.close().await.unwrap();
    browser.close().await;
}
