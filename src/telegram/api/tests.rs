//! `TelegramApiClient` 的測試：移植舊版 `test/unit/telegram-api.test.ts`
//! （6 個 `it`／`it.each` 標題；標題與 `doc/spec/ts-test-titles.tsv` 逐字相同）。
//!
//! 舊測試直接注入 fetch mock；本移植的假 HTTP 依計畫改為本機 ephemeral
//! server（只在本機收發，不連真實 Telegram）：伺服器收齊每個請求
//! （request line、headers、依 Content-Length 的 body）後按腳本回覆，測試以
//! 錄到的原始請求逐欄位斷言 URL／方法／headers／body。
//!
//! 舊測試以 `vi.spyOn(AbortSignal, 'timeout')` 斷言 `timeoutSeconds * 1000 +
//! SEND_TIMEOUT_MS` 的合成 deadline；reqwest 架構下這是內部實作，移植測試以
//! 兩個 hang 階段（request／body）的逾時分類結果（`timeout`）呈現（hang 由
//! `#[tokio::test(start_paused = true)]` 的虛擬時間結算，不等真實 35 秒）。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::{
    DEFAULT_API_BASE, TelegramApi, TelegramApiClient, TelegramApiClientOptions, TelegramApiError,
    TelegramBotCommand, TelegramErrorCategory, TelegramReplyKeyboardMarkup,
    TelegramSendMessageOptions, TelegramUpdate,
};
use tokio_util::sync::CancellationToken;

/// 舊測試的 `botToken`（測試 1）。
const TEST_BOT_TOKEN: &str = "test-bot-token";

/// 伺服器錄到的一個 HTTP 請求（on-the-wire 的原樣）。
struct RecordedRequest {
    method: String,
    path: String,
    /// headers 的原樣（name: value 逐行，保留順序與大小寫）。
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

/// 本機測試伺服器：先後回應 `responses` 的 `(status, body)` JSON，並把每個
/// 請求錄在 `recorded`（從任何執行緒也可取；一個連線服務一個請求）。
fn spawn_json_server(
    responses: &[(u16, &'static str)],
    recorded: Arc<Mutex<Vec<RecordedRequest>>>,
) -> u16 {
    let responses = Arc::new(Mutex::new(responses.to_vec()));
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let port = listener.local_addr().expect("local addr").port();
    std::thread::spawn(move || {
        while let Some(Ok(stream)) = listener.incoming().next() {
            let mut stream = stream;
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            let recorded = Arc::clone(&recorded);
            let responses = Arc::clone(&responses);
            // 一個連線只服務一個請求（回應帶 Connection: close）。
            std::thread::spawn(move || {
                let Some(request) = read_http_request(&mut stream) else {
                    return;
                };
                let script = {
                    let mut queue = responses.lock().expect("responses lock");
                    if queue.is_empty() {
                        None
                    } else {
                        Some(queue.remove(0))
                    }
                };
                recorded.lock().expect("recorded lock").push(request);
                let (status, body) = script.unwrap_or((200, "{}"));
                let reason = if status == 200 { "OK" } else { "Bad Gateway" };
                let head = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(body.as_bytes());
                let _ = stream.flush();
            });
        }
    });
    port
}

/// 接受後立即關線的伺服器（連線層失敗，no response；對應舊測試被 reject 的
/// fetch mock）。
fn spawn_closing_server() -> u16 {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let port = listener.local_addr().expect("local addr").port();
    std::thread::spawn(move || {
        while let Some(Ok(stream)) = listener.incoming().next() {
            let mut stream = stream;
            std::thread::spawn(move || {
                // 讀一點再關線（等不到完整請求）。
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let mut buffer = [0u8; 64];
                let _ = stream.read(&mut buffer);
                drop(stream);
            });
        }
    });
    port
}

/// 讀取單一 HTTP/1.1 請求後永不回應的伺服器（request 階段 hang）。
fn spawn_hanging_request_server() -> u16 {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let port = listener.local_addr().expect("local addr").port();
    std::thread::spawn(move || {
        while let Some(Ok(stream)) = listener.incoming().next() {
            let mut stream = stream;
            std::thread::spawn(move || {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(120)));
                let mut chunk = [0u8; 64];
                // 只讀一點（表示已收到請求頭）後就不回應，等 client 逾時斷線。
                let _ = stream.read(&mut chunk);
                loop {
                    std::thread::sleep(Duration::from_millis(100));
                }
            });
        }
    });
    port
}

/// 先送回 head（Content-Length 32，application/json）再不送 body 的伺服器
/// （body 階段 hang）。
fn spawn_hanging_body_server() -> u16 {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let port = listener.local_addr().expect("local addr").port();
    std::thread::spawn(move || {
        while let Some(Ok(stream)) = listener.incoming().next() {
            let mut stream = stream;
            std::thread::spawn(move || {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(120)));
                let mut chunk = [0u8; 64];
                let _ = stream.read(&mut chunk);
                let head = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 32\r\n\r\n";
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.flush();
                loop {
                    // body 永遠不來，等 client 逾時斷線。
                    std::thread::sleep(Duration::from_millis(100));
                }
            });
        }
    });
    port
}

/// 讀取單一 HTTP/1.1 請求（request line、headers、依 Content-Length 的 body）。
fn read_http_request(stream: &mut TcpStream) -> Option<RecordedRequest> {
    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(index) = find_double_crlf(&buffer) {
            break index;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let mut lines = head.lines();
    let request_line = lines.next()?;
    let mut request_line = request_line.split_whitespace();
    let method = request_line.next()?.to_string();
    let path = request_line.next()?.to_string();
    let mut headers = Vec::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_string(), value.trim().to_string()));
        }
    }
    let content_length = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = buffer[head_end + 4..].to_vec();
    while body.len() < content_length {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    Some(RecordedRequest {
        method,
        path,
        headers,
        body,
    })
}

fn find_double_crlf(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

/// 取出 Mutex 內容的長度（等待條件用）。
fn len<T>(lock: &Mutex<Vec<T>>) -> usize {
    lock.lock().expect("lock").len()
}

fn header_value(request: &RecordedRequest, name: &str) -> Option<String> {
    request
        .headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.clone())
}

/// 以 `base` 供本機 server URL 組建的 client（舊測試注入的 fetch mock 在
/// 本移植以 base_url 接縫呈現）。
fn client_with_base(port: u16, bot_token: &str) -> TelegramApiClient {
    TelegramApiClient::new(TelegramApiClientOptions {
        bot_token: bot_token.to_string(),
        base_url: Some(format!("http://127.0.0.1:{port}")),
    })
    .expect("client 應建立")
}

// 原測試：以 POST JSON 呼叫 getUpdates、setMyCommands 與 sendMessage
#[tokio::test]
async fn calls_get_updates_set_my_commands_and_send_message_with_post_json() {
    let recorded: Arc<Mutex<Vec<RecordedRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let port = spawn_json_server(
        &[
            // getUpdates
            (200, r#"{"ok":true,"result":[{"update_id":7}]}"#),
            // setMyCommands
            (200, r#"{"ok":true,"result":{"message_id":9}}"#),
            // sendMessage
            (200, r#"{"ok":true,"result":{"message_id":10}}"#),
            // sendPhoto
            (200, r#"{"ok":true,"result":true}"#),
        ],
        Arc::clone(&recorded),
    );
    let client = client_with_base(port, TEST_BOT_TOKEN);
    let cancel = CancellationToken::new();

    let updates = client
        .get_updates(Some(5.0), 25.0, &cancel)
        .await
        .expect("getUpdates 應成功");
    assert_eq!(
        updates,
        vec![TelegramUpdate {
            update_id: 7.0,
            message: None,
        }]
    );
    client
        .set_my_commands(&[TelegramBotCommand {
            command: "status".to_string(),
            description: "顯示狀態".to_string(),
        }])
        .await
        .expect("setMyCommands 應成功");
    client
        .send_message(
            "-100123",
            "hello",
            Some(&TelegramSendMessageOptions {
                reply_markup: Some(TelegramReplyKeyboardMarkup {
                    keyboard: vec![vec![super::TelegramKeyboardButton {
                        text: "/status".to_string(),
                    }]],
                    is_persistent: None,
                    resize_keyboard: Some(true),
                }),
            }),
        )
        .await
        .expect("sendMessage 應成功");
    client
        .send_photo("-100123", b"png-image", "channel.png", "channel screenshot")
        .await
        .expect("sendPhoto 應成功");

    let requests = recorded.lock().expect("recorded lock");
    assert_eq!(requests.len(), 4);

    // 第一個請求：getUpdates，body 照舊版 JSON.stringify 的欄位順序。
    assert_eq!(requests[0].method, "POST");
    assert_eq!(requests[0].path, "/bottest-bot-token/getUpdates");
    assert_eq!(
        header_value(&requests[0], "content-type").as_deref(),
        Some("application/json")
    );
    assert_eq!(
        String::from_utf8_lossy(&requests[0].body),
        r#"{"offset":5,"timeout":25,"allowed_updates":["message"]}"#
    );

    // 第二個請求：setMyCommands。
    assert_eq!(requests[1].path, "/bottest-bot-token/setMyCommands");
    assert_eq!(
        String::from_utf8_lossy(&requests[1].body),
        r#"{"commands":[{"command":"status","description":"顯示狀態"}]}"#
    );

    // 第三個請求：sendMessage（reply_markup 在最後）。
    assert_eq!(requests[2].path, "/bottest-bot-token/sendMessage");
    assert_eq!(
        String::from_utf8_lossy(&requests[2].body),
        r#"{"chat_id":"-100123","text":"hello","reply_markup":{"keyboard":[[{"text":"/status"}]],"resize_keyboard":true}}"#
    );

    // 第四個請求：sendPhoto（FormData；multipart 的 boundary 不檢查）。
    assert_eq!(requests[3].method, "POST");
    assert_eq!(requests[3].path, "/bottest-bot-token/sendPhoto");
    let body = String::from_utf8_lossy(&requests[3].body).into_owned();
    assert!(body.contains(r#"name="chat_id""#), "{body}");
    assert!(body.contains("-100123"), "{body}");
    assert!(body.contains(r#"name="caption""#), "{body}");
    assert!(body.contains("channel screenshot"), "{body}");
    assert!(
        body.contains(r#"name="photo"; filename="channel.png""#),
        "{body}"
    );
    assert!(body.contains("Content-Type: image/png"), "{body}");
    assert!(body.contains("png-image"), "{body}");
}

/// 舊測試 it.each 的案例表（`['request', 'body']`）。
enum HangStage {
    Request,
    Body,
}

// 原測試：getUpdates 的 %s 卡住時會逾時，後續可重試
#[tokio::test(start_paused = true)]
async fn get_updates_times_out_when_stuck_and_can_retry_afterwards() {
    // 舀版 `AbortSignal.timeout` 的 deadline 合成為內部實作；兩個 hang 階段
    // （request 卡住／body 讀取卡住）都以同 deadline 逾時，分類 `timeout`。
    for stage in [HangStage::Request, HangStage::Body] {
        let port = match stage {
            HangStage::Request => spawn_hanging_request_server(),
            HangStage::Body => spawn_hanging_body_server(),
        };
        let client = client_with_base(port, "test-token");

        let error = client
            .get_updates(None, 25.0, &CancellationToken::new())
            .await
            .expect_err("預期逾時");
        assert_eq!(error.category, TelegramErrorCategory::Timeout);
        assert_eq!(error.http_status, None);

        // 舊測試之後的重試（healthy 伺服器）。本機 socket 事件的就緒與
        // start_paused 的 auto-advance 有競態（deadline 會在事件就緒前被推進），
        // 重試階段切回真實時間（`time::resume`）確認「逾時後可重試」；快慢
        // 與舊測試無關（舊測試重試也不檢查時長）。
        tokio::time::resume();
        let retry_port = spawn_json_server(
            &[(200, r#"{"ok":true,"result":[]}"#)],
            Arc::new(Mutex::new(Vec::new())),
        );
        let retry = client_with_base(retry_port, "test-token")
            .get_updates(None, 25.0, &CancellationToken::new())
            .await
            .expect("重試應成功");
        assert_eq!(retry, Vec::<TelegramUpdate>::new());
        // 下一個 stage 的 hang 逾時需要虛擬時間（resume 之後再凍結）。
        tokio::time::pause();
    }
}

// 原測試：停止訊號會取消 getUpdates
#[tokio::test]
async fn stop_signal_cancels_get_updates() {
    let port = spawn_hanging_request_server();
    let client = client_with_base(port, "test-token");
    let cancel = CancellationToken::new();

    let handle = tokio::spawn({
        let cancel = cancel.clone();
        async move { client.get_updates(None, 25.0, &cancel).await }
    });
    // 舊測試：controller.abort() 立刻中止（請求是否已送出都會分類 aborted）。
    cancel.cancel();

    let error = handle
        .await
        .expect("getUpdates task 應完成")
        .expect_err("預期中止");
    assert_eq!(error.category, TelegramErrorCategory::Aborted);
}

// 原測試：網路錯誤不包含 bot token 或完整 URL
#[tokio::test]
async fn network_error_does_not_include_bot_token_or_full_url() {
    let secret = "123456:telegram-secret";
    let port = spawn_closing_server();
    // 正式環境的 base 為 https://api.telegram.org；舊測試的 reject 訊息帶完整
    // URL，本移植的分類丟出固定訊息（不轉送任何原始錯誤細節）。
    assert_eq!(DEFAULT_API_BASE, "https://api.telegram.org");
    let client = client_with_base(port, secret);

    let error = capture_error(client.send_message("1", "hello", None)).await;

    assert_eq!(error.method, "sendMessage");
    let displayed = error.to_string();
    assert!(!displayed.contains(secret), "{displayed}");
    assert!(!displayed.contains("api.telegram.org"), "{displayed}");
    assert_eq!(error.category, TelegramErrorCategory::Network);
    assert_eq!(error.http_status, None);
}

// 原測試：API 拒絕時不轉送 Telegram description 內容
#[tokio::test]
async fn api_rejection_does_not_forward_telegram_description() {
    let recorded: Arc<Mutex<Vec<RecordedRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let port = spawn_json_server(
        &[(
            // 舊測試以 Response(JSON.stringify({ok:false, description:...}), {status:401})。
            401,
            r#"{"ok":false,"description":"secret remote details"}"#,
        )],
        Arc::clone(&recorded),
    );
    let client = client_with_base(port, "safe-test-token");

    let error = capture_error(client.send_message("1", "hello", None)).await;

    let displayed = error.to_string();
    assert!(
        displayed.contains("Telegram API rejected sendMessage"),
        "{displayed}"
    );
    assert!(!displayed.contains("secret remote details"), "{displayed}");
    assert_eq!(error.category, TelegramErrorCategory::Http);
    assert_eq!(error.http_status, Some(401));
}

// 原測試：無效 JSON 保留安全的 HTTP status 與錯誤分類
#[tokio::test]
async fn invalid_json_keeps_safe_http_status_and_error_category() {
    let recorded: Arc<Mutex<Vec<RecordedRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let port = spawn_json_server(
        // 舊測試以 Response('not-json', {status: 502})。
        &[(502, "not-json")],
        Arc::clone(&recorded),
    );
    let client = client_with_base(port, "safe-test-token");

    let error = capture_error(client.send_message("1", "hello", None)).await;

    assert_eq!(error.category, TelegramErrorCategory::InvalidResponse);
    assert_eq!(error.http_status, Some(502));
}

/// 對應舊測試 `captureError`：await promise，正常結束即失敗。
async fn capture_error(
    promise: impl core::future::Future<Output = Result<(), TelegramApiError>>,
) -> TelegramApiError {
    match promise.await {
        Ok(()) => panic!("預期 promise 拋出錯誤"),
        Err(error) => error,
    }
}
