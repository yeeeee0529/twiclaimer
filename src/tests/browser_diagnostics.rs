//! e2e：`PlaywrightBrowserPageAdapter` 診斷層接真實 headless shell（S1.6）。
//!
//! 舊版 e2e 以 `page.route` 攔截製造失敗請求與 500 回應；Rust 層尚未移植
//! route，改用本機 TCP 伺服器取代：`/failing-api` 回 500、`/blocked.js` 收到
//! 請求後直接關線（連線層失敗 → `requestfailed`），mock 頁以相對路徑觸發三種
//! 診斷事件。
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::browser::cdp::context::BrowserAdapter;
use crate::browser::cdp::launcher::BrowserLauncher;
use crate::browser::cdp::page::WaitUntil;
use crate::browser::page_adapter::{
    BrowserConsoleDiagnostic, BrowserHttpResponseDiagnostic, BrowserRequestFailureDiagnostic,
    DiagnosticEventSource, PlaywrightBrowserPageAdapter,
};

/// 診斷事件到齊的輪詢上限（本機伺服器，事件應即時到達）。
const POLL_DEADLINE: Duration = Duration::from_secs(5);

/// 輪詢直到條件成立或超時。
async fn eventually(timeout: Duration, check: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if check() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// 啟動本機診斷伺服器：回傳（port、停止旗標）。連線逐條開執行緒處理；
/// `/diag.html` 服務 mock 頁、`/failing-api` 回 `500 Boom`、`/blocked.js`
/// 收完請求後直接關線（`net::ERR_EMPTY_RESPONSE` → `requestfailed`）、
/// 其餘（favicon 等）回 404。
fn spawn_local_diagnostic_server() -> (u16, Arc<AtomicBool>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(true).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_for_thread = Arc::clone(&stop);
    std::thread::spawn(move || {
        while !stop_for_thread.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((stream, _)) => {
                    std::thread::spawn(move || handle_connection(stream));
                }
                Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
    });
    (port, stop)
}

/// 處理單條連線：讀完請求 headers 後依路徑回應。
fn handle_connection(mut stream: TcpStream) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let mut request = Vec::new();
    let mut buffer = [0u8; 4096];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            Err(_) => return,
        }
    }
    let request_text = String::from_utf8_lossy(&request);
    let path = request_text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or_default();
    if path.starts_with("/failing-api") {
        // 非成功回應（照舊 e2e 的 `route.fulfill({status: 500, statusText: 'Boom'})`）。
        let _ = stream.write_all(
            b"HTTP/1.1 500 Boom\r\nContent-Type: text/plain\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        return;
    }
    if path.starts_with("/blocked.js") {
        // 不回應直接關線：瀏覽器收到空回應 → 失敗請求
        // （對應舊 e2e 的 `route.abort('failed')`）。
        return;
    }
    if path.starts_with("/diag.html") {
        let body = std::fs::read("testdata/mock-pages/browser-diagnostics.html")
            .unwrap_or_else(|_| panic!("無法讀取 mock 頁 browser-diagnostics.html"));
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.write_all(&body);
        return;
    }
    let _ = stream
        .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
}

// 原測試：page adapter 將失敗請求、非成功回應與 console 轉為消毒後的診斷
#[tokio::test]
#[ignore = "需要 CHROME_PATH"]
async fn page_adapter_converts_failed_requests_non_success_responses_and_console_into_sanitized_diagnostics()
 {
    let (server_port, server_stop) = spawn_local_diagnostic_server();

    // launch → new_context → new_page（照 probe 的流程；舊 e2e 的 page fixture
    // 為全新 context，不帶 storageState 與資源阻擋）。
    let launched = BrowserLauncher::new().launch().await.unwrap();
    let browser = BrowserAdapter::new(launched);
    let result = run_diagnostics_flow(&browser, server_port).await;
    server_stop.store(true, Ordering::SeqCst);
    browser.close().await;
    assert!(result.is_ok(), "流程失敗：{:?}", result.unwrap_err());
}

/// e2e 主流程：註冊三個診斷 listener → 導覽到 mock 頁 → 等診斷到齊 → 斷言。
async fn run_diagnostics_flow(
    browser: &Arc<BrowserAdapter>,
    server_port: u16,
) -> Result<(), String> {
    let context = browser
        .new_context(None, (1280, 720))
        .await
        .map_err(|error| error.to_string())?;
    let page = context
        .new_page()
        .await
        .map_err(|error| error.to_string())?;

    // adapter 接到真實頁面的診斷事件泵。
    let adapter = PlaywrightBrowserPageAdapter::new(
        Arc::clone(&page.shared) as Arc<dyn DiagnosticEventSource>
    );
    let request_failures: Arc<std::sync::Mutex<Vec<BrowserRequestFailureDiagnostic>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let responses: Arc<std::sync::Mutex<Vec<BrowserHttpResponseDiagnostic>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let console_messages: Arc<std::sync::Mutex<Vec<BrowserConsoleDiagnostic>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let failures_sink = Arc::clone(&request_failures);
    let _cancel_request_failed = adapter.on_request_failed(move |diagnostic| {
        failures_sink.lock().unwrap().push(diagnostic.clone());
    });
    let responses_sink = Arc::clone(&responses);
    let _cancel_response = adapter.on_response(move |diagnostic| {
        responses_sink.lock().unwrap().push(diagnostic.clone());
    });
    let console_sink = Arc::clone(&console_messages);
    let _cancel_console = adapter.on_console(move |diagnostic| {
        console_sink.lock().unwrap().push(diagnostic.clone());
    });

    // 導覽到 mock 頁：console.error、注入被封鎖的 script、fetch 500（相對路徑）。
    page.shared
        .goto(
            &format!("http://127.0.0.1:{server_port}/diag.html"),
            WaitUntil::DomContentLoaded,
        )
        .await
        .map_err(|error| error.to_string())?;

    // 等三種診斷到齊（照舊 e2e 的 `expect.poll`）。
    let blocked_url = "127.0.0.1/blocked.js";
    let failing_url = "127.0.0.1/failing-api";
    assert!(
        eventually(POLL_DEADLINE, || request_failures
            .lock()
            .unwrap()
            .iter()
            .any(|diagnostic| diagnostic.url == blocked_url))
        .await,
        "應收到 blocked.js 的 requestfailed 診斷"
    );
    assert!(
        eventually(POLL_DEADLINE, || responses
            .lock()
            .unwrap()
            .iter()
            .any(|diagnostic| diagnostic.url == failing_url))
        .await,
        "應收到 failing-api 的 response 診斷"
    );
    assert!(
        eventually(POLL_DEADLINE, || console_messages
            .lock()
            .unwrap()
            .iter()
            .any(|diagnostic| diagnostic.r#type == "error"
                && diagnostic.text.starts_with("Twitch bootstrap error")))
        .await,
        "應收到 bootstrap error 的 console 診斷"
    );

    // 逐欄位斷言（照舊 e2e 的 `toMatchObject`）。本機伺服器多了文件本身的
    // response，故以 url 找 failing-api（舊版為 `responses[0]`）。
    let failures = request_failures.lock().unwrap().clone();
    let blocked = failures
        .iter()
        .find(|diagnostic| diagnostic.url == blocked_url)
        .unwrap_or_else(|| panic!("找不到 blocked.js 的診斷：{failures:?}"));
    assert_eq!(blocked.method, "GET");
    assert_eq!(blocked.resource_type, "script");

    let responses = responses.lock().unwrap().clone();
    let failing = responses
        .iter()
        .find(|diagnostic| diagnostic.url == failing_url)
        .unwrap_or_else(|| panic!("找不到 failing-api 的診斷：{responses:?}"));
    assert_eq!(failing.method, "GET");
    assert_eq!(failing.resource_type, "fetch");
    assert_eq!(failing.status, 500);

    let messages = console_messages.lock().unwrap().clone();
    let boot_error = messages
        .iter()
        .find(|diagnostic| diagnostic.text.starts_with("Twitch bootstrap error"))
        .unwrap_or_else(|| panic!("找不到 bootstrap error 的診斷：{messages:?}"));
    assert_eq!(boot_error.r#type, "error");

    // 消毒：全部診斷序列化後不得含 query 值與 token（照舊 e2e 的
    // `not.toContain`；以 Debug 形狀做內容檢查）。
    let serialized = format!("{failures:?}{responses:?}{messages:?}");
    assert!(!serialized.contains("token=secret"), "{serialized}");
    assert!(!serialized.contains("client_id=hidden"), "{serialized}");
    assert!(!serialized.contains("abc123"), "{serialized}");

    // 收尾照 probe：頁面關閉 → context 關閉（瀏覽器由呼叫端關）。
    page.shared
        .close()
        .await
        .map_err(|error| error.to_string())?;
    context.close().await.map_err(|error| error.to_string())?;
    Ok(())
}
