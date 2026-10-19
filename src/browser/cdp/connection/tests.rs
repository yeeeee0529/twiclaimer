//! connection 路由的單元測試：id 遞增、回應解析、session 結束時的指令失敗、事件分類（S1.3）。
use super::{CdpError, Router, EVENT_CHANNEL_CAPACITY};
use serde_json::json;

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
        router.handle_message(&format!(r#"{{"sessionId":"s1","method":"Network.responseReceived","params":{{"n":{id}}}}}"#));
    }
    // channel 恰好滿：再來一則診斷事件必須被丟棄並計數。
    router.handle_message(r#"{"sessionId":"s1","method":"Network.responseReceived","params":{"n":9999}}"#);
    assert_eq!(router.dropped_diagnostics("s1"), Some(1));

    // 消費端仍收得到先前的 1024 則。
    drop(rx);
}

#[test]
fn lifecycle_events_are_queued_for_caller() {
    let mut router = Router::new();
    let _rx = router.register_session("s1");
    let outcome = router.handle_message(
        r#"{"sessionId":"s1","method":"Inspector.targetCrashed","params":{}}"#,
    );
    assert_eq!(outcome.lifecycle_events.len(), 1);
    assert_eq!(outcome.lifecycle_events[0].method, "Inspector.targetCrashed");
    assert_eq!(outcome.lifecycle_events[0].session.as_deref(), Some("s1"));
}

#[test]
fn attached_to_target_registers_session_and_reports() {
    let mut router = Router::new();
    let outcome = router.handle_message(
        r#"{"method":"Target.attachedToTarget","params":{"sessionId":"p1","targetInfo":{"type":"page"}}}"#,
    );
    assert_eq!(outcome.sessions_attached, vec!["p1".to_string()]);
    assert_eq!(outcome.lifecycle_events, Vec::new());
    // 新 session 的事件通道已就緒。
    assert_eq!(router.dropped_diagnostics("p1"), Some(0));
}

#[test]
fn detached_from_target_unregisters_session_and_fails_pending() {
    let mut router = Router::new();
    let _rx = router.register_session("p1");
    let (_id, mut rx) = request(&mut router, Some("p1"));

    let outcome = router.handle_message(
        r#"{"method":"Target.detachedFromTarget","params":{"sessionId":"p1"}}"#,
    );
    assert_eq!(outcome.sessions_detached, vec!["p1".to_string()]);
    assert_eq!(rx.try_recv().unwrap(), Err(CdpError::TargetClosed));
    assert_eq!(router.dropped_diagnostics("p1"), None, "session 已不存在");
}

#[test]
fn diagnostic_event_for_unknown_session_is_dropped_silently() {
    let mut router = Router::new();
    let outcome = router.handle_message(
        r#"{"sessionId":"ghost","method":"Network.loadingFailed","params":{}}"#,
    );
    assert_eq!(outcome.lifecycle_events, Vec::new());
    assert_eq!(outcome.sessions_attached, [] as [String; 0]);
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
        let outcome = router.handle_message(&format!(r#"{{"sessionId":"s1","method":"{method}","params":{{}}}}"#));
        assert!(
            outcome.lifecycle_events.is_empty(),
            "{method} 應走診斷事件通道"
        );
    }
    assert_eq!(router.dropped_diagnostics("s1"), Some(0));
}
