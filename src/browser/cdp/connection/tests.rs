//! connection 的單元測試：Router 路由（id 遞增、回應解析、session 結束時的指令失敗、
//! 事件分類）與 Connection 的讀寫 task 接線（S1.3）。
use std::time::Duration;

use super::{CdpError, Connection, DetachedSession, EVENT_CHANNEL_CAPACITY, Router, SessionNotice};
use crate::browser::cdp::context::DriverError;
use crate::browser::cdp::transport::encode_message;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

type PendingReply = tokio::sync::oneshot::Receiver<Result<serde_json::Value, CdpError>>;

/// 登記一個等待中的指令（測試輔助）。
fn request(router: &mut Router, session: Option<&str>) -> (u64, PendingReply) {
    let id = router.next_command_id();
    let (tx, rx) = tokio::sync::oneshot::channel();
    router.attach_waiter(id, session.map(str::to_string), tx);
    (id, rx)
}

#[test]
fn next_command_id_increments() {
    let mut router = Router::new();
    assert_eq!(router.next_command_id(), 1);
    assert_eq!(router.next_command_id(), 2);
    assert_eq!(router.next_command_id(), 3);
}

#[test]
fn response_resolves_pending_command_with_result() {
    let mut router = Router::new();
    let (_id, mut rx) = request(&mut router, Some("s1"));
    router.handle_message(r#"{"id":1,"result":{"frameTree":{}}}"#);
    let reply = rx.try_recv().unwrap().unwrap();
    assert_eq!(reply, json!({"frameTree": {}}));
    assert_eq!(router.pending_count(), 0);
}

#[test]
fn error_response_resolves_with_protocol_error() {
    let mut router = Router::new();
    let (_id, mut rx) = request(&mut router, None);
    router.handle_message(r#"{"id":1,"error":{"code":-32601,"message":"'X' wasn't found"}}"#);
    let reply = rx.try_recv().unwrap();
    assert_eq!(
        reply,
        Err(CdpError::Protocol {
            code: -32601,
            message: "'X' wasn't found".to_string()
        }
        .clone())
    );
}

#[test]
fn response_for_unknown_id_is_ignored() {
    let mut router = Router::new();
    let (_id, _rx) = request(&mut router, Some("s1"));
    // 未知 id 的回應（例如指令已逾時被移除）不應影響其他等待者。
    router.handle_message(r#"{"id":999,"result":{}}"#);
    assert_eq!(router.pending_count(), 1);
}

#[test]
fn unregister_session_fails_its_pending_commands() {
    let mut router = Router::new();
    let (_id, mut rx) = request(&mut router, Some("s1"));
    // 其他 session（與 browser session）的指令不受影響。
    let (_other, other_rx) = request(&mut router, Some("s2"));
    let (_browser, browser_rx) = request(&mut router, None);

    let cancelled = router.unregister_session("s1");
    assert_eq!(cancelled, vec![1]);
    let reply = rx.try_recv().unwrap();
    assert_eq!(reply, Err(CdpError::TargetClosed));
    assert_eq!(
        reply.unwrap_err().message(),
        "Target page, context or browser has been closed"
    );
    assert_eq!(router.pending_count(), 2);
    assert!(!other_rx.is_terminated());
    assert!(!browser_rx.is_terminated());
}

#[test]
fn diagnostic_events_drop_when_channel_full_and_count() {
    let mut router = Router::new();
    let rx = router.register_session("s1");
    for id in 0..EVENT_CHANNEL_CAPACITY {
        router.handle_message(&format!(
            r#"{{"sessionId":"s1","method":"Network.responseReceived","params":{{"n":{id}}}}}"#
        ));
    }
    // channel 恰好滿：再來一則診斷事件必須被丟棄並計數。
    router.handle_message(
        r#"{"sessionId":"s1","method":"Network.responseReceived","params":{"n":9999}}"#,
    );
    assert_eq!(router.dropped_diagnostics("s1"), Some(1));

    // 消費端仍收得到先前的 1024 則。
    drop(rx);
}

#[test]
fn lifecycle_events_are_queued_for_caller() {
    let mut router = Router::new();
    let _rx = router.register_session("s1");
    let outcome = router
        .handle_message(r#"{"sessionId":"s1","method":"Inspector.targetCrashed","params":{}}"#);
    assert_eq!(outcome.lifecycle_events.len(), 1);
    assert_eq!(
        outcome.lifecycle_events[0].method,
        "Inspector.targetCrashed"
    );
    assert_eq!(outcome.lifecycle_events[0].session.as_deref(), Some("s1"));
}

#[test]
fn attached_to_target_registers_session_and_reports() {
    let mut router = Router::new();
    let outcome = router.handle_message(
        r#"{"method":"Target.attachedToTarget","params":{"sessionId":"p1","targetInfo":{"type":"page"}}}"#,
    );
    assert_eq!(outcome.sessions_attached.len(), 1);
    let attached = &outcome.sessions_attached[0];
    assert_eq!(attached.session_id, "p1");
    assert_eq!(
        attached
            .params
            .get("targetInfo")
            .and_then(|t| t.get("type")),
        Some(&serde_json::json!("page"))
    );
    assert_eq!(outcome.lifecycle_events, Vec::new());
    // 新 session 的事件通道已就緒。
    assert_eq!(router.dropped_diagnostics("p1"), Some(0));
}

#[test]
fn detached_from_target_unregisters_session_and_fails_pending() {
    let mut router = Router::new();
    let _rx = router.register_session("p1");
    let (_id, mut rx) = request(&mut router, Some("p1"));

    let outcome = router
        .handle_message(r#"{"method":"Target.detachedFromTarget","params":{"sessionId":"p1"}}"#);
    assert_eq!(
        outcome.sessions_detached,
        vec![DetachedSession {
            session_id: "p1".to_string(),
            target_id: None,
        }]
    );
    assert_eq!(rx.try_recv().unwrap(), Err(CdpError::TargetClosed));
    assert_eq!(router.dropped_diagnostics("p1"), None, "session 已不存在");
}

#[test]
fn diagnostic_event_for_unknown_session_is_dropped_silently() {
    let mut router = Router::new();
    let outcome = router
        .handle_message(r#"{"sessionId":"ghost","method":"Network.loadingFailed","params":{}}"#);
    assert_eq!(outcome.lifecycle_events, Vec::new());
    assert_eq!(outcome.sessions_attached, Vec::new());
}

#[test]
fn non_json_message_is_ignored() {
    let mut router = Router::new();
    let outcome = router.handle_message("not json at all");
    assert_eq!(outcome, super::RouteOutcome::default());
}

#[test]
fn browser_session_event_without_registration_goes_to_lifecycle() {
    let mut router = Router::new();
    let outcome =
        router.handle_message(r#"{"method":"Browser.credentiallessCdpEnabled","params":{}}"#);
    assert_eq!(outcome.lifecycle_events.len(), 1);
    assert!(outcome.lifecycle_events[0].session.is_none());
}

#[test]
fn diagnostic_event_classification() {
    // §7.3.2：Network.*、Log.entryAdded、Runtime.consoleAPICalled 是可丟棄的診斷事件。
    let mut router = Router::new();
    let _rx = router.register_session("s1");
    for method in [
        "Network.requestWillBeSent",
        "Log.entryAdded",
        "Runtime.consoleAPICalled",
    ] {
        let outcome = router.handle_message(&format!(
            r#"{{"sessionId":"s1","method":"{method}","params":{{}}}}"#
        ));
        assert!(
            outcome.lifecycle_events.is_empty(),
            "{method} 應走診斷事件通道"
        );
    }
    assert_eq!(router.dropped_diagnostics("s1"), Some(0));
}

// ===== Connection：讀寫 task 接線 =====

/// 建立一條 in-memory 連線，回傳 (Connection, attach 通知接收端, 瀏覽器模擬端)。
///
/// Connection 拿 `tokio::io::split` 後的讀寫兩半；回傳的 `DuplexStream` 是模擬瀏覽器端，
/// 測試可以直接對它寫訊息（事件、回應）或交給 [`spawn_mock_browser`]。
fn spawn_connection() -> (
    Connection,
    tokio::sync::mpsc::Receiver<SessionNotice>,
    DuplexStream,
) {
    let (ours, theirs) = tokio::io::duplex(64 * 1024);
    let (reader, writer) = tokio::io::split(ours);
    let trace = crate::browser::cdp::launcher::shared_trace_from_env();
    let (connection, notices) = Connection::spawn(reader, writer, None, trace);
    (connection, notices, theirs)
}

/// 起一個模擬瀏覽器的回應 task：對每則帶 id 的指令回
/// `{"id":N,"result":{"sentId":N,"method":…,"sessionId":…}}`，回顯送出的內容供斷言；
/// `Browser.close` 不回應（模擬指令懸而未決）。
fn spawn_mock_browser(browser: DuplexStream) {
    tokio::spawn(async move {
        let (mut reader, mut writer) = tokio::io::split(browser);
        let mut decoder = crate::browser::cdp::transport::MessageDecoder::new();
        let mut buf = vec![0u8; 4096];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let Ok(messages) = decoder.push(&buf[..n]) else {
                        break;
                    };
                    for text in messages {
                        let Ok(message) = serde_json::from_str::<serde_json::Value>(&text) else {
                            continue;
                        };
                        if message.get("method").and_then(serde_json::Value::as_str)
                            == Some("Browser.close")
                        {
                            continue;
                        }
                        let Some(id) = message.get("id").and_then(serde_json::Value::as_u64) else {
                            continue;
                        };
                        let reply = json!({
                            "id": id,
                            "result": {
                                "sentId": id,
                                "method": message.get("method").cloned().unwrap_or(json!(null)),
                                "sessionId": message.get("sessionId").cloned().unwrap_or(json!(null)),
                            }
                        });
                        if writer
                            .write_all(&encode_message(reply.to_string().as_str()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        }
    });
}

/// 把一則訊息（JSON 文字）編碼後從模擬瀏覽器端寫出。
async fn browser_sends(browser: &mut DuplexStream, message: &str) {
    browser.write_all(&encode_message(message)).await.unwrap();
}

#[tokio::test]
async fn send_command_round_trip_with_session() {
    let (connection, _notices, browser) = spawn_connection();
    spawn_mock_browser(browser);
    let rx = connection
        .send_command(Some("page-1"), "Page.getFrameTree", json!({}))
        .await;
    let reply = rx.await.unwrap().unwrap();
    assert_eq!(reply["method"], json!("Page.getFrameTree"));
    assert_eq!(reply["sessionId"], json!("page-1"));
}

#[tokio::test]
async fn send_command_without_session_omits_session_id() {
    let (connection, _notices, browser) = spawn_connection();
    spawn_mock_browser(browser);
    let rx = connection
        .send_command(None, "Browser.getVersion", json!({}))
        .await;
    let reply = rx.await.unwrap().unwrap();
    assert_eq!(reply["method"], json!("Browser.getVersion"));
    assert_eq!(reply["sessionId"], json!(null));
}

#[tokio::test]
async fn send_command_ids_increment() {
    let (connection, _notices, browser) = spawn_connection();
    spawn_mock_browser(browser);
    for expected_id in 1..=3 {
        let rx = connection
            .send_command(None, "Browser.getVersion", json!({}))
            .await;
        let reply = rx.await.unwrap().unwrap();
        assert_eq!(reply["sentId"], json!(expected_id));
    }
}

#[tokio::test]
async fn close_fails_pending_commands_with_target_closed() {
    let (connection, _notices, browser) = spawn_connection();
    spawn_mock_browser(browser);
    // 模擬瀏覽器不回應 Browser.close：指令保持懸而未決。
    let rx = connection
        .send_command(Some("page-1"), "Browser.close", json!({}))
        .await;
    connection.close().await;
    assert_eq!(rx.await.unwrap(), Err(CdpError::TargetClosed));
}

#[tokio::test]
async fn read_loop_eof_fails_pending_commands() {
    let (connection, _notices, browser) = spawn_connection();
    let rx = connection
        .send_command(Some("page-1"), "Browser.close", json!({}))
        .await;
    // 模擬端消失：pipe EOF → 讀取迴圈結束 → 未完成的指令失敗。
    drop(browser);
    assert_eq!(rx.await.unwrap(), Err(CdpError::TargetClosed));
}

#[tokio::test]
async fn send_command_after_pipe_death_resolves_with_target_closed() {
    let (connection, _notices, browser) = spawn_connection();
    // 模擬瀏覽器端消失：pipe 兩個方向都斷（寫入失敗、讀取 EOF）。
    drop(browser);
    let rx = connection
        .send_command(None, "Browser.getVersion", json!({}))
        .await;
    assert_eq!(rx.await.unwrap(), Err(CdpError::TargetClosed));
}

#[tokio::test]
async fn attach_and_detach_notices_carry_session_events() {
    let (_connection, mut notices, mut browser) = spawn_connection();

    browser_sends(
        &mut browser,
        r#"{"method":"Target.attachedToTarget","params":{"sessionId":"p1","targetInfo":{"type":"page"}}}"#,
    )
    .await;
    let SessionNotice::Attached {
        session_id,
        mut events,
        ..
    } = notices.recv().await.unwrap()
    else {
        panic!("應收到 Attached 通知");
    };
    assert_eq!(session_id, "p1");

    // 診斷事件走 try_send、生命週期事件走 send：兩種都從通知附帶的接收端收到。
    browser_sends(
        &mut browser,
        r#"{"sessionId":"p1","method":"Network.loadingFailed","params":{}}"#,
    )
    .await;
    browser_sends(
        &mut browser,
        r#"{"sessionId":"p1","method":"Inspector.targetCrashed","params":{}}"#,
    )
    .await;
    let first = events.recv().await.unwrap();
    assert_eq!(first.method, "Network.loadingFailed");
    assert_eq!(first.session.as_deref(), Some("p1"));
    let second = events.recv().await.unwrap();
    assert_eq!(second.method, "Inspector.targetCrashed");

    browser_sends(
        &mut browser,
        r#"{"method":"Target.detachedFromTarget","params":{"sessionId":"p1"}}"#,
    )
    .await;
    match notices.recv().await.unwrap() {
        SessionNotice::Detached { session_id, .. } => assert_eq!(session_id, "p1"),
        SessionNotice::Attached { .. } => panic!("應收到 Detached 通知"),
    }
}

#[tokio::test]
async fn message_split_across_reads_is_reassembled() {
    let (_connection, mut notices, mut browser) = spawn_connection();
    let message = r#"{"method":"Target.attachedToTarget","params":{"sessionId":"p1","targetInfo":{"type":"page"}}}"#;
    let bytes = encode_message(message);
    let mid = bytes.len() / 2;
    browser.write_all(&bytes[..mid]).await.unwrap();
    // 間隔一下，讓兩半幾乎確定落在不同的讀取批次。
    tokio::time::sleep(Duration::from_millis(20)).await;
    browser.write_all(&bytes[mid..]).await.unwrap();
    let notice = tokio::time::timeout(Duration::from_millis(500), notices.recv())
        .await
        .expect("切半的訊息應在重新組合後送出 attach 通知");
    assert!(matches!(notice.unwrap(), SessionNotice::Attached { .. }));
}

#[tokio::test]
async fn lifecycle_events_wait_when_session_channel_full() {
    let (connection, mut notices, mut browser) = spawn_connection();
    browser_sends(
        &mut browser,
        r#"{"method":"Target.attachedToTarget","params":{"sessionId":"p1","targetInfo":{"type":"page"}}}"#,
    )
    .await;
    let SessionNotice::Attached {
        session_id: _,
        events,
        ..
    } = notices.recv().await.unwrap()
    else {
        panic!("應收到 Attached 通知");
    };
    // 背景：灌 1025 則生命週期事件（1024 則填滿 channel + 第 1025 則卡住），
    // 最後一則是指令 id=1 的回應——pipe 上的順序保證回應在最後。
    let flood = tokio::spawn(async move {
        let mut browser = browser;
        for i in 0..=EVENT_CHANNEL_CAPACITY as u64 {
            browser_sends(
                &mut browser,
                &format!(
                    r#"{{"sessionId":"p1","method":"Inspector.targetCrashed","params":{{"n":{i}}}}}"#
                ),
            )
            .await;
        }
        browser_sends(&mut browser, r#"{"id":1,"result":{"ok":true}}"#).await;
    });
    let mut rx = connection
        .send_command(None, "Target.createTarget", json!({}))
        .await;

    // channel 滿了：讀取迴圈必須等待（不可丟棄），回應不該被處理。
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(
            rx.try_recv().is_err(),
            "生命週期事件 channel 滿時必須等待，回應不該先被處理"
        );
    }

    // 消費者放掉接收端：後續事件沒有消費者（直接丟棄），讀取迴圈解堵，回應被處理。
    drop(events);
    let reply = tokio::time::timeout(Duration::from_millis(500), rx)
        .await
        .expect("消費者釋放後讀取迴圈應解堵")
        .unwrap()
        .unwrap();
    assert_eq!(reply["ok"], json!(true));
    flood.await.unwrap();
}

#[tokio::test]
async fn register_and_unregister_session_through_connection() {
    let (connection, _notices, mut browser) = spawn_connection();
    // 嵌套 auto-attach（worker／iframe）由上層登記：事件應送進登記時拿到的接收端。
    let mut worker_events = connection.register_session("worker-1").await;
    browser_sends(
        &mut browser,
        r#"{"sessionId":"worker-1","method":"Network.loadingFailed","params":{}}"#,
    )
    .await;
    let event = tokio::time::timeout(Duration::from_millis(500), worker_events.recv())
        .await
        .expect("事件應送進登記的 session")
        .unwrap();
    assert_eq!(event.method, "Network.loadingFailed");
    assert_eq!(event.session.as_deref(), Some("worker-1"));

    // detach：該 session 懸而未決的指令以 TargetClosed 失敗。
    let rx = connection
        .send_command(Some("worker-1"), "Runtime.evaluate", json!({}))
        .await;
    let cancelled = connection.unregister_session("worker-1").await;
    assert_eq!(cancelled, vec![1]);
    assert_eq!(rx.await.unwrap(), Err(CdpError::TargetClosed));
}

// ===== crash 的 session（缺口 1：照 CRSession._markAsCrashed）=====

#[test]
fn crashed_session_fails_pending_commands_with_target_crashed() {
    let mut router = Router::new();
    let (_id, mut rx) = request(&mut router, Some("p1"));
    // 其他 session 與 browser session 的指令不受影響。
    let (_other, other_rx) = request(&mut router, Some("p2"));
    let (_browser, browser_rx) = request(&mut router, None);

    router.fail_session_pending_crashed("p1");
    let reply = rx.try_recv().unwrap();
    assert_eq!(reply, Err(CdpError::Crashed));
    // 訊息結尾有一個空白（照 pw-errors.jsonl 的 after-crash-evaluate 案例）。
    assert_eq!(reply.unwrap_err().message(), "Target crashed ");
    assert_eq!(router.pending_count(), 2);
    assert!(!other_rx.is_terminated());
    assert!(!browser_rx.is_terminated());
}

#[tokio::test]
async fn crashed_session_rejects_pending_and_later_commands() {
    let (connection, _notices, browser) = spawn_connection();
    // 一個已送出但不會有回應的指令（模擬 crash 前的 in-flight）。
    let pending = connection
        .send_command(Some("p1"), "Runtime.evaluate", json!({}))
        .await;
    // crash：先擋後續（同步），再拒絕 pending（背景）。
    connection.mark_session_crashed("p1");
    connection.fail_crashed_session_pending("p1").await;

    assert_eq!(
        pending.await.unwrap(),
        Err(CdpError::Crashed),
        "crash 時 pending 的指令應被拒絕"
    );
    // 之後送到 crash session 的指令立即失敗（不會卡住）。
    let later = connection
        .send_command(Some("p1"), "Runtime.evaluate", json!({}))
        .await;
    assert_eq!(
        tokio::time::timeout(Duration::from_millis(500), later)
            .await
            .unwrap()
            .unwrap(),
        Err(CdpError::Crashed)
    );
    // browser session 不受影響。
    spawn_mock_browser(browser);
    let rx = connection
        .send_command(None, "Browser.getVersion", json!({}))
        .await;
    assert!(
        rx.await.unwrap().is_ok(),
        "browser session 不受頁面 crash 影響"
    );
}

#[test]
fn driver_error_crashed_helpers() {
    let error = DriverError::crashed();
    assert_eq!(error.name, "Error");
    assert_eq!(error.message, "Target crashed ");
    assert!(error.is_crashed());
    assert!(!error.is_context_destroyed());
    // X7 前綴保留尾端空白。
    assert_eq!(
        error.with_api("page.evaluate").message,
        "page.evaluate: Target crashed "
    );
    assert!(!DriverError::target_closed().is_crashed());
}
