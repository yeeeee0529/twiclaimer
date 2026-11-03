//! `DiscordApiClient` 的測試：移植舊版 `test/unit/discord-api.test.ts`（2 個
//! `it`；標題與 `doc/spec/ts-test-titles.tsv` 逐字相同）。
//!
//! 舊測試直接注入 fetch mock；本移植的假 HTTP 依計畫改為本機 ephemeral server
//! （只在本機收發，不連真實 Discord）：伺服器收齊每個請求（request line、
//! headers、依 Content-Length 的 body）後按腳本回覆，測試以錄到的原始請求
//! 逐欄位斷言 URL／方法／headers／body。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::{DiscordApi, DiscordApiClient, DiscordApiClientOptions, DiscordApiError};

/// 舊測試的 `discord-token`。
const BOT_TOKEN: &str = "discord-token";

/// 伺服器錄到的一個 HTTP 請求（on-the-wire 的原樣）。
struct RecordedRequest {
    method: String,
    path: String,
    /// headers 的原樣（name: value 逐行，保留順序與大小寫）。
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

/// 本機測試伺服器：先後回應 `responses` 的固定 JSON，並把每個請求錄在
/// `recorded`（從任何執行緒也可取）。
fn spawn_json_server(
    responses: &[&'static str],
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
                let index = {
                    let mut queue = responses.lock().expect("responses lock");
                    if queue.is_empty() {
                        return;
                    }
                    queue.remove(0)
                };
                recorded.lock().expect("recorded lock").push(request);
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    index.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(index.as_bytes());
                let _ = stream.flush();
            });
        }
    });
    port
}

/// 接受後立即關線的伺服器（連線層失敗，no response）。
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

fn header_value(request: &RecordedRequest, name: &str) -> Option<String> {
    request
        .headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.clone())
}

// 原測試：uses Discord REST routes for gateway, commands, messages, interactions, and follow-up files
#[tokio::test]
async fn uses_discord_rest_routes_for_gateway_commands_messages_interactions_and_follow_up_files() {
    let recorded: Arc<Mutex<Vec<RecordedRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let port = spawn_json_server(
        &[
            // GET /gateway/bot
            r#"{"url": "wss://gateway.discord.gg"}"#,
            // PUT commands
            "[]",
            // POST message
            r#"{"id": "message"}"#,
            // POST interaction callback
            r#"{"ok": true}"#,
            // POST interaction defer callback
            r#"{"ok": true}"#,
            // PATCH original message
            r#"{"ok": true}"#,
            // POST webhook files
            r#"{"id": "followup"}"#,
        ],
        Arc::clone(&recorded),
    );
    let client = DiscordApiClient::new(DiscordApiClientOptions {
        bot_token: BOT_TOKEN.to_string(),
        base_url: Some(format!("http://127.0.0.1:{port}")),
    })
    .expect("client 應建立");

    let gateway = client
        .get_gateway_bot(Duration::from_millis(1_000))
        .await
        .expect("getGatewayBot 應成功");
    assert_eq!(
        gateway,
        super::DiscordGatewayResponse {
            url: Some("wss://gateway.discord.gg".to_string()),
        }
    );

    let commands = vec![super::DiscordApplicationCommand {
        name: "status".to_string(),
        description: "顯示服務與觀看狀態".to_string(),
        options: None,
    }];
    let interaction_id = "interaction-id";
    let interaction_token = "interaction-token";

    client
        .register_commands("app-id", "guild-id", &commands)
        .await
        .expect("registerCommands 應成功");
    client
        .send_message("channel-id", "hello")
        .await
        .expect("sendMessage 應成功");
    client
        .send_interaction_response(interaction_id, interaction_token, "done")
        .await
        .expect("sendInteractionResponse 應成功");
    client
        .defer_interaction_response(interaction_id, interaction_token)
        .await
        .expect("deferInteractionResponse 應成功");
    client
        .edit_interaction_response("app-id", interaction_token, "updated")
        .await
        .expect("editInteractionResponse 應成功");
    client
        .send_interaction_photo(
            "app-id",
            interaction_token,
            b"png-image",
            "channel.png",
            "channel screenshot",
        )
        .await
        .expect("sendInteractionPhoto 應成功");

    let requests = recorded.lock().expect("recorded lock");
    assert_eq!(requests.len(), 7);

    // 第一個請求：GET /gateway/bot，Bot token 於 authorization header。
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[0].path, "/gateway/bot");
    assert_eq!(
        header_value(&requests[0], "authorization").as_deref(),
        Some("Bot discord-token")
    );

    // 第二個請求：PUT commands；body 照舊版 JSON.stringify 的欄位順序。
    assert_eq!(requests[1].method, "PUT");
    assert_eq!(
        requests[1].path,
        "/applications/app-id/guilds/guild-id/commands"
    );
    let body = String::from_utf8_lossy(&requests[1].body).into_owned();
    assert_eq!(
        body,
        r#"[{"name":"status","description":"顯示服務與觀看狀態"}]"#
    );

    // 第三個請求：POST message；content 最小化為 {"content":"hello"}。
    assert_eq!(requests[2].method, "POST");
    assert_eq!(requests[2].path, "/channels/channel-id/messages");
    let body = String::from_utf8_lossy(&requests[2].body).into_owned();
    assert_eq!(body, r#"{"content":"hello"}"#);

    // 第四個請求：POST interaction callback（type 4 + data）。
    assert_eq!(requests[3].method, "POST");
    assert_eq!(
        requests[3].path,
        "/interactions/interaction-id/interaction-token/callback"
    );
    let body = String::from_utf8_lossy(&requests[3].body).into_owned();
    assert_eq!(body, r#"{"type":4,"data":{"content":"done"}}"#);

    // 第五個請求：POST 同 path（type 5 defer）。
    assert_eq!(requests[4].method, "POST");
    assert_eq!(
        requests[4].path,
        "/interactions/interaction-id/interaction-token/callback"
    );
    let body = String::from_utf8_lossy(&requests[4].body).into_owned();
    assert_eq!(body, r#"{"type":5}"#);

    // 第六個請求：PATCH @original；authenticated = false（不帶 authorization）。
    assert_eq!(requests[5].method, "PATCH");
    assert_eq!(
        requests[5].path,
        "/webhooks/app-id/interaction-token/messages/@original"
    );
    let body = String::from_utf8_lossy(&requests[5].body).into_owned();
    assert_eq!(body, r#"{"content":"updated"}"#);
    assert_eq!(header_value(&requests[5], "authorization"), None);

    // 第七個請求：POST webhook files（multipart）。
    assert_eq!(requests[6].method, "POST");
    assert_eq!(requests[6].path, "/webhooks/app-id/interaction-token");
    let body = String::from_utf8_lossy(&requests[6].body).into_owned();
    assert!(body.contains(r#"name="payload_json""#), "{body}");
    assert!(
        body.contains(
            r#"{"content":"channel screenshot","attachments":[{"id":0,"filename":"channel.png"}]}"#
        ),
        "{body}"
    );
    assert!(
        body.contains(r#"name="files[0]"; filename="channel.png""#),
        "{body}"
    );
    assert!(body.contains("Content-Type: image/png"), "{body}");
    assert!(body.contains("png-image"), "{body}");
}

// 原測試：does not expose the bot token when network requests fail
#[tokio::test]
async fn does_not_expose_the_bot_token_when_network_requests_fail() {
    let secret = "discord-secret-token";
    let port = spawn_closing_server();
    let client = DiscordApiClient::new(DiscordApiClientOptions {
        bot_token: secret.to_string(),
        base_url: Some(format!("http://127.0.0.1:{port}")),
    })
    .expect("client 應建立");

    let error = capture_error(client.send_message("channel-id", "hello")).await;

    let DiscordApiError { method, .. } = &error;
    assert_eq!(*method, "POST");
    let text = error.to_string();
    assert!(!text.contains(secret), "{text}");
    let debugged = format!("{error:?}");
    assert!(!debugged.contains(secret), "{debugged}");
}

/// 對應舊測試 `captureError`：await promise，正常結束即失敗。
async fn capture_error(
    promise: impl core::future::Future<Output = Result<(), DiscordApiError>>,
) -> DiscordApiError {
    match promise.await {
        Ok(()) => panic!("預期 promise 拋出錯誤"),
        Err(error) => error,
    }
}
