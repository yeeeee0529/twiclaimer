//! `SessionManager`：移植舊版 `src/sessions/SessionManager.ts`（§4.2；
//! 行為依據 DEC-016 基準 `d49363d`）。
//!
//! 資料結構對照（與舊版語意共通）：
//! - 舊版的 `Map`／`Set` 有插入順序（`getActiveChannels`／
//!   `reorderSessions` 可觀察），Rust 以保留插入序的 `Vec` 對映（不新增
//!   IndexMap 依賴）；鍵只做相等比對，數量為設定規模。
//! - 舊版的 promise 尾巴 `channelOperationTails`（每頻道的 exclusive 串行）
//!   以 [`crate::browser::manager::ExclusiveQueue`]＋「登記序號」重現：
//!   登記在呼叫當下同步進行（對映舊版 promise 在當下接到尾巴），operation
//!   完成後若自己仍是最新登記者（`get(channel) === tail` 的等價判斷）才把
//!   地圖項目刪掉。
//! - `WeakSet`（cancelledStartSessions）以 `Weak` 清單重現：取消前先清掉
//!   已結束的弱參照，僅在「同一份 ChannelSession instance」尚未取消時登記，
//!   確保 cancel 對每個 session 至多呼叫一次。
//! - `Date.now()` 的兩個用途：`session_reconcile_completed`／
//!   `session_invalidate_completed` 的 `durationMs` 以 [`Instant`] 差值對應
//!   （DEC-020 note 4 前例：任意數字即可，不跟隨 paused 模擬時鐘）。
//! - `withSessionOperationTimeout` 的 timer race 以 `tokio::spawn`＋
//!   `tokio::select!` 重現：逾時後操作仍在背景跑完、結果丟棄
//!   （對映 `operation.catch(() => undefined)` 的「不取消」語意）。
//!
//! 逾時事件名稱（`session_start_timeout`／`session_start_cancel_timeout`／
//! `session_stop_timeout`／`session_stop_all_timeout`）經變數轉發（舊碼
//! `safeLog('warn', timeoutEvent, …)`），原本不在 `doc/spec/log-events.txt`
//! 清單內；S3.6 以 DEC-023 的補抓形狀重抽後已收錄（158 版），本檔的字面值
//! 照舊版輸出不變。
//!
//! 對映附註：`SessionChannelPointsResult` 的 `balance` 照舊版 JS number
//! 形狀留 f64（DEC-021）；正式端由 `ChannelSession::get_channel_points`
//! 的 [`crate::browser::session::ChannelSessionPointsResult`]
//! （`Available { balance: u64 }`）轉接。舊版 `displayValue` 欄位沿
//! DEC-020 的省略契約移除：唯一生產端（ChannelPointsReader）依
//! DEC-020 只回 balance，而舊版 bots 渲染一律用
//! `balance.toLocaleString('en-US')`（不讀 displayValue），全鏈路無消費端。
//!
//! 另一個收斂：舊版 `SessionManager.getChannelPoints` 的 catch 分支
//! （`{status: 'unavailable', reason: 'failed'}` 與
//! `session_channel_points_failed`）在 Rust 端不可達——
//! `ChannelSession::get_channel_points` 的契約不回錯（DEC-020 note 4：
//! 頁面錯誤在 session 層就折為 unavailable 原因），本檔不再附加 catch。

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures_util::future::join_all;

use crate::browser::cdp::context::DriverError;
use crate::browser::manager::ExclusiveQueue;
use crate::browser::safe_logging::safe_error_message;
use crate::browser::session::{
    ChannelSession, ChannelSessionPointsResult, ChannelSessionRefreshStatus,
    DefaultChannelSessionFactory,
};
use crate::browser::types::{
    BrowserManagerLogger, BrowserNavigationOutcome, BrowserNavigationStatus, SleepFn,
    positive_integer,
};
use crate::logging::logger::LogLevel;
use crate::logging::redact::SanitizedValue;
use crate::sessions::reconcile::ReconcileSessionManager;
use crate::sessions::recovery::{
    BeginRecoveryProbeResult, ChannelRecoveryAvailability, ChannelRecoveryPolicy,
    normalize_recovery_channel,
};

/// 舊版 `SessionRefreshResult.status`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionRefreshStatus {
    Refreshed,
    Unavailable,
    Failed,
}

/// 舊版 `SessionRefreshResult`。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SessionRefreshResult {
    pub(crate) channel: String,
    pub(crate) status: SessionRefreshStatus,
    /// 舊版 `error?: string`。
    pub(crate) error: Option<String>,
}

/// 舊版 `SessionScreenshot`；`image` 以位元組向量對應 Node 的 `Buffer`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SessionScreenshot {
    pub(crate) channel: String,
    pub(crate) image: Vec<u8>,
}

/// 舊版 `SessionChannelPointsResult` 的 `reason` 聯集合
/// （`ChannelSessionPointsResult` 的 `'not_found'` | `'parse_failed'` 加
/// `'page_unavailable'`，另加 SessionManager 的 `'failed'`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionChannelPointsUnavailableReason {
    NotFound,
    ParseFailed,
    PageUnavailable,
    Failed,
}

impl SessionChannelPointsUnavailableReason {
    /// 舊版聯集的字面值（log 用不到；正式碼不讀，供字面值測試鎖定——
    /// DEC-023 note 10）。
    #[allow(dead_code)]
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::ParseFailed => "parse_failed",
            Self::PageUnavailable => "page_unavailable",
            Self::Failed => "failed",
        }
    }
}

/// 舊版 `SessionChannelPointsResult`（`displayValue` 欄位省略：DEC-020 契約，
/// 見本檔模組文件）。
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum SessionChannelPointsResult {
    Available {
        channel: String,
        /// 舊版 `balance: number`（JS number）。
        balance: f64,
    },
    Unavailable {
        channel: String,
        reason: SessionChannelPointsUnavailableReason,
    },
}

// ---------------------------------------------------------------------------
// SessionManager 介面與依賴
// ---------------------------------------------------------------------------

/// 舊版 `SessionManager` 介面；由 [`DefaultSessionManager`] 實作。
#[async_trait]
pub(crate) trait SessionManager: Send + Sync {
    /// 舊版 `reconcile(activeChannels)`。
    async fn reconcile(&self, active_channels: &[String]);

    /// 舊版 `updateConfiguredChannels(channels)`。
    fn update_configured_channels(&self, channels: &[String]);

    /// 舊版 `cancelPendingStarts(reason)`。
    async fn cancel_pending_starts(&self, reason: &str);

    /// 舊版 `stopAll(reason)`。
    async fn stop_all(&self, reason: &str);

    /// 舊版 `invalidate(channel, reason)`。
    async fn invalidate(&self, channel: &str, reason: &str);

    /// 舊版 `getActiveChannels()`。
    fn get_active_channels(&self) -> Vec<String>;

    /// 舊版 `getRefreshStatuses()`。
    fn get_refresh_statuses(&self) -> Vec<ChannelSessionRefreshStatus>;

    /// 舊版 `refreshPages(channel?)`。
    async fn refresh_pages(&self, channel: Option<&str>) -> Vec<SessionRefreshResult>;

    /// 舊版 `captureScreenshot(channel?)`。
    async fn capture_screenshot(&self, channel: Option<&str>) -> Option<SessionScreenshot>;

    /// 舊版 `getChannelPoints(channel?)`。
    async fn get_channel_points(&self, channel: Option<&str>) -> Vec<SessionChannelPointsResult>;
}

/// 舊版 `SessionNavigationOutcomeObserver`（`(outcomes) => …`；回傳值可拋錯 →
/// `Err`，依 manager/types 的既有慣例表達）。
#[async_trait]
pub(crate) trait SessionNavigationOutcomeObserver: Send + Sync {
    async fn on_navigation_outcomes(
        &self,
        outcomes: &[crate::browser::types::BrowserNavigationOutcome],
    ) -> Result<(), crate::browser::cdp::context::DriverError>;
}

/// 舊版 `ChannelSessionFactory`（同步工廠，可拋錯 → `Err`）。
///
/// 舊版的 create 與 `DefaultChannelSession` 建構函式都是同步的（channel
/// 驗證失敗在建構當下拋出），SessionManager 以 `await factory.create()`
/// 包住 try——同步拋出的 `Error` 同樣進 catch；Rust 版以 `Err` 表達兩者。
pub(crate) trait SessionChannelSessionFactory: Send + Sync {
    fn create(&self, channel: &str) -> Result<Arc<dyn ChannelSession>, DriverError>;
}

/// 正式工廠（與舊版 `DefaultChannelSessionFactory implements
/// ChannelSessionFactory` 的關係同構）：inherent 的 `create`（回傳具體型別）
/// 包成 `Arc<dyn ChannelSession>`。本檔放 impl 是為了不動 session.rs。
impl SessionChannelSessionFactory for DefaultChannelSessionFactory {
    fn create(&self, channel: &str) -> Result<Arc<dyn ChannelSession>, DriverError> {
        // 兩個同名 create 並存時，完整路徑解析到 inherent 版（回傳具體型別）。
        let session = DefaultChannelSessionFactory::create(self, channel)?;
        Ok(Arc::new(session) as Arc<dyn ChannelSession>)
    }
}

/// 舊版 `SessionManagerDependencies`。
#[derive(Default)]
pub(crate) struct SessionManagerDependencies {
    /// 舊版 `logger`；預設 no-op（`NOOP_LOGGER`）。
    pub(crate) logger: Option<Arc<dyn BrowserManagerLogger>>,
    /// 舊版 `sleep`；預設 [`crate::browser::types::TokioSleep`]。
    pub(crate) sleep: Option<Arc<dyn SleepFn>>,
    /// 舊版 `startRetryAttempts`（預設 0；`maxStartAttempts = 1 + attempts`）。
    pub(crate) start_retry_attempts: Option<u64>,
    /// 舊版 `startRetryDelayMs`（預設 0）。
    pub(crate) start_retry_delay_ms: Option<u64>,
    /// 舊版 `startStaggerMs`（預設 0）。
    pub(crate) start_stagger_ms: Option<u64>,
    /// 舊版 `sessionOperationTimeoutMs`（預設 60_000）。
    pub(crate) session_operation_timeout_ms: Option<u64>,
    /// 舊版 `onNavigationOutcomes`。
    pub(crate) on_navigation_outcomes: Option<Arc<dyn SessionNavigationOutcomeObserver>>,
    /// 舊版 `recoveryPolicy`（Rust 版的 `&mut self` 介面需要 `Mutex`；
    /// 只在同步區段上鎖，絕不跨 `await` 持有）。
    pub(crate) recovery_policy: Option<Arc<Mutex<ChannelRecoveryPolicy>>>,
    /// 舊版 `configuredChannels`。
    pub(crate) configured_channels: Vec<String>,
}

/// 舊版 `DEFAULT_START_RETRY_ATTEMPTS`。
const DEFAULT_START_RETRY_ATTEMPTS: u64 = 0;
/// 舊版 `DEFAULT_START_RETRY_DELAY_MS`。
const DEFAULT_START_RETRY_DELAY_MS: u64 = 0;
/// 舊版 `DEFAULT_START_STAGGER_MS`。
const DEFAULT_START_STAGGER_MS: u64 = 0;
/// 舊版 `DEFAULT_SESSION_OPERATION_TIMEOUT_MS`。
const DEFAULT_SESSION_OPERATION_TIMEOUT_MS: u64 = 60_000;

/// 舊版 `withSessionOperationTimeout` 的事件名稱（變數轉發照舊版字面）。
const SESSION_START_TIMEOUT_EVENT: &str = "session_start_timeout";
/// 舊版 cancelStart 包裝的事件名稱。
const SESSION_START_CANCEL_TIMEOUT_EVENT: &str = "session_start_cancel_timeout";
/// 舊版 stopSession 包裝的事件名稱。
const SESSION_STOP_TIMEOUT_EVENT: &str = "session_stop_timeout";
/// 舊版 stopAll 整體包裝的事件名稱。
const SESSION_STOP_ALL_TIMEOUT_EVENT: &str = "session_stop_all_timeout";

/// 舊版逾時操作內建錯誤 `SessionOperationTimeoutError`：
/// `message = "${event} after ${timeoutMs}ms"`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SessionOperationTimeoutError {
    /// 舊版 `event`（逾時事件的字面名稱）。
    pub(crate) event: &'static str,
    /// 舊版 `timeoutMs`。
    pub(crate) timeout_ms: u64,
}

impl std::fmt::Display for SessionOperationTimeoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} after {}ms", self.event, self.timeout_ms)
    }
}

/// `with_session_operation_timeout` 的一次操作結果：成功為 `T`，失敗帶
/// `DriverError`（對映舊版被 catch 的 `Error`）或逾時錯誤。
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum SessionOperationFailure {
    /// 操作本身拋出的驅動錯誤（對映舊版的 `Error.message` 來源）。
    Driver(DriverError),
    /// 舊版 `SessionOperationTimeoutError`。
    Timeout(SessionOperationTimeoutError),
}

impl SessionOperationFailure {
    /// 舊版 `safeErrorMessage(error)`：`Error.message`／逾時的
    /// `"${event} after ${timeoutMs}ms"`，再經 [`redact_sensitive_string`] 消毒。
    pub(crate) fn safe_message(&self) -> String {
        match self {
            Self::Driver(error) => safe_error_message(Some(&error.message)),
            Self::Timeout(timeout_error) => safe_error_message(Some(&timeout_error.to_string())),
        }
    }
}

/// 舊版私有 `RecoveryProbeGate`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecoveryProbeGate {
    /// 舊版 `{ allowed: false }`。
    Denied,
    /// 舊版 `{ allowed: true, recoveryGeneration? }`。
    Allowed(Option<u32>),
}

// ---------------------------------------------------------------------------
// 排隊結構（channelOperationTails）
// ---------------------------------------------------------------------------

/// 舊版 `channelOperationTails` 的登記項目：每頻道一把 [`ExclusiveQueue`]，
/// 附「最近一次登記」的序號（對映舊版每次登記都 `set(channel, tail)` 的唯一性：
/// 完成後僅當自己仍是最新登記者——`get(channel) === tail` 的等價判斷——才把鍵刪掉）。
struct ChannelOperationTail {
    queue: Arc<ExclusiveQueue>,
    generation: u64,
}

// ---------------------------------------------------------------------------
// DefaultSessionManager
// ---------------------------------------------------------------------------

/// 舊版 `DefaultSessionManager`：包 [`Inner`] 的廉價把手。
#[derive(Clone)]
pub(crate) struct DefaultSessionManager {
    inner: Arc<Inner>,
}

/// manager 的全部狀態與相依（同步狀態以 `std::sync::Mutex` 保護；鎖內無 await）。
struct Inner {
    // options（建構時凍結）
    factory: Arc<dyn SessionChannelSessionFactory>,
    logger: Arc<dyn BrowserManagerLogger>,
    sleep: Arc<dyn SleepFn>,
    /// 舊版 `maxStartAttempts`。
    max_start_attempts: u64,
    /// 舊版 `startRetryDelayMs`。
    start_retry_delay_ms: u64,
    /// 舊版 `startStaggerMs`。
    start_stagger_ms: u64,
    /// 舊版 `sessionOperationTimeoutMs`。
    session_operation_timeout_ms: u64,
    /// 舊版 `onNavigationOutcomes`。
    on_navigation_outcomes: Option<Arc<dyn SessionNavigationOutcomeObserver>>,
    /// 舊版 `recoveryPolicy`。
    recovery_policy: Option<Arc<Mutex<ChannelRecoveryPolicy>>>,

    // 可變狀態（std Mutex；鎖內無 await）
    /// 舊版 `sessions`（插入順序有意義：`getActiveChannels`／查詢方法）。
    sessions: Mutex<Vec<(String, Arc<dyn ChannelSession>)>>,
    /// 舊版 `startingSessions`。
    starting_sessions: Mutex<Vec<(String, Arc<dyn ChannelSession>)>>,
    /// 舊版 `cancelledStartSessions`（WeakSet）。
    cancelled_start_sessions: Mutex<Vec<Weak<dyn ChannelSession>>>,
    /// 舊版 `shutdownRequested`。
    shutdown_requested: AtomicBool,
    /// 舊版 `configuredChannels`。
    configured_channels: Mutex<Vec<String>>,
    /// 舊版 `stableRecoveryGenerations`。
    stable_recovery_generations: Mutex<Vec<(String, u32)>>,
    /// 舊版 `channelOperationTails`。
    channel_operation_tails: Mutex<HashMap<String, ChannelOperationTail>>,
}

/// `startSession` 一次 attempt 的去向（對映舊版 `return`／retry 的流程）。
enum StartAttemptOutcome {
    /// `return { channel, status: 'succeeded' }`。
    Succeeded(BrowserNavigationOutcome),
    /// `return { channel, status: 'timed_out' }`。
    TimedOut(BrowserNavigationOutcome),
    /// `return undefined`。
    Failed,
    /// 短重試（已排 sleep）。
    Retry,
}

impl DefaultSessionManager {
    /// 舊版建構函式：`new(factory, dependencies = {})`。
    pub(crate) fn new(
        factory: Arc<dyn SessionChannelSessionFactory>,
        dependencies: SessionManagerDependencies,
    ) -> Self {
        let SessionManagerDependencies {
            logger,
            sleep,
            start_retry_attempts,
            start_retry_delay_ms,
            start_stagger_ms,
            session_operation_timeout_ms,
            on_navigation_outcomes,
            recovery_policy,
            configured_channels,
        } = dependencies;
        // 舊版 normalizeNonNegativeInteger 的域是 JS number（NaN／非有限數回
        // fallback）；此介面的輸入即設定值，經 loader 驗證為非負整數，
        // 端點型別已是 u64，跨不進 NaN 語意（DEC-021 同款取捨）。
        Self {
            inner: Arc::new(Inner {
                factory,
                logger: logger.unwrap_or_else(|| {
                    Arc::new(NoopSessionManagerLogger) as Arc<dyn BrowserManagerLogger>
                }),
                sleep: sleep.unwrap_or_else(|| {
                    Arc::new(crate::browser::types::TokioSleep) as Arc<dyn SleepFn>
                }),
                max_start_attempts: 1 + start_retry_attempts
                    .unwrap_or(DEFAULT_START_RETRY_ATTEMPTS),
                start_retry_delay_ms: start_retry_delay_ms.unwrap_or(DEFAULT_START_RETRY_DELAY_MS),
                start_stagger_ms: start_stagger_ms.unwrap_or(DEFAULT_START_STAGGER_MS),
                session_operation_timeout_ms: positive_integer(
                    session_operation_timeout_ms,
                    DEFAULT_SESSION_OPERATION_TIMEOUT_MS,
                ),
                on_navigation_outcomes,
                recovery_policy,
                sessions: Mutex::new(Vec::new()),
                starting_sessions: Mutex::new(Vec::new()),
                cancelled_start_sessions: Mutex::new(Vec::new()),
                shutdown_requested: AtomicBool::new(false),
                configured_channels: Mutex::new(normalize_channel_list(&configured_channels)),
                stable_recovery_generations: Mutex::new(Vec::new()),
                channel_operation_tails: Mutex::new(HashMap::new()),
            }),
        }
    }
}

#[async_trait]
impl SessionManager for DefaultSessionManager {
    async fn reconcile(&self, active_channels: &[String]) {
        Inner::reconcile(&self.inner, active_channels).await;
    }

    fn update_configured_channels(&self, channels: &[String]) {
        Inner::update_configured_channels(&self.inner, channels);
    }

    async fn cancel_pending_starts(&self, reason: &str) {
        Inner::cancel_pending_starts(&self.inner, reason).await;
    }

    async fn stop_all(&self, reason: &str) {
        Inner::stop_all(&self.inner, reason).await;
    }

    async fn invalidate(&self, channel: &str, reason: &str) {
        Inner::invalidate(&self.inner, channel, reason).await;
    }

    fn get_active_channels(&self) -> Vec<String> {
        self.inner.get_active_channels()
    }

    fn get_refresh_statuses(&self) -> Vec<ChannelSessionRefreshStatus> {
        self.inner.get_refresh_statuses()
    }

    async fn refresh_pages(&self, channel: Option<&str>) -> Vec<SessionRefreshResult> {
        self.inner.refresh_pages(channel).await
    }

    async fn capture_screenshot(&self, channel: Option<&str>) -> Option<SessionScreenshot> {
        self.inner.capture_screenshot(channel).await
    }

    async fn get_channel_points(&self, channel: Option<&str>) -> Vec<SessionChannelPointsResult> {
        self.inner.get_channel_points(channel).await
    }
}

/// 舊版預設 logger 的 `NOOP_LOGGER`。
struct NoopSessionManagerLogger;

// 舊版 `SessionManagerLogger` 是 `Pick<Logger, 'debug'|'error'|'warn'>`；
// trait 的 info 沒有舊版對應，保留 no-op。
impl BrowserManagerLogger for NoopSessionManagerLogger {
    fn debug(&self, _event: &str, _fields: Option<&SanitizedValue>) {}
    fn warn(&self, _event: &str, _fields: Option<&SanitizedValue>) {}
    fn error(&self, _event: &str, _fields: Option<&SanitizedValue>) {}
    fn info(&self, _event: &str, _fields: Option<&SanitizedValue>) {}
}

impl Inner {
    // ===== 公用 API（trait 對應的內部版） =====

    /// 舊版 `reconcile` 主體。
    async fn reconcile(this: &Arc<Self>, active_channels: &[String]) {
        if this.shutdown_requested.load(Ordering::Acquire) {
            return;
        }
        this.record_stable_recovery_resets();
        let desired_channels = unique_channels(active_channels);
        let mut navigation_outcomes: Vec<BrowserNavigationOutcome> = Vec::new();
        let started_at = Instant::now();
        this.safe_log(
            LogLevel::Debug,
            "session_reconcile_started",
            fields(vec![
                ("desiredChannels", sv_strs(&desired_channels)),
                ("activeBefore", sv_strs(&this.sessions_keys())),
            ]),
        );

        // 舊版：channelsToStop 的每個 stop 都在「登記當下」排進各頻道的尾巴，
        // 一起 await（join_all）。優先收集成 Vec，確保登記是同步的。
        let desired_matcher = desired_channels.clone();
        let channels_to_stop: Vec<String> = {
            let sessions = this
                .sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            sessions
                .iter()
                .filter(|(channel, _)| !desired_matcher.contains(channel))
                .map(|(channel, _)| channel.clone())
                .collect()
        };
        let stops: Vec<Pin<Box<dyn Future<Output = bool> + Send>>> = channels_to_stop
            .into_iter()
            .map(|channel| {
                let this = Arc::clone(this);
                let channel_key = channel.clone();
                let operation = {
                    let this = Arc::clone(&this);
                    Box::pin(async move {
                        // 舊版：在 exclusive 區內重讀 map（等待期間可能已被移除）。
                        let session = {
                            let mut sessions = Inner::lock_sessions(&this);
                            ordered_delete(&mut sessions, &channel)
                        };
                        match session {
                            None => false,
                            Some(session) => {
                                this.stop_session(&session, &channel, "inactive").await;
                                true
                            }
                        }
                    })
                };
                this.run_channel_exclusive(channel_key, operation)
            })
            .collect();
        let stop_results = join_all(stops).await;
        let stopped_count = stop_results.iter().filter(|stopped| **stopped).count();

        let mut started_count = 0_u64;
        let mut has_attempted_start = false;
        for channel in &desired_channels {
            if this.shutdown_requested.load(Ordering::Acquire) || this.sessions_contains(channel) {
                continue;
            }
            if has_attempted_start && this.start_stagger_ms > 0 {
                this.sleep.sleep(this.start_stagger_ms).await;
            }
            let channel_owned = channel.clone();
            let exclusive_task = Arc::clone(this);
            let exclusive = exclusive_task.run_channel_exclusive(channel_owned.clone(), {
                let this = Arc::clone(&exclusive_task);
                Box::pin(
                    async move { Inner::start_requested_channels(&this, &channel_owned).await },
                )
            });
            if let Some(outcome) = exclusive.await {
                navigation_outcomes.push(outcome);
            }
            if this.sessions_contains(channel) {
                started_count += 1;
            }
            has_attempted_start = true;
        }

        this.reorder_sessions(&desired_channels);
        this.safe_log(
            LogLevel::Debug,
            "session_reconcile_completed",
            fields(vec![
                ("desiredChannels", sv_strs(&desired_channels)),
                ("activeAfter", sv_strs(&this.sessions_keys())),
                ("stoppedCount", sv_num(stopped_count as f64)),
                ("startedCount", sv_num(started_count as f64)),
                ("durationMs", sv_num(elapsed_ms(&started_at))),
            ]),
        );

        this.notify_navigation_outcomes(&navigation_outcomes).await;
    }

    /// 舊版 reconcile 在 exclusive 區內的啟動步驟。
    async fn start_requested_channels(
        this: &Arc<Self>,
        channel: &str,
    ) -> Option<BrowserNavigationOutcome> {
        if this.shutdown_requested.load(Ordering::Acquire) || this.sessions_contains(channel) {
            return None;
        }
        let gate = this.begin_recovery_probe(channel);
        match gate {
            RecoveryProbeGate::Denied => None,
            RecoveryProbeGate::Allowed(recovery_generation) => {
                this.start_session(channel, recovery_generation).await
            }
        }
    }

    /// 舊版 `updateConfiguredChannels`。
    fn update_configured_channels(&self, channels: &[String]) {
        let next_channels = normalize_channel_list(channels);
        let current_channels: Vec<String> = {
            let configured_channels = self
                .configured_channels
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            configured_channels.clone()
        };
        {
            // 舊版：逐個現在的 configured channel 檢查是否被移除；移除的清復原狀態。
            let mut policy_lock = self.recovery_policy.as_ref().map(|policy| {
                policy
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
            });
            if let Some(policy) = policy_lock.as_deref_mut() {
                for channel in &current_channels {
                    if !next_channels.contains(channel) {
                        policy.remove_channel(channel);
                    }
                }
            }
            let mut stable = self
                .stable_recovery_generations
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            stable.retain(|(channel, _)| next_channels.contains(channel));
        }
        *self
            .configured_channels
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = next_channels;
    }

    /// 舊版 `cancelPendingStarts`。
    async fn cancel_pending_starts(this: &Arc<Self>, reason: &str) {
        this.shutdown_requested.store(true, Ordering::Release);
        let starting = {
            let starting_sessions = this
                .starting_sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            starting_sessions.clone()
        };
        Inner::cancel_selected(this, starting, reason).await;
    }

    /// 對已抓下的快照逐項 cancel(舊版 `Promise.all([...startingSessions]
    /// .map(cancelStart))`;stopAll 在呼叫當下就先抓快照,避免
    /// future 輪詢時的二次快照跟舊版的呼叫時點語意不同)。
    async fn cancel_selected(
        this: &Arc<Self>,
        starting: Vec<(String, Arc<dyn ChannelSession>)>,
        reason: &str,
    ) {
        let reason = reason.to_string();
        let cancellations = starting.into_iter().map(|(channel, session)| {
            // 舊版：直接 cancelStart，不排入頻道尾巴（與 stopAll 的
            // pendingStarts 空操作互補）。
            let reason = reason.clone();
            Box::pin(async move { this.cancel_start(&session, &channel, &reason).await })
        });
        join_all(cancellations).await;
    }

    /// 舊版 `stopAll`：
    ///
    /// 1. 快照 `sessions` 再快照 `startingSessions`，然後清空 `sessions`；
    /// 2. 同步把每個頻道的 stop／空操作排進尾巴（對映舊版 map() 的同步登記），
    ///    之後以同 `Promise.all` 一起等（join_all）；
    /// 3. 全程受 `sessionOperationTimeoutMs` 限制，逾時的錯誤一律吞掉——
    ///    Shutdown 返回後，各頻道的 lifecycle timeout 仍會各自完成清理。
    async fn stop_all(this: &Arc<Self>, reason: &str) {
        this.shutdown_requested.store(true, Ordering::Release);
        let reason = reason.to_string();
        let (session_entries, starting_channels) = {
            let mut sessions = Inner::lock_sessions(this);
            let session_entries = std::mem::take(&mut *sessions);
            let starting_sessions = this
                .starting_sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let starting_channels: Vec<String> = starting_sessions
                .iter()
                .map(|(channel, _)| channel.clone())
                .collect();
            (session_entries, starting_channels)
        };
        let cancellation: Pin<Box<dyn Future<Output = ()> + Send>> = {
            let this = Arc::clone(this);
            let reason = reason.clone();
            Box::pin(async move { Inner::cancel_pending_starts(&this, &reason).await })
        };
        let mut parts: Vec<Pin<Box<dyn Future<Output = ()> + Send>>> = vec![cancellation];
        parts.extend(session_entries.into_iter().map(|(channel, session)| {
            // 舊版：stopAll 以「閉包持有的 session」呼叫 stop（registry 已清空）。
            let this = Arc::clone(this);
            let channel_key = channel.clone();
            let reason = reason.clone();
            let operation = {
                let this = Arc::clone(&this);
                Box::pin(async move { this.stop_session(&session, &channel, &reason).await })
            };
            this.run_channel_exclusive(channel_key, operation)
                as Pin<Box<dyn Future<Output = ()> + Send>>
        }));
        parts.extend(starting_channels.into_iter().map(|channel| {
            // 舊版：對仍在 start 的頻道排空尾巴——等待進行中的 exclusive 區完成。
            let this = Arc::clone(this);
            let operation: Pin<Box<dyn Future<Output = ()> + Send>> = Box::pin(async {});
            this.run_channel_exclusive(channel, operation)
                as Pin<Box<dyn Future<Output = ()> + Send>>
        }));
        // 舊版 Promise.all 的語意：全部丟上一起跑（各元件本身不回錯）。
        let overall: Pin<Box<dyn Future<Output = Result<(), DriverError>> + Send>> =
            Box::pin(async move {
                join_all(parts).await;
                Ok(())
            });
        let timeout_fields_reason = reason;
        let _ = this
            .with_session_operation_timeout(overall, SESSION_STOP_ALL_TIMEOUT_EVENT, || {
                vec![("reason", sv_str(&timeout_fields_reason))]
            })
            .await;
    }

    /// 舊版 `invalidate`。
    async fn invalidate(this: &Arc<Self>, channel: &str, reason: &str) {
        this.safe_log(
            LogLevel::Debug,
            "session_invalidate_started",
            fields(vec![
                ("channel", sv_str(channel)),
                ("reason", sv_str(reason)),
                ("activeBefore", sv_strs(&this.sessions_keys())),
            ]),
        );
        let this = Arc::clone(this);
        let channel = channel.to_string();
        let reason = reason.to_string();
        let invalidated = this.run_channel_exclusive(channel.clone(), {
            let this = Arc::clone(&this);
            let started_at = Instant::now();
            Box::pin(async move {
                // 舊版：在 exclusive 區內重讀 map → 順次 record → delete → stop → log。
                let session = {
                    let mut sessions = Inner::lock_sessions(&this);
                    ordered_delete(&mut sessions, &channel)
                };
                let Some(session) = session else {
                    this.safe_log(
                        LogLevel::Debug,
                        "session_invalidate_completed",
                        fields(vec![
                            ("channel", sv_str(&channel)),
                            ("reason", sv_str(&reason)),
                            ("removed", sv_bool(false)),
                            ("activeAfter", sv_strs(&this.sessions_keys())),
                            ("durationMs", sv_num(elapsed_ms(&started_at))),
                        ]),
                    );
                    return;
                };

                match reason.as_str() {
                    "page_crashed" | "page_closed" => this.record_page_crash(&channel, &reason),
                    "health_failure_threshold" => this.record_start_failure(&channel, &reason),
                    _ => {}
                }

                this.stop_session(&session, &channel, &reason).await;
                this.safe_log(
                    LogLevel::Debug,
                    "session_invalidate_completed",
                    fields(vec![
                        ("channel", sv_str(&channel)),
                        ("reason", sv_str(&reason)),
                        ("removed", sv_bool(true)),
                        ("activeAfter", sv_strs(&this.sessions_keys())),
                        ("durationMs", sv_num(elapsed_ms(&started_at))),
                    ]),
                );
            })
        });
        invalidated.await;
    }

    /// 舊版 invalidate 內 page_crashed／page_closed 的分支（DEC-016）。
    fn record_page_crash(&self, channel: &str, reason: &str) {
        let Some(policy) = &self.recovery_policy else {
            return;
        };
        let decision = {
            let mut policy = policy
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            policy.record_page_crash(channel)
        };
        // 舊版：decision?.status === 'quarantined' 時記 warn。
        match decision {
            super::recovery::PageCrashRecoveryDecision::Quarantined {
                retry_at_ms,
                consecutive_page_crashes,
            } => {
                self.safe_log(
                    LogLevel::Warn,
                    "channel_recovery_quarantined",
                    fields(vec![
                        ("channel", sv_str(channel)),
                        ("retryAtMs", sv_num(retry_at_ms as f64)),
                        ("reason", sv_str(reason)),
                        (
                            "consecutivePageCrashes",
                            sv_num(consecutive_page_crashes as f64),
                        ),
                    ]),
                );
            }
            super::recovery::PageCrashRecoveryDecision::Backoff { .. } => {
                // 舊版：backoff 決策不記事件（下一次 reconcile 走 deferred log）。
            }
        }
    }

    /// 舊版 invalidate 內 health_failure_threshold 的分支（DEC-016）。
    fn record_start_failure(&self, channel: &str, reason: &str) {
        let Some(policy) = &self.recovery_policy else {
            return;
        };
        let decision = {
            let mut policy = policy
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            policy.record_start_failure(channel)
        };
        self.safe_log(
            LogLevel::Warn,
            "channel_health_recovery_backoff",
            fields(vec![
                ("channel", sv_str(channel)),
                ("reason", sv_str(reason)),
                ("retryAtMs", sv_num(decision.retry_at_ms as f64)),
                (
                    "consecutiveFailures",
                    sv_num(decision.consecutive_start_failures as f64),
                ),
            ]),
        );
    }

    /// 舊版 `getActiveChannels`。
    fn get_active_channels(&self) -> Vec<String> {
        self.sessions_keys()
    }

    /// 舊版 `getRefreshStatuses`。
    fn get_refresh_statuses(&self) -> Vec<ChannelSessionRefreshStatus> {
        let sessions = self.session_values();
        sessions
            .iter()
            .map(|session| session.get_refresh_status())
            .collect()
    }

    /// 舊版 `refreshPages`。
    async fn refresh_pages(&self, requested_channel: Option<&str>) -> Vec<SessionRefreshResult> {
        let entries = self.entries_for(requested_channel);
        let mut results = Vec::new();
        for (channel, session) in entries {
            match session.refresh_now().await {
                Ok(refreshed) => {
                    results.push(SessionRefreshResult {
                        channel,
                        status: if refreshed {
                            SessionRefreshStatus::Refreshed
                        } else {
                            SessionRefreshStatus::Unavailable
                        },
                        error: None,
                    });
                }
                Err(error) => {
                    let safe_message = safe_error_message(Some(&error.message));
                    self.safe_log(
                        LogLevel::Warn,
                        "session_manual_refresh_failed",
                        fields(vec![
                            ("channel", sv_str(&channel)),
                            ("error", sv_str(&safe_message)),
                        ]),
                    );
                    results.push(SessionRefreshResult {
                        channel,
                        status: SessionRefreshStatus::Failed,
                        error: Some(safe_message),
                    });
                }
            }
        }
        results
    }

    /// 舊版 `captureScreenshot`。
    async fn capture_screenshot(
        &self,
        requested_channel: Option<&str>,
    ) -> Option<SessionScreenshot> {
        let entries = self.entries_for(requested_channel);
        for (channel, session) in entries {
            match session.capture_screenshot().await {
                Ok(image) => {
                    return Some(SessionScreenshot { channel, image });
                }
                Err(error) => {
                    let safe_message = safe_error_message(Some(&error.message));
                    self.safe_log(
                        LogLevel::Warn,
                        "session_screenshot_failed",
                        fields(vec![
                            ("channel", sv_str(&channel)),
                            ("error", sv_str(&safe_message)),
                        ]),
                    );
                }
            }
        }
        None
    }

    /// 舊版 `getChannelPoints`。
    ///
    /// 舊版的 catch 分支（`reason: 'failed'`）在本介面下不可達：
    /// `ChannelSession::get_channel_points` 的契約不回錯（模組文件）。
    async fn get_channel_points(
        &self,
        requested_channel: Option<&str>,
    ) -> Vec<SessionChannelPointsResult> {
        let entries = self.entries_for(requested_channel);
        let mut results = Vec::new();
        for (channel, session) in entries {
            match session.get_channel_points().await {
                ChannelSessionPointsResult::Available { balance } => {
                    results.push(SessionChannelPointsResult::Available {
                        channel,
                        balance: balance as f64,
                    });
                }
                reason => {
                    results.push(SessionChannelPointsResult::Unavailable {
                        channel,
                        reason: points_unavailable_reason(reason),
                    });
                }
            }
        }
        results
    }

    // ===== 私有方法（舊版 private 對應） =====

    /// 舊版 `startSession`。
    async fn start_session(
        self: &Arc<Self>,
        channel: &str,
        recovery_generation: Option<u32>,
    ) -> Option<BrowserNavigationOutcome> {
        let max_start_attempts = self.max_start_attempts;
        let mut attempt = 1_u64;
        while attempt <= max_start_attempts {
            let started_at = Instant::now();
            self.safe_log(
                LogLevel::Debug,
                "session_start_attempt_started",
                fields(vec![
                    ("channel", sv_str(channel)),
                    ("attempt", sv_num(attempt as f64)),
                    ("maxAttempts", sv_num(max_start_attempts as f64)),
                ]),
            );
            let outcome = match self.factory.create(channel) {
                Err(factory_error) => {
                    // 舊版 catch：factory 失敗時 session === undefined，跳過清理。
                    finish_failed_start(
                        self,
                        channel,
                        attempt,
                        recovery_generation,
                        None,
                        SessionOperationFailure::Driver(factory_error),
                    )
                    .await
                }
                Ok(session) => {
                    {
                        let mut starting_sessions = Inner::lock_starting_sessions(self);
                        ordered_set(
                            &mut starting_sessions,
                            channel.to_string(),
                            Arc::clone(&session),
                        );
                    }
                    let operation = {
                        let session = Arc::clone(&session);
                        async move { session.start().await }
                    };
                    let timeout_fields_channel = channel.to_string();
                    let start_result = self
                        .with_session_operation_timeout(
                            Box::pin(operation),
                            SESSION_START_TIMEOUT_EVENT,
                            || {
                                vec![
                                    ("channel", sv_str(&timeout_fields_channel)),
                                    ("attempt", sv_num(attempt as f64)),
                                ]
                            },
                        )
                        .await;
                    let outcome = match start_result {
                        Ok(()) => {
                            if self.shutdown_requested.load(Ordering::Acquire) {
                                self.stop_session(&session, channel, "shutdown").await;
                                StartAttemptOutcome::Failed
                            } else {
                                {
                                    let mut sessions = Inner::lock_sessions(self);
                                    ordered_set(
                                        &mut sessions,
                                        channel.to_string(),
                                        Arc::clone(&session),
                                    );
                                }
                                self.mark_stable_if_probed(channel, recovery_generation);
                                self.safe_log(
                                    LogLevel::Debug,
                                    "session_start_attempt_completed",
                                    fields(vec![
                                        ("channel", sv_str(channel)),
                                        ("attempt", sv_num(attempt as f64)),
                                        ("durationMs", sv_num(elapsed_ms(&started_at))),
                                    ]),
                                );
                                StartAttemptOutcome::Succeeded(BrowserNavigationOutcome {
                                    channel: channel.to_string(),
                                    status: BrowserNavigationStatus::Succeeded,
                                })
                            }
                        }
                        Err(failure) => {
                            finish_failed_start(
                                self,
                                channel,
                                attempt,
                                recovery_generation,
                                Some(&session),
                                failure,
                            )
                            .await
                        }
                    };
                    // 舊版 finally：startingSessions 的登記仍為本 instance 時才刪除。
                    {
                        let mut starting_sessions = Inner::lock_starting_sessions(self);
                        remove_starting_if_same(&mut starting_sessions, channel, &session);
                    }
                    outcome
                }
            };
            match outcome {
                StartAttemptOutcome::Succeeded(outcome)
                | StartAttemptOutcome::TimedOut(outcome) => {
                    return Some(outcome);
                }
                StartAttemptOutcome::Failed => {
                    return None;
                }
                StartAttemptOutcome::Retry => {
                    attempt += 1;
                }
            }
        }
        None
    }

    /// 舊版 start 成功的搭配：generation 在手時 `markStable` 成功才登記
    /// stableRecoveryGenerations（key 以 normalizeChannel）。
    fn mark_stable_if_probed(&self, channel: &str, recovery_generation: Option<u32>) {
        let Some(recovery_generation) = recovery_generation else {
            return;
        };
        let Some(policy) = &self.recovery_policy else {
            return;
        };
        let marked_stable = {
            let mut policy = policy
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            policy.mark_stable(channel, recovery_generation)
        };
        if marked_stable {
            let mut stable = self
                .stable_recovery_generations
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let normalized = normalize_recovery_channel(channel);
            // 舊版 `stableRecoveryGenerations.set(normalizeChannel(channel), …)`：
            // 鍵已存在時原地覆值。
            if let Some(existing) = stable.iter_mut().find(|(key, _)| *key == normalized) {
                existing.1 = recovery_generation;
            } else {
                stable.push((normalized, recovery_generation));
            }
        }
    }

    /// 舊版 `notifyNavigationOutcomes`。
    async fn notify_navigation_outcomes(&self, outcomes: &[BrowserNavigationOutcome]) {
        let Some(observer) = &self.on_navigation_outcomes else {
            return;
        };
        if outcomes.is_empty() {
            return;
        }
        match observer.on_navigation_outcomes(outcomes).await {
            Ok(()) => {}
            Err(error) => {
                self.safe_log(
                    LogLevel::Warn,
                    "session_navigation_outcome_observer_failed",
                    fields(vec![(
                        "error",
                        sv_str(safe_error_message(Some(&error.message))),
                    )]),
                );
            }
        }
    }

    /// 舊版 `beginRecoveryProbe`。
    fn begin_recovery_probe(&self, channel: &str) -> RecoveryProbeGate {
        let Some(policy) = &self.recovery_policy else {
            return RecoveryProbeGate::Allowed(None);
        };
        let (state, availability) = {
            let mut policy = policy
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let state = policy.get_state(channel);
            let availability = policy.get_availability(channel);
            (state, availability)
        };
        let has_recovery_history =
            state.consecutive_page_crashes > 0 || state.consecutive_start_failures > 0;
        if !has_recovery_history {
            return RecoveryProbeGate::Allowed(None);
        }

        match availability {
            ChannelRecoveryAvailability::Quarantined { retry_at_ms, .. } => {
                self.safe_log(
                    LogLevel::Warn,
                    "channel_recovery_quarantined",
                    fields(vec![
                        ("channel", sv_str(channel)),
                        ("retryAtMs", sv_num(retry_at_ms as f64)),
                        ("reason", sv_str("quarantine_active")),
                    ]),
                );
                RecoveryProbeGate::Denied
            }
            ChannelRecoveryAvailability::Deferred { .. }
            | ChannelRecoveryAvailability::ProbeInFlight { .. } => {
                // 舊版：`'retryAtMs' in availability` 時補 retryAtMs，否則省略。
                let mut entries = vec![
                    ("channel", sv_str(channel)),
                    (
                        "reason",
                        sv_str(recovery_availability_status(&availability)),
                    ),
                ];
                if matches!(availability, ChannelRecoveryAvailability::Deferred { .. })
                    && let ChannelRecoveryAvailability::Deferred { retry_at_ms, .. } = availability
                {
                    entries.push(("retryAtMs", sv_num(retry_at_ms as f64)));
                }
                self.safe_log(
                    LogLevel::Debug,
                    "channel_recovery_deferred",
                    fields(entries),
                );
                RecoveryProbeGate::Denied
            }
            ChannelRecoveryAvailability::Eligible { .. } => {
                let probe = {
                    let mut policy = policy
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    policy.begin_probe(channel)
                };
                match probe {
                    BeginRecoveryProbeResult::Started {
                        recovery_generation,
                    } => {
                        self.safe_log(
                            LogLevel::Debug,
                            "channel_recovery_probe_started",
                            fields(vec![
                                ("channel", sv_str(channel)),
                                ("recoveryGeneration", sv_num(recovery_generation as f64)),
                            ]),
                        );
                        RecoveryProbeGate::Allowed(Some(recovery_generation))
                    }
                    BeginRecoveryProbeResult::NotEligible(availability) => {
                        // 舊版：只記 reason（probe.status），不帶 retryAtMs。
                        self.safe_log(
                            LogLevel::Debug,
                            "channel_recovery_deferred",
                            fields(vec![
                                ("channel", sv_str(channel)),
                                (
                                    "reason",
                                    sv_str(recovery_availability_status(&availability)),
                                ),
                            ]),
                        );
                        RecoveryProbeGate::Denied
                    }
                }
            }
        }
    }

    /// 舊版 `recordStableRecoveryResets`。
    fn record_stable_recovery_resets(&self) {
        let Some(policy) = &self.recovery_policy else {
            return;
        };
        let mut stable = self
            .stable_recovery_generations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // 舊版：逐項走訪（插入序），符合條件的鍵原地刪除並記 log；
        // Rust 的 Vec 先收集要刪的索引與記錄，再倒序刪除、正序記 log。
        let mut removal_indexes: Vec<usize> = Vec::new();
        let mut reset_records: Vec<(String, u32)> = Vec::new();
        for (index, (channel, recovery_generation)) in stable.iter().enumerate() {
            let (channel, recovery_generation) = (channel.clone(), *recovery_generation);
            let state = {
                let mut policy = policy
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                policy.get_state(&channel)
            };
            if state.recovery_generation == recovery_generation
                && state.consecutive_page_crashes == 0
                && state.consecutive_start_failures == 0
                && state.stable_since_ms.is_none()
            {
                removal_indexes.push(index);
                reset_records.push((channel, recovery_generation));
            }
        }
        for index in removal_indexes.into_iter().rev() {
            stable.remove(index);
        }
        for (channel, recovery_generation) in reset_records {
            self.safe_log(
                LogLevel::Debug,
                "channel_recovery_reset",
                fields(vec![
                    ("channel", sv_str(&channel)),
                    ("recoveryGeneration", sv_num(recovery_generation as f64)),
                ]),
            );
        }
    }

    /// 舊版 `cancelTimedOutStart`（reason 固定 `start_timeout`）。
    async fn cancel_timed_out_start(
        self: &Arc<Self>,
        session: &Arc<dyn ChannelSession>,
        channel: &str,
    ) {
        self.cancel_start(session, channel, "start_timeout").await;
    }

    /// 舊版 `cancelStart`：對同一份 ChannelSession instance 至多呼叫一次（WeakSet），
    /// 逾時或失敗只記 warn（`session_start_cancel_failed`）。
    async fn cancel_start(
        self: &Arc<Self>,
        session: &Arc<dyn ChannelSession>,
        channel: &str,
        reason: &str,
    ) {
        // 舊版 `session.cancelStart === undefined` 在 Rust trait 一定存在；
        // 其餘重現 WeakSet 的「至多一次」。
        if !self.mark_cancel_started(session) {
            return;
        }
        let session = Arc::clone(session);
        let reason = reason.to_string();
        let timeout_fields_reason = reason.clone();
        let timeout_fields_channel = channel.to_string();
        let operation = async move {
            session.cancel_start(&reason).await;
            Ok(())
        };
        match self
            .with_session_operation_timeout(
                Box::pin(operation),
                SESSION_START_CANCEL_TIMEOUT_EVENT,
                || {
                    vec![
                        ("channel", sv_str(&timeout_fields_channel)),
                        ("reason", sv_str(&timeout_fields_reason)),
                    ]
                },
            )
            .await
        {
            Ok(()) => {}
            Err(failure) => {
                self.safe_log(
                    LogLevel::Warn,
                    "session_start_cancel_failed",
                    fields(vec![
                        ("channel", sv_str(&timeout_fields_channel)),
                        ("error", sv_str(failure.safe_message())),
                    ]),
                );
            }
        }
    }

    /// 舊版 `cleanupFailedStart`。
    async fn cleanup_failed_start(
        self: &Arc<Self>,
        session: &Arc<dyn ChannelSession>,
        channel: &str,
    ) {
        self.stop_session(session, channel, "start_failed").await;
    }

    /// 舊版 `stopSession`：逾時或失敗只記 warn（`session_stop_failed`）。
    async fn stop_session(
        self: &Arc<Self>,
        session: &Arc<dyn ChannelSession>,
        channel: &str,
        reason: &str,
    ) {
        let session = Arc::clone(session);
        let reason = reason.to_string();
        let timeout_fields_reason = reason.clone();
        let timeout_fields_channel = channel.to_string();
        let operation = async move { session.stop(&reason).await };
        match self
            .with_session_operation_timeout(Box::pin(operation), SESSION_STOP_TIMEOUT_EVENT, || {
                vec![
                    ("channel", sv_str(&timeout_fields_channel)),
                    ("reason", sv_str(&timeout_fields_reason)),
                ]
            })
            .await
        {
            Ok(()) => {}
            Err(failure) => {
                self.safe_log(
                    LogLevel::Warn,
                    "session_stop_failed",
                    fields(vec![
                        ("channel", sv_str(&timeout_fields_channel)),
                        ("reason", sv_str(&timeout_fields_reason)),
                        ("error", sv_str(failure.safe_message())),
                    ]),
                );
            }
        }
    }

    /// 舊版 `withSessionOperationTimeout`：
    /// 操作逾時時把 `{...fields, timeoutMs}` 記成 warn 並回傳
    /// [`SessionOperationTimeoutError`]。逾時後 spawn 的操作留在背景跑完
    /// （對映 `operation.catch(() => undefined)`：不取消、結果丟棄）。
    /// `timeout_fields` 以傳回欄位清單的 closure 傳入（capture 皆為 Send 型別）：
    /// [`SanitizedValue`] 內含 `Rc` 不可被 Send future 跨 await 持有
    /// （`FatalRecoveryFieldValue` 的同款取捨），故只在逾時分支（select resolve 後）
    /// 才轉成 [`SanitizedValue`]。
    async fn with_session_operation_timeout<T: Send + 'static>(
        &self,
        operation: Pin<Box<dyn Future<Output = Result<T, DriverError>> + Send>>,
        timeout_event: &'static str,
        timeout_fields: impl FnOnce() -> Vec<(&'static str, SanitizedValue)> + Send,
    ) -> Result<T, SessionOperationFailure> {
        let handle = tokio::spawn(operation);
        let timeout_ms = self.session_operation_timeout_ms;
        tokio::select! {
            result = handle => match result {
                Ok(result) => result.map_err(SessionOperationFailure::Driver),
                Err(join_error) => {
                    // 舊版 promise 世界不可達的路徑（spawn 任務 panic）；
                    // 以防禦性訊息表達，仍走同步的 catch 流程。
                    Err(SessionOperationFailure::Driver(DriverError::new(
                        "Error",
                        format!("session operation panicked: {join_error}"),
                    )))
                }
            },
            _ = tokio::time::sleep(Duration::from_millis(timeout_ms)) => {
                // 舊版 `{...fields, timeoutMs}`：timeoutMs 永遠是最後一個欄位。
                let mut entries = timeout_fields();
                entries.push(("timeoutMs", sv_num(timeout_ms as f64)));
                self.safe_log(LogLevel::Warn, timeout_event, fields(entries));
                Err(SessionOperationFailure::Timeout(SessionOperationTimeoutError {
                    event: timeout_event,
                    timeout_ms,
                }))
            }
        }
    }

    /// 舊版 `reorderSessions`。
    fn reorder_sessions(&self, desired_channels: &[String]) {
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut reordered: Vec<(String, Arc<dyn ChannelSession>)> = Vec::new();
        for channel in desired_channels {
            let Some(index) = sessions
                .iter()
                .position(|(existing, _)| existing == channel)
            else {
                continue;
            };
            reordered.push(sessions.swap_remove(index));
        }
        *sessions = reordered;
    }

    /// 舊版 `runChannelExclusive`：同步登記（掛上尾巴）、回傳「在 exclusive 區
    /// 完成後」結束的 future。完成後若自己仍是最新登記者
    /// （`get(channel) === tail` 的等價判斷），把該頻道的尾巴項目刪掉。
    fn run_channel_exclusive<T: Send + 'static>(
        self: &Arc<Self>,
        channel: String,
        operation: Pin<Box<dyn Future<Output = T> + Send>>,
    ) -> Pin<Box<dyn Future<Output = T> + Send>> {
        let (queue, generation, slot) = {
            let mut tails = self
                .channel_operation_tails
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let tail = tails
                .entry(channel.clone())
                .or_insert_with(|| ChannelOperationTail {
                    queue: ExclusiveQueue::new(),
                    generation: 0,
                });
            tail.generation += 1;
            (
                Arc::clone(&tail.queue),
                tail.generation,
                tail.queue.enqueue(),
            )
        };
        let this = Arc::clone(self);
        Box::pin(async move {
            // 登記佔位在當下完成；acquire 之後才是自己的 exclusive 區。
            let guard = slot.acquire().await;
            let result = operation.await;
            drop(guard);
            this.prune_channel_tail(&channel, &queue, generation);
            result
        })
    }

    /// 對映舊版 `void tail.then(() => { if (get(channel) === tail) delete })`。
    fn prune_channel_tail(&self, channel: &str, queue: &Arc<ExclusiveQueue>, generation: u64) {
        let mut tails = self
            .channel_operation_tails
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let removable = matches!(
            tails.get(channel),
            Some(tail) if Arc::ptr_eq(&tail.queue, queue) && tail.generation == generation
        );
        if removable {
            tails.remove(channel);
        }
    }

    /// 舊版 `cancelledStartSessions` 的登記：先清掉已結束的弱參照，再比對活著的
    /// 弱參照；同一份 ChannelSession instance 至多登記一次。回傳 false 表示已取消過。
    fn mark_cancel_started(&self, session: &Arc<dyn ChannelSession>) -> bool {
        let mut cancelled = self
            .cancelled_start_sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        cancelled.retain(|weak| weak.strong_count() > 0);
        let already_cancelled = cancelled.iter().any(|weak| {
            weak.strong_count() > 0 && std::ptr::addr_eq(Weak::as_ptr(weak), Arc::as_ptr(session))
        });
        if already_cancelled {
            return false;
        }
        cancelled.push(Arc::downgrade(session));
        true
    }

    // ===== 同步狀態輔助（鎖內無 await） =====

    /// `sessions` 的鎖（呼叫端用完即還；絕不跨 await 持有）。
    fn lock_sessions(
        this: &Arc<Self>,
    ) -> std::sync::MutexGuard<'_, Vec<(String, Arc<dyn ChannelSession>)>> {
        this.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// `startingSessions` 的鎖。
    fn lock_starting_sessions(
        this: &Arc<Self>,
    ) -> std::sync::MutexGuard<'_, Vec<(String, Arc<dyn ChannelSession>)>> {
        this.starting_sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// `sessions` 的鍵清單（插入序）。
    fn sessions_keys(&self) -> Vec<String> {
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .map(|(channel, _)| channel.clone())
            .collect()
    }

    /// 舊版 `this.sessions.has(channel)`。
    fn sessions_contains(&self, key: &str) -> bool {
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .any(|(channel, _)| channel == key)
    }

    /// `sessions` 的值清單（插入序；查詢方法用）。
    fn session_values(&self) -> Vec<Arc<dyn ChannelSession>> {
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .map(|(_, session)| Arc::clone(session))
            .collect()
    }

    /// 舊版查詢方法的 entries：全部，或 findSession 命中的一筆。
    fn entries_for(
        &self,
        requested_channel: Option<&str>,
    ) -> Vec<(String, Arc<dyn ChannelSession>)> {
        let sessions = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match requested_channel {
            None => sessions.clone(),
            Some(requested_channel) => {
                let entry = find_session(&sessions, requested_channel);
                match entry {
                    Some(entry) => vec![entry],
                    None => Vec::new(),
                }
            }
        }
    }

    /// 安全轉發日誌：logger 失敗不得打斷 session lifecycle 清理。
    /// 舊版以 try/catch 達成；Rust 的 logger 契約本就沒有失敗回傳。
    fn safe_log(&self, level: LogLevel, event: &str, fields_value: SanitizedValue) {
        match level {
            LogLevel::Debug => self.logger.debug(event, Some(&fields_value)),
            LogLevel::Info => self.logger.info(event, Some(&fields_value)),
            LogLevel::Warn => self.logger.warn(event, Some(&fields_value)),
            LogLevel::Error => self.logger.error(event, Some(&fields_value)),
        }
    }
}

/// `startSession` catch 區段（session 可能未建立）：逾時的
/// `session_start_timeout` 先 cancel、一律 cleanup stop（`start_failed`），
/// 之後依 safeError 決定重試或收場。
async fn finish_failed_start(
    this: &Arc<Inner>,
    channel: &str,
    attempt: u64,
    recovery_generation: Option<u32>,
    session: Option<&Arc<dyn ChannelSession>>,
    failure: SessionOperationFailure,
) -> StartAttemptOutcome {
    let safe_error = failure.safe_message();
    if let Some(session) = &session {
        if let SessionOperationFailure::Timeout(timeout_error) = &failure
            && timeout_error.event == SESSION_START_TIMEOUT_EVENT
        {
            this.cancel_timed_out_start(session, channel).await;
        }
        this.cleanup_failed_start(session, channel).await;
    }

    let should_retry =
        attempt < this.max_start_attempts && is_retriable_session_start_error(&safe_error);
    if !should_retry {
        if is_recoverable_session_start_error(&safe_error) {
            let policy = this.recovery_policy.as_ref();
            if let Some(policy) = policy {
                policy
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .record_start_failure(channel);
            }
        } else if let Some(recovery_generation) = recovery_generation {
            let policy = this.recovery_policy.as_ref();
            if let Some(policy) = policy {
                policy
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .cancel_probe(channel, recovery_generation);
            }
        }
        this.safe_log(
            LogLevel::Error,
            "session_start_failed",
            fields(vec![
                ("channel", sv_str(channel)),
                ("error", sv_str(&safe_error)),
            ]),
        );
        return if is_navigation_timeout_error(&safe_error) {
            StartAttemptOutcome::TimedOut(BrowserNavigationOutcome {
                channel: channel.to_string(),
                status: BrowserNavigationStatus::TimedOut,
            })
        } else {
            StartAttemptOutcome::Failed
        };
    }

    this.safe_log(
        LogLevel::Warn,
        "session_start_retry_scheduled",
        fields(vec![
            ("channel", sv_str(channel)),
            ("attempt", sv_num(attempt as f64)),
            ("retryInMs", sv_num(this.start_retry_delay_ms as f64)),
            ("error", sv_str(&safe_error)),
        ]),
    );
    if this.start_retry_delay_ms > 0 {
        this.sleep.sleep(this.start_retry_delay_ms).await;
    }
    StartAttemptOutcome::Retry
}

// ---------------------------------------------------------------------------
// 通用輔助（舊版 module-level functions）
// ---------------------------------------------------------------------------

/// 舊版 `uniqueChannels`：完全相等字串的去重（保留原序）。
fn unique_channels(channels: &[String]) -> Vec<String> {
    let mut unique: Vec<String> = Vec::with_capacity(channels.len());
    for channel in channels {
        if !unique.contains(channel) {
            unique.push(channel.clone());
        }
    }
    unique
}

/// 舊版 `new Set(channels.map(normalizeChannel))`：正規化＋去重（保留原序）。
fn normalize_channel_list(channels: &[String]) -> Vec<String> {
    let mut normalized: Vec<String> = Vec::new();
    for channel in channels {
        let normalized_channel = normalize_recovery_channel(channel);
        if !normalized.contains(&normalized_channel) {
            normalized.push(normalized_channel);
        }
    }
    normalized
}

/// 舊版 `findSession`：以 trim＋小寫比對（registry 的鍵不 trim）。
fn find_session(
    sessions: &[(String, Arc<dyn ChannelSession>)],
    requested_channel: &str,
) -> Option<(String, Arc<dyn ChannelSession>)> {
    let normalized = requested_channel.trim().to_lowercase();
    sessions
        .iter()
        .find(|(channel, _)| channel.to_lowercase() == normalized)
        .map(|(channel, session)| (channel.clone(), Arc::clone(session)))
}

/// `Instant` 差值的毫秒數（`Date.now()` 差值同形：整數毫秒）。
fn elapsed_ms(started_at: &Instant) -> f64 {
    started_at.elapsed().as_millis() as f64
}

/// 舊版 `ChannelSessionPointsResult` → `SessionChannelPointsResult` 的 reason 映射。
fn points_unavailable_reason(
    result: ChannelSessionPointsResult,
) -> SessionChannelPointsUnavailableReason {
    match result {
        ChannelSessionPointsResult::Available { .. } => {
            // available 的分支已在呼叫端處理；此處不可達。
            SessionChannelPointsUnavailableReason::Failed
        }
        ChannelSessionPointsResult::NotFound => SessionChannelPointsUnavailableReason::NotFound,
        ChannelSessionPointsResult::ParseFailed => {
            SessionChannelPointsUnavailableReason::ParseFailed
        }
        ChannelSessionPointsResult::PageUnavailable => {
            SessionChannelPointsUnavailableReason::PageUnavailable
        }
    }
}

/// 舊版 `ChannelRecoveryAvailability.status` 的字面值。
fn recovery_availability_status(availability: &ChannelRecoveryAvailability) -> &'static str {
    match availability {
        ChannelRecoveryAvailability::Eligible { .. } => "eligible",
        ChannelRecoveryAvailability::Deferred { .. } => "deferred",
        ChannelRecoveryAvailability::Quarantined { .. } => "quarantined",
        ChannelRecoveryAvailability::ProbeInFlight { .. } => "probe_in_flight",
    }
}

// ---------------------------------------------------------------------------
// 插入序的 registry 小工具（對映舊版 Map／Set 的插入序語意）
// ---------------------------------------------------------------------------

/// 舊版 `Map.set`：鍵已存在時原地覆值（不改插入位置），否則 push。
fn ordered_set(
    sessions: &mut Vec<(String, Arc<dyn ChannelSession>)>,
    key: String,
    session: Arc<dyn ChannelSession>,
) {
    if let Some(existing) = sessions.iter_mut().find(|(channel, _)| *channel == key) {
        existing.1 = session;
        return;
    }
    sessions.push((key, session));
}

/// 舊版 `Map.delete`。
fn ordered_delete(
    sessions: &mut Vec<(String, Arc<dyn ChannelSession>)>,
    key: &str,
) -> Option<Arc<dyn ChannelSession>> {
    if let Some(index) = sessions.iter().position(|(channel, _)| channel == key) {
        return Some(sessions.remove(index).1);
    }
    None
}

/// 舊版 startingSessions 的 `finally` 刪除：登記的仍是本 instance 時才刪。
fn remove_starting_if_same(
    sessions: &mut Vec<(String, Arc<dyn ChannelSession>)>,
    key: &str,
    session: &Arc<dyn ChannelSession>,
) {
    // 找到同鍵之後比對 instance（Arc 指針），不合就不動。
    let entry = sessions.iter().position(|(channel, _)| channel == key);
    if let Some(index) = entry
        && std::ptr::addr_eq(Arc::as_ptr(&sessions[index].1), Arc::as_ptr(session))
    {
        sessions.remove(index);
    }
}

// ---------------------------------------------------------------------------
// 錯誤分類（§7.7 的 regex 轉換規則）
// ---------------------------------------------------------------------------

/// 編譯固定樣式（樣式是程式內常數且測試覆蓋；編譯失敗即實作錯誤）。
fn compile_pattern(source: &str) -> regex::Regex {
    match regex::Regex::new(source) {
        Ok(regex) => regex,
        Err(error) => panic!("invalid built-in session error pattern {source:?}: {error}"),
    }
}

/// 舊版 `isRetriableSessionStartError`。
fn is_retriable_session_start_error(message: &str) -> bool {
    !is_fatal_prerequisite_error(message) && is_recoverable_session_start_error(message)
}

/// 舊版 `isRecoverableSessionStartError`（`/…/iu`；`i` → `(?i)`，`\s` → 展開）。
fn is_recoverable_session_start_error(message: &str) -> bool {
    closed_session_pattern().is_match(message)
        || closed_browser_pattern().is_match(message)
        || closed_context_pattern().is_match(message)
        || closed_page_pattern().is_match(message)
        || crashed_target_pattern().is_match(message)
        || goto_timeout_pattern().is_match(message)
        || unexpected_twitch_url_pattern().is_match(message)
        || manager_not_started_pattern().is_match(message)
}

/// 舊版 `isFatalPrerequisiteError`。
fn is_fatal_prerequisite_error(message: &str) -> bool {
    auth_failed_pattern().is_match(message)
        || configuration_invalid_pattern().is_match(message)
        || storage_state_pattern().is_match(message)
}

/// 舊版 `isNavigationTimeoutError`。
fn is_navigation_timeout_error(message: &str) -> bool {
    goto_timeout_pattern().is_match(message)
}

/// 舊版 `/Target page, context or browser has been closed/iu`。
fn closed_session_pattern() -> &'static regex::Regex {
    static PATTERN: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    PATTERN.get_or_init(|| compile_pattern("(?i)Target page, context or browser has been closed"))
}

/// 舊版 `/browser(?: manager)? (?:has been )?closed/iu`。
fn closed_browser_pattern() -> &'static regex::Regex {
    static PATTERN: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    PATTERN.get_or_init(|| compile_pattern("(?i)browser(?: manager)? (?:has been )?closed"))
}

/// 舊版 `/context .*closed/iu`。
fn closed_context_pattern() -> &'static regex::Regex {
    static PATTERN: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    PATTERN.get_or_init(|| compile_pattern("(?i)context .*closed"))
}

/// 舊版 `/page .*closed/iu`。
fn closed_page_pattern() -> &'static regex::Regex {
    static PATTERN: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    PATTERN.get_or_init(|| compile_pattern("(?i)page .*closed"))
}

/// 舊版 `/target .*crashed/iu`。
fn crashed_target_pattern() -> &'static regex::Regex {
    static PATTERN: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    PATTERN.get_or_init(|| compile_pattern("(?i)target .*crashed"))
}

/// 舊版 `/page\.goto: Timeout \d+ms exceeded/iu`。
fn goto_timeout_pattern() -> &'static regex::Regex {
    static PATTERN: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    PATTERN.get_or_init(|| compile_pattern("(?i)page\\.goto: Timeout \\d+ms exceeded"))
}

/// 舊版 `/導向非預期 Twitch URL/iu`。
fn unexpected_twitch_url_pattern() -> &'static regex::Regex {
    static PATTERN: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    PATTERN.get_or_init(|| compile_pattern("(?i)導向非預期 Twitch URL"))
}

/// 舊版 `/Browser Manager 尚未啟動/iu`。
fn manager_not_started_pattern() -> &'static regex::Regex {
    static PATTERN: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    PATTERN.get_or_init(|| compile_pattern("(?i)Browser Manager 尚未啟動"))
}

/// 舊版 `/auth(?:entication|orization)? failed/iu`。
fn auth_failed_pattern() -> &'static regex::Regex {
    static PATTERN: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    PATTERN.get_or_init(|| compile_pattern("(?i)auth(?:entication|orization)? failed"))
}

/// 舊版 `/configuration (?:error|invalid)/iu`。
fn configuration_invalid_pattern() -> &'static regex::Regex {
    static PATTERN: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    PATTERN.get_or_init(|| compile_pattern("(?i)configuration (?:error|invalid)"))
}

/// 舊版 `/storage[\s_-]?state (?:error|invalid|missing)/iu`
///（`\s` 依 §7.7 展開成明確字元類別）。
fn storage_state_pattern() -> &'static regex::Regex {
    static PATTERN: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    PATTERN.get_or_init(|| {
        compile_pattern(
            "(?i)storage[\t\n\x0B\x0C\r \u{a0}\u{1680}\u{2000}-\u{200a}\u{2028}\u{2029}\
\u{202f}\u{205f}\u{3000}\u{feff}_-]?state (?:error|invalid|missing)",
        )
    })
}

// ---------------------------------------------------------------------------
// 日誌欄位建構子（同 manager／session 模組的小型 helper；插入順序照呼叫端，R10）
// ---------------------------------------------------------------------------

fn sv_str(value: impl Into<String>) -> SanitizedValue {
    SanitizedValue::String(value.into())
}

fn sv_num(value: impl Into<f64>) -> SanitizedValue {
    SanitizedValue::Number(value.into())
}

fn sv_bool(value: bool) -> SanitizedValue {
    SanitizedValue::Bool(value)
}

/// 組日誌欄位物件（插入順序照舊版）。
fn fields(entries: Vec<(&'static str, SanitizedValue)>) -> SanitizedValue {
    SanitizedValue::Object(
        entries
            .into_iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect(),
    )
}

/// `desiredChannels`／`activeBefore` 的清單欄位。
fn sv_strs(values: &[String]) -> SanitizedValue {
    SanitizedValue::Array(
        values
            .iter()
            .map(|value| SanitizedValue::String(value.clone()))
            .collect(),
    )
}

#[cfg(test)]
mod tests;

// ---------------------------------------------------------------------------
// S3.6 接線的窄介面實作（DEC-023／DEC-024 的接線備註）
// ---------------------------------------------------------------------------

#[async_trait]
impl ReconcileSessionManager for DefaultSessionManager {
    /// 舊版 `reconcile(activeChannels)` 的 promise 不 reject（所有清理在
    /// manager 內收斂）；coordinator 的 `reconcile_completed {outcome:`
    /// `'failed'}` 與 `reconcile_submit_failed` 記點因此不可達
    ///（照 DEC-024 note 6 的語意保留事件名）。
    async fn reconcile(
        &self,
        desired_channels: &[String],
    ) -> Result<(), crate::sessions::reconcile::ReconcileError> {
        SessionManager::reconcile(self, desired_channels).await;
        Ok(())
    }

    /// 舊版 `cancelPendingStarts?(reason)`（正式 SessionManager 兩者都實作）。
    async fn cancel_pending_starts(&self, reason: &str) {
        SessionManager::cancel_pending_starts(self, reason).await;
    }
}

#[async_trait]
impl crate::scheduler::watchdog::SchedulerSessionReconcile for DefaultSessionManager {
    /// 舊版 `Pick<SessionManager, 'reconcile'>`（同 [`ReconcileSessionManager`]）。
    async fn reconcile(
        &self,
        channels: &[String],
    ) -> Result<(), crate::sessions::reconcile::ReconcileError> {
        SessionManager::reconcile(self, channels).await;
        Ok(())
    }
}
