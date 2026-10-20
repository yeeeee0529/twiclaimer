//! 錯誤對照（X7、G5）的 e2e：重放 `doc/spec/pw-errors.jsonl` 的 33 案例，
//! 共用一個瀏覽器實例，比對 `name` 與 `message`（第一行；`127.0.0.1:<port>`
//! 正規化；`ok: true` 的案例必須成功）。
//!
//! 伺服器照 `tools/ref/pw-errors.mjs`：`/` 回四按鈕樣本頁、`/hang` 永不回應、
//! `/once` 只回應第一次；另有立刻關閉的 listener 當作連線被拒的 port。
//! 導覽類案例各用一個新頁面（照 `fresh()`，避免前一個逾時的導覽干擾）。
use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

use crate::browser::cdp::context::{BrowserAdapter, DriverError};
use crate::browser::cdp::launcher::BrowserLauncher;
use crate::browser::cdp::locator::WaitForState;
use crate::browser::cdp::page::{PageShared, WaitUntil};

/// 一般案例的逾時（pw-errors.mjs 的 `T = { timeout: 300 }`）。
const T: Option<u64> = Some(300);

/// 樣本頁（pw-errors.mjs 的 HTML；四個按鈕＋隱藏 div＋全頁蓋層）。
const SAMPLE_HTML: &str = r#"<!doctype html><button>a</button><button>b</button>
<div id="hidden" style="display:none">x</div><button id="disabled" disabled>d</button>
<button id="covered">c</button><div style="position:fixed;inset:0;background:#fff"></div>"#;

/// 迷你 HTTP server：每個連線一個 thread，單請求後 `connection: close`。
fn spawn_errors_server() -> (u16, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let once = Arc::new(AtomicBool::new(false));
    let handle = thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let once = Arc::clone(&once);
            thread::spawn(move || {
                let mut buffer = [0u8; 4096];
                let mut request = String::new();
                loop {
                    match stream.read(&mut buffer) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => {
                            request.push_str(&String::from_utf8_lossy(&buffer[..n]));
                            if request.contains("\r\n\r\n") {
                                break;
                            }
                        }
                    }
                }
                let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
                // /hang 永不回應；/once 只回應第一次（之後同 hang）。
                let hang =
                    path == "/hang" || (path == "/once" && once.swap(true, Ordering::SeqCst));
                if hang {
                    loop {
                        match stream.read(&mut buffer) {
                            Ok(0) | Err(_) => return,
                            Ok(_) => {}
                        }
                    }
                }
                let body = SAMPLE_HTML;
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            });
        }
    });
    (port, handle)
}

/// 連線會被拒的 port：bind 後立刻關閉。
fn refused_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

/// 訊息正規化：把動態 port（`127.0.0.1:<port>`）全部遮蔽。
fn normalize(message: &str) -> String {
    static PATTERN: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let pattern = PATTERN.get_or_init(|| {
        regex::Regex::new(r"127\.0\.0\.1:\d+")
            .unwrap_or_else(|_| panic!("invalid built-in port normalization pattern"))
    });
    pattern
        .replace_all(message, "127.0.0.1:<port>")
        .into_owned()
}

/// 斷言案例失敗且 name／message 與規格相同（port 正規化後）。
fn expect_error(
    case: &str,
    result: Result<impl std::fmt::Debug, DriverError>,
    expected_name: &str,
    expected_message: &str,
) {
    match result {
        Ok(value) => panic!("{case}: 應該失敗，實際成功：{value:?}"),
        Err(error) => {
            assert_eq!(error.name, expected_name, "{case} 的 name");
            assert_eq!(
                normalize(&error.message),
                expected_message,
                "{case} 的 message"
            );
        }
    }
}

/// 斷言案例成功（`ok: true`）。
fn expect_ok(case: &str, result: Result<impl std::fmt::Debug, DriverError>) {
    result.unwrap_or_else(|error| panic!("{case}: 應該成功，實際失敗：{}", error.message));
}

/// 建一個新頁（照 pw-errors.mjs 的 fresh；viewport 固定錄製值）。
async fn fresh_page(browser: &BrowserAdapter) -> Arc<PageShared> {
    let context = browser.new_context(None, (1280, 720)).await.unwrap();
    let page = context.new_page().await.unwrap();
    Arc::clone(&page.shared)
}

#[tokio::test]
#[ignore = "需要 CHROME_PATH"]
async fn error_messages_match_the_recorded_playwright_spec() {
    let (port, _server) = spawn_errors_server();
    let base = format!("http://127.0.0.1:{port}");

    let launched = BrowserLauncher::new().launch().await.unwrap();
    let browser = BrowserAdapter::new(launched);
    let context = browser.new_context(None, (1280, 720)).await.unwrap();

    // ===== 導覽類案例（各用一個新頁）=====

    // goto-timeout：`{timeout: 300}` 以頁面預設值等效重放（新頁專用）。
    let scratch = fresh_page(&browser).await;
    scratch.set_default_navigation_timeout(300);
    let result = scratch
        .goto(&format!("{base}/hang"), WaitUntil::DomContentLoaded)
        .await;
    expect_error(
        "goto-timeout",
        result,
        "TimeoutError",
        "page.goto: Timeout 300ms exceeded.",
    );
    let _ = scratch.close().await;

    // goto-default-navigation-timeout：setDefaultNavigationTimeout(400)。
    let scratch = fresh_page(&browser).await;
    scratch.set_default_navigation_timeout(400);
    let result = scratch
        .goto(&format!("{base}/hang"), WaitUntil::DomContentLoaded)
        .await;
    expect_error(
        "goto-default-navigation-timeout",
        result,
        "TimeoutError",
        "page.goto: Timeout 400ms exceeded.",
    );
    let _ = scratch.close().await;

    // goto-refused：對沒有 listener 的 port 導覽。
    let scratch = fresh_page(&browser).await;
    let refused = refused_port();
    let result = scratch
        .goto(
            &format!("http://127.0.0.1:{refused}/"),
            WaitUntil::DomContentLoaded,
        )
        .await;
    expect_error(
        "goto-refused",
        result,
        "Error",
        "page.goto: net::ERR_CONNECTION_REFUSED at http://127.0.0.1:<port>/",
    );
    let _ = scratch.close().await;

    // goto-abort-signal：100ms 後中止（AbortController.signal）。
    let scratch = fresh_page(&browser).await;
    let abort = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let abort = Arc::clone(&abort);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            abort.store(true, Ordering::SeqCst);
        });
    }
    let result = scratch
        .goto_abortable(
            &format!("{base}/hang"),
            WaitUntil::DomContentLoaded,
            Some(abort),
        )
        .await;
    expect_error(
        "goto-abort-signal",
        result,
        "AbortError",
        "page.goto: This operation was aborted",
    );
    let _ = scratch.close().await;

    // reload-timeout：先到 /once（只回應第一次），reload 之後掛住。
    // （pw-errors 的 reload(T) 以頁面預設值 300 等效重放。）
    let scratch = fresh_page(&browser).await;
    scratch.set_default_navigation_timeout(300);
    scratch
        .goto(&format!("{base}/once"), WaitUntil::DomContentLoaded)
        .await
        .unwrap();
    let result = scratch
        .reload_abortable(WaitUntil::DomContentLoaded, None)
        .await;
    expect_error(
        "reload-timeout",
        result,
        "TimeoutError",
        "page.reload: Timeout 300ms exceeded.",
    );
    let _ = scratch.close().await;

    // ===== 定位器與 evaluate 案例（共用頁）=====

    let page = context.new_page().await.unwrap();
    let page = Arc::clone(&page.shared);
    page.goto(&format!("{base}/"), WaitUntil::DomContentLoaded)
        .await
        .unwrap();

    let result = page.locator("button").click(T).await;
    expect_error(
        "click-strict",
        result,
        "Error",
        "locator.click: Error: strict mode violation: locator('button') resolved to 4 elements:",
    );
    for case in ["click-hidden", "click-disabled", "click-covered"] {
        let selector = match case {
            "click-hidden" => "#hidden",
            "click-disabled" => "#disabled",
            "click-covered" => "#covered",
            _ => unreachable!("unknown click case"),
        };
        let result = page.locator(selector).click(T).await;
        expect_error(
            case,
            result,
            "TimeoutError",
            "locator.click: Timeout 300ms exceeded.",
        );
    }

    let result = page.locator("#missing").hover(T).await;
    expect_error(
        "hover-missing",
        result,
        "TimeoutError",
        "locator.hover: Timeout 300ms exceeded.",
    );

    let result = page
        .locator("#missing")
        .wait_for(WaitForState::Visible, T)
        .await;
    expect_error(
        "waitfor-missing",
        result,
        "TimeoutError",
        "locator.waitFor: Timeout 300ms exceeded.",
    );

    let result = page
        .locator("#hidden")
        .wait_for(WaitForState::Visible, T)
        .await;
    expect_error(
        "waitfor-hidden-visible",
        result,
        "TimeoutError",
        "locator.waitFor: Timeout 300ms exceeded.",
    );

    let result = page.locator("#missing").text_content(T).await;
    expect_error(
        "textcontent-missing",
        result,
        "TimeoutError",
        "locator.textContent: Timeout 300ms exceeded.",
    );

    let result = page.locator("button").get_attribute("id", T).await;
    expect_error(
        "getattribute-strict",
        result,
        "Error",
        "locator.getAttribute: Error: strict mode violation: locator('button') resolved to 4 elements:",
    );

    // isvisible-strict：strict 違規不可重試（第一行）。
    let result = page.locator("button").is_visible().await;
    expect_error(
        "isvisible-strict",
        result,
        "Error",
        "locator.isVisible: Error: strict mode violation: locator('button') resolved to 4 elements:",
    );

    // isvisible-missing／count-missing／evaluateall-missing：ok（沒有拋錯）。
    let visible = page.locator("#missing").is_visible().await;
    expect_ok(
        "isvisible-missing",
        visible.map(|value| value.then_some(())),
    );
    let count = page.locator("#missing").count().await;
    expect_ok(
        "count-missing",
        count.map(|value| (value == 0).then_some(())),
    );
    let all = page
        .locator("#missing")
        .evaluate_all("(elements) => elements.length")
        .await;
    expect_ok(
        "evaluateall-missing",
        all.map(|value| (value == json!(0)).then_some(())),
    );

    // locator-evaluate-missing：等到元素存在 → 逾時。
    let result = page
        .locator("#missing")
        .evaluate("(element) => element.id", None, T)
        .await;
    expect_error(
        "locator-evaluate-missing",
        result,
        "TimeoutError",
        "locator.evaluate: Timeout 300ms exceeded.",
    );

    let result = page
        .locator("#missing")
        .dispatch_event("click", Value::Null, T)
        .await;
    expect_error(
        "dispatch-missing",
        result,
        "TimeoutError",
        "locator.dispatchEvent: Timeout 300ms exceeded.",
    );

    // evaluate-throw／evaluate-reject：JS 例外的第一行。
    let result = page
        .evaluate_function("() => { throw new Error('boom'); }", None)
        .await;
    expect_error(
        "evaluate-throw",
        result,
        "Error",
        "page.evaluate: Error: boom",
    );
    let result = page
        .evaluate_function("() => Promise.reject(new TypeError('nope'))", None)
        .await;
    expect_error(
        "evaluate-reject",
        result,
        "Error",
        "page.evaluate: TypeError: nope",
    );

    let result = page.locator("#missing").get_attribute("id", T).await;
    expect_error(
        "getattribute-missing",
        result,
        "TimeoutError",
        "locator.getAttribute: Timeout 300ms exceeded.",
    );

    // evaluate-navigation：evaluate 進行中頁面導覽 → context 被摧毀。
    let result = page
        .evaluate_function(
            "() => { location.href = '/'; return new Promise(() => {}); }",
            None,
        )
        .await;
    expect_error(
        "evaluate-navigation",
        result,
        "Error",
        "page.evaluate: Execution context was destroyed, most likely because of a navigation.",
    );

    // ===== crash 案例（新頁）=====

    let crashing = context.new_page().await.unwrap();
    let crashing = Arc::clone(&crashing.shared);
    let result = crashing
        .goto("chrome://crash", WaitUntil::DomContentLoaded)
        .await;
    expect_error(
        "goto-crash",
        result,
        "Error",
        "page.goto: net::ERR_ABORTED at chrome://crash",
    );
    // after-crash-evaluate：指令送不進 crash 的 session（pending 在 targetCrashed
    // 事件時被拒絕；訊息結尾有一個空白）。
    let result = crashing.evaluate_function("() => 1", None).await;
    expect_error(
        "after-crash-evaluate",
        result,
        "Error",
        "page.evaluate: Target crashed ",
    );
    // after-crash-close：closeTarget 對 crash 的 target 也要能完成。
    let result = crashing.close().await;
    expect_ok("after-crash-close", result.map(|_| ()));

    // ===== 關閉後的案例 =====

    let result = page.close().await;
    expect_ok("page-close", result.map(|_| ()));

    let result = page
        .goto(&format!("{base}/"), WaitUntil::DomContentLoaded)
        .await;
    expect_error(
        "closed-goto",
        result,
        "Error",
        "page.goto: Target page, context or browser has been closed",
    );
    let result = page.locator("button").first().click(T).await;
    expect_error(
        "closed-click",
        result,
        "Error",
        "locator.click: Target page, context or browser has been closed",
    );
    let result = page.evaluate_function("() => 1", None).await;
    expect_error(
        "closed-evaluate",
        result,
        "Error",
        "page.evaluate: Target page, context or browser has been closed",
    );
    let result = page.screenshot().await;
    expect_error(
        "closed-screenshot",
        result,
        "Error",
        "page.screenshot: Target page, context or browser has been closed",
    );
    let result = page.locator("button").first().is_visible().await;
    expect_error(
        "closed-isvisible",
        result,
        "Error",
        "locator.isVisible: Target page, context or browser has been closed",
    );

    // ===== 瀏覽器關閉後 =====

    browser.close().await;
    let result = context.new_page().await.map(|_| ());
    expect_error(
        "browser-closed-newpage",
        result,
        "Error",
        "browserContext.newPage: Target page, context or browser has been closed",
    );
}
