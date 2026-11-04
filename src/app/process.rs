//! `app/installProcessHandlers.ts` 逐函式移植（§4.2，S3.6；§7.9 的訊號與 panic）。
//!
//! 行為照舊版：
//! - SIGINT／SIGTERM → 以訊號名為 reason 呼叫 `app.stop`（§7.9：exit code 0；
//!   stop 的 rejection 另記 `service_stop_failed {reason, error}` 並把 exit
//!   code 設為 1）。舊版每個訊號事件都跑一次 `requestStop`；資源的單次釋放
//!   由 runner 的 stop flight 保證。
//! - panic（取代 `uncaughtException`，X6）：panic hook 記 `uncaught_exception
//!   {error}`、把 exit code 設為 1，再以 reason `uncaught_exception` 呼叫
//!   stop。`unhandledRejection`（X6）在 Rust 沒有對應機制，不新增第二條
//!   路徑（`tools/bench/log-schema.py` 的 `ALLOWED_OLD_ONLY_EVENTS` 已聲明
//!   該事件為舊版專屬）。
//!
//! 依賴以窄介面接縫注入（§4.2、DEC-020 同類）：
//! - [`ProcessHandlerTarget`] 對映舊版 `ProcessHandlerTarget`（`on`／
//!   `removeListener`／`exitCode`）；正式端 [`DefaultProcessHandlerTarget`]
//!   以 `tokio::signal` 的 SIGINT／SIGTERM stream 實作，事件以 unbounded
//!   channel 轉交（remove 時 abort 泵任務，對映 removeListener）。測試端
//!   直接實作 trait（對應舊測試 `FakeProcessTarget`）。
//! - panic payload 的訊息對映：`String`／`&'static str` payload 以其顯示值
//!   為 `error.message`；其餘 payload 落在舊版 `safeErrorMessage` 的 default
//!   訊息（[`super::runner::UNKNOWN_APPLICATION_FAILURE`]）。
//!
//! `target.exitCode` 的正式端是程序層的 exit code（[`PROCESS_EXIT_CODE`]，
//! 未來 main 收尾讀取）；測試 fake 各保存自己的值以便斷言。
//!
//! `#![allow(dead_code)]` 保留至 main 的 no-arg 啟動接線（安裝 handlers 的
//! 正式呼叫端；下一段）：先以測試鎖定接縫形狀與訊號／panic 行為。
#![allow(dead_code)]
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio::signal::unix::{Signal, SignalKind};

use super::runner::{AppRunner, RunnerLogger, UNKNOWN_APPLICATION_FAILURE, safe_error_message};
use crate::logging::redact::SanitizedValue;

/// 舊版 `target.exitCode` 的程序層值：處理器設定後的 exit code（main 收尾
/// 讀取；0 代表尚未設定，just 與舊版 `exitCode` 的 undefined-with-default 相同）。
static PROCESS_EXIT_CODE: AtomicI32 = AtomicI32::new(0);

/// 舊版 `SIGINT`／`SIGTERM` union。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProcessSignal {
    Sigint,
    Sigterm,
}

impl ProcessSignal {
    /// 舊版 union 的字面值（stop 的 reason）。
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ProcessSignal::Sigint => "SIGINT",
            ProcessSignal::Sigterm => "SIGTERM",
        }
    }

    /// fake 的 `listenerCount` 排列（先 SIGINT、後 SIGTERM，照舊版 listener 表）。
    pub(crate) fn index(self) -> usize {
        match self {
            ProcessSignal::Sigint => 0,
            ProcessSignal::Sigterm => 1,
        }
    }
}

/// 訊號事件的來源介面（[`ProcessSignalReceiver`] 持有；正式端為 tokio stream
/// 的泵任務 channel，測試端為可注入的 channel）。
#[async_trait]
pub(crate) trait SignalDelivery: Send + Sync {
    /// 下一個訊號；`None` 表示來源已關閉（`remove` 已撤銷接聽）。
    async fn next_signal(&self) -> Option<ProcessSignal>;
}

/// 舊版 `target.on('SIGINT'…)`／`on('SIGTERM'…)` 的接收端：由 install 的訊號
/// 任務持有，`remove` 後不再送出訊號。
pub(crate) struct ProcessSignalReceiver {
    source: Arc<dyn SignalDelivery>,
}

impl ProcessSignalReceiver {
    pub(crate) fn new(source: Arc<dyn SignalDelivery>) -> Self {
        Self { source }
    }

    /// 下一個訊號（`None` 之後不再有訊號）。
    pub(crate) async fn next_signal(&self) -> Option<ProcessSignal> {
        self.source.next_signal().await
    }
}

/// 舊版 `ProcessHandlerTarget`：訊號接聽的註冊／撤銷與 exit code 寫入
///（舊版 `on`／`removeListener`／`exitCode` 三個成員）。
pub(crate) trait ProcessHandlerTarget: Send + Sync {
    /// 舊版 `target.on('SIGINT', …)` 與 `on('SIGTERM', …)` 兩次掛載：一次註冊
    /// 兩種訊號的接聽，回傳事件接收端。
    fn subscribe(&self) -> ProcessSignalReceiver;

    /// 舊版 `removeListener` 兩次呼叫：關閉接聽（receiver 不再有訊號）。
    fn remove(&self);

    /// 舊版 `target.exitCode = 1`。
    fn set_exit_code(&self, code: i32);
}

/// 舊版 `RemoveProcessHandlers`：撤銷訊號接聽與 panic hook；呼叫後不再有
/// 路徑觸發停止。
pub(crate) type RemoveProcessHandlers = Box<dyn FnOnce() + Send + Sync>;

/// 舊版 `InstallProcessHandlersOptions`。
pub(crate) struct InstallProcessHandlersOptions {
    pub(crate) app: Arc<dyn AppRunner>,
    pub(crate) logger: Arc<dyn RunnerLogger>,
    /// 舊版 `target?`；`None` 用正式的 [`DefaultProcessHandlerTarget`]。
    pub(crate) target: Option<Arc<dyn ProcessHandlerTarget>>,
}

/// 對應舊版 `installProcessHandlers`：註冊訊號任務與 panic hook。
pub(crate) fn install_process_handlers(
    options: InstallProcessHandlersOptions,
) -> RemoveProcessHandlers {
    let target = options
        .target
        .unwrap_or_else(|| DefaultProcessHandlerTarget::new() as Arc<dyn ProcessHandlerTarget>);

    // 訊號任務：逐事件 `requestStop`（舊版 stopForSignal 的對映）。
    let receiver = target.subscribe();
    let signal_app = options.app.clone();
    let signal_logger = options.logger.clone();
    let signal_target = Arc::clone(&target);
    let signal_task = tokio::spawn(signal_stream(
        signal_app,
        signal_logger,
        signal_target,
        receiver,
    ));

    // panic hook（X6：uncaughtException → uncaught_exception）。
    let panic_app = options.app.clone();
    let panic_logger = options.logger.clone();
    let panic_target = Arc::clone(&target);
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        deliver_panic(&panic_app, &panic_logger, &panic_target, info);
    }));

    let remove_target = Arc::clone(&target);
    Box::new(move || {
        remove_target.remove();
        signal_task.abort();
        std::panic::set_hook(previous_hook);
    })
}

/// 訊號任務：每個訊號事件呼叫一次 `requestStop`（舊版 stopForSignal 的行為；
/// 資源的單次釋放由 runner 的 stop flight 保證）。
async fn signal_stream(
    app: Arc<dyn AppRunner>,
    logger: Arc<dyn RunnerLogger>,
    target: Arc<dyn ProcessHandlerTarget>,
    receiver: ProcessSignalReceiver,
) {
    while let Some(signal) = receiver.next_signal().await {
        request_stop(&app, &logger, &target, signal.as_str());
    }
}

/// 舊版 `requestStop(app, logger, target, reason)`：fire-and-forget 的 stop；
/// rejection 時把 exit code 設為 1 並記 `service_stop_failed`。
///
/// panic hook 會在「任何」panic 的執行緒上執行（含無 tokio runtime 的執行緒，
/// 例如測試程序內其他同步測試的 panic）；沒有 runtime 就無法 spawn stop——
/// 捨去該次停止（記點與 exit code 已在 hook 內完成），不得讓 hook 內的
/// spawn panic（「panic while processing panic」會 abort 整個程序）。
fn request_stop(
    app: &Arc<dyn AppRunner>,
    logger: &Arc<dyn RunnerLogger>,
    target: &Arc<dyn ProcessHandlerTarget>,
    reason: &str,
) {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let app = Arc::clone(app);
    let logger = Arc::clone(logger);
    let target = Arc::clone(target);
    let reason = reason.to_string();
    handle.spawn(async move {
        if let Err(message) = app.stop(&reason).await {
            // 舊版 .catch：exit code 1＋`service_stop_failed {reason, error}`。
            // log 本身不回錯（safeLog 的對應）：fatal handling 不因日誌中斷。
            target.set_exit_code(1);
            let error = safe_error_message(Some(&message), None);
            logger.error(
                "service_stop_failed",
                Some(&SanitizedValue::Object(vec![
                    ("reason".to_string(), SanitizedValue::String(reason)),
                    ("error".to_string(), SanitizedValue::String(error)),
                ])),
            );
        }
    });
}

/// 舊版 `handleFatal(app, logger, target, event, error)` 的 panic 入口：
/// exit code 1 → 記 `uncaught_exception {error}` → 以該事件名為 reason 停止。
fn deliver_panic(
    app: &Arc<dyn AppRunner>,
    logger: &Arc<dyn RunnerLogger>,
    target: &Arc<dyn ProcessHandlerTarget>,
    info: &std::panic::PanicHookInfo<'_>,
) {
    target.set_exit_code(1);
    let message = panic_payload_message(info);
    let error = safe_error_message(Some(&message), None);
    // 舊版 safeLog：fatal handling 必須繼續；logger 呼叫不回錯即 try/catch 的對應。
    logger.error(
        "uncaught_exception",
        Some(&SanitizedValue::Object(vec![(
            "error".to_string(),
            SanitizedValue::String(error),
        )])),
    );
    request_stop(app, logger, target, "uncaught_exception");
}

/// panic payload 的訊息對映：`String`／`&'static str` 直接取顯示值；其餘
/// payload 落在舊版 `safeErrorMessage` 的 default 訊息。
fn panic_payload_message(info: &std::panic::PanicHookInfo<'_>) -> String {
    if let Some(text) = info.payload().downcast_ref::<&'static str>() {
        return (*text).to_string();
    }
    if let Some(text) = info.payload().downcast_ref::<String>() {
        return text.clone();
    }
    UNKNOWN_APPLICATION_FAILURE.to_string()
}

// ---------------------------------------------------------------------------
// 正式 target（舊版 `process as ProcessHandlerTarget` 的對映）
// ---------------------------------------------------------------------------

/// 對應舊版 target 預設值的正式實作：`tokio::signal` 的 SIGINT／SIGTERM。
///
/// 訊號事件的泵任務在 [`subscribe`] 啟動、[`remove`] 時 abort（對映
/// removeListener）。stream 註冊失敗（平台限制）時捨去該來源：訊號事件不會
/// 送達（呼叫端在 runtime task 內，不可在接縫內 panic——以「無訊號」呈現）。
struct DefaultProcessHandlerTarget {
    /// 泵任務的把手（remove 時 abort）。
    pump: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl DefaultProcessHandlerTarget {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            pump: Mutex::new(None),
        })
    }
}

impl ProcessHandlerTarget for DefaultProcessHandlerTarget {
    fn subscribe(&self) -> ProcessSignalReceiver {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        // 兩個 stream 在同一泵任務 select（先到先送；失敗的 stream 捨去）。
        let interrupt = tokio::signal::unix::signal(SignalKind::interrupt()).ok();
        let terminate = tokio::signal::unix::signal(SignalKind::terminate()).ok();
        let pump = tokio::spawn(dual_signal_pump(interrupt, terminate, sender));
        *self
            .pump
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(pump);
        ProcessSignalReceiver::new(Arc::new(TokioSignalDelivery {
            receiver: tokio::sync::Mutex::new(receiver),
        }))
    }

    fn remove(&self) {
        if let Some(pump) = self
            .pump
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            pump.abort();
        }
    }

    fn set_exit_code(&self, code: i32) {
        // 舊版 `target.exitCode = 1`：程序層的 exit code。
        PROCESS_EXIT_CODE.store(code, Ordering::SeqCst);
    }
}

/// 訊號串流的單泵任務：SIGINT／SIGTERM 先到先送（缺任一來源時只收存在的）。
async fn dual_signal_pump(
    interrupt: Option<Signal>,
    terminate: Option<Signal>,
    sender: tokio::sync::mpsc::UnboundedSender<ProcessSignal>,
) {
    let (mut interrupt, mut terminate) = (interrupt, terminate);
    loop {
        let signal = match (interrupt.as_mut(), terminate.as_mut()) {
            (Some(interrupt), Some(terminate)) => {
                tokio::select! {
                    _ = interrupt.recv() => ProcessSignal::Sigint,
                    _ = terminate.recv() => ProcessSignal::Sigterm,
                }
            }
            (Some(interrupt), None) => {
                interrupt.recv().await;
                ProcessSignal::Sigint
            }
            (None, Some(terminate)) => {
                terminate.recv().await;
                ProcessSignal::Sigterm
            }
            // 兩個來源都無法註冊：無訊號可送（見本檔模組註解）。
            (None, None) => return,
        };
        if sender.send(signal).is_err() {
            return;
        }
    }
}

/// 正式 target 的訊號來源：泵任務 channel 的接收端。
struct TokioSignalDelivery {
    receiver: tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<ProcessSignal>>,
}

#[async_trait]
impl SignalDelivery for TokioSignalDelivery {
    async fn next_signal(&self) -> Option<ProcessSignal> {
        self.receiver.lock().await.recv().await
    }
}

#[cfg(test)]
mod tests;
