//! CDP 連線的指令路由與 session 管理（§7.3.2）。
//!
//! 一條 pipe、多個 session：沒有 `sessionId` 的是 browser session；
//! `Target.attachedToTarget`（flatten）建立子 session，`Target.detachedFromTarget` 結束子 session。
//! 分兩層：[`Router`] 是純路由邏輯（id 遞增、回應解析、session 登記）；
//! [`Connection`] 在其上接讀寫 task——讀取迴圈把輸入端的位元組切成訊息交給
//! [`Router::handle_message`]，依回傳的結果完成回應與事件派送；寫入迴圈把指令
//! 編碼後寫到輸出端。
// `Connection` 已完成：等 launcher 接線後移除。
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::task::JoinHandle;

use super::transport::{self, MessageDecoder};

/// 每個 session 的事件 channel 容量（§7.3.2：1024）。
pub(crate) const EVENT_CHANNEL_CAPACITY: usize = 1024;

/// attach／detach 通知 channel 的容量。
pub(crate) const SESSION_NOTICE_CHANNEL_CAPACITY: usize = 64;

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
            CdpError::TargetClosed => "Target page, context or browser has been closed".to_string(),
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

/// session 生命週期通知：讀取迴圈送給上層（S1.4）。
#[derive(Debug)]
pub(crate) enum SessionNotice {
    /// 子 session 已 attach；`events` 是它的事件接收端，上層持有並消費
    /// （生命週期事件在裡面等待、診斷事件可丟棄）。
    Attached {
        session_id: String,
        events: mpsc::Receiver<Event>,
    },
    /// 子 session 已結束（`Target.detachedFromTarget`）。
    Detached { session_id: String },
}

/// 指令路由器：id 遞增、回應依 id 交給等待者、session 結束時未完成的指令全部失敗。
#[derive(Default)]
pub(crate) struct Router {
    next_id: u64,
    pending: HashMap<u64, PendingCommand>,
    sessions: HashMap<String, SessionHandle>,
    /// attach 中 session 的事件接收端暫存：`handle_message` 於 attach 時放入，
    /// 讀取迴圈取走後併入 [`SessionNotice::Attached`] 交給上層。
    session_receivers: HashMap<String, mpsc::Receiver<Event>>,
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
        // 來不及交給上層的 attach 接收端一併移除（上層會收到 Detached 通知）。
        self.session_receivers.remove(session_id);
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
    fn fail_pending(&mut self, predicate: impl Fn(&PendingCommand) -> bool) -> Vec<u64> {
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

    /// 取消單一個等待中的指令（指令排不進寫入佇列時用），以 `TargetClosed` 失敗。
    fn cancel_command(&mut self, id: u64) {
        if let Some(pending) = self.pending.remove(&id) {
            let _ = pending.reply.send(Err(CdpError::TargetClosed));
        }
    }

    /// 連線結束：所有未完成的指令以 `TargetClosed` 失敗，回傳被取消的指令 id。
    fn fail_all_pending(&mut self) -> Vec<u64> {
        self.fail_pending(|_| true)
    }

    /// 送出一則生命週期事件到所屬 session 的事件 channel（不可丟棄；channel 滿時等待）。
    ///
    /// session 已不存在時直接丟棄：session 結束後的生命週期事件沒有消費者
    /// （例如 `Target.detachedFromTarget` 之後才到的 `Inspector.targetCrashed`）。
    /// 消費者已 drop 接收端時同樣丟棄。沒有 `sessionId`（瀏覽器 session）目前
    /// 沒有註冊管道，也會丟棄。
    pub(crate) async fn send_lifecycle(&self, session: Option<&str>, event: Event) {
        let Some(handle) = session.and_then(|session| self.sessions.get(session)) else {
            return;
        };
        // 送出失敗只代表接收端已 drop（事件沒有消費者），忽略即可。
        let _ = handle.events.send(event).await;
    }

    /// 取走 attach 中 session 的事件接收端（讀取迴圈併入 attach 通知用）。
    fn take_session_receiver(&mut self, session_id: &str) -> Option<mpsc::Receiver<Event>> {
        self.session_receivers.remove(session_id)
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
                        code: error
                            .get("code")
                            .and_then(serde_json::Value::as_i64)
                            .unwrap_or(0),
                        message: error
                            .get("message")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    }),
                    None => Ok(message
                        .get("result")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null)),
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
        let params = message
            .get("params")
            .cloned()
            .unwrap_or(serde_json::Value::Null);

        // connection 自身要處理的 session 生命週期。
        if session.is_none() {
            match method {
                "Target.attachedToTarget" => {
                    let session_id = params
                        .get("sessionId")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    // 接收端先暫存在 router，讀取迴圈取走後隨 attach 通知交給上層。
                    let events = self.register_session(&session_id);
                    self.session_receivers.insert(session_id.clone(), events);
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

/// CDP 連線：在 [`Router`] 之上接上讀寫 task（§7.3.2）。
///
/// - 讀取 task：從輸入端讀位元組 → [`MessageDecoder`] 切出訊息 → [`Router::handle_message`]；
///   生命週期事件 `send().await` 進該 session 的事件 channel（滿時等待，不可丟棄），
///   attach／detach 以 [`SessionNotice`] 通知上層。
/// - 寫入 task：從指令佇列收 JSON 文字 → [`transport::encode_message`] → 寫到輸出端。
///
/// 連線結束（pipe EOF、讀取錯誤、訊息過大或 [`Connection::close`]）時，
/// 所有未完成的指令以 [`CdpError::TargetClosed`] 失敗。`Connection` 未呼叫
/// [`Connection::close`] 就 drop 時，兩個 task 也會自行收尾，只是不等收尾完成。
pub(crate) struct Connection {
    router: Arc<Mutex<Router>>,
    /// 指令佇列的送端：drop 後寫入 task 寫完排隊訊息即收尾。
    write_tx: mpsc::UnboundedSender<String>,
    /// 關閉訊號的送端：drop（或送出）後讀取 task 於下一個讀取點收尾。
    shutdown: oneshot::Sender<()>,
    read_task: JoinHandle<()>,
    write_task: JoinHandle<()>,
}

impl Connection {
    /// 建立連線並啟動讀寫 task。
    ///
    /// 輸入端／輸出端對應 `--remote-debugging-pipe` 的父程序端：輸入端讀瀏覽器的
    /// 回應與事件，輸出端寫指令。測試可用 `tokio::io::duplex` 的兩半
    /// （`tokio::io::split` 後的讀寫兩半）代替。回傳連線與 attach／detach 通知的
    /// 接收端（上層持有並消費）。
    pub(crate) fn spawn<R, W>(reader: R, writer: W) -> (Self, mpsc::Receiver<SessionNotice>)
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let router = Arc::new(Mutex::new(Router::new()));
        let (shutdown, shutdown_rx) = oneshot::channel();
        // 指令佇列不設上限：指令體積小，且在 router 鎖內排隊才能保證送出順序（G2）。
        let (write_tx, write_rx) = mpsc::unbounded_channel();
        let (notices_tx, notices_rx) = mpsc::channel(SESSION_NOTICE_CHANNEL_CAPACITY);
        let read_task = tokio::spawn(read_loop(
            reader,
            Arc::clone(&router),
            notices_tx,
            shutdown_rx,
        ));
        let write_task = tokio::spawn(write_loop(writer, write_rx, Arc::clone(&router)));
        (
            Self {
                router,
                write_tx,
                shutdown,
                read_task,
                write_task,
            },
            notices_rx,
        )
    }

    /// 送出一則 CDP 指令，回傳回應的接收端（§7.3.2：id 遞增、回應依 id 交給等待者）。
    ///
    /// `session` 為 `Some` 時訊息帶 `sessionId`（子 session 指令）；`None` 是
    /// 瀏覽器 session 的指令。連線已關閉時，回傳的 receiver 直接收到 `TargetClosed`。
    pub(crate) async fn send_command(
        &self,
        session: Option<&str>,
        method: &str,
        params: serde_json::Value,
    ) -> oneshot::Receiver<Result<serde_json::Value, CdpError>> {
        let (reply_tx, reply_rx) = oneshot::channel();
        {
            let mut router = self.router.lock().await;
            let id = router.next_command_id();
            router.attach_waiter(id, session.map(str::to_string), reply_tx);
            // 鎖內組訊息並排入寫入佇列：確保每個 session 內的送出順序（G2）。
            let mut message = serde_json::json!({ "id": id, "method": method, "params": params });
            if let Some(session) = session {
                message["sessionId"] = serde_json::Value::String(session.to_string());
            }
            if self.write_tx.send(message.to_string()).is_err() {
                // 寫入 task 已收尾（連線關閉）：指令送不出去也等不到回應，
                // 直接失敗，不要讓等待者掛到連線收尾才清理。
                router.cancel_command(id);
            }
        }
        reply_rx
    }

    /// 登記一個子 session 並取得事件接收端。
    ///
    /// 給嵌套 auto-attach 用（S1.4）：頁面 session 底下的 worker／iframe 的
    /// `Target.attachedToTarget` 是頁面 session 上的生命週期事件，由上層解析後登記。
    pub(crate) async fn register_session(&self, session_id: &str) -> mpsc::Receiver<Event> {
        self.router.lock().await.register_session(session_id)
    }

    /// 結束一個子 session：未完成的指令全部失敗，回傳被取消的指令 id（S1.4 用）。
    pub(crate) async fn unregister_session(&self, session_id: &str) -> Vec<u64> {
        self.router.lock().await.unregister_session(session_id)
    }

    /// 關閉連線：停止讀取 task、讓寫入 task 寫完排隊的指令後收尾，
    /// 所有未完成的指令以 `TargetClosed` 失敗。
    pub(crate) async fn close(self) {
        // 讀取 task 可能卡在事件／通知的送出（消費端已停止）：先取消，順便釋放
        // router 鎖，讓下面的 fail_all_pending 拿得到鎖。
        self.read_task.abort();
        let _ = self.read_task.await;
        self.router.lock().await.fail_all_pending();
        // 寫入 task：送端 drop 後，寫完排隊訊息即自然收尾（瀏覽器已結束時，
        // 寫入會失敗，task 也會結束）。瀏覽器還活著且不讀 pipe 時可能會等——
        // 呼叫端（launcher）應先讓瀏覽器結束。
        drop(self.write_tx);
        let _ = self.write_task.await;
    }
}

/// 讀取迴圈：從輸入端讀位元組、切訊息、交給 [`Router::handle_message`] 路由。
///
/// 結束時機：pipe EOF、讀取錯誤、訊息過大（協定錯誤）或關閉訊號。
/// 結束時所有未完成的指令以 `TargetClosed` 失敗（§7.3.2：pipe EOF 或瀏覽器結束）。
async fn read_loop<R: AsyncRead + Unpin>(
    mut reader: R,
    router: Arc<Mutex<Router>>,
    notices: mpsc::Sender<SessionNotice>,
    mut shutdown: oneshot::Receiver<()>,
) {
    let mut decoder = MessageDecoder::new();
    let mut buf = vec![0u8; 8192];
    'read: loop {
        // 讀取與關閉訊號競爭；連線關閉（含 Connection 被 drop）時立即收尾。
        let read = tokio::select! {
            biased;
            _ = &mut shutdown => None,
            read = reader.read(&mut buf) => read.ok().filter(|n| *n > 0),
        };
        let Some(n) = read else { break 'read };
        let messages = match decoder.push(&buf[..n]) {
            Ok(messages) => messages,
            Err(_) => break 'read, // 訊息過大：視為協定錯誤，結束連線
        };
        for text in messages {
            // Connection 已關閉（訊號送出或送端 drop）：停止處理後續訊息。
            match shutdown.try_recv() {
                Err(oneshot::error::TryRecvError::Empty) => {}
                _ => break 'read,
            }
            // 鎖內做純路由並取走 attach 的事件接收端；所有 `.await` 都在鎖外。
            let (outcome, attached) = {
                let mut guard = router.lock().await;
                let outcome = guard.handle_message(&text);
                let attached = outcome
                    .sessions_attached
                    .iter()
                    .map(|session_id| {
                        (
                            session_id.clone(),
                            // attach 時一定有放入接收端；fallback 是立即關閉的 channel。
                            guard
                                .take_session_receiver(session_id)
                                .unwrap_or_else(|| mpsc::channel(1).1),
                        )
                    })
                    .collect::<Vec<_>>();
                (outcome, attached)
            };
            // 生命週期事件：`send().await` 進該 session 的事件 channel（滿時等待，
            // 不可丟棄）。鎖內 await：生命週期事件稀少，且消費端不需要 router 鎖。
            {
                let guard = router.lock().await;
                for event in outcome.lifecycle_events {
                    // 先取出 session 再把 event 交出去（避免借用與搬移衝突）。
                    let session = event.session.clone();
                    guard.send_lifecycle(session.as_deref(), event).await;
                }
            }
            // attach／detach 通知交給上層；上層已停止消費（連線關閉中）時丟棄。
            for (session_id, events) in attached {
                let _ = notices
                    .send(SessionNotice::Attached { session_id, events })
                    .await;
            }
            for session_id in outcome.sessions_detached {
                let _ = notices.send(SessionNotice::Detached { session_id }).await;
            }
        }
    }
    router.lock().await.fail_all_pending();
}

/// 寫入迴圈：從指令佇列收 JSON 文字、編碼、寫到輸出端。
///
/// 寫入失敗（瀏覽器端已關閉）時結束，所有未完成的指令以 `TargetClosed` 失敗；
/// 佇列關閉（連線關閉）時把排隊的指令寫完後正常收尾。
async fn write_loop<W: AsyncWrite + Unpin>(
    mut writer: W,
    mut commands: mpsc::UnboundedReceiver<String>,
    router: Arc<Mutex<Router>>,
) {
    let mut broken = false;
    while let Some(message) = commands.recv().await {
        let bytes = transport::encode_message(&message);
        if writer.write_all(&bytes).await.is_err() || writer.flush().await.is_err() {
            broken = true;
            break;
        }
    }
    if broken {
        router.lock().await.fail_all_pending();
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
