//! Watchdog 排程器：`scheduler/WatchdogScheduler.ts` 逐函式移植（§4.2，
//! S3.5 part B）。
//!
//! 語意重點（照舊版程式碼）：
//! - [`WatchdogScheduler`] trait 對應舊版介面；[`DefaultWatchdogScheduler`]
//!   為預設實作。
//! - `start`：`started = true`、以 `checkIntervalSeconds * 1000` 註冊
//!   interval（只在本次 start 進場，重複 start 無效）並**立即** trigger 一次
//!   tick。callback 內含 `if (this.started)` 門檻。
//! - `run_once`：in-flight 門檻（`scheduler_tick_skipped {reason:'in_face'}`）
//!   → rate-limit 門檻（`{reason:'rate_limited', retryAt}`）→ 記
//!   `scheduler_tick_started` → 執行 tick → 記 `scheduler_tick_completed`。
//!   舊版追蹤 promise 鏈的 `scheduler_tick_failed {reason:'unexpected_error'}`
//!   只可能由「可拋錯的注入端」觸發（注入端在 Rust 版全部是無失敗型別，
//!   live-status 的錯誤全數進 `handleApiError`），分支不可達（照 DEC-020 對
//!   不可達記點的前例保留事件名，不呼叫）。
//! - `execute_tick`：live-status（錯誤→`handleApiError`）→
//!   `collectConfiguredLiveStatuses`（缺 configured channel → warn
//!   `twitch_api_temporary_error {reason:'invalid-response'}`）→ 狀態轉換記
//!   `stream_online`/`stream_offline`＋observer 通知（fire-and-forget，tick
//!   不等待）→ 更新 `lastCheckedAt`／清 `retryAt` → select → submit reconcile
//!   （reconcile 也 fire-and-forget）。
//! - `update_config`：先 `await` in-flight、凍結新 config、清掉不在新頻道
//!   內的先前狀態、以先前狀態＋`lastCheckedAt` 組 known statuses、select、
//!   submit、最後 `await run_once()`。
//! - `get_snapshot`：`running`／`checkInFlight`／`retryAt?`／`lastCheckedAt?`
//!   ＋頻道狀態（`isLive` 由先前狀態查表；未知省略為 [`None`]）。
//!
//! 依賴以窄介面注入（§4.2、DEC-020 同類）：
//! - [`SchedulerLiveStatusProvider`] 取代舊版 `LiveStatusProvider`：舊版
//!   `executeTick` 的 catch 把「非三類錯誤」也收到 `handleApiError`（→
//!   `twitch_api_request_failed {kind:'unknown'}`）；Rust 版 twitch 介面把
//!   錯誤定型為三類 [`TwitchApiError`]，此接縫補 [`WatchdogLiveStatusError`
//!   ::Unknown] 保留 catch-all（三類經 blanket impl 自動轉接
//!   [`LiveStatusProvider`] 的正式實作）。
//! - [`SchedulerStreamSelector`] 取代舊版 `StreamSelector` 介面（預設轉催
//!   [`crate::scheduler::selector::select_active_channels`] 的自由函式）；
//!   selector 模組的 `ChannelLiveStatus` 是 S2.5 的最小形（其檔案註明 S3.1
//!   後改 import 的債），本模組以逐欄轉接。
//! - [`SchedulerSessionReconcile`]／[`SchedulerReconcileRequest`] 分別對應
//!   `Pick<SessionManager,'reconcile'>` 與 `Pick<ReconcileCoordinator,
//!   'request'>`（錯誤重用 [`crate::sessions::reconcile::ReconcileError`]，
//!   S3.6 的 SessionManager 一併實作兩側的窄介面時轉換一致）。
//! - [`SchedulerTimer`] 取代舊版 `SchedulerTimer`（interval 把手以 u64 呈現；
//!   測試注入手動 fire 的 fake，正式端 [`DefaultSchedulerTimer`] 對應
//!   `globalThis.setInterval`）。
//! - [`StreamStatusObserver`] 對應舊版 `onStreamStatusChanged`（bot 通知為
//!   無失敗介面；舊版 promise rejection／sync throw → warn
//!   `stream_status_notification_failed` 的分支不可達，照 DEC-020 前例保留
//!   事件名）。
//! - 舊版 `now?: () => Date` 以 epoch 毫秒 [`DefaultSchedulerClock`] 呈現；
//!   ISO 字串以 [`iso_utc_string_from_epoch_ms`] 對應 `Date.toISOString()`
//!   （格式照計畫 §7.7，與 twitch／logging 模組同一組、各模組獨立定義）。

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use time::OffsetDateTime;
use time::format_description::StaticFormatDescription;
use time::macros::format_description;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::config::thresholds::MAX_SAFE_INTEGER;
use crate::config::writeback::RuntimeWatchConfig;
use crate::logging::logger::{JsonLineLogger, LogEvents};
use crate::logging::redact::SanitizedValue;
use crate::logging::redact::redact_sensitive_string;
use crate::scheduler::selector::{StreamSelectionInput, select_active_channels};
use crate::sessions::reconcile::ReconcileCoordinator;
use crate::sessions::reconcile::ReconcileError;
use crate::twitch::errors::TwitchApiError;
use crate::twitch::live_status::{ChannelLiveStatus, LiveStatusProvider};

// ---------------------------------------------------------------------------
// 共用快照型別（S3.5 前置整併自 notifications；欄位與舊版一一對應）
// ---------------------------------------------------------------------------

/// 舊版 `WatchdogSchedulerSnapshot`。
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct WatchdogSchedulerSnapshot {
    pub(crate) running: bool,
    pub(crate) check_in_flight: bool,
    /// 舊版 `retryAt?: string`（ISO 時間字串）。
    pub(crate) retry_at: Option<String>,
    /// 舊版 `lastCheckedAt?: string`。
    pub(crate) last_checked_at: Option<String>,
    pub(crate) channels: Vec<SchedulerChannelStatus>,
}

/// 舊版 `SchedulerChannelStatus`；`is_live` 反映舊版 `isLive?: boolean`
/// （`None` 表示狀態未知，顯示 `❔`）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SchedulerChannelStatus {
    pub(crate) channel: String,
    pub(crate) is_live: Option<bool>,
}

/// 舊版 `StreamStatusChange`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StreamStatusChange {
    pub(crate) channel: String,
    pub(crate) is_live: bool,
}

// ---------------------------------------------------------------------------
// 依賴接縫
// ---------------------------------------------------------------------------

/// 舊版 `WatchdogScheduler` 介面（AppRunner 等呼叫端的型別依據）。
#[async_trait]
pub(crate) trait WatchdogScheduler: Send + Sync {
    /// 舊版 `start()`（同步，void）。
    fn start(&self);

    /// 舊版 `stop()`：清 interval、等 in-flight 結束。
    async fn stop(&self);

    /// 舊版 `runOnce()`：單輪檢查（in-flight 时 Skip 並立即完成）。
    async fn run_once(&self);

    /// 舊版 `getSnapshot()`。
    fn get_snapshot(&self) -> WatchdogSchedulerSnapshot;

    /// 舊版 `updateConfig(config)`。
    async fn update_config(&self, config: RuntimeWatchConfig);
}

/// 舊版 `Logger` 用到的四個層級（`StallWatchdogLogger` 同型前例；欄位值為
/// 消毒前的 [`SanitizedValue`]）。
pub(crate) trait WatchdogSchedulerLogger: Send + Sync {
    fn debug(&self, event: &str, fields: Option<&SanitizedValue>);
    fn info(&self, event: &str, fields: Option<&SanitizedValue>);
    fn warn(&self, event: &str, fields: Option<&SanitizedValue>);
    fn error(&self, event: &str, fields: Option<&SanitizedValue>);
}

/// 正式 logger 的轉接（browser 層 `BrowserManagerLogger` 的同型前例）。
impl WatchdogSchedulerLogger for JsonLineLogger {
    fn debug(&self, event: &str, fields: Option<&SanitizedValue>) {
        JsonLineLogger::debug(self, event, fields);
    }

    fn info(&self, event: &str, fields: Option<&SanitizedValue>) {
        JsonLineLogger::info(self, event, fields);
    }

    fn warn(&self, event: &str, fields: Option<&SanitizedValue>) {
        JsonLineLogger::warn(self, event, fields);
    }

    fn error(&self, event: &str, fields: Option<&SanitizedValue>) {
        JsonLineLogger::error(self, event, fields);
    }
}

/// 對應舊版 options 的 `now: () => Date`：回傳 epoch 毫秒（預設
/// [`default_now_ms`]；ISO 字串由 [`iso_utc_string_from_epoch_ms`] 換算）。
#[allow(clippy::type_complexity)]
pub(crate) type DefaultSchedulerClock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// 舊版 `setInterval` 註冊的 tick 回呼：同步，內部 fire-and-forget 驅動 tick。
pub(crate) type SchedulerTickCallback = Arc<dyn Fn() + Send + Sync>;

/// 對應舊版 `SchedulerTimer`：interval 把手以 u64 ids 呈現（DI 測試用手動
/// fire 的 fake；正式端 [`DefaultSchedulerTimer`]）。
pub(crate) trait SchedulerTimer: Send + Sync {
    /// 舊版 `setInterval(callback, ms)`：回傳 opaque 把手。
    fn set_interval(&self, on_tick: SchedulerTickCallback, interval_ms: f64) -> u64;

    /// 舊版 `clearInterval(handle)`：未知把手照 no-op（Node 的 clearInterval
    /// 對陌生值也不拋錯）。
    fn clear_interval(&self, handle: u64);
}

/// 正式計時器（舊版 `DEFAULT_TIMER`）：tokio spawn 的睡眠迴圈＋handle abort。
///
/// 舊 Node 的 setInterval 在 callback 尚未跑完時不追趕（期間用過期不再重複
/// 觸發）；迴圈被 abort 取消即對應 `clearInterval`。callback 由
/// [`SchedulerTickCallback`] 承載；每輪直接呼叫（內部自行 fire-and-forget）。
#[derive(Default)]
pub(crate) struct DefaultSchedulerTimer {
    next_handle: AtomicU64,
    handles: StdMutex<HashMap<u64, JoinHandle<()>>>,
}

impl SchedulerTimer for DefaultSchedulerTimer {
    fn set_interval(&self, on_tick: SchedulerTickCallback, interval_ms: f64) -> u64 {
        let handle = self.next_handle.fetch_add(1, Ordering::SeqCst) + 1;
        // 舊 Node 的 setInterval 對小數毫秒截整數；本處 floor 並下夾 1ms（0ms
        // 會變成 busy-loop；設定驗證保證為正，夾值屬防衛分支）。
        let delay_ms = if interval_ms.is_finite() {
            (interval_ms.trunc() as u64).max(1)
        } else {
            1
        };
        let task = tokio::spawn({
            let on_tick = Arc::clone(&on_tick);
            async move {
                loop {
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                    on_tick();
                }
            }
        });
        lock(&self.handles).insert(handle, task);
        handle
    }

    fn clear_interval(&self, handle: u64) {
        if let Some(task) = lock(&self.handles).remove(&handle) {
            task.abort();
        }
    }
}

/// 舊版 `LiveStatusProvider` 的 scheduler 窄接縫。錯誤補上 [`Watchdog
/// _LiveStatusError::Unknown`] 以保留舊版 `handleApiError` 的 catch-all
/// （`twitch_api_request_failed {kind:'unknown'}`）；三類錯誤由 blanket impl
/// 自 [`LiveStatusProvider`] 自動轉接，正式接線不需要手動包裝。
#[async_trait]
pub(crate) trait SchedulerLiveStatusProvider: Send + Sync {
    async fn get_live_statuses(
        &self,
        channels: &[String],
    ) -> Result<Vec<ChannelLiveStatus>, WatchdogLiveStatusError>;
}

/// 對應舊版 provider 的錯誤空間：三類 [`TwitchApiError`] ＋catch-all 的未知
/// 錯誤（`Unknown` 值不進日誌——舊版 unknown 分支只記 `kind:'unknown'`，
/// 保留訊息供除錯）。
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum WatchdogLiveStatusError {
    Api(TwitchApiError),
    /// crate 內不建構（blanket impl 只轉接三類）；保留給外部
    /// [`LiveStatusProvider`] 實作的 catch-all（DEC-024 (7)）。
    #[allow(dead_code)]
    Unknown(String),
}

/// 三類錯誤的自動轉接：任何 [`LiveStatusProvider`] 實作都直接可注入
/// （S3.6 正式接線時把 [`crate::twitch::api::TwitchApiClient`] 丟進來即可）。
#[async_trait]
impl<T: LiveStatusProvider + ?Sized + Send + Sync> SchedulerLiveStatusProvider for T {
    async fn get_live_statuses(
        &self,
        channels: &[String],
    ) -> Result<Vec<ChannelLiveStatus>, WatchdogLiveStatusError> {
        LiveStatusProvider::get_live_statuses(self, channels)
            .await
            .map_err(WatchdogLiveStatusError::Api)
    }
}

/// 舊版 `StreamSelector` 的物件接縫：預設轉催選台器的自由函式（測試可注入
/// fake，對應舊測試 harness 的 `{ selectActiveChannels: select }`）。
pub(crate) trait SchedulerStreamSelector: Send + Sync {
    fn select(&self, input: StreamSelectionInput<'_>) -> Vec<String>;
}

/// 預設 selector（對應舊版以自由函式當 `StreamSelector` 的實作）。
#[derive(Default)]
pub(crate) struct DefaultSchedulerStreamSelector;

impl SchedulerStreamSelector for DefaultSchedulerStreamSelector {
    fn select(&self, input: StreamSelectionInput<'_>) -> Vec<String> {
        select_active_channels(input)
    }
}

/// 舊版 `Pick<SessionManager, 'reconcile'>` 的窄介面（錯誤承載重用
/// [`ReconcileError`]）。
#[async_trait]
pub(crate) trait SchedulerSessionReconcile: Send + Sync {
    async fn reconcile(&self, channels: &[String]) -> Result<(), ReconcileError>;
}

/// 舊版 `Pick<ReconcileCoordinator, 'request'>` 的窄介面（同步、void、
/// fire-and-forget）。
pub(crate) trait SchedulerReconcileRequest: Send + Sync {
    fn request(&self, desired_channels: &[String]);
}

/// `ReconcileCoordinator` 實作的自動轉接：任何 coordinator 都可直接注入
/// [`DefaultWatchdogSchedulerOptions.reconcile_coordinator`]
/// （request 是同步方法，僅轉催）。
impl<T: ?Sized + ReconcileCoordinator> SchedulerReconcileRequest for T {
    fn request(&self, desired_channels: &[String]) {
        ReconcileCoordinator::request(self, desired_channels);
    }
}

/// trait 物件的轉接（S3.6 以 `Arc<dyn ReconcileCoordinator>` 持有 coordinator
/// 時的注入點；`Arc<T>` 不自動轉發 trait 方法，故單獨給）。
impl SchedulerReconcileRequest for Arc<dyn ReconcileCoordinator> {
    fn request(&self, desired_channels: &[String]) {
        ReconcileCoordinator::request(self.as_ref(), desired_channels);
    }
}

/// 舊版 `onStreamStatusChanged` 的窄介面：bot 通知為無失敗 promise
/// （舊版 catch 的 `stream_status_notification_failed` 分支不可達）。
#[async_trait]
pub(crate) trait StreamStatusObserver: Send + Sync {
    async fn on_stream_status_changed(&self, change: &StreamStatusChange);
}

/// 對應舊版 `WatchdogSchedulerConfig`（`Pick<AppConfig, 'channels' |
/// 'checkIntervalSeconds' | 'maxConcurrentStreams'>`）。`checkIntervalSeconds`
/// 只在 `start` 使用；`update_config` 端另有 [`RuntimeWatchConfig`]。
pub(crate) struct WatchdogSchedulerConfig {
    pub(crate) channels: Vec<String>,
    pub(crate) check_interval_seconds: f64,
    pub(crate) max_concurrent_streams: f64,
}

/// 對應舊版 `DefaultWatchdogSchedulerOptions`；可選接縫照舊版 `?` 欄位。
pub(crate) struct DefaultWatchdogSchedulerOptions {
    pub(crate) config: WatchdogSchedulerConfig,
    pub(crate) live_status_provider: Arc<dyn SchedulerLiveStatusProvider>,
    pub(crate) stream_selector: Arc<dyn SchedulerStreamSelector>,
    pub(crate) session_manager: Option<Arc<dyn SchedulerSessionReconcile>>,
    pub(crate) reconcile_coordinator: Option<Arc<dyn SchedulerReconcileRequest>>,
    pub(crate) logger: Arc<dyn WatchdogSchedulerLogger>,
    /// 舊版 `timer?`；`None` 用預設（[`DefaultSchedulerTimer`]）。
    pub(crate) timer: Option<Arc<dyn SchedulerTimer>>,
    /// 舊版 `now?`；`None` 用系統時鐘。
    pub(crate) now: Option<DefaultSchedulerClock>,
    pub(crate) on_stream_status_changed: Option<Arc<dyn StreamStatusObserver>>,
}

// ---------------------------------------------------------------------------
// 內部實作
// ---------------------------------------------------------------------------

/// 老版 `inFlight` promise 的完成信號（同 reconcile 模組的 FlightSignal 前例；
/// 多個等待者 stop／updateConfig 各自訂閱）。
struct TickFlight {
    done: watch::Sender<Option<()>>,
}

impl TickFlight {
    fn new() -> Self {
        Self {
            done: watch::channel(None).0,
        }
    }

    async fn wait(&self) {
        let mut receiver = self.done.subscribe();
        while receiver.borrow_and_update().is_none() {
            if receiver.changed().await.is_err() {
                continue;
            }
        }
    }

    fn complete(&self) {
        let _ = self.done.send(Some(()));
    }
}

/// 舊版 `NO_INTERVAL` 以 [`None`] 呈現；其餘可變狀態集中在單一 std 鎖
///（鎖內無 await；`started` 同時是 timer callback 門檻）。
#[derive(Default)]
struct SchedulerState {
    started: bool,
    /// 舊版 `retryAt`（`Date` 的 epoch 毫秒）。
    retry_at_ms: Option<i64>,
    /// 舊版 `lastCheckedAt`（`Date` 的 epoch 毫秒）。
    last_checked_at_ms: Option<i64>,
    tick_sequence: u64,
    /// 舊版 `previousLiveStatuses`：正規化頻道 → 是否直播。
    previous_live: HashMap<String, bool>,
    /// 舊版 `intervalHandle`（`NO_INTERVAL` → `None`）。
    interval_handle: Option<u64>,
    /// 舊版 `inFlight`：目前 tick 的完成信號（`None` 即無 in-flight）。
    tick_flight: Option<Arc<TickFlight>>,
}

/// 對應舊版 `DefaultWatchdogScheduler`。`Clone` 供測試 spawn（共用同一份內部
/// 狀態；正式端以 `Arc<dyn WatchdogScheduler>` 持有）。
#[derive(Clone)]
pub(crate) struct DefaultWatchdogScheduler(Arc<Inner>);

struct Inner {
    timer: Arc<dyn SchedulerTimer>,
    now: DefaultSchedulerClock,
    live_status_provider: Arc<dyn SchedulerLiveStatusProvider>,
    stream_selector: Arc<dyn SchedulerStreamSelector>,
    session_manager: Option<Arc<dyn SchedulerSessionReconcile>>,
    reconcile_coordinator: Option<Arc<dyn SchedulerReconcileRequest>>,
    logger: Arc<dyn WatchdogSchedulerLogger>,
    on_stream_status_changed: Option<Arc<dyn StreamStatusObserver>>,
    /// 舊版 `options.config.checkIntervalSeconds`（`start` 時乘以 1000 用）。
    check_interval_seconds: f64,
    /// 舊版 `runtimeConfig`（`freezeRuntimeConfig` 後的值；`update_config` 替換）。
    runtime_config: StdMutex<RuntimeWatchConfig>,
    state: StdMutex<SchedulerState>,
}

impl DefaultWatchdogScheduler {
    pub(crate) fn new(mut options: DefaultWatchdogSchedulerOptions) -> Self {
        let timer = options.timer.take().unwrap_or_else(|| {
            Arc::new(DefaultSchedulerTimer::default()) as Arc<dyn SchedulerTimer>
        });
        let now = options
            .now
            .take()
            .unwrap_or_else(|| Arc::new(default_now_ms));
        // 舊版 `freezeRuntimeConfig(options.config)`：channels 複本＋max 原值。
        let runtime_config = RuntimeWatchConfig {
            channels: options.config.channels.clone(),
            max_concurrent_streams: options.config.max_concurrent_streams,
        };
        Self(Arc::new(Inner {
            timer,
            now,
            live_status_provider: options.live_status_provider,
            stream_selector: options.stream_selector,
            session_manager: options.session_manager,
            reconcile_coordinator: options.reconcile_coordinator,
            logger: options.logger,
            on_stream_status_changed: options.on_stream_status_changed,
            check_interval_seconds: options.config.check_interval_seconds,
            runtime_config: StdMutex::new(runtime_config),
            state: StdMutex::new(SchedulerState::default()),
        }))
    }
}

#[async_trait]
impl WatchdogScheduler for DefaultWatchdogScheduler {
    fn start(&self) {
        self.0.start();
    }

    async fn stop(&self) {
        self.0.stop().await;
    }

    async fn run_once(&self) {
        self.0.run_once().await;
    }

    fn get_snapshot(&self) -> WatchdogSchedulerSnapshot {
        self.0.get_snapshot()
    }

    async fn update_config(&self, config: RuntimeWatchConfig) {
        self.0.update_config(config).await;
    }
}

impl Inner {
    fn start(self: &Arc<Self>) {
        let already_started = {
            let mut state = lock(&self.state);
            let old = state.started;
            state.started = true;
            old
        };
        if already_started {
            return;
        }

        // 舊版 callback：if (this.started) { this.triggerTick(); }
        let on_tick: SchedulerTickCallback = {
            let inner = Arc::clone(self);
            Arc::new(move || {
                let started = {
                    let state = lock(&inner.state);
                    state.started
                };
                if started {
                    inner.spawn_tick();
                }
            })
        };
        // 舊版：this.options.config.checkIntervalSeconds * 1_000（start 當下用）。
        let interval_ms = self.check_interval_seconds * 1_000.0;
        let handle = self.timer.set_interval(on_tick, interval_ms);
        {
            let mut state = lock(&self.state);
            state.interval_handle = Some(handle);
        }

        // 舊版：start 立即 triggerTick()。
        self.spawn_tick();
    }

    async fn stop(&self) {
        // 舊版：started = false；clearInterval（若有）；await inFlight。
        let interval_handle = {
            let mut state = lock(&self.state);
            state.started = false;
            state.interval_handle.take()
        };
        if let Some(handle) = interval_handle {
            self.timer.clear_interval(handle);
        }
        let flight = flight_snapshot(&self.state);
        if let Some(flight) = flight {
            flight.wait().await;
        }
    }

    /// 舊版 `triggerTick`：void runOnce().catch(() => undefined)。
    fn spawn_tick(self: &Arc<Self>) {
        let inner = Arc::clone(self);
        tokio::spawn(async move {
            inner.run_once().await;
        });
    }

    /// 對應舊版 `runOnce`。in-flight／rate-limited 的 skip 在同步前綴完成
    /// （鎖內一次決定＋登記 flight，跨任務呼叫端也不會雙插隊）；舊版的
    /// trackedExecution promise 鏈在 Rust 版為同一 async 本體（執行＋completed
    /// 記點＋清 in-flight 同段落，順序照舊）。
    async fn run_once(self: &Arc<Self>) {
        let flight = Arc::new(TickFlight::new());
        enum TickGate {
            Run(u64),
            SkipInFlight,
            SkipRateLimited(i64),
        }
        let gate = {
            let mut state = lock(&self.state);
            if state.tick_flight.is_some() {
                TickGate::SkipInFlight
            } else if let Some(retry_at_ms) = rate_limited_locked(&mut state, (self.now)()) {
                TickGate::SkipRateLimited(retry_at_ms)
            } else {
                let tick_id = next_tick_id(&mut state.tick_sequence);
                state.tick_flight = Some(Arc::clone(&flight));
                TickGate::Run(tick_id)
            }
        };

        let tick_id = match gate {
            TickGate::SkipInFlight => {
                self.logger.warn(
                    "scheduler_tick_skipped",
                    Some(&sv_object(vec![(
                        "reason",
                        SanitizedValue::String("in_flight".to_string()),
                    )])),
                );
                return;
            }
            TickGate::SkipRateLimited(retry_at_ms) => {
                self.logger.warn(
                    "scheduler_tick_skipped",
                    Some(&sv_object(vec![
                        ("reason", SanitizedValue::String("rate_limited".to_string())),
                        (
                            "retryAt",
                            SanitizedValue::String(iso_utc_string_from_epoch_ms(retry_at_ms)),
                        ),
                    ])),
                );
                return;
            }
            TickGate::Run(tick_id) => tick_id,
        };

        // 舊版：const startedAtMs = Date.now()（durationMs 的差值基準）；
        // 照 DEC-020 的前例以 std Instant 承接（不跟隨模擬時鐘，測試僅記
        // 「任意數字」）。
        let started_at = Instant::now();
        let (channel_count, max_streams) = {
            let runtime = lock(&self.runtime_config);
            (
                runtime.channels.len() as f64,
                runtime.max_concurrent_streams,
            )
        };
        self.logger.debug(
            "scheduler_tick_started",
            Some(&sv_object(vec![
                ("tickId", SanitizedValue::Number(tick_id as f64)),
                ("channelCount", SanitizedValue::Number(channel_count)),
                ("maxConcurrentStreams", SanitizedValue::Number(max_streams)),
            ])),
        );

        self.execute_tick(tick_id).await;

        // 舊版 trackedExecution.then(completed)：executeTick 不可失敗
        // （逐項檢查：live-status 錯誤全數進 handleApiError；select、撮合
        // 與 submit 為無失敗型別），unexpected_error 的 scheduler_tick_failed
        // 分支不存在。
        self.logger.debug(
            "scheduler_tick_completed",
            Some(&sv_object(vec![
                ("tickId", SanitizedValue::Number(tick_id as f64)),
                (
                    "durationMs",
                    SanitizedValue::Number(started_at.elapsed().as_millis() as f64),
                ),
            ])),
        );
        release_flight(&self.state, &flight);
        flight.complete();
    }

    fn get_snapshot(&self) -> WatchdogSchedulerSnapshot {
        // 順序照舊版物件字面值的欄位序：running → checkInFlight → retryAt?
        // → lastCheckedAt? → channels。
        let snapshot_fields = {
            let state = lock(&self.state);
            (
                state.started,
                state.tick_flight.is_some(),
                state.retry_at_ms,
                state.last_checked_at_ms,
                state.previous_live.clone(),
            )
        };
        let (started, check_in_flight, retry_at_ms, last_checked_ms, previous_live) =
            snapshot_fields;
        let channels: Vec<SchedulerChannelStatus> = {
            let runtime = lock(&self.runtime_config);
            runtime
                .channels
                .iter()
                .map(|channel| {
                    let is_live = previous_live.get(&normalize_channel(channel)).copied();
                    // 舊版：isLive === undefined ? { channel } : { channel, isLive }。
                    SchedulerChannelStatus {
                        channel: channel.clone(),
                        is_live,
                    }
                })
                .collect()
        };
        WatchdogSchedulerSnapshot {
            running: started,
            check_in_flight,
            retry_at: retry_at_ms.map(iso_utc_string_from_epoch_ms),
            last_checked_at: last_checked_ms.map(iso_utc_string_from_epoch_ms),
            channels,
        }
    }

    async fn update_config(self: &Arc<Self>, config: RuntimeWatchConfig) {
        // 舊版：await this.inFlight。
        let flight = flight_snapshot(&self.state);
        if let Some(flight) = flight {
            flight.wait().await;
        }

        // 舊版 freezeRuntimeConfig：頻道清單複本＋max 原值。
        {
            let mut runtime = lock(&self.runtime_config);
            *runtime = RuntimeWatchConfig {
                channels: config.channels.clone(),
                max_concurrent_streams: config.max_concurrent_streams,
            };
        }
        // 舊版：刪除不在新頻道的 previousLiveStatuses 鍵（retain 等價）。
        let configured: HashSet<String> = config
            .channels
            .iter()
            .map(|channel| normalize_channel(channel))
            .collect();
        {
            let mut state = lock(&self.state);
            state
                .previous_live
                .retain(|channel, _| configured.contains(channel));
        }

        // 舊版 knownStatuses：isLive 取自 previous（=== true）、checkedAt =
        // lastCheckedAt?.toISOString() ?? now().toISOString()。
        let now_ms = (self.now)();
        let checked_at = {
            let state = lock(&self.state);
            match state.last_checked_at_ms {
                Some(last_ms) => iso_utc_string_from_epoch_ms(last_ms),
                None => iso_utc_string_from_epoch_ms(now_ms as i64),
            }
        };
        let known_statuses: Vec<ChannelLiveStatus> = {
            let state = lock(&self.state);
            config
                .channels
                .iter()
                .map(|channel| ChannelLiveStatus {
                    channel: channel.clone(),
                    is_live: state
                        .previous_live
                        .get(&normalize_channel(channel))
                        .copied()
                        == Some(true),
                    checked_at: checked_at.clone(),
                    stream_id: None,
                    title: None,
                    started_at: None,
                    viewer_count: None,
                })
                .collect()
        };

        // 舊版：selectActiveChannels({configuredChannels, liveStatuses, max})。
        let max = {
            let runtime = lock(&self.runtime_config);
            runtime.max_concurrent_streams
        };
        let active_channels = self.stream_selector.select(StreamSelectionInput {
            configured_channels: &config.channels,
            live_statuses: &as_selector_statuses(&known_statuses),
            max_concurrent_streams: max,
        });
        submit_reconcile(self, active_channels);

        // 舊版：await this.runOnce()
        self.run_once().await;
    }

    /// 對應舊版 `executeTick`（永不拋出；錯誤全數進 `handleApiError`）。
    async fn execute_tick(self: &Arc<Self>, tick_id: u64) {
        let (channels, max) = {
            let runtime = lock(&self.runtime_config);
            (runtime.channels.clone(), runtime.max_concurrent_streams)
        };
        let live_statuses = match self.live_status_provider.get_live_statuses(&channels).await {
            Ok(live_statuses) => live_statuses,
            Err(error) => {
                self.handle_api_error(&error);
                return;
            }
        };

        let current = match collect_configured_live_statuses(&channels, &live_statuses) {
            Some(current) => current,
            None => {
                self.logger.warn(
                    "twitch_api_temporary_error",
                    Some(&sv_object(vec![(
                        "reason",
                        SanitizedValue::String("invalid-response".to_string()),
                    )])),
                );
                return;
            }
        };

        // 舊版順序：logStatusChanges → replacePreviousStatuses →
        // lastCheckedAt／retryAt 更新 → select → selection log → submit。
        self.log_status_changes(&channels, &current);
        {
            let mut state = lock(&self.state);
            state.previous_live = current;
            state.last_checked_at_ms = Some((self.now)() as i64);
            state.retry_at_ms = None;
        }

        let active_channels = self.stream_selector.select(StreamSelectionInput {
            configured_channels: &channels,
            live_statuses: &as_selector_statuses(&live_statuses),
            max_concurrent_streams: max,
        });

        let live_count = live_statuses.iter().filter(|status| status.is_live).count() as f64;
        self.logger.debug(
            "scheduler_tick_selection",
            Some(&sv_object(vec![
                ("tickId", SanitizedValue::Number(tick_id as f64)),
                ("liveCount", SanitizedValue::Number(live_count)),
                ("activeChannels", sv_string_array(&active_channels)),
            ])),
        );

        submit_reconcile(self, active_channels);
    }

    /// 對應舊版 `handleApiError`（按類別分流；retryAt 取 max 保留）。
    fn handle_api_error(&self, error: &WatchdogLiveStatusError) {
        let api = match error {
            WatchdogLiveStatusError::Api(api) => api,
            WatchdogLiveStatusError::Unknown(_) => {
                // 舊版 catch-all：不轉送原始錯誤（連訊息都不進日誌，測試
                // 「錯誤日誌不轉送…」鎖住）。
                self.logger.warn(
                    "twitch_api_request_failed",
                    Some(&sv_object(vec![(
                        "kind",
                        SanitizedValue::String("unknown".to_string()),
                    )])),
                );
                return;
            }
        };
        match api {
            TwitchApiError::RateLimit(rate_limit) => {
                {
                    // 舊版：若無 retryAt 或 error.retryAt 較晚，就換成新的。
                    let mut state = lock(&self.state);
                    let replace = match state.retry_at_ms {
                        Some(existing) => rate_limit.retry_at_ms() > existing,
                        None => true,
                    };
                    if replace {
                        state.retry_at_ms = Some(rate_limit.retry_at_ms());
                    }
                }
                self.logger.warn(
                    "twitch_api_rate_limited",
                    Some(&sv_object(vec![
                        (
                            "retryAt",
                            SanitizedValue::String(iso_utc_string_from_epoch_ms(
                                rate_limit.retry_at_ms(),
                            )),
                        ),
                        (
                            "retryAfterMs",
                            SanitizedValue::Number(rate_limit.retry_after_ms() as f64),
                        ),
                    ])),
                );
            }
            TwitchApiError::Auth(auth) => {
                self.logger.error(
                    LogEvents::TWITCH_API_AUTH_FAILED,
                    Some(&sv_object(vec![(
                        "statusCode",
                        SanitizedValue::Number(auth.status_code() as f64),
                    )])),
                );
            }
            TwitchApiError::Temporary(temporary) => {
                // 舊版：reason 必放；statusCode undefined 的欄位整個省略。
                let mut fields = vec![(
                    "reason",
                    SanitizedValue::String(temporary.reason().as_str().to_string()),
                )];
                if let Some(status_code) = temporary.status_code() {
                    fields.push(("statusCode", SanitizedValue::Number(status_code as f64)));
                }
                self.logger
                    .warn("twitch_api_temporary_error", Some(&sv_object(fields)));
            }
        }
    }

    /// 對應舊版 `logStatusChanges`：逐設定頻道（正規化去重）的狀態轉換記
    /// `stream_online`/`stream_offline`，並 fire-and-forget 通知 observer。
    fn log_status_changes(self: &Arc<Self>, channels: &[String], current: &HashMap<String, bool>) {
        let mut logged: HashSet<String> = HashSet::new();
        // 先在鎖內收集轉換（wasLive 由先前狀態查），出鎖後記 log＋通知
        // （避免鎖覆蓋 spawn／log 的成本與 notify 的排程）。
        let mut changes: Vec<StreamStatusChange> = Vec::new();
        {
            let state = lock(&self.state);
            for channel in channels {
                let normalized_channel = normalize_channel(channel);
                if !logged.insert(normalized_channel.clone()) {
                    continue;
                }
                let is_live = current.get(&normalized_channel).copied();
                let was_live = state.previous_live.get(&normalized_channel).copied();
                // 舊版：isLive === true && wasLive !== true → online；
                // isLive === false && wasLive === true → offline。
                if is_live == Some(true) && was_live != Some(true) {
                    changes.push(StreamStatusChange {
                        channel: channel.clone(),
                        is_live: true,
                    });
                } else if is_live == Some(false) && was_live == Some(true) {
                    changes.push(StreamStatusChange {
                        channel: channel.clone(),
                        is_live: false,
                    });
                }
            }
        }
        for change in &changes {
            let event = if change.is_live {
                LogEvents::STREAM_ONLINE
            } else {
                LogEvents::STREAM_OFFLINE
            };
            self.logger.info(
                event,
                Some(&sv_object(vec![(
                    "channel",
                    SanitizedValue::String(change.channel.clone()),
                )])),
            );
            self.notify_status_change(change);
        }
    }

    /// 對應舊版 `notifyStatusChange`：`void Promise.resolve(observer(change))
    /// .catch(...)` 的語意——通知被排程但不等（tick 立即繼續）；Rust 版
    /// observer 介面無失敗（bot 通知 infallible），舊版 catch 的
    /// `stream_status_notification_failed` 分支不可達（照 DEC-020 前例保留
    /// 事件名）。spawn 的任務 panic／JoinError 不理（對應 void promise）。
    fn notify_status_change(&self, change: &StreamStatusChange) {
        if let Some(observer) = self.on_stream_status_changed.as_ref() {
            let observer = Arc::clone(observer);
            let change = change.clone();
            tokio::spawn(async move {
                observer.on_stream_status_changed(&change).await;
            });
        }
    }
}

/// 對應舊版 `submitReconcile`：有 coordinator 走同步 `request`；否則走
/// `sessionManager.reconcile`（fire-and-forget spawn；reject 記
/// `reconcile_submit_failed {error}`）。兩者皆無 → no-op（與舊版相同）。
fn submit_reconcile(inner: &Arc<Inner>, active_channels: Vec<String>) {
    if let Some(coordinator) = inner.reconcile_coordinator.as_ref() {
        coordinator.request(&active_channels);
        return;
    }
    if let Some(manager) = inner.session_manager.as_ref() {
        let manager = Arc::clone(manager);
        let logger = Arc::clone(&inner.logger);
        // 舊版：void reconcile?.catch((error) =>
        //   log error reconcile_submit_failed {error: safeErrorMessage})。
        tokio::spawn(async move {
            if let Err(error) = manager.reconcile(&active_channels).await {
                logger.error(
                    "reconcile_submit_failed",
                    Some(&sv_object(vec![(
                        "error",
                        SanitizedValue::String(redact_sensitive_string(&error.message)),
                    )])),
                );
            }
        });
    }
}

/// 頻道集合檢查（對應舊版 `collectConfiguredLiveStatuses`）：去重合併後，
/// 任何 configured channel 缺值即整批視為失敗（`None`）。
fn collect_configured_live_statuses(
    channels: &[String],
    live_statuses: &[ChannelLiveStatus],
) -> Option<HashMap<String, bool>> {
    let mut statuses_by_channel: HashMap<String, bool> = HashMap::new();
    for status in live_statuses {
        let channel = normalize_channel(&status.channel);
        // 舊版：previous === undefined ? status.isLive : previous && status.isLive。
        statuses_by_channel
            .entry(channel)
            .and_modify(|previous| *previous &= status.is_live)
            .or_insert(status.is_live);
    }
    for channel in channels {
        if !statuses_by_channel.contains_key(&normalize_channel(channel)) {
            return None;
        }
    }
    Some(statuses_by_channel)
}

/// 鎖內判斷 rate-limit 窗口（到期即清 `retryAt`；對應舊版 `isRateLimited`）。
fn rate_limited_locked(state: &mut SchedulerState, now_ms: u64) -> Option<i64> {
    match state.retry_at_ms {
        None => None,
        Some(retry_at_ms) => {
            if now_ms >= retry_at_ms as u64 {
                state.retry_at_ms = None;
                None
            } else {
                Some(retry_at_ms)
            }
        }
    }
}

/// 清 flight 的守護（對應舊版 `if (this.inFlight === trackedExecution)`）。
fn release_flight(state_lock: &StdMutex<SchedulerState>, flight: &Arc<TickFlight>) {
    let mut state = lock(state_lock);
    let same = state
        .tick_flight
        .as_ref()
        .map(|current| Arc::ptr_eq(current, flight))
        .unwrap_or(false);
    if same {
        state.tick_flight = None;
    }
}

/// 鎖內取下 tick flight 的 Arc 複製（stop／updateConfig 等 await 端用；
/// 鎖不跨 await）。
fn flight_snapshot(state_lock: &StdMutex<SchedulerState>) -> Option<Arc<TickFlight>> {
    lock(state_lock).tick_flight.clone()
}

/// 舊版 `nextTickId`：`(tickSequence % Number.MAX_SAFE_INTEGER) + 1`。
fn next_tick_id(sequence: &mut u64) -> u64 {
    *sequence = (*sequence % (MAX_SAFE_INTEGER as u64)) + 1;
    *sequence
}

/// 對應舊版 `normalizeChannel`：照 DEC-009／DEC-019 的前例以
/// `to_lowercase` 對應 `toLocaleLowerCase('en-US')`。
fn normalize_channel(channel: &str) -> String {
    channel.to_lowercase()
}

/// 舊版 `toISOString()` 的格式：UTC 的 `YYYY-MM-DDTHH:MM:SS.sssZ`
/// （`[subsecond digits:3]` 固定 3 位毫秒；格式與 twitch/logging 模組
/// 同一組，各模組獨立定義以維持邊界——計畫 §7.7）。
const ISO_UTC_FORMAT: StaticFormatDescription =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z");

/// 對應舊版 `Date` 的 ISO 字串換算（epoch 毫秒 → `toISOString()`）。
///
/// `retryAt`／`lastCheckedAt` 由現在時間加非負退避組成，不會超出 `Date` 的
/// 範圍；防衛分支照 reachable 慣例以明確訊息終止（同 twitch api 的前例）。
fn iso_utc_string_from_epoch_ms(epoch_ms: i64) -> String {
    let datetime = match OffsetDateTime::from_unix_timestamp_nanos(i128::from(epoch_ms) * 1_000_000)
    {
        Ok(datetime) => datetime,
        Err(_) => panic!("scheduler timestamp is outside the supported range"),
    };
    match datetime.format(&ISO_UTC_FORMAT) {
        Ok(formatted) => formatted,
        Err(error) => panic!("failed to format scheduler timestamp: {error}"),
    }
}

/// 對應舊版 `Date.now()` 的預設值：UNIX epoch 起的毫秒（1970 年前取 0）。
fn default_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as u64)
}

/// selector 模組的 `ChannelLiveStatus` 是 S2.5 的最小形（三選用欄位尚未
/// 併入；其檔案註明 S3.1 後改 import 為債）。本模組逐欄轉接。
fn as_selector_statuses(
    statuses: &[ChannelLiveStatus],
) -> Vec<crate::scheduler::selector::ChannelLiveStatus> {
    statuses
        .iter()
        .map(|status| crate::scheduler::selector::ChannelLiveStatus {
            channel: status.channel.clone(),
            is_live: status.is_live,
            checked_at: status.checked_at.clone(),
        })
        .collect()
}

/// 欄位物件（照插入順序；對應舊版逐字面的欄位序）。
fn sv_object(entries: Vec<(&'static str, SanitizedValue)>) -> SanitizedValue {
    SanitizedValue::Object(
        entries
            .into_iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect(),
    )
}

/// 欄位值為字串陣列（對應舊版 `activeChannels: string[]`）。
fn sv_string_array(values: &[String]) -> SanitizedValue {
    SanitizedValue::Array(
        values
            .iter()
            .map(|value| SanitizedValue::String(value.clone()))
            .collect(),
    )
}

/// std 鎖的固定還原慣例（同 `browser/session` 前例；毒性鎖照樣取值）。
fn lock<T>(mutex: &StdMutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests;
