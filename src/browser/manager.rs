//! `BrowserManager`：逐函式移植舊版 `src/browser/BrowserManager.ts`（§4.2）。
//!
//! 結構對照：舊版以 promise 尾巴（`operationTail`）串行化操作、以
//! `restartFlight`／`automaticRestartFlight` 做 single-flight；Rust 版以
//! [`tokio::sync::Mutex`] 表達串行（FIFO 等價）、以 watch 訊號表達 flight 的
//! 完成通知。狀態全部在 [`Inner`]（`Arc` 共享給背景任務），對外的
//! [`DefaultBrowserManager`] 是廉價可複製的把手。
//!
//! 行為重點（與舊版逐行對應）：
//! - `start`／`stop`／`createPage`／`closePage`／`restart` 都在 exclusive 區內
//!   進行；`createPage` 先等待進行中的 restart flight。
//! - page 的 crash／非預期 close 移除 registry 並通知 invalidation；crash 會
//!   呼叫頁面清理，並以「正規化 channel ＋ 15 秒窗」偵測多頻道連環 crash，
//!   達門檻時排程一次 full browser recycle（同 generation 只一次）。
//! - browser disconnect 通知 `trackedChannels` 快照（generation 語意）、清資源、
//!   依退避（1s 起步、2 倍成長、上限 30s）排程自動重啟；達自動重啟次數上限或
//!   browser failure breaker（600 秒窗 3 次）時要求 fatal recovery。
//! - 關閉資源一律帶逾時（預設 10 秒）；page close 逾時／失敗時排程 full
//!   recycle（不在同一 context 偷偷換 page）；browser close 無法證明終止時
//!   不啟動替換、要求 fatal recovery 並拋 `BrowserTerminationError`。
//! - 診斷事件（requestfailed／response／console）經 `page_adapter` 的消毒後，
//!   以 `browser-diagnostics.ts`（S2.1）的分級純函式決定層級，再帶
//!   channel／browserGeneration／pageGeneration 記錄。
//!
//! 與舊版的允許差異：
//! - 舊版 `closeWithTimeout` 的 try/catch（`browser_page_invalidation_failed`、
//!   `browser_diagnostic_failed` 等）涵蓋的是 JS 端可能拋錯的點；Rust 版的
//!   對應呼叫不回錯（註冊取消函式、消毒函式皆不可失敗），防禦分支只保留
//!   會真的失敗的路徑。
//! - 舊版 `sleep` 注入可能 reject（`browser_restart_backoff_failed`）；
//!   Rust 的 [`SleepFn`] 不回錯，該分支不可達。
//! - 舊版觀察者「拋錯」以 `Err` 回傳值表達（見 types.rs 的 trait 註解）。
//! - 舊版逾時事件名稱（`browser_page_close_timeout` 等，由
//!   `event.replace('_failed', '_timeout')` 衍生）不在 `doc/spec/log-events.txt`
//!   清單內（DEC-023 的補抓形狀只涵蓋 `withSessionOperationTimeout` 的
//!   `session_…_timeout`），但舊測試逐字斷言，照舊版輸出。
//! - `now` 注入時鐘只用於排程邏輯（crash 相關、退避重置）；close 的
//!   `durationMs` 用單調時鐘（對應舊版直接呼叫 `Date.now()`）。
#![allow(dead_code)]

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;

use super::cdp::context::{DriverError, ResourceBlockingOptions};
use super::cdp::launcher::CancelListener;
use super::channel_url::sanitize_browser_url;
use super::diagnostics::{
    DiagnosticLogLevel, decide_console_level, decide_http_response_level,
    decide_request_failure_level,
};
use super::page_adapter::{
    BrowserConsoleDiagnostic, BrowserHttpResponseDiagnostic, BrowserRequestFailureDiagnostic,
};
use super::types::{
    BrowserAdapter, BrowserContextAdapter, BrowserContextOptions, BrowserFatalRecoveryObserver,
    BrowserInvalidation, BrowserInvalidationObserver, BrowserInvalidationReason,
    BrowserLaunchOptions, BrowserLauncher, BrowserManagerConfig, BrowserManagerDependencies,
    BrowserManagerLogger, BrowserNavigationOutcome, BrowserNavigationStatus, BrowserPage,
    BrowserPageAdapter, BrowserRestartedEvent, BrowserRestartedMode, BrowserRestartedObserver,
    BrowserTeardownResult, BrowserViewport, CloseOutcome, DetachedResources,
    FatalRecoveryFieldValue, FatalRecoveryRequest, ManagerClock, PageEntry, RestartSchedule,
    SleepFn, evict_old_timestamps, normalize_channel, positive_integer,
};
use crate::logging::logger::LogEvents;
use crate::logging::redact::{LogError, SanitizedValue, redact_sensitive_string};

/// 舊版 `DEFAULT_RESTART_BACKOFF_MS`。
const DEFAULT_RESTART_BACKOFF_MS: u64 = 1_000;

/// 舊版 `DEFAULT_RESTART_BACKOFF_MAX_MS`。
const DEFAULT_RESTART_BACKOFF_MAX_MS: u64 = 30_000;

/// 舊版 `DEFAULT_MAX_AUTOMATIC_RESTART_ATTEMPTS`。
const DEFAULT_MAX_AUTOMATIC_RESTART_ATTEMPTS: u64 = 3;

/// 舊版 `DEFAULT_RESTART_ATTEMPT_RESET_MS`。
const DEFAULT_RESTART_ATTEMPT_RESET_MS: u64 = 60_000;

/// 舊版 `DEFAULT_RESOURCE_CLOSE_TIMEOUT_MS`。
const DEFAULT_RESOURCE_CLOSE_TIMEOUT_MS: u64 = 10_000;

/// 舊版 `BrowserTerminationError` 的訊息（舊測試以
/// `/termination was not confirmed/i` 斷言）。
const TERMINATION_UNCONFIRMED_MESSAGE: &str =
    "Old browser termination was not confirmed; replacement launch skipped";

/// 舊版 createPage／startUnlocked 的「尚未啟動」錯誤訊息（SessionManager 依賴）。
const NOT_STARTED_MESSAGE: &str = "Browser Manager 尚未啟動";

/// restart flight 的完成訊號值：`None` 表示進行中。
type FlightResult = Option<Result<(), DriverError>>;

/// 等待 restart flight 的把手（多個呼叫端可同時 await 同一 flight）。
#[derive(Clone)]
struct RestartFlight {
    completion: tokio::sync::watch::Receiver<FlightResult>,
}

impl RestartFlight {
    /// 等 flight 完成並回傳結果（對應 `await flight`；reject 會傳給每個等待者）。
    async fn completed(&self) -> Result<(), DriverError> {
        let mut receiver = self.completion.clone();
        loop {
            if let Some(result) = receiver.borrow_and_update().clone() {
                return result;
            }
            if receiver.changed().await.is_err() {
                // 發送端消失（flight 任務被取消，只發生於 runtime 結束）：視為完成。
                return Ok(());
            }
        }
    }
}

/// 登記在 [`Inner::restart_flight`] 的 flight 記錄。
struct RestartFlightRecord {
    completion: tokio::sync::watch::Receiver<FlightResult>,
    /// flight 的識別碼（完成清除時比對，避免清掉新一代的 flight）。
    id: u64,
}

/// 登記在 [`Inner::automatic_restart`] 的排程記錄
///（對應舊版 `automaticRestartToken`／`automaticRestartRecoveryEpoch`）。
struct AutomaticRestartRecord {
    token: u64,
    recovery_epoch: u64,
}

/// manager 的可變狀態（`std::sync::Mutex` 保護；鎖內不做 await）。
#[derive(Default)]
struct ManagerState {
    browser: Option<Arc<dyn BrowserAdapter>>,
    context: Option<Arc<dyn BrowserContextAdapter>>,
    unsubscribe_browser: Option<CancelListener>,
    /// channel（原樣，未正規化）→ entry；插入順序不影響行為，僅計數與查找。
    pages: HashMap<String, PageEntry>,
    browser_generation: u64,
    next_page_generation: u64,
    /// 本 generation 追蹤的 channels（插入順序保留：disconnect 通知的陣列順序照此）。
    tracked_channels: Vec<String>,
    desired_running: bool,
    recovery_epoch: u64,
    automatic_restart_attempts: u64,
    last_browser_crash_at: Option<u64>,
    pending_forced_recycle_reason: Option<String>,
    /// 正規化 channel → 最近 crash 時間（插入順序保留：affectedChannels 照此）。
    correlated_page_crashes: Vec<(String, u64)>,
    page_crash_recycle_generation: Option<u64>,
    browser_failure_timestamps: Vec<u64>,
}

/// manager 的全部狀態與相依（Arc 共享給 listener 與 flight 任務）。
struct Inner {
    config: BrowserManagerConfig,
    launcher: Arc<dyn BrowserLauncher>,
    logger: Arc<dyn BrowserManagerLogger>,
    on_invalidated: Option<Arc<dyn BrowserInvalidationObserver>>,
    on_fatal_recovery: Option<Arc<dyn BrowserFatalRecoveryObserver>>,
    on_browser_restarted: Option<Arc<dyn BrowserRestartedObserver>>,
    sleep: Arc<dyn SleepFn>,
    now: ManagerClock,
    restart_backoff_ms: u64,
    restart_backoff_max_ms: u64,
    max_automatic_restart_attempts: u64,
    restart_attempt_reset_ms: u64,
    resource_close_timeout_ms: u64,
    multi_channel_crash_threshold: f64,
    multi_channel_crash_window_ms: u64,
    browser_failure_container_threshold: f64,
    browser_failure_window_ms: u64,

    state: Mutex<ManagerState>,
    /// 舊版 `operationTail`：exclusive 區的串行（FIFO，同步登記的 promise 尾巴）。
    operation_lock: Arc<ExclusiveQueue>,
    /// 舊版 `navigationOutcomeTail`：導覽結果批次的串行（同步登記）。
    navigation_outcome_lock: Arc<ExclusiveQueue>,
    /// 舊版 `restartFlight`：single-flight（同步登記，createPage 才能立即觀察）。
    restart_flight: Mutex<Option<RestartFlightRecord>>,
    flight_sequence: AtomicU64,
    /// 舊版 `automaticRestartToken`／`automaticRestartRecoveryEpoch`。
    automatic_restart: Mutex<Option<AutomaticRestartRecord>>,
    automatic_restart_sequence: AtomicU64,
}

/// 舊版 `runExclusive` 的 promise 尾巴：以 FIFO 佇列表達，「登記」（排隊）是
/// 同步動作——對應 TS 的 `operationTail = new Promise(release)` 在呼叫當下
/// 就把 critical section 接上尾巴，之後的申請只能排在後面。session 模組重用
/// 同一語意（`lifecycleTail`／`pageOperationTail`），故提升為 pub(crate)。
pub(crate) struct ExclusiveQueue {
    state: Mutex<ExclusiveQueueState>,
}

/// 佇列狀態（std Mutex；鎖內無 await）。
#[derive(Default)]
struct ExclusiveQueueState {
    /// 是否有人在 exclusive 區（第一筆已開始或已直接取得）。
    occupied: bool,
    /// 等待者的喚送端（FIFO）。
    waiters: VecDeque<tokio::sync::oneshot::Sender<()>>,
}

/// 一次 exclusive 區的佇列位置：同步登記，[`ExclusiveSlot::acquire`] 後取得鎖
///（session 模組重用，故 pub(crate)）。
pub(crate) struct ExclusiveSlot {
    permit: Option<tokio::sync::oneshot::Receiver<()>>,
    queue: Arc<ExclusiveQueue>,
    /// 尚未轉移給 guard（放棄時要還位）。
    holds_position: bool,
}

/// exclusive 區的持有權；drop 時交棒或釋放（session 模組重用，故 pub(crate)）。
pub(crate) struct ExclusiveGuard {
    queue: Option<Arc<ExclusiveQueue>>,
}

impl ExclusiveQueue {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(ExclusiveQueueState::default()),
        })
    }

    /// 同步登記排隊位置（對應 TS 在呼叫當下把 release promise 接上尾巴）。
    pub(crate) fn enqueue(self: &Arc<Self>) -> ExclusiveSlot {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !state.occupied {
            state.occupied = true;
            return ExclusiveSlot {
                permit: None,
                queue: Arc::clone(self),
                holds_position: true,
            };
        }
        let (sender, receiver) = tokio::sync::oneshot::channel();
        state.waiters.push_back(sender);
        ExclusiveSlot {
            permit: Some(receiver),
            queue: Arc::clone(self),
            holds_position: true,
        }
    }

    /// 釋放／交棒：喚醒下一個等待者，或清出佇列。
    fn release(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            match state.waiters.pop_front() {
                Some(waiter) => {
                    if waiter.send(()).is_ok() {
                        // 交棒：occupied 維持 true（下一筆直接開始）。
                        return;
                    }
                    // 等待者已消失（排隊的 future 被取消）：繼續找下一個。
                }
                None => {
                    state.occupied = false;
                    return;
                }
            }
        }
    }
}

impl ExclusiveSlot {
    /// 取得鎖（等待佇列喚醒）；回傳的 guard drop 時交棒／釋放。
    pub(crate) async fn acquire(mut self) -> ExclusiveGuard {
        if let Some(permit) = self.permit.take() {
            // 發送端消失（佇列已 drop，runtime 結束）：視為取得。
            let _ = permit.await;
        }
        self.holds_position = false;
        ExclusiveGuard {
            queue: Some(Arc::clone(&self.queue)),
        }
    }
}

impl Drop for ExclusiveSlot {
    fn drop(&mut self) {
        // 排了隊卻沒等到（future 被丟棄）：還位。
        if self.holds_position {
            self.queue.release();
        }
    }
}

impl Drop for ExclusiveGuard {
    fn drop(&mut self) {
        if let Some(queue) = self.queue.take() {
            queue.release();
        }
    }
}

/// 舊版 `DefaultBrowserManager`：包 [`Inner`] 的廉價把手（Clone 共享狀態）。
#[derive(Clone)]
pub(crate) struct DefaultBrowserManager {
    inner: Arc<Inner>,
}

/// 舊版預設 logger 的 `NOOP_LOGGER`。
struct NoopManagerLogger;

impl BrowserManagerLogger for NoopManagerLogger {
    fn debug(&self, _event: &str, _fields: Option<&SanitizedValue>) {}
    fn info(&self, _event: &str, _fields: Option<&SanitizedValue>) {}
    fn warn(&self, _event: &str, _fields: Option<&SanitizedValue>) {}
    fn error(&self, _event: &str, _fields: Option<&SanitizedValue>) {}
}

/// 舊版預設 `now`（`Date.now`）：epoch 毫秒。時鐘倒退（理論上不會）以 0 對應。
fn default_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// 日誌欄位的小型建構子（對應舊版物件字面值與省略條件）
// ---------------------------------------------------------------------------

fn sv_str(value: impl Into<String>) -> SanitizedValue {
    SanitizedValue::String(value.into())
}

fn sv_num(value: f64) -> SanitizedValue {
    SanitizedValue::Number(value)
}

fn sv_bool(value: bool) -> SanitizedValue {
    SanitizedValue::Bool(value)
}

fn sv_str_array(values: &[String]) -> SanitizedValue {
    SanitizedValue::Array(
        values
            .iter()
            .map(|value| SanitizedValue::String(value.clone()))
            .collect(),
    )
}

/// 組日誌欄位物件（插入順序照呼叫端，R10）。
fn fields(entries: Vec<(&'static str, SanitizedValue)>) -> SanitizedValue {
    SanitizedValue::Object(
        entries
            .into_iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect(),
    )
}

/// 依分級層級呼叫 logger（對應舊版 `this.logger[level](...)`）。
fn log_at(
    logger: &Arc<dyn BrowserManagerLogger>,
    level: DiagnosticLogLevel,
    event: &str,
    log_fields: Option<&SanitizedValue>,
) {
    match level {
        DiagnosticLogLevel::Warn => logger.warn(event, log_fields),
        DiagnosticLogLevel::Debug => logger.debug(event, log_fields),
        // 舊版 `level === 'none'`：直接 return（上層已檢查，防衛性分支）。
        DiagnosticLogLevel::None => {}
    }
}

/// 舊版 `event.replace('_failed', '_timeout')`：關閉逾時事件名稱的衍生規則。
fn timeout_event_of(event: &str) -> String {
    event.replacen("_failed", "_timeout", 1)
}

/// fatal recovery 欄位的字串值（對應舊版物件字面值）。
fn fatal_str(key: &str, value: &str) -> (String, FatalRecoveryFieldValue) {
    (
        key.to_string(),
        FatalRecoveryFieldValue::Str(value.to_string()),
    )
}

/// fatal recovery 欄位的數字值。
fn fatal_num(key: &str, value: f64) -> (String, FatalRecoveryFieldValue) {
    (key.to_string(), FatalRecoveryFieldValue::Num(value))
}

impl Inner {
    fn new(config: BrowserManagerConfig, dependencies: BrowserManagerDependencies) -> Self {
        let BrowserManagerDependencies {
            launcher,
            logger,
            on_invalidated,
            on_fatal_recovery,
            on_browser_restarted,
            sleep,
            now,
            resource_close_timeout_ms,
            restart_backoff_ms,
            restart_backoff_max_ms,
            max_automatic_restart_attempts,
            restart_attempt_reset_ms,
        } = dependencies;
        Self {
            multi_channel_crash_threshold: config.browser.recovery.multi_channel_crash_threshold,
            multi_channel_crash_window_ms: (config
                .browser
                .recovery
                .multi_channel_crash_window_seconds
                * 1_000.0) as u64,
            browser_failure_container_threshold: config
                .browser
                .recovery
                .browser_failure_container_threshold,
            browser_failure_window_ms: (config.browser.recovery.browser_failure_window_seconds
                * 1_000.0) as u64,
            launcher: launcher.unwrap_or_else(|| {
                Arc::new(super::types::CdpLauncherAdapter::new()) as Arc<dyn BrowserLauncher>
            }),
            logger: logger
                .unwrap_or_else(|| Arc::new(NoopManagerLogger) as Arc<dyn BrowserManagerLogger>),
            on_invalidated,
            on_fatal_recovery,
            on_browser_restarted,
            sleep: sleep.unwrap_or_else(|| Arc::new(super::types::TokioSleep) as Arc<dyn SleepFn>),
            now: now.unwrap_or_else(|| Arc::new(default_now_ms) as ManagerClock),
            restart_backoff_ms: positive_integer(restart_backoff_ms, DEFAULT_RESTART_BACKOFF_MS),
            restart_backoff_max_ms: positive_integer(
                restart_backoff_max_ms,
                DEFAULT_RESTART_BACKOFF_MAX_MS,
            ),
            max_automatic_restart_attempts: positive_integer(
                max_automatic_restart_attempts,
                DEFAULT_MAX_AUTOMATIC_RESTART_ATTEMPTS,
            ),
            restart_attempt_reset_ms: positive_integer(
                restart_attempt_reset_ms,
                DEFAULT_RESTART_ATTEMPT_RESET_MS,
            ),
            resource_close_timeout_ms: positive_integer(
                resource_close_timeout_ms,
                DEFAULT_RESOURCE_CLOSE_TIMEOUT_MS,
            ),
            config,
            state: Mutex::new(ManagerState::default()),
            operation_lock: ExclusiveQueue::new(),
            navigation_outcome_lock: ExclusiveQueue::new(),
            restart_flight: Mutex::new(None),
            flight_sequence: AtomicU64::new(0),
            automatic_restart: Mutex::new(None),
            automatic_restart_sequence: AtomicU64::new(0),
        }
    }

    /// 舊版 `runExclusive`：串行化 exclusive 區（同步登記、FIFO 執行）。
    async fn run_exclusive<T>(&self, operation: impl Future<Output = T>) -> T {
        let slot = self.operation_lock.enqueue();
        let _guard = slot.acquire().await;
        operation.await
    }

    // ===== 公開 API（舊版 BrowserManager 介面與 DefaultBrowserManager 方法）=====

    async fn start(self: &Arc<Self>) -> Result<(), DriverError> {
        let this = Arc::clone(self);
        self.run_exclusive(async move {
            {
                let mut state = this
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if state.browser.is_some() && state.context.is_some() {
                    // 已啟動：只確定 desiredRunning，不重建（重複 start 安全）。
                    state.desired_running = true;
                    return Ok(());
                }
                state.desired_running = true;
                state.recovery_epoch += 1;
                state.automatic_restart_attempts = 0;
                state.last_browser_crash_at = None;
            }

            match Inner::start_unlocked(&this).await {
                Ok(()) => Ok(()),
                Err(error) => {
                    {
                        let mut state = this
                            .state
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        state.desired_running = false;
                    }
                    this.logger.error(
                        "browser_start_failed",
                        Some(&fields(vec![("error", this.safe_error_value(&error))])),
                    );
                    Err(error)
                }
            }
        })
        .await
    }

    async fn stop(self: &Arc<Self>) -> Result<(), DriverError> {
        let this = Arc::clone(self);
        self.run_exclusive(async move {
            {
                let mut state = this
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                state.desired_running = false;
                state.recovery_epoch += 1;
                state.automatic_restart_attempts = 0;
                state.last_browser_crash_at = None;
            }
            let resources = {
                let mut state = this
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                this.detach_resources_unlocked(&mut state)
            };
            this.close_resources_unlocked(resources, "stop").await;
            Ok(())
        })
        .await
    }

    async fn create_page(
        self: &Arc<Self>,
        channel: &str,
    ) -> Result<Arc<dyn BrowserPage>, DriverError> {
        // 先等進行中的 full recycle（page close 失敗後排程者）；flight reject 時
        // createPage 也 reject（對應 `await pendingRestart`）。
        let pending_restart = self.current_restart_flight();
        if let Some(flight) = pending_restart {
            flight.completed().await?;
        }

        let this = Arc::clone(self);
        let channel = channel.to_string();
        self.run_exclusive(async move {
            {
                let state = this
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if let Some(reason) = state.pending_forced_recycle_reason.as_ref() {
                    return Err(DriverError::new(
                        "Error",
                        format!("Browser recycle required ({reason}); cannot create page"),
                    ));
                }
            }

            {
                let state = this
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if let Some(entry) = state.pages.get(&channel) {
                    // 同一 channel 重複申請回傳既有 page（把手識別相等）。
                    return Ok(entry.adapter.page());
                }
                if state.context.is_none() || state.browser.is_none() {
                    return Err(DriverError::new("Error", NOT_STARTED_MESSAGE));
                }
            }
            let context = {
                let state = this
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                state.context.as_ref().map(Arc::clone)
            };
            let Some(context) = context else {
                // 與舊版相同：browser／context 缺一即視為未啟動。
                return Err(DriverError::new("Error", NOT_STARTED_MESSAGE));
            };

            let mut adapter: Option<Arc<dyn BrowserPageAdapter>> = None;
            let result = async {
                let page_adapter = context.new_page().await?;
                adapter = Some(Arc::clone(&page_adapter));
                {
                    let mut state = this
                        .state
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    let browser_generation = state.browser_generation;
                    state.next_page_generation += 1;
                    let page_generation = state.next_page_generation;
                    let entry = this.attach_page_unlocked(
                        &channel,
                        Arc::clone(&page_adapter),
                        browser_generation,
                        page_generation,
                    );
                    state.pages.insert(channel.clone(), entry);
                    if !state.tracked_channels.contains(&channel) {
                        state.tracked_channels.push(channel.clone());
                    }
                }
                Ok(page_adapter.page())
            }
            .await;
            if let Err(error) = &result {
                // 舊版：attach 失敗時以 'create' phase 清理 page。Rust 的
                // listener 註冊不會失敗，此分支只涵蓋 newPage 之後的失敗
                //（防衛性，與舊版結構相同）。
                if let Some(page_adapter) = adapter.as_ref() {
                    this.close_page_adapter_for_cleanup(page_adapter, Some(&channel), "create")
                        .await;
                }
                this.logger.error(
                    "browser_page_create_failed",
                    Some(&fields(vec![
                        ("channel", sv_str(&channel)),
                        ("error", this.safe_error_value(error)),
                    ])),
                );
            }
            result
        })
        .await
    }

    async fn close_page(self: &Arc<Self>, channel: &str) -> Result<(), DriverError> {
        let this = Arc::clone(self);
        let channel = channel.to_string();
        let schedule_recycle_reason = self
            .run_exclusive(async move {
                // detach listeners（鎖內同步）並取回 adapter 與 generation。
                let entry_info = {
                    let mut state = this
                        .state
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    match state.pages.get_mut(&channel) {
                        None => None,
                        Some(entry) => {
                            for unsubscribe in entry.unsubscribes.drain(..) {
                                unsubscribe();
                            }
                            Some((
                                Arc::clone(&entry.adapter),
                                entry.browser_generation,
                                entry.page_generation,
                            ))
                        }
                    }
                };
                let (adapter, browser_generation, page_generation) = entry_info?;

                let outcome = this
                    .close_page_adapter_with_timeout(&adapter, Some(&channel), "close")
                    .await;

                let close_confirmed =
                    matches!(outcome, CloseOutcome::Closed | CloseOutcome::AlreadyClosed)
                        || adapter.is_closed();

                {
                    let mut state = this
                        .state
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    if state
                        .pages
                        .get(&channel)
                        .is_some_and(|current| current.page_generation == page_generation)
                    {
                        state.pages.remove(&channel);
                    }
                    if browser_generation == state.browser_generation {
                        state.tracked_channels.retain(|tracked| tracked != &channel);
                    }
                    if close_confirmed {
                        return None;
                    }

                    // 逾時或失敗、且 page 可能還活著：不允許在同一共用 context 換
                    // page。釋放 exclusive 鎖後以 full browser recycle 處理
                    //（避免 SessionManager／BrowserManager 的鎖循環）。
                    let reason = match outcome {
                        CloseOutcome::TimedOut => "page_close_timeout",
                        _ => "page_close_failed",
                    };
                    state.pending_forced_recycle_reason = Some(reason.to_string());
                    drop(state);
                    this.logger.warn(
                        "browser_page_close_requires_recycle",
                        Some(&fields(vec![
                            ("channel", sv_str(&channel)),
                            ("reason", sv_str(reason)),
                            ("phase", sv_str("close")),
                        ])),
                    );
                    Some(reason.to_string())
                }
            })
            .await;

        if let Some(reason) = schedule_recycle_reason {
            // 舊版 `void this.restart().catch(...)`：flight 同步登記，錯誤處理在
            // 背景任務。
            let flight = self.begin_restart_flight();
            let this = Arc::clone(self);
            tokio::spawn(async move {
                if let Err(error) = flight.completed().await {
                    this.logger.error(
                        "browser_page_close_recycle_failed",
                        Some(&fields(vec![
                            ("reason", sv_str(reason.clone())),
                            ("error", this.safe_error_value(&error)),
                        ])),
                    );
                    this.request_fatal_recovery(
                        "browser_page_close_recycle_failed",
                        Some(vec![fatal_str("reason", &reason)]),
                    );
                }
            });
        }
        Ok(())
    }

    async fn restart(self: &Arc<Self>) -> Result<(), DriverError> {
        // flight 的登記在本方法第一次被 poll 時同步完成；fire-and-forget 的
        // 內部呼叫端直接用 [`Inner::begin_restart_flight`]（同步登記）。
        let flight = self.begin_restart_flight();
        flight.completed().await
    }

    fn get_page_count(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .pages
            .len()
    }

    fn get_browser_generation(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .browser_generation
    }

    fn get_browser_version(&self) -> Option<String> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.browser.as_ref().map(|browser| browser.get_version())
    }

    /// 舊版 `reportNavigationOutcomes` 的處理本體（批次按到達順序串行）。
    async fn process_navigation_outcomes(
        self: &Arc<Self>,
        outcomes: Vec<BrowserNavigationOutcome>,
        reported_recovery_epoch: u64,
    ) {
        let this = Arc::clone(self);
        self.run_exclusive(async move {
            {
                let state = this
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if !state.desired_running
                    || state.browser.is_none()
                    || state.context.is_none()
                    || reported_recovery_epoch != state.recovery_epoch
                {
                    return;
                }
            }
            let browser_generation = {
                let state = this
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                state.browser_generation
            };
            for outcome in &outcomes {
                if outcome.status != BrowserNavigationStatus::TimedOut {
                    continue;
                }
                this.logger.debug(
                    "browser_navigation_failure_recorded",
                    Some(&fields(vec![
                        ("channel", sv_str(&outcome.channel)),
                        ("status", sv_str(outcome.status.as_str())),
                        ("recovery", sv_str("per_channel")),
                        ("browserGeneration", sv_num(browser_generation as f64)),
                    ])),
                );
            }
        })
        .await;
    }

    // ===== 重啟流程 =====

    /// 舊版 `restartManually` 的 exclusive 本體（exclusive 鎖由呼叫端持有）。
    async fn restart_manually_in_exclusive(
        self: &Arc<Self>,
    ) -> (Vec<String>, bool, Option<DriverError>) {
        let this = Arc::clone(self);
        async move {
            {
                let mut state = this
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                state.desired_running = true;
                state.recovery_epoch += 1;
                state.automatic_restart_attempts = 0;
                state.last_browser_crash_at = None;
                state.pending_forced_recycle_reason = None;
            }

            let invalidated_channels: Vec<String> = {
                let state = this
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                state.pages.keys().cloned().collect()
            };
            let resources = {
                let mut state = this
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                this.detach_resources_unlocked(&mut state)
            };
            let teardown = this.close_resources_unlocked(resources, "restart").await;
            if !teardown.browser_terminated {
                this.logger.error(
                    "browser_termination_unconfirmed",
                    Some(&fields(vec![
                        ("mode", sv_str("manual")),
                        ("pageCloseTimedOut", sv_bool(teardown.page_close_timed_out)),
                        (
                            "browserCloseTimedOut",
                            sv_bool(teardown.browser_close_timed_out),
                        ),
                        ("browserCloseFailed", sv_bool(teardown.browser_close_failed)),
                    ])),
                );
                return (invalidated_channels, false, None);
            }

            match Inner::start_unlocked(&this).await {
                Ok(()) => {
                    let browser_generation = this
                        .state
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .browser_generation;
                    this.logger.warn(
                        LogEvents::BROWSER_RESTARTED,
                        Some(&fields(vec![
                            ("mode", sv_str("manual")),
                            (
                                "affectedChannelCount",
                                sv_num(invalidated_channels.len() as f64),
                            ),
                            ("browserGeneration", sv_num(browser_generation as f64)),
                        ])),
                    );
                    this.emit_browser_restarted(BrowserRestartedEvent {
                        mode: BrowserRestartedMode::Manual,
                    });
                    (invalidated_channels, true, None)
                }
                Err(error) => {
                    this.logger.error(
                        "browser_restart_failed",
                        Some(&fields(vec![
                            ("mode", sv_str("manual")),
                            ("error", this.safe_error_value(&error)),
                        ])),
                    );
                    // 舊版：非 Error 的擲出值包成 `Browser relaunch failed`；
                    // Rust 的錯誤一律是 DriverError，直接傳播。
                    (invalidated_channels, true, Some(error))
                }
            }
        }
        .await
    }

    /// 舊版 `restartManually`（exclusive 區排隊 → 本體 → finally 的通知與拋錯）。
    async fn restart_manually(self: &Arc<Self>) -> Result<(), DriverError> {
        let slot = self.operation_lock.enqueue();
        let _guard = slot.acquire().await;
        let outcome = self.restart_manually_in_exclusive().await;
        finish_restart_manually(self, outcome).await
    }

    /// 舊版 `startUnlocked`：實際的啟動流程（exclusive 區內呼叫）。
    async fn start_unlocked(self: &Arc<Self>) -> Result<(), DriverError> {
        {
            let state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if state.browser.is_some() && state.context.is_some() {
                return Ok(());
            }
        }

        let mut browser: Option<Arc<dyn BrowserAdapter>> = None;
        let mut context: Option<Arc<dyn BrowserContextAdapter>> = None;
        let mut unsubscribe_browser: Option<CancelListener> = None;

        let result: Result<(), DriverError> = async {
            let next_browser_generation = {
                let state = self
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                state.browser_generation + 1
            };
            let launched = self
                .launcher
                .launch(BrowserLaunchOptions {
                    headless: self.config.headless,
                })
                .await?;
            // 斷線 handler：比對 browser 識別（Arc）與 generation（舊版以物件
            // 身分比對 `this.browser !== disconnectedBrowser`）。
            let disconnect_handler: Box<dyn Fn() + Send + Sync> = {
                let this = Arc::clone(self);
                let browser = Arc::clone(&launched);
                Box::new(move || {
                    let this = Arc::clone(&this);
                    let browser = Arc::clone(&browser);
                    tokio::spawn(async move {
                        this.handle_browser_disconnected(browser, next_browser_generation)
                            .await;
                    });
                })
            };
            unsubscribe_browser = Some(launched.on_disconnected(disconnect_handler));
            // 錯誤清理用（舊版的 local `browser` 在 newContext 失敗時也有值）。
            browser = Some(Arc::clone(&launched));

            let new_context = launched
                .new_context(BrowserContextOptions {
                    storage_state: self.config.storage_state_path.clone(),
                    viewport: BrowserViewport {
                        width: self.config.browser.viewport_width,
                        height: self.config.browser.viewport_height,
                    },
                })
                .await?;
            context = Some(Arc::clone(&new_context));
            new_context
                .configure_resource_blocking(ResourceBlockingOptions {
                    block_images: self.config.browser.block_images,
                    block_fonts: self.config.browser.block_fonts,
                    block_known_tracking: self.config.browser.block_known_tracking,
                })
                .await?;
            new_context
                .configure_chat_blocking(self.config.browser.disable_chat)
                .await?;

            browser = Some(Arc::clone(&launched));
            {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                state.browser = Some(launched);
                state.context = Some(new_context);
                state.unsubscribe_browser = unsubscribe_browser.take();
                state.browser_generation = next_browser_generation;
                state.tracked_channels = Vec::new();
                state.correlated_page_crashes = Vec::new();
                state.page_crash_recycle_generation = None;
            }
            Ok(())
        }
        .await;

        if let Err(error) = result {
            if let Some(unsubscribe) = unsubscribe_browser.take() {
                unsubscribe();
            }
            let resources = DetachedResources {
                browser: browser.take(),
                context: context.take(),
                pages: Vec::new(),
                unsubscribe_browser: None,
            };
            self.close_resources_unlocked(resources, "start_failure")
                .await;
            return Err(error);
        }
        Ok(())
    }

    /// 舊版 `attachPageUnlocked`：建立 entry、註冊六個 listener。
    ///
    /// Rust 的 listener 註冊不會失敗（舊版失敗時 detach 已註冊者的防禦分支
    /// 不需要）。
    fn attach_page_unlocked(
        self: &Arc<Self>,
        channel: &str,
        adapter: Arc<dyn BrowserPageAdapter>,
        browser_generation: u64,
        page_generation: u64,
    ) -> PageEntry {
        let mut entry = PageEntry {
            adapter: Arc::clone(&adapter),
            browser_generation,
            page_generation,
            unsubscribes: Vec::with_capacity(6),
        };

        // crash／close：通知 invalidation（背景任務，對應 `void this.handle…`）。
        let invalidation_listener =
            |this: &Arc<Self>, reason: BrowserInvalidationReason| -> Box<dyn Fn() + Send + Sync> {
                let this = Arc::clone(this);
                let channel = channel.to_string();
                let adapter = Arc::clone(&adapter);
                Box::new(move || {
                    let this = Arc::clone(&this);
                    let adapter = Arc::clone(&adapter);
                    let channel = channel.clone();
                    tokio::spawn(async move {
                        this.handle_page_invalidation(
                            &channel,
                            adapter,
                            browser_generation,
                            page_generation,
                            reason,
                        )
                        .await;
                    });
                })
            };
        entry
            .unsubscribes
            .push(adapter.on_crash(invalidation_listener(
                self,
                BrowserInvalidationReason::PageCrashed,
            )));
        entry
            .unsubscribes
            .push(adapter.on_close(invalidation_listener(
                self,
                BrowserInvalidationReason::PageClosed,
            )));

        // popup：立即關閉（不加入 channel registry）。
        {
            let this = Arc::clone(self);
            let channel = channel.to_string();
            entry
                .unsubscribes
                .push(adapter.on_popup(Box::new(move |popup| {
                    let this = Arc::clone(&this);
                    let channel = channel.clone();
                    tokio::spawn(async move {
                        this.close_unexpected_popup(&channel, popup).await;
                    });
                })));
        }

        // 診斷：分級後以 channel＋generation 記錄。
        let generations = (browser_generation, page_generation);
        {
            let this = Arc::clone(self);
            let channel = channel.to_string();
            entry.unsubscribes.push(adapter.on_request_failed(Box::new(
                move |diagnostic: &BrowserRequestFailureDiagnostic| {
                    this.log_request_failure(&channel, generations, diagnostic);
                },
            )));
        }
        {
            let this = Arc::clone(self);
            let channel = channel.to_string();
            entry.unsubscribes.push(adapter.on_response(Box::new(
                move |diagnostic: &BrowserHttpResponseDiagnostic| {
                    this.log_http_response(&channel, generations, diagnostic);
                },
            )));
        }
        {
            let this = Arc::clone(self);
            let channel = channel.to_string();
            entry.unsubscribes.push(adapter.on_console(Box::new(
                move |diagnostic: &BrowserConsoleDiagnostic| {
                    this.log_console_message(&channel, generations, diagnostic);
                },
            )));
        }
        entry
    }

    /// 舊版 `reattachPageListeners`：重建既有 entry 的六個 listener（沿用同一
    /// entry 的 generation；舊版程式內沒有呼叫端，照原樣移植）。
    fn reattach_page_listeners(self: &Arc<Self>, channel: &str, entry: &mut PageEntry) {
        let adapter = Arc::clone(&entry.adapter);
        let invalidation_listener =
            |this: &Arc<Self>, reason: BrowserInvalidationReason| -> Box<dyn Fn() + Send + Sync> {
                let this = Arc::clone(this);
                let channel = channel.to_string();
                let adapter = Arc::clone(&adapter);
                let browser_generation = entry.browser_generation;
                let page_generation = entry.page_generation;
                Box::new(move || {
                    let this = Arc::clone(&this);
                    let adapter = Arc::clone(&adapter);
                    let channel = channel.clone();
                    tokio::spawn(async move {
                        this.handle_page_invalidation(
                            &channel,
                            adapter,
                            browser_generation,
                            page_generation,
                            reason,
                        )
                        .await;
                    });
                })
            };
        entry.unsubscribes.clear();
        entry
            .unsubscribes
            .push(entry.adapter.on_crash(invalidation_listener(
                self,
                BrowserInvalidationReason::PageCrashed,
            )));
        entry
            .unsubscribes
            .push(entry.adapter.on_close(invalidation_listener(
                self,
                BrowserInvalidationReason::PageClosed,
            )));
        {
            let this = Arc::clone(self);
            let channel = channel.to_string();
            entry
                .unsubscribes
                .push(entry.adapter.on_popup(Box::new(move |popup| {
                    let this = Arc::clone(&this);
                    let channel = channel.clone();
                    tokio::spawn(async move {
                        this.close_unexpected_popup(&channel, popup).await;
                    });
                })));
        }
        let generations = (entry.browser_generation, entry.page_generation);
        {
            let this = Arc::clone(self);
            let channel = channel.to_string();
            entry
                .unsubscribes
                .push(entry.adapter.on_request_failed(Box::new(
                    move |diagnostic: &BrowserRequestFailureDiagnostic| {
                        this.log_request_failure(&channel, generations, diagnostic);
                    },
                )));
        }
        {
            let this = Arc::clone(self);
            let channel = channel.to_string();
            entry.unsubscribes.push(entry.adapter.on_response(Box::new(
                move |diagnostic: &BrowserHttpResponseDiagnostic| {
                    this.log_http_response(&channel, generations, diagnostic);
                },
            )));
        }
        {
            let this = Arc::clone(self);
            let channel = channel.to_string();
            entry.unsubscribes.push(entry.adapter.on_console(Box::new(
                move |diagnostic: &BrowserConsoleDiagnostic| {
                    this.log_console_message(&channel, generations, diagnostic);
                },
            )));
        }
    }

    /// 舊版 `closeUnexpectedPopup`：popup 立即關閉；成功記 `browser_popup_blocked`。
    async fn close_unexpected_popup(&self, channel: &str, popup: Arc<dyn BrowserPage>) {
        if !popup.is_closed() {
            match popup.close().await {
                Ok(()) => {
                    self.logger.warn(
                        "browser_popup_blocked",
                        Some(&fields(vec![("channel", sv_str(channel))])),
                    );
                }
                Err(error) => {
                    self.logger.warn(
                        "browser_popup_close_failed",
                        Some(&fields(vec![
                            ("channel", sv_str(channel)),
                            ("error", self.safe_error_value(&error)),
                        ])),
                    );
                }
            }
            return;
        }
        // popup 已關閉：仍記 blocked（對應舊版 if 之後直接 warn 的流程）。
        self.logger.warn(
            "browser_popup_blocked",
            Some(&fields(vec![("channel", sv_str(channel))])),
        );
    }

    /// 舊版 `handlePageInvalidation`：page crash／非預期 close 的清理與通知。
    async fn handle_page_invalidation(
        self: &Arc<Self>,
        channel: &str,
        adapter: Arc<dyn BrowserPageAdapter>,
        browser_generation: u64,
        page_generation: u64,
        reason: BrowserInvalidationReason,
    ) {
        // 舊版用 `Date.now()`（非注入時鐘）量 durationMs。
        let started_at = Instant::now();
        self.logger.debug(
            "browser_page_invalidation_started",
            Some(&fields(vec![
                ("channel", sv_str(channel)),
                ("reason", sv_str(reason.as_str())),
                ("pageCountBefore", sv_num(self.get_page_count() as f64)),
                ("browserGeneration", sv_num(browser_generation as f64)),
                ("pageGeneration", sv_num(page_generation as f64)),
            ])),
        );

        let this = Arc::clone(self);
        let channel_owned = channel.to_string();
        let adapter_for_close = Arc::clone(&adapter);
        let should_notify = self
            .run_exclusive(async move {
                let current_generation = {
                    let state = this
                        .state
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    state
                        .pages
                        .get(&channel_owned)
                        .map(|entry| entry.page_generation)
                };
                if current_generation != Some(page_generation) {
                    // entry 已不在 registry（先行清理或已換頁）：不通知。
                    this.logger.debug(
                        "browser_page_invalidation_completed",
                        Some(&fields(vec![
                            ("channel", sv_str(&channel_owned)),
                            ("reason", sv_str(reason.as_str())),
                            ("pageKnown", sv_bool(false)),
                            ("notified", sv_bool(false)),
                            ("pageCountAfter", sv_num(this.get_page_count() as f64)),
                            (
                                "durationMs",
                                sv_num(started_at.elapsed().as_millis() as f64),
                            ),
                            ("browserGeneration", sv_num(browser_generation as f64)),
                            ("pageGeneration", sv_num(page_generation as f64)),
                        ])),
                    );
                    return false;
                }

                let mut removed_entry = {
                    let mut state = this
                        .state
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    state.pages.remove(&channel_owned)
                };
                if let Some(entry) = removed_entry.as_mut() {
                    for unsubscribe in entry.unsubscribes.drain(..) {
                        unsubscribe();
                    }
                }

                // crash 時才清理 page adapter（非預期 close 的頁面已關閉）。
                if reason == BrowserInvalidationReason::PageCrashed {
                    this.close_page_adapter_for_cleanup(
                        &adapter_for_close,
                        Some(&channel_owned),
                        "crash",
                    )
                    .await;
                }

                this.logger.warn(
                    reason.as_str(),
                    Some(&fields(vec![
                        ("channel", sv_str(&channel_owned)),
                        ("browserGeneration", sv_num(browser_generation as f64)),
                        ("pageGeneration", sv_num(page_generation as f64)),
                    ])),
                );
                this.logger.debug(
                    "browser_page_invalidation_completed",
                    Some(&fields(vec![
                        ("channel", sv_str(&channel_owned)),
                        ("reason", sv_str(reason.as_str())),
                        ("pageKnown", sv_bool(true)),
                        ("notified", sv_bool(true)),
                        ("pageCountAfter", sv_num(this.get_page_count() as f64)),
                        (
                            "durationMs",
                            sv_num(started_at.elapsed().as_millis() as f64),
                        ),
                        ("browserGeneration", sv_num(browser_generation as f64)),
                        ("pageGeneration", sv_num(page_generation as f64)),
                    ])),
                );
                true
            })
            .await;

        if should_notify {
            self.notify_invalidation(BrowserInvalidation {
                channel: channel.to_string(),
                reason,
            })
            .await;
            self.logger.debug(
                "browser_page_invalidation_notified",
                Some(&fields(vec![
                    ("channel", sv_str(channel)),
                    ("reason", sv_str(reason.as_str())),
                    (
                        "durationMs",
                        sv_num(started_at.elapsed().as_millis() as f64),
                    ),
                    ("browserGeneration", sv_num(browser_generation as f64)),
                    ("pageGeneration", sv_num(page_generation as f64)),
                ])),
            );
            if reason == BrowserInvalidationReason::PageCrashed {
                self.record_page_crash(channel, browser_generation, page_generation);
            }
        }
    }

    /// 舊版 `handleBrowserDisconnected`：清理、通知全部 tracked channel，並依
    /// 設定排程自動重啟或升級 fatal recovery。
    async fn handle_browser_disconnected(
        self: &Arc<Self>,
        disconnected_browser: Arc<dyn BrowserAdapter>,
        disconnected_browser_generation: u64,
    ) {
        let this = Arc::clone(self);
        let browser_for_check = Arc::clone(&disconnected_browser);
        let (invalidated_channels, escalate_to_container, restart_schedule) = self
            .run_exclusive(async move {
                // 舊版 `this.browser !== disconnectedBrowser`：以 Arc 識別比對。
                let is_current_browser = {
                    let state = this
                        .state
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    state
                        .browser
                        .as_ref()
                        .is_some_and(|current| Arc::ptr_eq(current, &browser_for_check))
                };
                if !is_current_browser {
                    return (Vec::new(), false, None);
                }

                let invalidated_channels;
                let recovery_epoch;
                let resources;
                {
                    let mut state = this
                        .state
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    invalidated_channels = state.tracked_channels.clone();
                    state.recovery_epoch += 1;
                    recovery_epoch = state.recovery_epoch;
                    resources = this.detach_resources_unlocked(&mut state);
                    this.logger.warn(
                        "browser_disconnected",
                        Some(&fields(vec![
                            ("affectedChannels", sv_str_array(&invalidated_channels)),
                            (
                                "affectedChannelCount",
                                sv_num(invalidated_channels.len() as f64),
                            ),
                            (
                                "browserGeneration",
                                sv_num(disconnected_browser_generation as f64),
                            ),
                        ])),
                    );
                }
                this.close_resources_unlocked(resources, "disconnect").await;

                let mut escalate_to_container = false;
                let mut restart_schedule = None;
                {
                    let mut state = this
                        .state
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    if state.desired_running && this.config.browser.restart_on_crash {
                        escalate_to_container = this.record_browser_failure(&mut state);
                        if !escalate_to_container {
                            restart_schedule = Inner::next_restart_schedule_locked(
                                &this,
                                &mut state,
                                recovery_epoch,
                                true,
                            );
                        }
                    }
                }
                (
                    invalidated_channels,
                    escalate_to_container,
                    restart_schedule,
                )
            })
            .await;

        self.notify_invalidations(
            &invalidated_channels,
            BrowserInvalidationReason::BrowserDisconnected,
        )
        .await;

        if escalate_to_container {
            self.request_fatal_recovery(
                "browser_crash_loop",
                Some(vec![
                    fatal_num(
                        "browserFailureWindowMs",
                        self.browser_failure_window_ms as f64,
                    ),
                    fatal_num(
                        "browserFailureContainerThreshold",
                        self.browser_failure_container_threshold,
                    ),
                ]),
            );
            return;
        }
        if let Some(schedule) = restart_schedule {
            self.schedule_automatic_restart(schedule);
        }
    }

    /// 舊版 `nextRestartSchedule`（鎖內呼叫）：退避排程與次數上限。
    ///
    /// `reset_attempts_after_stable_period` 為 true 時（disconnect 觸發），依
    /// `restartAttemptResetMs` 重置 attempts；false（重啟失敗的重試）不重置。
    fn next_restart_schedule_locked(
        this: &Arc<Self>,
        state: &mut ManagerState,
        recovery_epoch: u64,
        reset_attempts_after_stable_period: bool,
    ) -> Option<RestartSchedule> {
        if reset_attempts_after_stable_period {
            let crash_at = (this.now)();
            let reset = match state.last_browser_crash_at {
                None => true,
                Some(last) => crash_at.saturating_sub(last) >= this.restart_attempt_reset_ms,
            };
            if reset {
                state.automatic_restart_attempts = 0;
            }
            state.last_browser_crash_at = Some(crash_at);
        }

        if state.automatic_restart_attempts >= this.max_automatic_restart_attempts {
            this.logger.error(
                "browser_restart_limit_reached",
                Some(&fields(vec![(
                    "maxAttempts",
                    sv_num(this.max_automatic_restart_attempts as f64),
                )])),
            );
            this.request_fatal_recovery(
                "automatic_restart_limit_reached",
                Some(vec![fatal_num(
                    "maxAttempts",
                    this.max_automatic_restart_attempts as f64,
                )]),
            );
            return None;
        }

        state.automatic_restart_attempts += 1;
        let attempt = state.automatic_restart_attempts;
        let delay_ms = restart_backoff_delay(
            this.restart_backoff_ms,
            this.restart_backoff_max_ms,
            attempt,
        );
        this.logger.warn(
            "browser_restart_scheduled",
            Some(&fields(vec![
                ("attempt", sv_num(attempt as f64)),
                ("delayMs", sv_num(delay_ms as f64)),
            ])),
        );
        Some(RestartSchedule {
            attempt,
            delay_ms,
            recovery_epoch,
        })
    }

    /// 舊版 `scheduleAutomaticRestart`：不同 recovery epoch 的排程互換
    ///（token 語意）；同 epoch 已排程者不重複。
    fn schedule_automatic_restart(self: &Arc<Self>, schedule: RestartSchedule) {
        {
            let automatic = self
                .automatic_restart
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if automatic
                .as_ref()
                .is_some_and(|record| record.recovery_epoch == schedule.recovery_epoch)
            {
                return;
            }
        }
        let token = self
            .automatic_restart_sequence
            .fetch_add(1, Ordering::SeqCst)
            + 1;
        *self
            .automatic_restart
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(AutomaticRestartRecord {
            token,
            recovery_epoch: schedule.recovery_epoch,
        });

        let this = Arc::clone(self);
        tokio::spawn(async move {
            // 舊版 try：退避睡眠（Rust 的 SleepFn 不可失敗，catch 分支不存在）。
            this.sleep.sleep(schedule.delay_ms).await;
            // 舊版 finally：token 仍是自己時清除排程狀態（新的 crash 排程會
            // 蓋掉 token，此時不清除）。
            {
                let mut automatic = this
                    .automatic_restart
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if automatic
                    .as_ref()
                    .is_some_and(|record| record.token == token)
                {
                    *automatic = None;
                }
            }
            this.recover_automatically(schedule).await;
        });
    }

    /// 舊版 `recoverAutomatically`：退避後的實際重啟（需 desiredRunning、
    /// recovery epoch 相符且瀏覽器尚未重建）。
    async fn recover_automatically(self: &Arc<Self>, schedule: RestartSchedule) {
        let this = Arc::clone(self);
        self.run_exclusive(async move {
            {
                let state = this
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if !state.desired_running
                    || schedule.recovery_epoch != state.recovery_epoch
                    || (state.browser.is_some() && state.context.is_some())
                {
                    return;
                }
            }

            match Inner::start_unlocked(&this).await {
                Ok(()) => {
                    let browser_generation = this
                        .state
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .browser_generation;
                    this.logger.warn(
                        LogEvents::BROWSER_RESTARTED,
                        Some(&fields(vec![
                            ("mode", sv_str("automatic")),
                            ("attempt", sv_num(schedule.attempt as f64)),
                            ("browserGeneration", sv_num(browser_generation as f64)),
                        ])),
                    );
                    this.emit_browser_restarted(BrowserRestartedEvent {
                        mode: BrowserRestartedMode::Automatic,
                    });
                }
                Err(error) => {
                    this.logger.error(
                        "browser_restart_failed",
                        Some(&fields(vec![
                            ("mode", sv_str("automatic")),
                            ("attempt", sv_num(schedule.attempt as f64)),
                            ("error", this.safe_error_value(&error)),
                        ])),
                    );
                    let retry_schedule = {
                        let mut state = this
                            .state
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        Inner::next_restart_schedule_locked(
                            &this,
                            &mut state,
                            schedule.recovery_epoch,
                            false,
                        )
                    };
                    if let Some(retry_schedule) = retry_schedule {
                        this.schedule_automatic_restart(retry_schedule);
                    }
                    // retry 為 None 時，nextRestartSchedule 已要求 container restart。
                }
            }
        })
        .await;
    }

    /// 舊版 `emitBrowserRestarted`：同步觀察者；錯誤記 debug。
    fn emit_browser_restarted(&self, event: BrowserRestartedEvent) {
        let Some(observer) = self.on_browser_restarted.as_ref() else {
            return;
        };
        if let Err(error) = observer.on_browser_restarted(event) {
            self.logger.debug(
                "browser_restarted_observer_failed",
                Some(&fields(vec![("error", self.safe_error_value(&error))])),
            );
        }
    }

    // ===== 資源 detach 與清理 =====

    /// 舊版 `detachResourcesUnlocked`：取走全部資源並重置 registry（鎖內呼叫）。
    fn detach_resources_unlocked(&self, state: &mut ManagerState) -> DetachedResources {
        let resources = DetachedResources {
            browser: state.browser.take(),
            context: state.context.take(),
            pages: state
                .pages
                .values()
                .map(|entry| Arc::clone(&entry.adapter))
                .collect(),
            unsubscribe_browser: state.unsubscribe_browser.take(),
        };
        // detach 每頁的 listener（取消函式在 registry entry 上，drain 時逐一呼叫）。
        for (_, mut entry) in state.pages.drain() {
            Self::detach_page_listeners(&mut entry);
        }
        state.tracked_channels = Vec::new();
        if let Some(unsubscribe) = resources.unsubscribe_browser.as_ref() {
            unsubscribe();
        }
        resources
    }

    /// 舊版 `closeResourcesUnlocked`：pages → context → browser 的清理；
    /// 部分失敗不中斷其餘清理。
    async fn close_resources_unlocked(
        &self,
        resources: DetachedResources,
        phase: &str,
    ) -> BrowserTeardownResult {
        let mut page_close_timed_out = false;
        for adapter in &resources.pages {
            let page_outcome = self
                .close_page_adapter_for_cleanup(adapter, None, phase)
                .await;
            if page_outcome == CloseOutcome::TimedOut {
                page_close_timed_out = true;
            }
        }

        if let Some(context) = resources.context.as_ref() {
            self.close_resource_for_cleanup(context.close(), "browser_context_close_failed", phase)
                .await;
        }

        let mut browser_close_timed_out = false;
        let mut browser_close_failed = false;
        if let Some(browser) = resources.browser.as_ref() {
            // 已斷線：wrapper 上的清理錯誤不具致命性。
            if !browser.is_connected() {
                self.close_resource_for_cleanup(browser.close(), "browser_close_failed", phase)
                    .await;
                return BrowserTeardownResult {
                    browser_terminated: true,
                    page_close_timed_out,
                    browser_close_timed_out: false,
                    browser_close_failed: false,
                };
            }
            let browser_outcome = self
                .close_resource_for_cleanup(browser.close(), "browser_close_failed", phase)
                .await;
            browser_close_timed_out = browser_outcome == CloseOutcome::TimedOut;
            browser_close_failed = matches!(browser_outcome, CloseOutcome::Failed { .. });
        }

        // 舊版 `browser === undefined || !browser.isConnected()`。
        let browser_terminated = resources
            .browser
            .as_ref()
            .is_none_or(|browser| !browser.is_connected());
        BrowserTeardownResult {
            browser_terminated,
            page_close_timed_out,
            browser_close_timed_out,
            browser_close_failed,
        }
    }

    /// 舊版 `closePageAdapterForCleanup`：已關閉直接回 `already_closed`；
    /// 失敗記 `browser_page_cleanup_failed`。
    async fn close_page_adapter_for_cleanup(
        &self,
        adapter: &Arc<dyn BrowserPageAdapter>,
        channel: Option<&str>,
        phase: &str,
    ) -> CloseOutcome {
        if adapter.is_closed() {
            return CloseOutcome::AlreadyClosed;
        }
        let outcome = self
            .close_page_adapter_with_timeout(adapter, channel, phase)
            .await;
        if let CloseOutcome::Failed { error } = &outcome {
            let mut entries = Vec::new();
            if let Some(channel) = channel {
                entries.push(("channel", sv_str(channel)));
            }
            entries.push(("phase", sv_str(phase)));
            entries.push(("error", self.safe_error_value(error)));
            self.logger
                .warn("browser_page_cleanup_failed", Some(&fields(entries)));
        }
        outcome
    }

    /// 舊版 `closeResourceForCleanup`（context／browser 共用；逾時事件名稱由
    /// `event.replace('_failed', '_timeout')` 衍生）。
    async fn close_resource_for_cleanup(
        &self,
        close: impl Future<Output = Result<(), DriverError>>,
        event: &str,
        phase: &str,
    ) -> CloseOutcome {
        let outcome = self
            .close_with_timeout(
                close,
                &timeout_event_of(event),
                vec![("phase", phase.to_string())],
            )
            .await;
        if let CloseOutcome::Failed { error } = &outcome {
            self.logger.warn(
                event,
                Some(&fields(vec![
                    ("phase", sv_str(phase)),
                    ("error", self.safe_error_value(error)),
                ])),
            );
        }
        outcome
    }

    /// 舊版 `closePageAdapterWithTimeout`：已關閉直接回 `already_closed`。
    async fn close_page_adapter_with_timeout(
        &self,
        adapter: &Arc<dyn BrowserPageAdapter>,
        channel: Option<&str>,
        phase: &str,
    ) -> CloseOutcome {
        if adapter.is_closed() {
            return CloseOutcome::AlreadyClosed;
        }
        let mut base_fields = Vec::new();
        if let Some(channel) = channel {
            base_fields.push(("channel", channel.to_string()));
        }
        base_fields.push(("phase", phase.to_string()));
        self.close_with_timeout(adapter.close(), "browser_page_close_timeout", base_fields)
            .await
    }

    /// 舊版 `closeWithTimeout`：race close promise 與逾時；逾時永遠不算成功。
    ///
    /// Rust 版以 `tokio::time::timeout` 表達 race；逾時時 close future 被 drop
    ///（對應舊版「late rejection 不處理」的防線）。基礎欄位以純字串傳遞，
    /// 每個日誌點才組 [`SanitizedValue`]（含 Rc 不可跨 await 持有，Send 安全）。
    async fn close_with_timeout(
        &self,
        close: impl Future<Output = Result<(), DriverError>>,
        timeout_event: &str,
        base_fields: Vec<(&'static str, String)>,
    ) -> CloseOutcome {
        let started_at = Instant::now();
        // started：`{...fields, timeoutEvent, timeoutMs}`。
        self.logger.debug(
            "browser_resource_close_started",
            Some(&fields(close_entries(
                base_fields.clone(),
                Some(timeout_event),
                Some(self.resource_close_timeout_ms),
                None,
                None,
            ))),
        );

        let result =
            tokio::time::timeout(Duration::from_millis(self.resource_close_timeout_ms), close)
                .await;
        let duration_ms = started_at.elapsed().as_millis() as f64;
        match result {
            Err(_) => {
                // warn：`{...fields, timeoutMs}`（不含 timeoutEvent，照舊版）。
                self.logger.warn(
                    timeout_event,
                    Some(&fields(close_entries(
                        base_fields.clone(),
                        None,
                        Some(self.resource_close_timeout_ms),
                        None,
                        None,
                    ))),
                );
                self.logger.debug(
                    "browser_resource_close_completed",
                    Some(&fields(close_entries(
                        base_fields,
                        Some(timeout_event),
                        None,
                        Some(true),
                        Some(("timed_out", duration_ms)),
                    ))),
                );
                CloseOutcome::TimedOut
            }
            Ok(Ok(())) => {
                self.logger.debug(
                    "browser_resource_close_completed",
                    Some(&fields(close_entries(
                        base_fields,
                        Some(timeout_event),
                        None,
                        Some(false),
                        Some(("closed", duration_ms)),
                    ))),
                );
                CloseOutcome::Closed
            }
            Ok(Err(error)) => {
                self.logger.debug(
                    "browser_resource_close_completed",
                    Some(&fields(close_entries(
                        base_fields,
                        Some(timeout_event),
                        None,
                        Some(false),
                        Some(("failed", duration_ms)),
                    ))),
                );
                CloseOutcome::Failed { error }
            }
        }
    }

    // ===== crash 迴圈與 failure breaker =====

    /// 舊版 `recordPageCrash`：多頻道連環 crash 的偵測與 full recycle 排程。
    ///
    /// 全程同步（鎖內無 await）：與舊版的單一同步函式等價。
    fn record_page_crash(
        self: &Arc<Self>,
        channel: &str,
        browser_generation: u64,
        page_generation: u64,
    ) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if browser_generation != state.browser_generation
            || state.browser.is_none()
            || state.context.is_none()
            || self
                .restart_flight
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_some()
            || state.page_crash_recycle_generation == Some(browser_generation)
        {
            return;
        }

        let now_ms = (self.now)();
        state.correlated_page_crashes.retain(|(_, timestamp_ms)| {
            now_ms.saturating_sub(*timestamp_ms) <= self.multi_channel_crash_window_ms
        });
        let normalized_channel = normalize_channel(channel);
        // 舊版 `Map.set`：既有鍵原地更新時間戳（插入順序不變）。
        match state
            .correlated_page_crashes
            .iter_mut()
            .find(|(name, _)| name == &normalized_channel)
        {
            Some((_, timestamp_ms)) => *timestamp_ms = now_ms,
            None => state
                .correlated_page_crashes
                .push((normalized_channel, now_ms)),
        }
        let affected_channels: Vec<String> = state
            .correlated_page_crashes
            .iter()
            .map(|(name, _)| name.clone())
            .collect();
        if (affected_channels.len() as f64) < self.multi_channel_crash_threshold {
            return;
        }

        state.page_crash_recycle_generation = Some(browser_generation);
        let reason = "multi_channel_page_crash";
        self.logger.warn(
            "browser_crash_loop_recycle_requested",
            Some(&fields(vec![
                ("reason", sv_str(reason)),
                ("channel", sv_str(channel)),
                ("affectedChannels", sv_str_array(&affected_channels)),
                (
                    "distinctChannelCount",
                    sv_num(affected_channels.len() as f64),
                ),
                ("browserGeneration", sv_num(browser_generation as f64)),
                ("pageGeneration", sv_num(page_generation as f64)),
            ])),
        );

        // crash 驅動的 full recycle 計入 browser failure breaker。
        if self.record_browser_failure(&mut state) {
            self.request_fatal_recovery(
                "browser_crash_loop",
                Some(vec![
                    fatal_str("reason", reason),
                    fatal_str("channel", channel),
                ]),
            );
            return;
        }

        let flight = self.begin_restart_flight();
        let channel = channel.to_string();
        drop(state);
        let this = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(error) = flight.completed().await {
                this.logger.error(
                    "browser_crash_loop_recycle_failed",
                    Some(&fields(vec![
                        ("reason", sv_str("multi_channel_page_crash")),
                        ("error", this.safe_error_value(&error)),
                    ])),
                );
                this.request_fatal_recovery(
                    "browser_crash_loop_recycle_failed",
                    Some(vec![
                        fatal_str("reason", "multi_channel_page_crash"),
                        fatal_str("channel", &channel),
                    ]),
                );
            }
        });
    }

    /// 舊版 `recordBrowserFailure`（鎖內呼叫）：記錄一次意外失敗；達門檻時
    /// 應要求 container restart 而不是再做一次 browser recycle。
    fn record_browser_failure(&self, state: &mut ManagerState) -> bool {
        let now_ms = (self.now)();
        state.browser_failure_timestamps = evict_old_timestamps(
            std::mem::take(&mut state.browser_failure_timestamps),
            now_ms,
            self.browser_failure_window_ms,
        );
        state.browser_failure_timestamps.push(now_ms);
        state.browser_failure_timestamps.len() as f64 >= self.browser_failure_container_threshold
    }

    /// 舊版 `requestFatalRecovery`：fire-and-forget 的觀察者呼叫；錯誤記錄。
    fn request_fatal_recovery(
        self: &Arc<Self>,
        reason: &str,
        request_fields: Option<Vec<(String, FatalRecoveryFieldValue)>>,
    ) {
        let Some(observer) = self.on_fatal_recovery.as_ref() else {
            return;
        };
        let request = FatalRecoveryRequest {
            reason: reason.to_string(),
            fields: request_fields,
        };
        let logged_reason = reason.to_string();
        let this = Arc::clone(self);
        let observer = Arc::clone(observer);
        tokio::spawn(async move {
            if let Err(error) = observer.on_fatal_recovery(request).await {
                this.logger.error(
                    "browser_fatal_recovery_observer_failed",
                    Some(&fields(vec![
                        ("reason", sv_str(logged_reason)),
                        ("error", this.safe_error_value(&error)),
                    ])),
                );
            }
        });
    }

    // ===== listener detach 與診斷分級 =====

    /// 舊版 `detachPageListeners`：取消六個 listener 並清空清單。
    fn detach_page_listeners(entry: &mut PageEntry) {
        for unsubscribe in entry.unsubscribes.drain(..) {
            unsubscribe();
        }
    }

    /// 舊版 `logRequestFailure`：消毒後的端點＋分級層級記錄。
    fn log_request_failure(
        &self,
        channel: &str,
        generations: (u64, u64),
        diagnostic: &BrowserRequestFailureDiagnostic,
    ) {
        let endpoint = sanitize_browser_url(Some(&diagnostic.url));
        let level = decide_request_failure_level(
            endpoint.endpoint_category,
            diagnostic.failure_text.as_deref(),
        );
        if level == DiagnosticLogLevel::None {
            return;
        }
        let mut entries = vec![
            ("channel", sv_str(channel)),
            ("browserGeneration", sv_num(generations.0 as f64)),
            ("pageGeneration", sv_num(generations.1 as f64)),
            (
                "endpointCategory",
                sv_str(endpoint.endpoint_category.as_str()),
            ),
            ("host", sv_str(endpoint.host)),
            ("path", sv_str(endpoint.path)),
            ("method", sv_str(&diagnostic.method)),
            ("resourceType", sv_str(&diagnostic.resource_type)),
        ];
        if let Some(failure_text) = diagnostic.failure_text.as_ref() {
            entries.push(("failureText", sv_str(failure_text)));
        }
        if let Some(names) = non_empty(&diagnostic.graphql_operation_names) {
            entries.push(("graphQlOperationNames", sv_str_array(names)));
        }
        log_at(
            &self.logger,
            level,
            "browser_request_failed",
            Some(&fields(entries)),
        );
    }

    /// 舊版 `logHttpResponse`。
    fn log_http_response(
        &self,
        channel: &str,
        generations: (u64, u64),
        diagnostic: &BrowserHttpResponseDiagnostic,
    ) {
        let endpoint = sanitize_browser_url(Some(&diagnostic.url));
        let level = decide_http_response_level(
            u16::try_from(diagnostic.status).unwrap_or(u16::MAX),
            &diagnostic.resource_type,
            endpoint.endpoint_category,
        );
        if level == DiagnosticLogLevel::None {
            return;
        }
        let mut entries = vec![
            ("channel", sv_str(channel)),
            ("browserGeneration", sv_num(generations.0 as f64)),
            ("pageGeneration", sv_num(generations.1 as f64)),
            (
                "endpointCategory",
                sv_str(endpoint.endpoint_category.as_str()),
            ),
            ("host", sv_str(endpoint.host)),
            ("path", sv_str(endpoint.path)),
            ("method", sv_str(&diagnostic.method)),
            ("resourceType", sv_str(&diagnostic.resource_type)),
            ("httpStatus", sv_num(diagnostic.status as f64)),
            ("statusText", sv_str(&diagnostic.status_text)),
        ];
        if let Some(names) = non_empty(&diagnostic.graphql_operation_names) {
            entries.push(("graphQlOperationNames", sv_str_array(names)));
        }
        log_at(
            &self.logger,
            level,
            "browser_http_response",
            Some(&fields(entries)),
        );
    }

    /// 舊版 `logConsoleMessage`：位置 URL 再消毒一次（泵輸出的 host＋path 形狀
    /// 對本函式是恆等）。
    fn log_console_message(
        &self,
        channel: &str,
        generations: (u64, u64),
        diagnostic: &BrowserConsoleDiagnostic,
    ) {
        let source = diagnostic
            .source_url
            .as_ref()
            .map(|source_url| sanitize_browser_url(Some(source_url)));
        let level = decide_console_level(
            &diagnostic.r#type,
            &diagnostic.text,
            source.as_ref().map(|endpoint| endpoint.endpoint_category),
        );
        if level == DiagnosticLogLevel::None {
            return;
        }
        let mut entries = vec![
            ("channel", sv_str(channel)),
            ("browserGeneration", sv_num(generations.0 as f64)),
            ("pageGeneration", sv_num(generations.1 as f64)),
            ("consoleType", sv_str(&diagnostic.r#type)),
            ("message", sv_str(&diagnostic.text)),
        ];
        if let Some(source) = source.as_ref() {
            entries.push(("sourceHost", sv_str(&source.host)));
            if !source.path.is_empty() {
                entries.push(("sourcePath", sv_str(&source.path)));
            }
        }
        entries.push(("lineNumber", sv_num(diagnostic.line_number as f64)));
        entries.push(("columnNumber", sv_num(diagnostic.column_number as f64)));
        log_at(
            &self.logger,
            level,
            "browser_console_message",
            Some(&fields(entries)),
        );
    }

    // ===== invalidation 通知 =====

    /// 舊版 `notifyInvalidations`：全部頻道並行通知（`Promise.all`）。
    async fn notify_invalidations(&self, channels: &[String], reason: BrowserInvalidationReason) {
        let notifications = channels
            .iter()
            .map(|channel| {
                self.notify_invalidation(BrowserInvalidation {
                    channel: channel.clone(),
                    reason,
                })
            })
            .collect::<Vec<_>>();
        futures_util::future::join_all(notifications).await;
    }

    /// 舊版 `notifyInvalidation`：觀察者錯誤記 `browser_invalidation_observer_failed`。
    async fn notify_invalidation(&self, invalidation: BrowserInvalidation) {
        let Some(observer) = self.on_invalidated.as_ref() else {
            return;
        };
        let channel = invalidation.channel.clone();
        let reason = invalidation.reason;
        if let Err(error) = observer.on_invalidated(invalidation).await {
            self.logger.error(
                "browser_invalidation_observer_failed",
                Some(&fields(vec![
                    ("channel", sv_str(channel)),
                    ("reason", sv_str(reason.as_str())),
                    ("error", self.safe_error_value(&error)),
                ])),
            );
        }
    }

    /// 舊版 `safeError`：`{name, message}` 並消毒、遮蔽 storageState 路徑。
    fn safe_error(&self, error: &DriverError) -> (String, String) {
        let name = redact_sensitive_string(&error.name);
        let redacted_message = redact_sensitive_string(&error.message);
        let storage_state_path = &self.config.storage_state_path;
        let message = if storage_state_path.is_empty() {
            redacted_message
        } else {
            redacted_message.replace(storage_state_path.as_str(), "[REDACTED]")
        };
        (name, message)
    }

    /// 日誌用的 `error` 欄位（對應 `sanitizeError` 的輸出形狀）。
    fn safe_error_value(&self, error: &DriverError) -> SanitizedValue {
        let (name, message) = self.safe_error(error);
        SanitizedValue::Error(LogError {
            name,
            message,
            cause: None,
            extra: Vec::new(),
        })
    }

    // ===== restart flight（single-flight）=====

    /// 目前登記中的 restart flight（`createPage` 等待用）。
    fn current_restart_flight(&self) -> Option<RestartFlight> {
        let flight = self
            .restart_flight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        flight.as_ref().map(|record| RestartFlight {
            completion: record.completion.clone(),
        })
    }

    /// 舊版 `restart()` 的 flight 登記（同步）：已有 flight 就回同一個；
    /// 否則建立 slot 並 spawn flight 任務。
    fn begin_restart_flight(self: &Arc<Self>) -> RestartFlight {
        let (completion_tx, completion_rx) = tokio::sync::watch::channel(None);
        let id = self.flight_sequence.fetch_add(1, Ordering::SeqCst) + 1;
        let joined;
        {
            let mut flight = self
                .restart_flight
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match flight.as_ref() {
                Some(existing) => {
                    joined = Some(existing.completion.clone());
                }
                None => {
                    *flight = Some(RestartFlightRecord {
                        completion: completion_rx.clone(),
                        id,
                    });
                    joined = None;
                }
            }
        }
        if let Some(existing) = joined {
            return RestartFlight {
                completion: existing,
            };
        }

        // 舊版 restart() 在呼叫當下就把 restartManually 的 exclusive 區接上
        // operationTail（同步）；這裡同步登記佇列位置，flight 任務取得鎖後
        // 執行本體（不重複排隊）。
        let exclusive_slot = self.operation_lock.enqueue();
        let this = Arc::clone(self);
        tokio::spawn(async move {
            // exclusive 區結束後的收尾（舊版 restartManually 的後半）：
            // finally 的失效通知 → 終止未確認的 fatal 與拋錯。
            let result = (async {
                let _guard = exclusive_slot.acquire().await;
                this.restart_manually_in_exclusive().await
            })
            .await;
            let result = finish_restart_manually(&this, result).await;
            let _ = completion_tx.send(Some(result));
            // 完成後清除 slot（仍是本 flight 時；新一代 flight 不清）。
            let mut flight = this
                .restart_flight
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if flight.as_ref().is_some_and(|record| record.id == id) {
                *flight = None;
            }
        });
        RestartFlight {
            completion: completion_rx,
        }
    }
}

/// 舊版 `closeWithTimeout` 各日誌點的欄位組合：
/// - `timeout_event`：`Some` 時加上 `timeoutEvent`（started／completed）。
/// - `timeout_ms`：`Some` 時加上 `timeoutMs`（started／逾時的 warn）。
/// - `timed_out`＋`outcome`：completed 日誌的 `timedOut`、`status`、`durationMs`。
fn close_entries(
    base: Vec<(&'static str, String)>,
    timeout_event: Option<&str>,
    timeout_ms: Option<u64>,
    timed_out: Option<bool>,
    outcome: Option<(&'static str, f64)>,
) -> Vec<(&'static str, SanitizedValue)> {
    let mut entries: Vec<(&'static str, SanitizedValue)> = base
        .into_iter()
        .map(|(key, value)| (key, sv_str(value)))
        .collect();
    if let Some(timeout_event) = timeout_event {
        entries.push(("timeoutEvent", sv_str(timeout_event)));
    }
    if let Some(timeout_ms) = timeout_ms {
        entries.push(("timeoutMs", sv_num(timeout_ms as f64)));
    }
    if let Some(timed_out) = timed_out {
        entries.push(("timedOut", sv_bool(timed_out)));
    }
    if let Some((status, duration_ms)) = outcome {
        entries.push(("status", sv_str(status)));
        entries.push(("durationMs", sv_num(duration_ms)));
    }
    entries
}

/// 舊版 `restartManually` 的 exclusive 之後收尾（對應 finally 與 throw）：
/// 失效通知 → 終止未確認時的 fatal recovery 與拋錯 → relaunch 錯誤傳播。
async fn finish_restart_manually(
    this: &Arc<Inner>,
    outcome: (Vec<String>, bool, Option<DriverError>),
) -> Result<(), DriverError> {
    let (invalidated_channels, termination_confirmed, relaunch_error) = outcome;
    // 舊版 finally：失效通知在拋錯之前完成。
    this.notify_invalidations(
        &invalidated_channels,
        BrowserInvalidationReason::BrowserRestarted,
    )
    .await;

    if !termination_confirmed {
        this.request_fatal_recovery(
            "browser_termination_unconfirmed",
            Some(vec![fatal_str("mode", "manual")]),
        );
        // 舊版 `BrowserTerminationError`：名稱保留在 DriverError.name。
        return Err(DriverError::new(
            "BrowserTerminationError",
            TERMINATION_UNCONFIRMED_MESSAGE,
        ));
    }
    match relaunch_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// 舊版退避公式：`Math.min(backoff * 2 ** (attempt - 1), max)`。
///
/// 指數過大時飽和（u64）；`2 ** 63` 已遠大於任何 max 值，結果相同。
fn restart_backoff_delay(backoff_ms: u64, backoff_max_ms: u64, attempt: u64) -> u64 {
    let shift = attempt.saturating_sub(1).min(63);
    backoff_ms.saturating_mul(1u64 << shift).min(backoff_max_ms)
}

/// 舊版的省略條件：`graphQlOperationNames` 非空才輸出（adapter 層已把空名單
/// 折成 None，這裡為防衛性再檢查）。
fn non_empty(names: &Option<Vec<String>>) -> Option<&[String]> {
    match names {
        Some(names) if !names.is_empty() => Some(names),
        _ => None,
    }
}

/// 舊版 `BrowserManager` 介面（types.ts）；由 [`DefaultBrowserManager`] 實作。
///
/// `restart`／`createPage` 的方法第一次被 poll 時同步登記 flight／等待
/// 進行中 flight（呼叫端立即 await，與舊版 promise 語意等價）。
#[async_trait]
pub(crate) trait BrowserManager: Send + Sync {
    async fn start(&self) -> Result<(), DriverError>;
    async fn stop(&self) -> Result<(), DriverError>;
    async fn create_page(&self, channel: &str) -> Result<Arc<dyn BrowserPage>, DriverError>;
    async fn close_page(&self, channel: &str) -> Result<(), DriverError>;
    async fn restart(&self) -> Result<(), DriverError>;
    fn get_page_count(&self) -> usize;
    fn get_browser_generation(&self) -> u64;
    fn get_browser_version(&self) -> Option<String>;
}

#[async_trait]
impl BrowserManager for DefaultBrowserManager {
    async fn start(&self) -> Result<(), DriverError> {
        Inner::start(&self.inner).await
    }

    async fn stop(&self) -> Result<(), DriverError> {
        Inner::stop(&self.inner).await
    }

    async fn create_page(&self, channel: &str) -> Result<Arc<dyn BrowserPage>, DriverError> {
        Inner::create_page(&self.inner, channel).await
    }

    async fn close_page(&self, channel: &str) -> Result<(), DriverError> {
        Inner::close_page(&self.inner, channel).await
    }

    async fn restart(&self) -> Result<(), DriverError> {
        Inner::restart(&self.inner).await
    }

    fn get_page_count(&self) -> usize {
        self.inner.get_page_count()
    }

    fn get_browser_generation(&self) -> u64 {
        self.inner.get_browser_generation()
    }

    fn get_browser_version(&self) -> Option<String> {
        self.inner.get_browser_version()
    }
}

impl DefaultBrowserManager {
    pub(crate) fn new(
        config: BrowserManagerConfig,
        dependencies: BrowserManagerDependencies,
    ) -> Self {
        Self {
            inner: Arc::new(Inner::new(config, dependencies)),
        }
    }

    /// 舊版 `reportNavigationOutcomes`：不屬於 `BrowserManager` 介面，
    /// 是 DefaultBrowserManager 的公開方法（S3.5 的 SessionManager 接線用）。
    ///
    /// recovery epoch 在呼叫當下同步捕捉（對應舊版 promise 建立即捕捉的語意；
    /// 併發回收時，舊 epoch 的批次會被忽略）；回傳的 future 已 'static，
    /// 允許 fire-and-forget。
    pub(crate) fn report_navigation_outcomes(
        &self,
        outcomes: Vec<BrowserNavigationOutcome>,
    ) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        if outcomes.is_empty() {
            // 舊版：outcomes 為空時直接 resolve（不排隊、不捕捉 epoch）。
            return Box::pin(async {});
        }
        let reported_recovery_epoch = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .recovery_epoch;
        let this = Arc::clone(&self.inner);
        Box::pin(async move {
            let slot = this.navigation_outcome_lock.enqueue();
            let _guard = slot.acquire().await;
            this.process_navigation_outcomes(outcomes, reported_recovery_epoch)
                .await;
        })
    }
}

#[cfg(test)]
mod tests;
