//! e2e：`StreamPlaybackOptimizer` 接真實 headless shell（S1.7；需要 `CHROME_PATH`）。
//!
//! 舊版 e2e 以 `page.setContent` 打出播放器 mock 頁；Rust 層沒有 setContent，
//! 依計畫 §9÷§4.1 的規則改用測試內建的本機 HTTP server（任務規則：
//! `tokio::net::TcpListener` 手寫）提供同一份 HTML 於 `/`，`page.goto` 導覽後
//! 以正式入口 [`optimize_page_shared`] 驗證整合行為（對應舊版
//! `optimizer.optimize(page, 'test_channel')`）。
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::{Value, json};

use crate::browser::cdp::context::BrowserAdapter;
use crate::browser::cdp::launcher::BrowserLauncher;
use crate::browser::cdp::page::WaitUntil;
use crate::browser::stream_playback_optimizer::{
    PlaybackOptimizationResult, StreamPlaybackOptimizerConfig, optimize_page_shared,
};
use crate::browser::types::BrowserManagerLogger;
use crate::config::model::StreamQuality;
use crate::logging::redact::SanitizedValue;

/// 單條連線的處理防線（瀏覽器只會打幾個 GET，不許卡住任務）。
const CONNECTION_DEADLINE: Duration = Duration::from_secs(2);

/// accept 迴圈輪詢停止旗標的間隔。
const IDLE_TICK: Duration = Duration::from_millis(20);

/// 舊版 e2e 的 `page.setContent` 內容（逐字照抄 $OLD
/// `test/e2e/playback-optimizer.spec.ts` 的模板字串）。
const OPTIMIZER_MOCK_HTML: &str = r#"
    <video></video>
    <button data-a-target="player-settings-button">Settings</button>
    <button data-a-target="player-settings-menu-item-quality">Quality</button>
    <button role="menuitemradio">Picture-in-picture</button>
    <button data-a-target="player-settings-menu-item-quality-option">Auto</button>
    <button data-a-target="player-settings-menu-item-quality-option">360p</button>
    <script>
      document.querySelector('[role="menuitemradio"]').addEventListener(
        'click',
        () => {
          document.body.dataset.pictureInPictureClicked = 'true';
        },
      );
      for (const button of document.querySelectorAll(
        '[data-a-target="player-settings-menu-item-quality-option"]',
      )) {
        button.addEventListener('click', () => {
          document.body.dataset.selectedQuality = button.textContent.trim();
        });
      }
    </script>
  "#;

/// 收尾斷言用的頁內狀態（對應舊版四個 `expect`：video 的 JS property、body 的
/// dataset 屬性與 PiP 未點）。`querySelector('video')` 在 mock 頁必中。
const VIDEO_AND_BODY_STATE: &str = r#"() => {
  const video = document.querySelector('video');
  const body = document.body;
  return {
    muted: video.muted,
    disablePictureInPicture: video.disablePictureInPicture,
    selectedQuality: body.getAttribute('data-selected-quality'),
    pipClicked: body.getAttribute('data-picture-in-picture-clicked'),
  };
}"#;

/// e2e 的 no-op logger（對應舊測試注入的 `{ debug: () => undefined,
/// info: () => undefined }`；本測試只關心 optimize 的回傳與最終 DOM 狀態）。
struct NoopLogger;

impl BrowserManagerLogger for NoopLogger {
    fn debug(&self, _event: &str, _fields: Option<&SanitizedValue>) {}
    fn info(&self, _event: &str, _fields: Option<&SanitizedValue>) {}
    fn warn(&self, _event: &str, _fields: Option<&SanitizedValue>) {}
    fn error(&self, _event: &str, _fields: Option<&SanitizedValue>) {}
}

/// 啟動本機 HTML server：任一路徑回 mock 頁，其餘（favicon 等）回 404。
/// 回傳（port、停止旗標）；accept 迴圈以 [`IDLE_TICK`] 的 select 輪詢停止旗標。
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

// 原測試：播放器優化器會靜音並選擇最低可用畫質
#[tokio::test]
#[ignore = "需要 CHROME_PATH"]
async fn optimize_mutes_audio_and_picks_lowest_available_quality() {
    let (server_port, server_stop) = spawn_html_server(OPTIMIZER_MOCK_HTML.to_string()).await;

    // launch → 新 context → 新頁（照 probe 的流程；舊 e2e 的 page fixture
    // 為全新 context，不帶 storageState 與資源阻擋）。
    let launched = BrowserLauncher::new().launch().await.unwrap();
    let browser = BrowserAdapter::new(launched);
    let context = browser.new_context(None, (1280, 720)).await.unwrap();
    let page = context.new_page().await.unwrap();

    page.shared
        .goto(
            &format!("http://127.0.0.1:{server_port}/"),
            WaitUntil::DomContentLoaded,
        )
        .await
        .unwrap();

    // 舊版 optimizer：{ muteAudio: true, streamQuality: '160p' }；
    // mock 頁只有 Auto／360p 兩個可選畫質 → 選最低可用 360p。
    let result = optimize_page_shared(
        &page.shared,
        "test_channel",
        StreamPlaybackOptimizerConfig {
            mute_audio: true,
            stream_quality: StreamQuality::P160,
        },
        &NoopLogger,
    )
    .await;
    assert_eq!(
        result,
        PlaybackOptimizationResult {
            muted: true,
            selected_quality: Some(StreamQuality::P360),
        }
    );

    // 舊版的後續斷言：video.muted、video.disablePictureInPicture 為 true；
    // body 的 data-selected-quality="360p"；PiP 按鈕未被點擊。
    let state = page
        .shared
        .evaluate_function(VIDEO_AND_BODY_STATE, None)
        .await
        .unwrap();
    assert_eq!(state.get("muted"), Some(&Value::Bool(true)));
    assert_eq!(
        state.get("disablePictureInPicture"),
        Some(&Value::Bool(true))
    );
    assert_eq!(state.get("selectedQuality"), Some(&json!("360p")));
    assert_eq!(
        state.get("pipClicked"),
        Some(&Value::Null),
        "PiP 按鈕不該被點擊：{state}"
    );

    // 收尾（停 server → 頁 → context → browser）。
    server_stop.store(true, Ordering::SeqCst);
    page.shared.close().await.unwrap();
    context.close().await.unwrap();
    browser.close().await;
}
