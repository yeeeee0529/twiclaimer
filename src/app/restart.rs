//! 容器重啟控制器：`app/ContainerRestartController.ts` 逐函式移植（§4.2，
//! S3.2）。單飛的致命退出：第一個 request 拿到 flight，其他呼叫端共用同一
//! flight；先 best-effort 通知、再等 logger flush（各有上限），最後 exit(1)。
//! 供 resource guard、scheduler stall、reward escalation 與 browser fatal
//! 路徑使用（舊版註解）。
//!
//! 取捨：
//! - 舊版 `setTimeoutFn`／`clearTimeoutFn` 的測試縫改由 tokio 的 paused time
//!   涵蓋（計畫 §9：時間相關測試一律 paused 或注入 clock），逾時以
//!   [`tokio::time::timeout`] 實作。逾時後 Rust 版會取消（drop）尚未完成的
//!   future；舊版讓它在背景懸掛——對 exit 的收斂沒有差別。
//! - 舊版 `request` 外層的 catch（execute 本身意外失敗時仍 exit(1)）在 Rust
//!   版沒有對應的失敗模式：notify 與 flush 的錯誤都在內部吸收，此函式不會
//!   回傳錯誤。
//! - `isRestartInFlight` 改為 async（flight 狀態在 tokio Mutex 內，不可跨
//!   await 持 std 鎖）；語意相同（首個 request 起即為 true，不清除）。

#![allow(dead_code)]

use std::io;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{Mutex, watch};

use crate::config::thresholds::MAX_SAFE_INTEGER;
use crate::logging::logger::LogEvents;
use crate::logging::redact::SanitizedValue;

/// 舊版預設 flush 上限：5 秒。
const DEFAULT_FLUSH_TIMEOUT_MS: f64 = 5_000.0;
/// 舊版預設 notify 上限：3 秒。
const DEFAULT_NOTIFY_TIMEOUT_MS: f64 = 3_000.0;

/// 舊版 `ContainerRestartRequest`：`fields` 的 `undefined` 欄位以 `None`
/// 對應，鍵值照插入順序保留（值為消毒前的 [`SanitizedValue`]）。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ContainerRestartRequest {
    pub reason: String,
    pub source: String,
    pub fields: Option<Vec<(String, SanitizedValue)>>,
}

impl ContainerRestartRequest {
    /// 舊版物件字面值的簡便建構（無 fields）。
    pub(crate) fn new(reason: &str, source: &str) -> Self {
        Self {
            reason: reason.to_string(),
            source: source.to_string(),
            fields: None,
        }
    }
}

/// 舊版 `ContainerRestartNotifyObserver`（controller 的 `onNotify` 與 monitor
/// 的 `onContainerRestartRequested` 同形）：`(request) => Promise<void> | void`。
/// 錯誤以 `Err` 呈現（對應舊版 reject 後由呼叫端 catch 忽略／記錄）。
pub(crate) type ContainerRestartNotifyObserver =
    Arc<dyn Fn(ContainerRestartRequest) -> NotifyOutcomeFuture + Send + Sync>;

/// 觀察者的非同步回傳值（`Promise<void>` 的對應）。
pub(crate) type NotifyOutcomeFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send>>;

/// 舊版 `Pick<Logger, 'error' | 'flush'>` 的注入介面。
#[async_trait]
pub(crate) trait RestartLogger: Send + Sync {
    fn error(&self, event: &str, fields: Option<&SanitizedValue>);

    async fn flush(&self) -> io::Result<()>;
}

#[async_trait]
impl RestartLogger for crate::logging::logger::JsonLineLogger {
    fn error(&self, event: &str, fields: Option<&SanitizedValue>) {
        crate::logging::logger::JsonLineLogger::error(self, event, fields);
    }

    async fn flush(&self) -> io::Result<()> {
        crate::logging::logger::JsonLineLogger::flush(self).await
    }
}

/// 舊版 `ContainerRestartControllerOptions`；exit 未注入時為
/// `process.exit`（對應 [`default_exit`]）。
pub(crate) struct ContainerRestartControllerOptions {
    pub logger: Arc<dyn RestartLogger>,
    pub exit: Option<Arc<dyn Fn(i32) + Send + Sync>>,
    pub flush_timeout_ms: Option<f64>,
    /// Best-effort bot / external notify before exit. Bounded by notifyTimeoutMs.
    pub on_notify: Option<ContainerRestartNotifyObserver>,
    pub notify_timeout_ms: Option<f64>,
}

/// 舊版 flight 槽位：`started` 對應 `flight !== undefined`（不清除，只增不減）；
/// `done` 讓共用 flight 的呼叫端等到結束。
#[derive(Default)]
struct FlightSlot {
    started: bool,
    done: Option<watch::Receiver<()>>,
}

/// 對應舊版 `ContainerRestartController`。
pub(crate) struct ContainerRestartController {
    logger: Arc<dyn RestartLogger>,
    exit: Arc<dyn Fn(i32) + Send + Sync>,
    flush_timeout_ms: f64,
    on_notify: Option<ContainerRestartNotifyObserver>,
    notify_timeout_ms: f64,
    flight: Mutex<FlightSlot>,
}

impl ContainerRestartController {
    pub(crate) fn new(options: ContainerRestartControllerOptions) -> Self {
        Self {
            logger: options.logger,
            exit: options.exit.unwrap_or_else(default_exit),
            flush_timeout_ms: positive_integer(options.flush_timeout_ms, DEFAULT_FLUSH_TIMEOUT_MS),
            on_notify: options.on_notify,
            notify_timeout_ms: positive_integer(
                options.notify_timeout_ms,
                DEFAULT_NOTIFY_TIMEOUT_MS,
            ),
            flight: Mutex::new(FlightSlot::default()),
        }
    }

    pub(crate) async fn is_restart_in_flight(&self) -> bool {
        self.flight.lock().await.started
    }

    /// 對應舊版 `request`：並發的呼叫端共用同一 flight；不會丟出錯誤
    /// （handler 失敗已記錄，exit 照常進行；舊版註解）。
    pub(crate) async fn request(&self, request: ContainerRestartRequest) {
        enum Flight {
            /// 已有 flight：等到它完成（舊版回傳同一 Promise）。
            Wait(watch::Receiver<()>),
            /// 第一個進入的呼叫端：持有結束訊號的 sender。
            Initiate(watch::Sender<()>),
        }

        let choice = {
            let mut slot = self.flight.lock().await;
            if let Some(done) = slot.done.clone() {
                Flight::Wait(done)
            } else {
                let (sender, receiver) = watch::channel(());
                slot.started = true;
                slot.done = Some(receiver);
                Flight::Initiate(sender)
            }
        };

        match choice {
            Flight::Wait(mut done) => {
                // sender 在 flight 結束時 drop：changed() 回 Ok（有送訊）或
                // Err（sender 已 drop）都代表 flight 已完成。
                let _ = done.changed().await;
            }
            Flight::Initiate(sender) => {
                self.execute(request).await;
                drop(sender);
            }
        }
    }

    /// 對應舊版 `execute`。
    async fn execute(&self, request: ContainerRestartRequest) {
        let sanitized_fields = sanitize_fields(request.fields.as_deref());
        let mut log_fields = vec![
            (
                "reason".to_string(),
                SanitizedValue::String(request.reason.clone()),
            ),
            (
                "source".to_string(),
                SanitizedValue::String(request.source.clone()),
            ),
        ];
        log_fields.extend(sanitized_fields);
        self.logger.error(
            LogEvents::CONTAINER_RESTART_REQUESTED,
            Some(&SanitizedValue::Object(log_fields)),
        );

        self.notify_with_timeout(&request).await;
        self.flush_with_timeout().await;
        (self.exit)(1);
    }

    /// 對應舊版 `notifyWithTimeout`：best-effort 通知，逾時即繼續；通知本身的
    /// 錯誤忽略（舊版 `.catch(() => undefined)`）。
    async fn notify_with_timeout(&self, request: &ContainerRestartRequest) {
        let Some(observer) = self.on_notify.clone() else {
            return;
        };
        let notify = (observer)(request.clone());
        let _ = tokio::time::timeout(Duration::from_millis(self.notify_timeout_ms as u64), notify)
            .await;
    }

    /// 對應舊版 `flushWithTimeout`：等 logger flush（錯誤忽略），逾時即繼續。
    async fn flush_with_timeout(&self) {
        let _ = tokio::time::timeout(
            Duration::from_millis(self.flush_timeout_ms as u64),
            self.logger.flush(),
        )
        .await;
    }
}

/// 舊版 `exit ?? ((code) => process.exit(code))` 的預設值。
fn default_exit() -> Arc<dyn Fn(i32) + Send + Sync> {
    Arc::new(|code| std::process::exit(code))
}

/// 對應舊版 `sanitizeFields`：丟棄 `undefined` 與巢狀物件（避免意外帶入
/// 敏感物件，舊版註解）；`null` 與純量保留。
fn sanitize_fields(fields: Option<&[(String, SanitizedValue)]>) -> Vec<(String, SanitizedValue)> {
    let Some(fields) = fields else {
        return Vec::new();
    };
    fields
        .iter()
        .filter(|(_, value)| {
            !matches!(
                value,
                SanitizedValue::Undefined
                    | SanitizedValue::Object(_)
                    | SanitizedValue::Map(_)
                    | SanitizedValue::Array(_)
                    | SanitizedValue::Error(_)
                    | SanitizedValue::Shared(_)
            )
        })
        .cloned()
        .collect()
}

/// 對應舊版 `positiveInteger`：非正整數或非有限數時用預設值。
fn positive_integer(value: Option<f64>, fallback: f64) -> f64 {
    match value {
        Some(value)
            if value.is_finite()
                && value > 0.0
                && value.fract() == 0.0
                && value <= MAX_SAFE_INTEGER as f64 =>
        {
            value
        }
        _ => fallback,
    }
}

#[cfg(test)]
mod tests;
