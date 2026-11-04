//! `test/integration/browser-invalidation.test.ts` 的 1 個舊測試移植（S3.6）：
//! browser manager 的 page crash → invalidated 到 SessionManager 延後重啟、
//! 不影響其他 session、不重啟 browser 的完整接線。
//!
//! fake 照舊測試逐一對應（`TestPageAdapter`／`TestContextAdapter`／
//! `TestBrowserAdapter`／`TestBrowserLauncher`／
//! `RecordingChannelSessionFactory`）；`vi.waitFor` 以 [`eventually`] 輪詢
//! 對應（單元測試不等真實時間）。
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;

use crate::browser::cdp::context::DriverError;
use crate::browser::cdp::launcher::CancelListener;
use crate::browser::cdp::page::PageShared;
use crate::browser::health::{ChannelHealthReason, ChannelHealthResult};
use crate::browser::manager::{BrowserManager as _, DefaultBrowserManager};
use crate::browser::types::{
    BrowserInvalidation, BrowserInvalidationObserver, BrowserLaunchOptions,
    BrowserManagerBrowserConfig, BrowserManagerConfig, BrowserManagerDependencies,
    BrowserManagerRecoveryConfig,
};

use crate::browser::page_adapter::{
    BrowserConsoleDiagnostic, BrowserHttpResponseDiagnostic, BrowserRequestFailureDiagnostic,
};
use crate::browser::reward_claimer::RewardClaimResult;
use crate::browser::session::{
    ChannelSession, ChannelSessionPointsResult, ChannelSessionRefreshStatus, ChannelSessionState,
};
use crate::browser::types::{
    BrowserAdapter, BrowserContextAdapter, BrowserContextOptions, BrowserLauncher,
    BrowserManagerLogger, BrowserPage, BrowserPageAdapter,
};
use crate::config::model::{
    AppConfig, BrowserConfig, BrowserEngine, BrowserRecoveryConfig, BrowserSessionStartConfig,
    DiscordConfig, ResourceGuardConfig, StreamQuality, TelegramConfig, TwitchApiConfig,
};
use crate::config::thresholds::{
    ResourceGuardAnchorThresholds, compute_effective_resource_guard_thresholds,
};
use crate::logging::logger::{
    JsonLineLogger, JsonLineLoggerOptions, LogClock, LogLevel, LogWriter,
};
use crate::scheduler::watchdog::{
    DefaultSchedulerStreamSelector, DefaultWatchdogScheduler, DefaultWatchdogSchedulerOptions,
    SchedulerLiveStatusProvider, SchedulerReconcileRequest, WatchdogScheduler,
    WatchdogSchedulerConfig,
};
use crate::sessions::manager::{
    DefaultSessionManager, SessionChannelSessionFactory, SessionManager, SessionManagerDependencies,
};
use crate::sessions::reconcile::{
    DefaultReconcileCoordinator, ReconcileCoordinator as _, ReconcileCoordinatorOptions,
};
use crate::sessions::recovery::{ChannelRecoveryPolicy, ChannelRecoveryPolicyOptions};
use crate::twitch::live_status::ChannelLiveStatus;

/// 舊測試常數：受測頻道與健康頻道。
const CHANNEL: &str = "recoverable_channel";
/// 舊測試常數：健康頻道清單（第二段斷言的期望）。
const HEALTHY_CHANNELS: [&str; 2] = ["healthy_b", "healthy_c"];
/// 舊測試常數：`createTestConfig()` 的固定時間（`checkedAt`／scheduler 的 now）。
const CHECKED_AT: &str = "2026-06-14T12:00:00.000Z";
/// `CHECKED_AT` 的 epoch 毫秒（scheduler 與 fake provider 的 `now`）。
const CHECKED_AT_MS: i64 = 1_781_438_400_000;

// ---------------------------------------------------------------------------
// 斷言輔助（同 manager/session 測試的寫法）
// ---------------------------------------------------------------------------

/// 對應舊測試的 `vi.waitFor`：輪詢條件。
async fn eventually(condition: impl Fn() -> bool) -> bool {
    for _ in 0..1_000 {
        if condition() {
            return true;
        }
        tokio::task::yield_now().await;
    }
    false
}

async fn assert_eventually(condition: impl Fn() -> bool, message: &str) {
    assert!(eventually(condition).await, "{message}");
}

// ---------------------------------------------------------------------------
// 測試設定（對應舊 createTestConfig + browser 覆寫 restartOnCrash: false）
// ---------------------------------------------------------------------------

fn test_config() -> AppConfig {
    AppConfig {
        channels: vec![
            CHANNEL.to_string(),
            HEALTHY_CHANNELS[0].to_string(),
            HEALTHY_CHANNELS[1].to_string(),
        ],
        check_interval_seconds: 60.0,
        max_concurrent_streams: 3.0,
        headless: true,
        storage_state_path: "/tmp/test-storage-state.json".to_string(),
        log_level: LogLevel::Debug,
        twitch_api: TwitchApiConfig {
            client_id: "integration-client-id".to_string(),
            access_token: "integration-access-token".to_string(),
            client_secret: String::new(),
        },
        browser: BrowserConfig {
            engine: BrowserEngine::Firefox,
            navigation_timeout_ms: 30_000.0,
            page_health_check_interval_seconds: 60.0,
            reward_check_interval_seconds: 30.0,
            page_refresh_interval_seconds: 300.0,
            // 舊測試覆寫 `browser: { restartOnCrash: false }`。
            restart_on_crash: false,
            stream_quality: StreamQuality::P160,
            enforce_stream_quality_seconds: 120.0,
            viewport_width: 1280.0,
            viewport_height: 720.0,
            mute_audio: true,
            block_images: false,
            block_fonts: false,
            block_known_tracking: false,
            disable_chat: true,
            resource_telemetry_interval_seconds: 300.0,
            recovery: BrowserRecoveryConfig {
                page_crash_backoff_seconds: vec![30.0, 60.0, 120.0],
                channel_crash_window_seconds: 600.0,
                channel_quarantine_threshold: 4.0,
                channel_quarantine_seconds: 900.0,
                stable_reset_seconds: 1_800.0,
                multi_channel_crash_window_seconds: 15.0,
                multi_channel_crash_threshold: 2.0,
                browser_failure_window_seconds: 600.0,
                browser_failure_container_threshold: 3.0,
            },
            session_start: BrowserSessionStartConfig {
                failure_backoff_seconds: vec![30.0, 60.0, 120.0],
                maximum_cooldown_seconds: 900.0,
            },
            resource_guard: {
                // 舊測試的 `resourceGuard` 設定：`enabled: true`，effective 值
                // 以移植的計算函式重現（`N = maxConcurrentStreams`）。
                let anchors = ResourceGuardAnchorThresholds {
                    scale_with_streams: true,
                    baseline_streams: 3.0,
                    base_memory_mib: 512.0,
                    warning_memory_mib: 4_096.0,
                    warning_reset_memory_mib: 3_840.0,
                    browser_recycle_memory_mib: 4_608.0,
                    emergency_memory_mib: 5_376.0,
                    post_recycle_target_memory_mib: 4_096.0,
                };
                ResourceGuardConfig {
                    enabled: true,
                    sample_interval_seconds: 2.0,
                    startup_rate_grace_seconds: 120.0,
                    scale_with_streams: true,
                    baseline_streams: 3.0,
                    base_memory_mib: 512.0,
                    warning_memory_mib: 4_096.0,
                    warning_reset_memory_mib: 3_840.0,
                    browser_recycle_memory_mib: 4_608.0,
                    browser_recycle_consecutive_samples: 2.0,
                    emergency_memory_mib: 5_376.0,
                    emergency_swap_mib: 768.0,
                    fast_growth_mib: 1_024.0,
                    fast_growth_window_seconds: 30.0,
                    post_browser_restart_rate_grace_seconds: 120.0,
                    post_recycle_observation_seconds: 20.0,
                    post_recycle_target_memory_mib: 4_096.0,
                    post_recycle_minimum_drop_mib: 512.0,
                    effective: compute_effective_resource_guard_thresholds(&anchors, 3.0),
                }
            },
        },
        telegram: TelegramConfig {
            enabled: false,
            bot_token: String::new(),
            allowed_chat_ids: vec![],
            polling_timeout_seconds: 25.0,
        },
        discord: DiscordConfig {
            enabled: false,
            bot_token: String::new(),
            application_id: String::new(),
            guild_id: String::new(),
            allowed_channel_ids: vec![],
            allow_direct_messages: false,
            allowed_user_ids: vec![],
        },
    }
}

/// `DefaultBrowserManager` 的設定轉換（同 create.rs 接線的形狀）。
fn browser_manager_config(config: &AppConfig) -> BrowserManagerConfig {
    BrowserManagerConfig {
        headless: config.headless,
        storage_state_path: config.storage_state_path.clone(),
        browser: BrowserManagerBrowserConfig {
            restart_on_crash: config.browser.restart_on_crash,
            viewport_width: config.browser.viewport_width,
            viewport_height: config.browser.viewport_height,
            block_images: config.browser.block_images,
            block_fonts: config.browser.block_fonts,
            block_known_tracking: config.browser.block_known_tracking,
            disable_chat: config.browser.disable_chat,
            recovery: BrowserManagerRecoveryConfig {
                page_crash_backoff_seconds: config
                    .browser
                    .recovery
                    .page_crash_backoff_seconds
                    .clone(),
                channel_crash_window_seconds: config.browser.recovery.channel_crash_window_seconds,
                channel_quarantine_threshold: config.browser.recovery.channel_quarantine_threshold,
                channel_quarantine_seconds: config.browser.recovery.channel_quarantine_seconds,
                stable_reset_seconds: config.browser.recovery.stable_reset_seconds,
                multi_channel_crash_window_seconds: config
                    .browser
                    .recovery
                    .multi_channel_crash_window_seconds,
                multi_channel_crash_threshold: config
                    .browser
                    .recovery
                    .multi_channel_crash_threshold,
                browser_failure_window_seconds: config
                    .browser
                    .recovery
                    .browser_failure_window_seconds,
                browser_failure_container_threshold: config
                    .browser
                    .recovery
                    .browser_failure_container_threshold,
            },
        },
    }
}

// ---------------------------------------------------------------------------
// 日誌（對應舊 createRecordingLogger；斷言不讀記錄行，僅需介面）
// ---------------------------------------------------------------------------

struct BufferWriter;

impl LogWriter for BufferWriter {
    fn write_line(&self, _line: &str) {}
    fn flush(&self) -> std::io::Result<()> {
        Ok(())
    }
}

struct FixedClock;

impl LogClock for FixedClock {
    fn now(&self) -> time::OffsetDateTime {
        let date = time::Date::from_calendar_date(2026, time::Month::June, 14)
            .expect("固定的測試日期應有效");
        let time = time::Time::from_hms_milli(12, 0, 0, 0).expect("固定的測試時間應有效");
        time::PrimitiveDateTime::new(date, time).assume_utc()
    }
}

/// 舊測試的 `recordingLogger`。
fn recording_logger() -> Arc<JsonLineLogger> {
    Arc::new(JsonLineLogger::new(JsonLineLoggerOptions {
        level: Some(LogLevel::Debug),
        output: Some(Box::new(BufferWriter)),
        clock: Some(Box::new(FixedClock)),
    }))
}

// ---------------------------------------------------------------------------
// fake browser adapters
// ---------------------------------------------------------------------------

/// 舊 `TestPageAdapter`：close 計數與 crash listener 登記。
struct TestPageAdapter {
    close_calls: AtomicU32,
    crash_listeners: Mutex<Vec<Box<dyn Fn() + Send + Sync>>>,
}

impl TestPageAdapter {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            close_calls: AtomicU32::new(0),
            crash_listeners: Mutex::new(Vec::new()),
        })
    }

    fn emit_crash(&self) {
        // 舊版 `[...this.crashListeners].forEach(listener)`：快照逐一呼叫
        //（借同把鎖；listener 的呼叫不會登記或移除）。
        let listeners = self
            .crash_listeners
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for listener in listeners.iter() {
            listener();
        }
    }
}

/// 舊 `TestPageAdapter` 的 close 計數讀取（測試斷言用）。
fn page_close_calls(page: &Arc<TestPageAdapter>) -> u32 {
    page.close_calls.load(Ordering::SeqCst)
}

#[async_trait]
impl BrowserPageAdapter for TestPageAdapter {
    fn page(&self) -> Arc<dyn BrowserPage> {
        // 舊版 `page` 屬性（本測試不使用；fake 無實體頁）。
        struct FakePage;
        #[async_trait]
        impl BrowserPage for FakePage {
            fn is_closed(&self) -> bool {
                false
            }

            async fn close(&self) -> Result<(), DriverError> {
                Ok(())
            }

            fn as_page_shared(&self) -> Option<Arc<PageShared>> {
                None
            }
        }
        Arc::new(FakePage)
    }

    async fn close(&self) -> Result<(), DriverError> {
        self.close_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn is_closed(&self) -> bool {
        false
    }

    fn on_crash(&self, listener: Box<dyn Fn() + Send + Sync>) -> CancelListener {
        self.crash_listeners
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(listener);
        Box::new(|| {})
    }

    fn on_close(&self, _listener: Box<dyn Fn() + Send + Sync>) -> CancelListener {
        Box::new(|| {})
    }

    fn on_popup(
        &self,
        _listener: Box<dyn Fn(Arc<dyn BrowserPage>) + Send + Sync>,
    ) -> CancelListener {
        Box::new(|| {})
    }

    fn on_request_failed(
        &self,
        _listener: Box<dyn Fn(&BrowserRequestFailureDiagnostic) + Send + Sync>,
    ) -> CancelListener {
        Box::new(|| {})
    }

    fn on_response(
        &self,
        _listener: Box<dyn Fn(&BrowserHttpResponseDiagnostic) + Send + Sync>,
    ) -> CancelListener {
        Box::new(|| {})
    }

    fn on_console(
        &self,
        _listener: Box<dyn Fn(&BrowserConsoleDiagnostic) + Send + Sync>,
    ) -> CancelListener {
        Box::new(|| {})
    }
}

/// 舊 `TestContextAdapter`：newPage 依序回傳指定的 pages。
struct TestContextAdapter {
    pages: Mutex<Vec<Arc<TestPageAdapter>>>,
    new_page_calls: AtomicU32,
    close_calls: AtomicU32,
}

impl TestContextAdapter {
    fn new(pages: Vec<Arc<TestPageAdapter>>) -> Arc<Self> {
        Arc::new(Self {
            pages: Mutex::new(pages),
            new_page_calls: AtomicU32::new(0),
            close_calls: AtomicU32::new(0),
        })
    }

    fn new_page_calls(&self) -> u32 {
        self.new_page_calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl BrowserContextAdapter for TestContextAdapter {
    async fn configure_resource_blocking(
        &self,
        _options: crate::browser::cdp::context::ResourceBlockingOptions,
    ) -> Result<(), DriverError> {
        Ok(())
    }

    async fn configure_chat_blocking(&self, _enabled: bool) -> Result<(), DriverError> {
        Ok(())
    }

    async fn new_page(&self) -> Result<Arc<dyn BrowserPageAdapter>, DriverError> {
        let index = self.new_page_calls.fetch_add(1, Ordering::SeqCst);
        let page = self
            .pages
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(index as usize)
            .cloned();
        match page {
            Some(page) => Ok(page as Arc<dyn BrowserPageAdapter>),
            None => Err(DriverError::new("Error", "沒有可用的測試 page")),
        }
    }

    async fn close(&self) -> Result<(), DriverError> {
        self.close_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// 舊 `TestBrowserAdapter`。
struct TestBrowserAdapter {
    context: Arc<TestContextAdapter>,
    close_calls: AtomicU32,
    _disconnect_listeners: Mutex<Vec<Box<dyn Fn() + Send + Sync>>>,
}

impl TestBrowserAdapter {
    fn new(context: Arc<TestContextAdapter>) -> Arc<Self> {
        Arc::new(Self {
            context,
            close_calls: AtomicU32::new(0),
            _disconnect_listeners: Mutex::new(Vec::new()),
        })
    }

    fn close_calls(&self) -> u32 {
        self.close_calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl BrowserAdapter for TestBrowserAdapter {
    async fn new_context(
        &self,
        _options: BrowserContextOptions,
    ) -> Result<Arc<dyn BrowserContextAdapter>, DriverError> {
        Ok(Arc::clone(&self.context) as Arc<dyn BrowserContextAdapter>)
    }

    async fn close(&self) -> Result<(), DriverError> {
        self.close_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn on_disconnected(&self, listener: Box<dyn Fn() + Send + Sync>) -> CancelListener {
        self._disconnect_listeners
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(listener);
        Box::new(|| {})
    }

    fn is_connected(&self) -> bool {
        true
    }

    fn get_version(&self) -> String {
        "integration-browser-1.0".to_string()
    }
}

/// 舊 `TestBrowserLauncher`。
struct TestBrowserLauncher {
    browser: Arc<TestBrowserAdapter>,
    launch_calls: AtomicU32,
}

impl TestBrowserLauncher {
    fn new(browser: Arc<TestBrowserAdapter>) -> Arc<Self> {
        Arc::new(Self {
            browser,
            launch_calls: AtomicU32::new(0),
        })
    }

    fn launch_calls(&self) -> u32 {
        self.launch_calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl BrowserLauncher for TestBrowserLauncher {
    async fn launch(
        &self,
        _options: BrowserLaunchOptions,
    ) -> Result<Arc<dyn BrowserAdapter>, DriverError> {
        self.launch_calls.fetch_add(1, Ordering::SeqCst);
        Ok(Arc::clone(&self.browser) as Arc<dyn BrowserAdapter>)
    }
}

// ---------------------------------------------------------------------------
// RecordingChannelSessionFactory（舊 helper 的 Port）
// ---------------------------------------------------------------------------

/// 舊 `SessionLifecycleEvent`。
#[derive(Clone, Debug, PartialEq, Eq)]
enum SessionEvent {
    Created {
        channel: String,
        generation: u32,
    },
    Started {
        channel: String,
        generation: u32,
    },
    Stopped {
        channel: String,
        generation: u32,
        reason: String,
    },
}

/// 舊 `hooks.onStart`：以 channel 呼叫；錯誤以 `Err` 表達（舊版 throw →
/// session 的 start catch）。
type OnStartHook = Arc<
    dyn Fn(&str) -> Pin<Box<dyn Future<Output = Result<(), DriverError>> + Send>> + Send + Sync,
>;
/// 舊 `hooks.onStop`。
type OnStopHook = Arc<
    dyn Fn(&str, &str) -> Pin<Box<dyn Future<Output = Result<(), DriverError>> + Send>>
        + Send
        + Sync,
>;

/// 舊 `RecordingSession`：start／stop 呼叫 hooks。
struct RecordingSession {
    channel: String,
    generation: u32,
    events: Arc<Mutex<Vec<SessionEvent>>>,
    on_start: OnStartHook,
    on_stop: OnStopHook,
    state: Mutex<ChannelSessionState>,
}

#[async_trait]
impl ChannelSession for RecordingSession {
    fn channel(&self) -> &str {
        &self.channel
    }

    fn state(&self) -> ChannelSessionState {
        *self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    async fn start(&self) -> Result<(), DriverError> {
        *self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = ChannelSessionState::Starting;
        (self.on_start)(&self.channel).await?;
        *self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = ChannelSessionState::Watching;
        push_event(
            &self.events,
            SessionEvent::Started {
                channel: self.channel.clone(),
                generation: self.generation,
            },
        );
        Ok(())
    }

    async fn cancel_start(&self, _reason: &str) {
        // 舊 fake：`currentState = 'failed'`。
        *self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = ChannelSessionState::Failed;
    }

    async fn stop(&self, reason: &str) -> Result<(), DriverError> {
        *self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = ChannelSessionState::Stopping;
        (self.on_stop)(&self.channel, reason).await?;
        *self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = ChannelSessionState::Stopped;
        push_event(
            &self.events,
            SessionEvent::Stopped {
                channel: self.channel.clone(),
                generation: self.generation,
                reason: reason.to_string(),
            },
        );
        Ok(())
    }

    async fn check_health(&self) -> ChannelHealthResult {
        // 舊 fake：`{ healthy: true, reason: 'live' }`。
        ChannelHealthResult {
            healthy: true,
            reason: ChannelHealthReason::Live,
            error: None,
        }
    }

    async fn tick_reward_claim(&self) -> RewardClaimResult {
        // 舊 fake：`{ status: 'not_found', channel, checkedAt }`。
        RewardClaimResult::NotFound {
            channel: self.channel.clone(),
            checked_at: CHECKED_AT.to_string(),
        }
    }

    async fn capture_screenshot(&self) -> Result<Vec<u8>, DriverError> {
        Ok(format!("screenshot:{}", self.channel).into_bytes())
    }

    async fn get_channel_points(&self) -> ChannelSessionPointsResult {
        // 舊 fake：`{ status: 'unavailable', reason: 'not_found' }`。
        ChannelSessionPointsResult::NotFound
    }

    fn get_refresh_status(&self) -> ChannelSessionRefreshStatus {
        // 舊 fake：`{ channel, enabled: false }`（餘欄位省略為 `None`）。
        ChannelSessionRefreshStatus {
            channel: self.channel.clone(),
            enabled: false,
            next_refresh_at: None,
            seconds_until_refresh: None,
        }
    }

    async fn refresh_now(&self) -> Result<bool, DriverError> {
        Ok(self.state() == ChannelSessionState::Watching)
    }
}

fn push_event(events: &Arc<Mutex<Vec<SessionEvent>>>, event: SessionEvent) {
    events
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(event);
}

/// 舊 `RecordingChannelSessionFactory`。
struct RecordingSessionFactory {
    events: Arc<Mutex<Vec<SessionEvent>>>,
    sessions: Mutex<Vec<Arc<RecordingSession>>>,
    on_start: OnStartHook,
    on_stop: OnStopHook,
}

impl SessionChannelSessionFactory for RecordingSessionFactory {
    fn create(&self, channel: &str) -> Result<Arc<dyn ChannelSession>, DriverError> {
        let generation = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .filter(|session| session.channel == channel)
            .count() as u32
            + 1;
        let session = Arc::new(RecordingSession {
            channel: channel.to_string(),
            generation,
            events: Arc::clone(&self.events),
            on_start: Arc::clone(&self.on_start),
            on_stop: Arc::clone(&self.on_stop),
            state: Mutex::new(ChannelSessionState::Stopped),
        });
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(Arc::clone(&session));
        push_event(
            &self.events,
            SessionEvent::Created {
                channel: channel.to_string(),
                generation,
            },
        );
        Ok(session as Arc<dyn ChannelSession>)
    }
}

impl RecordingSessionFactory {
    fn sessions_for(&self, channel: &str) -> usize {
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .filter(|session| session.channel == channel)
            .count()
    }

    fn contains_event(&self, event: &SessionEvent) -> bool {
        self.events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(event)
    }
}

// ---------------------------------------------------------------------------
// browser manager 的 invalidation 橋接（舊測試 wiring）
// ---------------------------------------------------------------------------

/// 舊 `onInvalidated`：session manager 尚未 wiring 時以訊息拋錯（同舊）。
struct TestInvalidationBridge {
    session_manager: Arc<OnceLock<Arc<DefaultSessionManager>>>,
}

#[async_trait]
impl BrowserInvalidationObserver for TestInvalidationBridge {
    async fn on_invalidated(&self, invalidation: BrowserInvalidation) -> Result<(), DriverError> {
        let Some(manager) = self.session_manager.get() else {
            // 舊版 `throw new Error('Session Manager 尚未完成測試 wiring')`。
            return Err(DriverError::new(
                "Error",
                "Session Manager 尚未完成測試 wiring",
            ));
        };
        SessionManager::invalidate(
            manager.as_ref(),
            &invalidation.channel,
            invalidation.reason.as_str(),
        )
        .await;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// live status provider（舊測試的 fake：全部回 live）
// ---------------------------------------------------------------------------

/// 舊 fake `LiveStatusProvider`：每個頻道都 live、`checkedAt` 為 fixed clock。
struct AlwaysLiveProvider;

#[async_trait]
impl SchedulerLiveStatusProvider for AlwaysLiveProvider {
    async fn get_live_statuses(
        &self,
        channels: &[String],
    ) -> Result<Vec<ChannelLiveStatus>, crate::scheduler::watchdog::WatchdogLiveStatusError> {
        Ok(channels
            .iter()
            .map(|channel| ChannelLiveStatus {
                channel: channel.clone(),
                is_live: true,
                stream_id: Some(format!("stream-{channel}")),
                title: Some(format!("{channel} live")),
                started_at: Some(CHECKED_AT.to_string()),
                viewer_count: Some(10),
                checked_at: CHECKED_AT.to_string(),
            })
            .collect())
    }
}

// ---------------------------------------------------------------------------
// 測試本體
// ---------------------------------------------------------------------------

// 原測試：單一頻道 page crash 只延後該頻道，不影響其他 session 或重啟 browser
#[tokio::test]
async fn single_channel_page_crash_defers_only_that_channel() {
    let config = test_config();
    let configured_channels = config.channels.clone();
    // 舊測試：nowMs 由 recovery policy 的 `now: () => nowMs` 讀取；30 秒後
    // 重新安排低優先序頻道。
    let now_ms = Arc::new(AtomicU64::new(0));
    let recording_logger = recording_logger();
    let first_page = TestPageAdapter::new();
    let second_page = TestPageAdapter::new();
    let third_page = TestPageAdapter::new();
    let replacement_page = TestPageAdapter::new();
    let context = TestContextAdapter::new(vec![
        Arc::clone(&first_page),
        Arc::clone(&second_page),
        Arc::clone(&third_page),
        Arc::clone(&replacement_page),
    ]);
    let browser = TestBrowserAdapter::new(Arc::clone(&context));
    let launcher = TestBrowserLauncher::new(Arc::clone(&browser));

    let session_manager_cell: Arc<OnceLock<Arc<DefaultSessionManager>>> = Arc::new(OnceLock::new());
    let browser_manager = Arc::new(DefaultBrowserManager::new(
        browser_manager_config(&config),
        BrowserManagerDependencies {
            launcher: Some(Arc::clone(&launcher) as Arc<dyn BrowserLauncher>),
            logger: Some(Arc::clone(&recording_logger) as Arc<dyn BrowserManagerLogger>),
            on_invalidated: Some(Arc::new(TestInvalidationBridge {
                session_manager: Arc::clone(&session_manager_cell),
            }) as Arc<dyn BrowserInvalidationObserver>),
            ..BrowserManagerDependencies::default()
        },
    ));

    // 舊 hooks：session start → browserManager.createPage、stop → closePage。
    let on_start: OnStartHook = {
        let browser_manager = Arc::clone(&browser_manager);
        Arc::new(move |channel: &str| {
            let browser_manager = Arc::clone(&browser_manager);
            let channel = channel.to_string();
            Box::pin(async move {
                browser_manager.create_page(&channel).await?;
                Ok(())
            }) as Pin<Box<dyn Future<Output = Result<(), DriverError>> + Send>>
        })
    };
    let on_stop: OnStopHook = {
        let browser_manager = Arc::clone(&browser_manager);
        Arc::new(move |channel: &str, _reason: &str| {
            let browser_manager = Arc::clone(&browser_manager);
            let channel = channel.to_string();
            Box::pin(async move {
                browser_manager.close_page(&channel).await?;
                Ok(())
            }) as Pin<Box<dyn Future<Output = Result<(), DriverError>> + Send>>
        })
    };

    // 舊測試的 recovery policy（`now: () => nowMs`）與 session manager。
    let recovery_policy = Arc::new(Mutex::new(ChannelRecoveryPolicy::new(
        ChannelRecoveryPolicyOptions {
            recovery: config.browser.recovery.clone(),
            session_start: config.browser.session_start.clone(),
            now: Some(Box::new({
                let now_ms = Arc::clone(&now_ms);
                move || now_ms.load(Ordering::SeqCst)
            })),
        },
    )));
    let session_factory = Arc::new(RecordingSessionFactory {
        events: Arc::new(Mutex::new(Vec::new())),
        sessions: Mutex::new(Vec::new()),
        on_start,
        on_stop,
    });
    let session_manager = Arc::new(DefaultSessionManager::new(
        Arc::clone(&session_factory) as Arc<dyn SessionChannelSessionFactory>,
        SessionManagerDependencies {
            logger: Some(Arc::clone(&recording_logger) as Arc<dyn BrowserManagerLogger>),
            recovery_policy: Some(Arc::clone(&recovery_policy)),
            configured_channels: configured_channels.clone(),
            ..Default::default()
        },
    ));
    assert!(
        session_manager_cell
            .set(Arc::clone(&session_manager))
            .is_ok(),
        "session manager 只設定一次"
    );

    let reconcile_coordinator = Arc::new(DefaultReconcileCoordinator::new(
        ReconcileCoordinatorOptions {
            session_manager: Arc::clone(&session_manager)
                as Arc<dyn crate::sessions::reconcile::ReconcileSessionManager>,
            logger: Arc::clone(&recording_logger)
                as Arc<dyn crate::sessions::reconcile::ReconcileLogger>,
            now: None,
            shutdown_timeout_ms: None,
        },
    ));

    let scheduler = Arc::new(DefaultWatchdogScheduler::new(
        DefaultWatchdogSchedulerOptions {
            config: WatchdogSchedulerConfig {
                channels: configured_channels.clone(),
                check_interval_seconds: config.check_interval_seconds,
                max_concurrent_streams: config.max_concurrent_streams,
            },
            live_status_provider: Arc::new(AlwaysLiveProvider)
                as Arc<dyn SchedulerLiveStatusProvider>,
            stream_selector: Arc::new(DefaultSchedulerStreamSelector)
                as Arc<dyn crate::scheduler::watchdog::SchedulerStreamSelector>,
            session_manager: Some(Arc::clone(&session_manager)
                as Arc<dyn crate::scheduler::watchdog::SchedulerSessionReconcile>),
            reconcile_coordinator: Some(
                Arc::clone(&reconcile_coordinator) as Arc<dyn SchedulerReconcileRequest>
            ),
            logger: Arc::clone(&recording_logger)
                as Arc<dyn crate::scheduler::watchdog::WatchdogSchedulerLogger>,
            timer: None,
            now: Some(Arc::new(move || CHECKED_AT_MS as u64)
                as crate::scheduler::watchdog::DefaultSchedulerClock),
            on_stream_status_changed: None,
        },
    ));

    browser_manager.start().await.expect("browser start 應成功");
    scheduler.run_once().await;
    reconcile_coordinator.wait_for_idle().await;

    assert_eq!(
        session_manager.get_active_channels(),
        configured_channels,
        "啟動後活躍頻道照設定"
    );
    assert_eq!(context.new_page_calls(), 3);
    assert_eq!(session_factory.sessions_for(CHANNEL), 1);

    // crash 第一個 page → 該頻道被延後（重試 30 秒）。
    // 舊版 `emitCrash` 以 listener 內 `tokio::spawn` 起非同步清理；
    // 以後續輪詢推進任務，不等真實時間。
    first_page.emit_crash();

    assert_eventually(
        || {
            session_manager.get_active_channels()
                == vec![
                    HEALTHY_CHANNELS[0].to_string(),
                    HEALTHY_CHANNELS[1].to_string(),
                ]
        },
        "crash 後活躍頻道應只餘健康頻道",
    )
    .await;
    assert_eq!(page_close_calls(&first_page), 1);
    assert!(session_factory.contains_event(&SessionEvent::Stopped {
        channel: CHANNEL.to_string(),
        generation: 1,
        reason: "page_crashed".to_string(),
    }));

    scheduler.run_once().await;
    reconcile_coordinator.wait_for_idle().await;

    assert_eq!(
        session_manager.get_active_channels(),
        vec![
            HEALTHY_CHANNELS[0].to_string(),
            HEALTHY_CHANNELS[1].to_string()
        ],
    );
    assert_eq!(context.new_page_calls(), 3, "不該建新 page");
    assert_eq!(session_factory.sessions_for(CHANNEL), 1);
    assert_eq!(browser.close_calls(), 0, "不該重啟 browser");
    assert_eq!(launcher.launch_calls(), 1);

    // 30 秒後（recovery backoff）重跑由 scheduler 重新建立（generation 2）。
    now_ms.store(30_000, Ordering::SeqCst);
    scheduler.run_once().await;
    reconcile_coordinator.wait_for_idle().await;

    assert_eq!(
        session_manager.get_active_channels(),
        configured_channels,
        "backoff 過後應恢復"
    );
    assert_eq!(context.new_page_calls(), 4);
    assert_eq!(session_factory.sessions_for(CHANNEL), 2);
    assert!(session_factory.contains_event(&SessionEvent::Started {
        channel: CHANNEL.to_string(),
        generation: 2,
    }));

    // 舊測試的收尾情緒（coordinator → sessions → browser）。
    reconcile_coordinator.stop("test_cleanup").await;
    session_manager.stop_all("test_cleanup").await;
    browser_manager.stop().await.expect("browser stop 應成功");
}
