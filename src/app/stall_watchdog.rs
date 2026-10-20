//! Scheduler stall watchdog：`app/SchedulerStallWatchdog.ts` 逐函式移植
//! （§4.2，S3.2）。週期檢查 scheduler 的 `checkInFlight`；卡住超過門檻時
//! 透過 ContainerRestartController 要求重啟容器（或走舊版測試縫的 flush +
//! exit(1)）。
//!
//! 取捨（macOS 可驗證性，照計畫 §122 與 DI 前例）：
//! - scheduler 端以 [`StallSchedulerSource`] 注入；`WatchdogSchedulerSnapshot`
//!   的正式型別屬 S3.5（禁改 `src/scheduler/`），此處以 watchdog 實際讀取的
//!   四個欄位定義 [`SchedulerSnapshot`]，S3.5 落地後由呼叫端轉接（watchdog
//!   不讀取 `channels`）。
//! - `setInterval` 改用工作迴圈 + tokio paused time（計畫 §9）：每次檢查後
//!   sleep 同樣的間隔。舊版 setInterval 在檢查落後時會連續追趕觸發；watchdog
//!   的檢查是同步瞬時的，實務等價。
//! - 舊版預設時鐘是 `Date.now()`（wall clock），照舊保留；`inFlightDurationMs`
//!   可能為負（NTP 回撥）時照舊版以帶符號整數比較（小於門檻即不觸發）。

#![allow(dead_code)]

use std::io;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::config::thresholds::MAX_SAFE_INTEGER;
use crate::logging::redact::SanitizedValue;

/// 舊版 `MAX_TIMER_DELAY_MS`。
const MAX_TIMER_DELAY_MS: f64 = 2_147_483_647.0;

/// 舊版 `WatchdogSchedulerSnapshot` 中 watchdog 讀取的欄位
/// （`channels` 不被 watchdog 讀取，省略；S3.5 落地後由呼叫端轉接）。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SchedulerSnapshot {
    pub running: bool,
    pub check_in_flight: bool,
    pub retry_at: Option<String>,
    pub last_checked_at: Option<String>,
}

/// 舊版 `Pick<WatchdogScheduler, 'getSnapshot'>`。
pub(crate) trait StallSchedulerSource: Send + Sync {
    fn get_snapshot(&self) -> SchedulerSnapshot;
}

/// 舊版 `Pick<Logger, 'error' | 'info' | 'flush'>`。
#[async_trait]
pub(crate) trait StallWatchdogLogger: Send + Sync {
    fn info(&self, event: &str, fields: Option<&SanitizedValue>);
    fn error(&self, event: &str, fields: Option<&SanitizedValue>);

    async fn flush(&self) -> io::Result<()>;
}

#[async_trait]
impl StallWatchdogLogger for crate::logging::logger::JsonLineLogger {
    fn info(&self, event: &str, fields: Option<&SanitizedValue>) {
        crate::logging::logger::JsonLineLogger::info(self, event, fields);
    }

    fn error(&self, event: &str, fields: Option<&SanitizedValue>) {
        crate::logging::logger::JsonLineLogger::error(self, event, fields);
    }

    async fn flush(&self) -> io::Result<()> {
        crate::logging::logger::JsonLineLogger::flush(self).await
    }
}

/// 舊版 options 的 `now: () => number`；預設 `Date.now()`（wall clock 毫秒）。
pub(crate) type StallClock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// 舊版 `Pick<ContainerRestartController, 'request'>`：watchdog 以
/// fire-and-forget（舊版 `void`）呼叫，因此介面是同步方法。
pub(crate) trait StallRestartRequester: Send + Sync {
    fn request(&self, request: super::restart::ContainerRestartRequest);
}

/// 對應舊版 `SchedulerStallWatchdogOptions`。
pub(crate) struct SchedulerStallWatchdogOptions {
    pub scheduler: Arc<dyn StallSchedulerSource>,
    pub logger: Arc<dyn StallWatchdogLogger>,
    pub interval_seconds: f64,
    pub stall_threshold_ms: f64,
    pub now: Option<StallClock>,
    /// Preferred: shared fatal-exit controller（舊版註解）。
    pub container_restart_controller: Option<Arc<dyn StallRestartRequester>>,
    /// Legacy test seam; used only when containerRestartController is omitted.
    pub exit: Option<Arc<dyn Fn(i32) + Send + Sync>>,
}

/// watchdog 的可變狀態（檢查是同步的，鎖不跨 await）。
struct WatchdogState {
    in_flight_since_ms: Option<u64>,
    exit_requested: bool,
}

/// 對應舊版 `SchedulerStallWatchdog`。
pub(crate) struct SchedulerStallWatchdog(Arc<Inner>);

struct Inner {
    scheduler: Arc<dyn StallSchedulerSource>,
    logger: Arc<dyn StallWatchdogLogger>,
    interval_seconds: f64,
    stall_threshold_ms: f64,
    now: StallClock,
    container_restart_controller: Option<Arc<dyn StallRestartRequester>>,
    exit: Arc<dyn Fn(i32) + Send + Sync>,
    /// std 鎖：check() 是同步段落，鎖不跨 await（與 `check` 的前提一致）。
    state: StdMutex<WatchdogState>,
    /// 舊版 `this.timer`：計時迴圈的 handle（stop 時取消）。
    task: Mutex<Option<JoinHandle<()>>>,
}

impl SchedulerStallWatchdog {
    /// 舊版建構子：兩個間隔參數必須是正整數（舊版 TypeError 的訊息逐字）。
    pub(crate) fn new(mut options: SchedulerStallWatchdogOptions) -> Result<Self, String> {
        let interval_seconds = options.interval_seconds;
        if !is_positive_safe_integer(interval_seconds) {
            return Err("intervalSeconds must be a positive integer".to_string());
        }
        let stall_threshold_ms = options.stall_threshold_ms;
        if !is_positive_safe_integer(stall_threshold_ms) {
            return Err("stallThresholdMs must be a positive integer".to_string());
        }

        Ok(Self(Arc::new(Inner {
            scheduler: options.scheduler,
            logger: options.logger,
            interval_seconds,
            stall_threshold_ms,
            now: options
                .now
                .take()
                .unwrap_or_else(|| Arc::new(wall_clock_ms)),
            container_restart_controller: options.container_restart_controller,
            exit: options.exit.unwrap_or_else(|| {
                // 舊版預設：process.exit(code)。
                Arc::new(|code| std::process::exit(code))
            }),
            state: StdMutex::new(WatchdogState {
                in_flight_since_ms: None,
                exit_requested: false,
            }),
            task: Mutex::new(None),
        })))
    }

    /// 對應舊版 `start()`：記錄啟動、立即檢查一次、再排週期檢查。
    pub(crate) async fn start(&self) {
        // 舊版：`if (this.timer !== undefined) return;`
        let mut task = self.0.task.lock().await;
        if task.is_some() {
            return;
        }
        self.0.logger.info(
            "scheduler_stall_watchdog_started",
            Some(&SanitizedValue::Object(vec![
                (
                    "intervalSeconds".to_string(),
                    SanitizedValue::Number(self.0.interval_seconds),
                ),
                (
                    "stallThresholdMs".to_string(),
                    SanitizedValue::Number(self.0.stall_threshold_ms),
                ),
            ])),
        );
        self.0.check();

        let watchdog = Arc::clone(&self.0);
        *task = Some(tokio::spawn(async move { run_loop(watchdog).await }));
    }

    /// 對應舊版 `stop()`：取消計時並重置卡住計時。
    pub(crate) async fn stop(&self) {
        let task = self.0.task.lock().await.take();
        if let Some(task) = task {
            // 對應 clearInterval：取消迴圈（檢查本身是同步段落，不會被切割）。
            task.abort();
            let _ = task.await;
        }
        let mut state = self
            .0
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.in_flight_since_ms = None;
    }
}

/// 對應舊版的 `setInterval` 本體：每次檢查後再睡同一個間隔。
async fn run_loop(inner: Arc<Inner>) {
    // 舊版：`Math.min(MAX_TIMER_DELAY_MS, this.options.intervalSeconds * 1_000)`。
    let delay_ms = ((inner.interval_seconds * 1000.0).min(MAX_TIMER_DELAY_MS)) as u64;
    loop {
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        inner.check();
    }
}

impl Inner {
    /// 對應舊版私有 `check()`（同步；鎖不跨 await）。
    fn check(&self) {
        // std 鎖只在同步段落內持有；毒性鎖（同 task panic 時）照原狀取值。
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.exit_requested {
            return;
        }

        let snapshot = self.scheduler.get_snapshot();
        let now_ms = (self.now)();
        if !snapshot.check_in_flight {
            state.in_flight_since_ms = None;
            return;
        }

        // 舊版：`this.inFlightSinceMs ??= nowMs;`
        let in_flight_since_ms = *state.in_flight_since_ms.get_or_insert(now_ms);
        // 帶符號比較：wall clock 回撥時照舊版得到負的持續時間（不觸發）。
        let in_flight_duration_ms = (now_ms as i64) - (in_flight_since_ms as i64);
        if in_flight_duration_ms < (self.stall_threshold_ms as i64) {
            return;
        }

        state.exit_requested = true;
        self.logger.error(
            "scheduler_stall_detected",
            Some(&SanitizedValue::Object(vec![
                (
                    "inFlightDurationMs".to_string(),
                    SanitizedValue::Number(in_flight_duration_ms as f64),
                ),
                (
                    "stallThresholdMs".to_string(),
                    SanitizedValue::Number(self.stall_threshold_ms),
                ),
                (
                    "running".to_string(),
                    SanitizedValue::Bool(snapshot.running),
                ),
                (
                    "lastCheckedAt".to_string(),
                    optional_text(snapshot.last_checked_at.clone()),
                ),
                (
                    "retryAt".to_string(),
                    optional_text(snapshot.retry_at.clone()),
                ),
            ])),
        );

        if let Some(controller) = self.container_restart_controller.as_ref() {
            controller.request(super::restart::ContainerRestartRequest {
                reason: "scheduler_stall".to_string(),
                source: "scheduler_stall_watchdog".to_string(),
                fields: Some(vec![
                    (
                        "inFlightDurationMs".to_string(),
                        SanitizedValue::Number(in_flight_duration_ms as f64),
                    ),
                    (
                        "stallThresholdMs".to_string(),
                        SanitizedValue::Number(self.stall_threshold_ms),
                    ),
                ]),
            });
            return;
        }

        // Legacy path for unit tests that inject exit without a controller.
        // （舊版註解）fire-and-forget：flush 後 exit(1)。
        let logger = Arc::clone(&self.logger);
        let exit = Arc::clone(&self.exit);
        tokio::spawn(async move {
            let _ = logger.flush().await;
            exit(1);
        });
    }
}

/// 舊版 number 欄位（日期字串缺值時為 undefined 的對應）。
fn optional_text(value: Option<String>) -> SanitizedValue {
    match value {
        Some(value) => SanitizedValue::String(value),
        None => SanitizedValue::Undefined,
    }
}

/// 舊版 `Number.isSafeInteger(value) && value > 0`。
fn is_positive_safe_integer(value: f64) -> bool {
    value.is_finite() && value > 0.0 && value.fract() == 0.0 && value <= MAX_SAFE_INTEGER as f64
}

/// 舊版 `Date.now()` 的預設值：UNIX epoch 起的毫秒（1970 年前取 0）。
fn wall_clock_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as u64)
}

#[cfg(test)]
mod tests;
