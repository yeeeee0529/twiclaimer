//! CDP 連線的指令路由與 session 管理（§7.3.2）。
//!
//! 一條 pipe、多個 session：沒有 `sessionId` 的是 browser session；
//! `Target.attachedToTarget`（flatten）建立子 session，`Target.detachedFromTarget` 結束子 session。
//! 本檔只做「純路由邏輯」：讀取迴圈 task（`read_loop`，之後在 `Connection` 整合）
//! 收到訊息後交給 [`Router::handle_message`]，依回傳的結果完成回應與事件派送。
// S1.3 進行中：`Connection`（讀寫 task 接線）完成後移除。
#![allow(dead_code)]

use std::collections::HashMap;

use tokio::sync::{mpsc, oneshot};

/// 每個 session 的事件 channel 容量（§7.3.2：1024）。
pub(crate) const EVENT_CHANNEL_CAPACITY: usize = 1024;

/// CDP 層的錯誤（§7.4：錯誤帶 `name` 與 `message`）。
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CdpError {
    /// CDP 回應的 `error` 欄位：`{code, message}`。
    Protocol { code: i64, message: String },
    /// session 已結束（`Target.detachedFromTarget`、pipe EOF 或瀏覽器結束）。
    ///
    /// 對應 Playwright 的 `Target page, context or browser has been closed`。
    TargetClosed,
}

impl CdpError {
    pub(crate) fn message(&self) -> String {
        match self {
            CdpError::Protocol { message, .. } => message.clone(),
            CdpError::TargetClosed => {
                "Target page, context or browser has been closed".to_string()
            }
        }
    }
}

/// 一個等待中的指令：等待者與所屬 session。
struct PendingCommand {
    session: Option<String>,
    reply: oneshot::Sender<Result<serde_json::Value, CdpError>>,
}

/// 子 session 的事件通道：診斷事件可丟棄並計數，生命週期事件不可丟（§7.3.2）。
struct SessionHandle {
    events: mpsc::Sender<Event>,
    dropped_diagnostics: u64,
}

/// 一則要交給消費者的 CDP 事件。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Event {
    pub(crate) session: Option<String>,
    pub(crate) method: String,
    /// 事件的 `params` 原文（未改寫）。
    pub(crate) params: serde_json::Value,
}

/// `handle_message` 處理單則訊息後的結果，交由呼叫端執行需要 `.await` 的部分。
#[derive(Debug, Default, PartialEq)]
pub(crate) struct RouteOutcome {
    /// 生命週期事件：呼叫端必須 `send().await`（不可丟棄）。
    pub(crate) lifecycle_events: Vec<Event>,
    /// browser session 上新 attach 的子 session（`Target.attachedToTarget`）。
    pub(crate) sessions_attached: Vec<String>,
    /// 結束的子 session（`Target.detachedFromTarget`）。
    pub(crate) sessions_detached: Vec<String>,
}

/// 指令路由器：id 遞增、回應依 id 交給等待者、session 結束時未完成的指令全部失敗。
#[derive(Default)]
pub(crate) struct Router {
    next_id: u64,
    pending: HashMap<u64, PendingCommand>,
    sessions: HashMap<String, SessionHandle>,
}

impl Router {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// 下一個指令 id（遞增）。
    pub(crate) fn next_command_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    /// 登記一個子 session，回傳事件接收端（呼叫端持有並消費）。
    pub(crate) fn register_session(&mut self, session_id: &str) -> mpsc::Receiver<Event> {
        let (tx, rx) = mpsc::channel::<Event>(EVENT_CHANNEL_CAPACITY);
        self.sessions.insert(
            session_id.to_string(),
            SessionHandle {
                events: tx,
                dropped_diagnostics: 0,
            },
        );
        rx
    }

    /// 結束子 session：未完成的指令全部失敗，回傳被取消的指令 id。
    pub(crate) fn unregister_session(&mut self, session_id: &str) -> Vec<u64> {
        self.sessions.remove(session_id);
        self.fail_pending(|pending| pending.session.as_deref() == Some(session_id))
    }

    /// 登記一個等待中的指令。
    fn attach_waiter(
        &mut self,
        id: u64,
        session: Option<String>,
        reply: oneshot::Sender<Result<serde_json::Value, CdpError>>,
    ) {
        self.pending.insert(id, PendingCommand { session, reply });
    }

    /// 對所有符合條件的 pending 指令送出失敗，回傳被取消的 id。
    fn fail_pending(
        &mut self,
        predicate: impl Fn(&PendingCommand) -> bool,
    ) -> Vec<u64> {
        let mut cancelled = Vec::new();
        let ids: Vec<u64> = self
            .pending
            .iter()
            .filter(|(_, pending)| predicate(pending))
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            if let Some(pending) = self.pending.remove(&id) {
                let _ = pending.reply.send(Err(CdpError::TargetClosed));
                cancelled.push(id);
            }
        }
        cancelled
    }

    /// 處理一則從 pipe 收到的訊息（JSON 文字）。
    ///
    /// 回應（含 `id`）直接 resolve 等待者；事件依 session 分派：
    /// 診斷事件（`Network.*`、`Log.entryAdded`、`Runtime.consoleAPICalled`）用 `try_send`，
    /// 滿了丟棄並計數；其餘（生命週期）放進 [`RouteOutcome::lifecycle_events`] 由呼叫端 await。
    pub(crate) fn handle_message(&mut self, raw: &str) -> RouteOutcome {
        let mut outcome = RouteOutcome::default();
        let message: serde_json::Value = match serde_json::from_str(raw) {
            Ok(value) => value,
            Err(_) => return outcome, // 非 JSON 的訊息忽略（pipe 上不應出現）。
        };

        // 回應：帶 id 的訊息（即使帶 error 欄位也是對指令的回應）。
        if let Some(id) = message.get("id").and_then(serde_json::Value::as_u64) {
            if let Some(pending) = self.pending.remove(&id) {
                let result = match message.get("error") {
                    Some(error) => Err(CdpError::Protocol {
                        code: error.get("code").and_then(serde_json::Value::as_i64).unwrap_or(0),
                        message: error
                            .get("message")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    }),
                    None => Ok(message.get("result").cloned().unwrap_or(serde_json::Value::Null)),
                };
                let _ = pending.reply.send(result);
            }
            return outcome;
        }

        // 事件：method（+ sessionId）。
        let method = message.get("method").and_then(serde_json::Value::as_str);
        let Some(method) = method else {
            return outcome;
        };
        let session = message
            .get("sessionId")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let params = message.get("params").cloned().unwrap_or(serde_json::Value::Null);

        // connection 自身要處理的 session 生命週期。
        if session.is_none() {
            match method {
                "Target.attachedToTarget" => {
                    let session_id = params
                        .get("sessionId")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    self.register_session(&session_id);
                    outcome.sessions_attached.push(session_id);
                    return outcome;
                }
                "Target.detachedFromTarget" => {
                    let session_id = params
                        .get("sessionId")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    self.unregister_session(&session_id);
                    outcome.sessions_detached.push(session_id);
                    return outcome;
                }
                _ => {}
            }
        }

        let event = Event {
            session: session.clone(),
            method: method.to_string(),
            params,
        };
        if is_diagnostic_event(method) {
            if let Some(handle) = session.as_deref().and_then(|s| self.sessions.get_mut(s)) {
                match handle.events.try_send(event) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        handle.dropped_diagnostics += 1;
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        // 消費者已結束：保留為丟棄，session 結束時會一併清理。
                        handle.dropped_diagnostics += 1;
                    }
                }
            }
            // 未知 session 的診斷事件直接丟棄（session 已不存在）。
            return outcome;
        }

        // 生命週期事件：交給呼叫端 await（channel 滿時等待，不丟棄）。
        outcome.lifecycle_events.push(event);
        outcome
    }

    /// 某個 session 目前丟棄的診斷事件數。
    pub(crate) fn dropped_diagnostics(&self, session_id: &str) -> Option<u64> {
        self.sessions
            .get(session_id)
            .map(|handle| handle.dropped_diagnostics)
    }

    /// 目前等待中的指令數（測試與診斷用）。
    pub(crate) fn pending_count(&self) -> usize {
        self.pending.len()
    }
}

/// 診斷事件：滿了可以丟棄（§7.3.2）。
fn is_diagnostic_event(method: &str) -> bool {
    method.starts_with("Network.")
        || method == "Log.entryAdded"
        || method == "Runtime.consoleAPICalled"
}

#[cfg(test)]
mod tests;
