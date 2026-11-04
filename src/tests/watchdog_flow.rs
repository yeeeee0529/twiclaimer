//! `test/integration/watchdog-flow.test.ts` 的 1 個舊測試移植（S3.6）：
//! `TwitchApiClient`（mock fetch）→ `DefaultWatchdogScheduler` →
//! `DefaultReconcileCoordinator` → `DefaultSessionManager`（Recording factory）
//! 的 Helix 到 session 協調接線。
//!
//! fake 照舊測試逐一對應（`RecordingChannelSessionFactory`／記錄行 logger／
//! mock transport 的 `mockResolvedValueOnce` 步驟序列）；`runOnce` 後以
//! `wait_for_idle` 收斂（單元測試不等真實時間）。
use async_trait::async_trait;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time};

use crate::browser::health::{ChannelHealthReason, ChannelHealthResult};
use crate::browser::reward_claimer::RewardClaimResult;
use crate::browser::session::{
    ChannelSession, ChannelSessionPointsResult, ChannelSessionRefreshStatus, ChannelSessionState,
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
    DefaultSchedulerClock, DefaultSchedulerStreamSelector, DefaultWatchdogScheduler,
    DefaultWatchdogSchedulerOptions, SchedulerLiveStatusProvider, SchedulerReconcileRequest,
    SchedulerSessionReconcile, SchedulerStreamSelector, WatchdogScheduler, WatchdogSchedulerConfig,
    WatchdogSchedulerLogger,
};
use crate::sessions::manager::{
    DefaultSessionManager, SessionChannelSessionFactory, SessionManager, SessionManagerDependencies,
};
use crate::sessions::reconcile::{
    DefaultReconcileCoordinator, ReconcileCoordinator, ReconcileCoordinatorOptions,
    ReconcileLogger, ReconcileSessionManager,
};
use crate::twitch::api::{
    TwitchApiClient, TwitchApiClientOptions, TwitchApiLogger, TwitchHttpBody, TwitchHttpHeaders,
    TwitchHttpRequest, TwitchHttpResponse, TwitchHttpTransport, TwitchTransportError,
};

/// 舊測試常數：`ACCESS_TOKEN`（應不被記錄）。
const ACCESS_TOKEN: &str = "integration-token-must-remain-secret";
/// 舊測試常數：高／中／低優先序頻道（設定的順序）。
const HIGH: &str = "high_priority";
const MEDIUM: &str = "medium_priority";
const LOW: &str = "low_priority";
/// 舊測試常數：`stream.started_at`。
const STREAM_STARTED_AT: &str = "2026-06-14T11:00:00.000Z";

// ---------------------------------------------------------------------------
// 日誌（對應舊 createRecordingLogger；本測試有讀記錄行）
// ---------------------------------------------------------------------------

struct BufferWriter {
    lines: Arc<Mutex<Vec<String>>>,
}

impl LogWriter for BufferWriter {
    fn write_line(&self, line: &str) {
        self.lines
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(line.to_string());
    }

    fn flush(&self) -> std::io::Result<()> {
        Ok(())
    }
}

/// 舊 `createFixedClock()` 的固定時間（`DEFAULT_TEST_TIME`）。
fn fixed_now() -> OffsetDateTime {
    let date = Date::from_calendar_date(2026, Month::June, 14).expect("固定的測試日期應有效");
    let time = Time::from_hms_milli(12, 0, 0, 0).expect("固定的測試時間應有效");
    PrimitiveDateTime::new(date, time).assume_utc()
}

struct FixedClock;

impl LogClock for FixedClock {
    fn now(&self) -> OffsetDateTime {
        fixed_now()
    }
}

/// 舊 `RecordingLogger`：logger 本體（含 clock）＋ `records()`／`serialized()`。
struct RecordingLogger {
    logger: Arc<JsonLineLogger>,
    lines: Arc<Mutex<Vec<String>>>,
}

impl RecordingLogger {
    fn new() -> Self {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let logger = Arc::new(JsonLineLogger::new(JsonLineLoggerOptions {
            level: Some(LogLevel::Debug),
            output: Some(Box::new(BufferWriter {
                lines: Arc::clone(&lines),
            })),
            clock: Some(Box::new(FixedClock)),
        }));
        Self { logger, lines }
    }

    /// 舊 `records()`：每行解析的 JSON 物件。
    fn records(&self) -> Vec<serde_json::Value> {
        self.lines
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .map(|line| serde_json::from_str(line).unwrap_or(serde_json::Value::Null))
            .collect()
    }

    /// 舊 `serialized()`：全部行的串接。
    fn serialized(&self) -> String {
        self.lines
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .join("")
    }
}

// ---------------------------------------------------------------------------
// 測試設定（對應舊 createTestConfig 的覆寫參數）
// ---------------------------------------------------------------------------

fn test_config() -> AppConfig {
    // 舊 `createTestConfig({...})` 的覆寫：channels、maxConcurrentStreams 2、
    // twitchApi.accessToken（resourceGuard 走 helper 的預設錨點）。
    let max_concurrent_streams = 2.0;
    AppConfig {
        channels: vec![HIGH.to_string(), MEDIUM.to_string(), LOW.to_string()],
        check_interval_seconds: 60.0,
        max_concurrent_streams,
        headless: true,
        storage_state_path: "/tmp/test-storage-state.json".to_string(),
        log_level: LogLevel::Debug,
        twitch_api: TwitchApiConfig {
            client_id: "integration-client-id".to_string(),
            access_token: ACCESS_TOKEN.to_string(),
            client_secret: String::new(),
        },
        browser: BrowserConfig {
            engine: BrowserEngine::Firefox,
            navigation_timeout_ms: 30_000.0,
            page_health_check_interval_seconds: 60.0,
            reward_check_interval_seconds: 30.0,
            page_refresh_interval_seconds: 300.0,
            restart_on_crash: true,
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
                    effective: compute_effective_resource_guard_thresholds(
                        &anchors,
                        max_concurrent_streams,
                    ),
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

// ---------------------------------------------------------------------------
// RecordingChannelSessionFactory（舊 helper 的 Port；本測試無 hooks）
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

/// 舊 `RecordingChannelSession`（hooks 未提供 → 立即完成）。
struct RecordingSession {
    channel: String,
    generation: u32,
    events: Arc<Mutex<Vec<SessionEvent>>>,
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

    async fn start(&self) -> Result<(), crate::browser::cdp::context::DriverError> {
        *self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = ChannelSessionState::Starting;
        *self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = ChannelSessionState::Watching;
        self.push(SessionEvent::Started {
            channel: self.channel.clone(),
            generation: self.generation,
        });
        Ok(())
    }

    async fn cancel_start(&self, _reason: &str) {
        // 舊 fake：`currentState = 'failed'`。
        *self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = ChannelSessionState::Failed;
    }

    async fn stop(&self, reason: &str) -> Result<(), crate::browser::cdp::context::DriverError> {
        *self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = ChannelSessionState::Stopping;
        *self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = ChannelSessionState::Stopped;
        self.push(SessionEvent::Stopped {
            channel: self.channel.clone(),
            generation: self.generation,
            reason: reason.to_string(),
        });
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
        // 舊 fake：checkedAt 為固定時間。
        RewardClaimResult::NotFound {
            channel: self.channel.clone(),
            checked_at: "2026-06-14T12:00:00.000Z".to_string(),
        }
    }

    async fn capture_screenshot(
        &self,
    ) -> Result<Vec<u8>, crate::browser::cdp::context::DriverError> {
        Ok(format!("screenshot:{}", self.channel).into_bytes())
    }

    async fn get_channel_points(&self) -> ChannelSessionPointsResult {
        // 舊 fake：`{ status: 'unavailable', reason: 'not_found' }`。
        ChannelSessionPointsResult::NotFound
    }

    fn get_refresh_status(&self) -> ChannelSessionRefreshStatus {
        // 舊 fake：`{ channel, enabled: false }`。
        ChannelSessionRefreshStatus {
            channel: self.channel.clone(),
            enabled: false,
            next_refresh_at: None,
            seconds_until_refresh: None,
        }
    }

    async fn refresh_now(&self) -> Result<bool, crate::browser::cdp::context::DriverError> {
        Ok(self.state() == ChannelSessionState::Watching)
    }
}

impl RecordingSession {
    fn push(&self, event: SessionEvent) {
        self.events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(event);
    }
}

/// 舊 `RecordingChannelSessionFactory`（無 hooks 建構）。
struct RecordingSessionFactory {
    events: Arc<Mutex<Vec<SessionEvent>>>,
    sessions: Mutex<Vec<Arc<RecordingSession>>>,
}

impl SessionChannelSessionFactory for RecordingSessionFactory {
    fn create(
        &self,
        channel: &str,
    ) -> Result<Arc<dyn ChannelSession>, crate::browser::cdp::context::DriverError> {
        let generation = {
            let sessions = self
                .sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            sessions
                .iter()
                .filter(|session| session.channel == channel)
                .count() as u32
                + 1
        };
        let session = Arc::new(RecordingSession {
            channel: channel.to_string(),
            generation,
            events: Arc::clone(&self.events),
            state: Mutex::new(ChannelSessionState::Stopped),
        });
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(Arc::clone(&session));
        self.events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(SessionEvent::Created {
                channel: channel.to_string(),
                generation,
            });
        Ok(session as Arc<dyn ChannelSession>)
    }
}

impl RecordingSessionFactory {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            events: Arc::new(Mutex::new(Vec::new())),
            sessions: Mutex::new(Vec::new()),
        })
    }

    fn events(&self) -> Vec<SessionEvent> {
        self.events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

// ---------------------------------------------------------------------------
// mock transport（舊測試的 fetchMock：mockResolvedValueOnce 序列）
// ---------------------------------------------------------------------------

/// 一次 `mockResolvedValueOnce` 的回應。
struct MockStep {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

/// 舊 `helixResponse(data)`。
fn helix_response(data: Vec<serde_json::Value>) -> MockStep {
    MockStep {
        status: 200,
        headers: vec![("Content-Type".to_string(), "application/json".to_string())],
        body: serde_json::to_vec(&serde_json::json!({ "data": data })).expect("JSON 序列化應成功"),
    }
}

/// 舊 `helixStream(channel, streamId)`。
fn helix_stream(channel: &str, stream_id: &str) -> serde_json::Value {
    serde_json::json!({
        "id": stream_id,
        "user_login": channel,
        "title": format!("{channel} live"),
        "started_at": STREAM_STARTED_AT,
        "viewer_count": 10,
    })
}

/// 舊 `new Response(null, { status: 503 })`。
fn status_response(status: u16) -> MockStep {
    MockStep {
        status,
        headers: vec![],
        body: Vec::new(),
    }
}

/// mock transport：逐步取用（步驟用完即 panic，對應舊測試的意外呼叫）。
struct MockTransport {
    steps: Mutex<VecDeque<MockStep>>,
    calls: Mutex<Vec<TwitchHttpRequest>>,
}

impl MockTransport {
    fn sequence(steps: Vec<MockStep>) -> Arc<Self> {
        Arc::new(Self {
            steps: Mutex::new(steps.into()),
            calls: Mutex::new(Vec::new()),
        })
    }

    /// 舊 `fetchMock.mock.calls` 的長度。
    fn call_count(&self) -> usize {
        self.calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }

    /// 第 `index` 個請求的 URL（[`TwitchHttpRequest`] 不 Clone，鎖內取出）。
    fn call_url(&self, index: usize) -> Option<String> {
        self.calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(index)
            .map(|request| request.url.clone())
    }
}

#[async_trait]
impl TwitchHttpTransport for MockTransport {
    async fn fetch(
        &self,
        request: TwitchHttpRequest,
    ) -> Result<TwitchHttpResponse, TwitchTransportError> {
        self.calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(request);
        let step = self
            .steps
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .pop_front();
        let Some(step) = step else {
            panic!("unexpected extra fetch call");
        };
        Ok(TwitchHttpResponse {
            status: step.status,
            headers: TwitchHttpHeaders::new(step.headers),
            body: Box::new(OneshotBody {
                chunks: VecDeque::from(vec![step.body]),
            }),
        })
    }
}

/// 一次送完的 body。
struct OneshotBody {
    chunks: VecDeque<Vec<u8>>,
}

#[async_trait]
impl TwitchHttpBody for OneshotBody {
    async fn next_chunk(&mut self) -> Option<Result<Vec<u8>, TwitchTransportError>> {
        self.chunks.pop_front().map(Ok)
    }
}

// ---------------------------------------------------------------------------
// 測試本體
// ---------------------------------------------------------------------------

// 原測試：建立部分 live sessions、替換低優先序頻道，暫時 API 失敗時保留
#[tokio::test]
async fn creates_partial_live_sessions_and_replaces_low_priority_channel() {
    let config = test_config();
    let recording_logger = RecordingLogger::new();
    let transport = MockTransport::sequence(vec![
        // 第 1 個 tick：low／medium live。
        helix_response(vec![
            helix_stream(LOW, "stream-low"),
            helix_stream(MEDIUM, "stream-medium"),
        ]),
        // 第 2 個 tick：三個都 live。
        helix_response(vec![
            helix_stream(LOW, "stream-low"),
            helix_stream(HIGH, "stream-high"),
            helix_stream(MEDIUM, "stream-medium"),
        ]),
        // 第 3 個 tick：503。
        status_response(503),
    ]);
    let twitch_api_client = Arc::new(
        TwitchApiClient::new(TwitchApiClientOptions {
            client_id: config.twitch_api.client_id.clone(),
            access_token: Some(config.twitch_api.access_token.clone()),
            client_secret: Some(config.twitch_api.client_secret.clone()),
            check_interval_seconds: config.check_interval_seconds,
            logger: Arc::clone(&recording_logger.logger) as Arc<dyn TwitchApiLogger>,
            transport: Some(Arc::clone(&transport) as Arc<dyn TwitchHttpTransport>),
            base_url: None,
            oauth_base_url: None,
            request_timeout_ms: None,
            token_validation_interval_ms: None,
            token_refresh_threshold_seconds: None,
            // 舊測試的 `now: clock.date`。
            now: Some(Box::new(fixed_now)),
        })
        .expect("TwitchApiClient 建構應成功"),
    );

    let session_factory = RecordingSessionFactory::new();
    let session_manager = Arc::new(DefaultSessionManager::new(
        Arc::clone(&session_factory) as Arc<dyn SessionChannelSessionFactory>,
        SessionManagerDependencies {
            logger: Some(Arc::clone(&recording_logger.logger)
                as Arc<dyn crate::browser::types::BrowserManagerLogger>),
            ..Default::default()
        },
    ));
    let reconcile_coordinator = Arc::new(DefaultReconcileCoordinator::new(
        ReconcileCoordinatorOptions {
            session_manager: Arc::clone(&session_manager) as Arc<dyn ReconcileSessionManager>,
            logger: Arc::clone(&recording_logger.logger) as Arc<dyn ReconcileLogger>,
            now: None,
            shutdown_timeout_ms: None,
        },
    ));
    let scheduler = Arc::new(DefaultWatchdogScheduler::new(
        DefaultWatchdogSchedulerOptions {
            config: WatchdogSchedulerConfig {
                channels: config.channels.clone(),
                check_interval_seconds: config.check_interval_seconds,
                max_concurrent_streams: config.max_concurrent_streams,
            },
            live_status_provider: Arc::clone(&twitch_api_client)
                as Arc<dyn SchedulerLiveStatusProvider>,
            stream_selector: Arc::new(DefaultSchedulerStreamSelector)
                as Arc<dyn SchedulerStreamSelector>,
            session_manager: Some(
                Arc::clone(&session_manager) as Arc<dyn SchedulerSessionReconcile>
            ),
            reconcile_coordinator: Some(
                Arc::clone(&reconcile_coordinator) as Arc<dyn SchedulerReconcileRequest>
            ),
            logger: Arc::clone(&recording_logger.logger) as Arc<dyn WatchdogSchedulerLogger>,
            timer: None,
            // 舊測試的 `now: clock.date`。
            now: Some(Arc::new(move || 1_781_438_400_000u64) as DefaultSchedulerClock),
            on_stream_status_changed: None,
        },
    ));

    // 第 1 個 tick：medium／low 被選上（high 未 live）。
    scheduler.run_once().await;
    reconcile_coordinator.wait_for_idle().await;

    assert_eq!(
        session_manager.get_active_channels(),
        vec![MEDIUM.to_string(), LOW.to_string()],
    );
    assert_eq!(
        session_factory.events(),
        vec![
            SessionEvent::Created {
                channel: MEDIUM.to_string(),
                generation: 1,
            },
            SessionEvent::Started {
                channel: MEDIUM.to_string(),
                generation: 1,
            },
            SessionEvent::Created {
                channel: LOW.to_string(),
                generation: 1,
            },
            SessionEvent::Started {
                channel: LOW.to_string(),
                generation: 1,
            },
        ],
    );

    // 第 2 個 tick：三個都 live → high／medium 上場、low 以 inactive 停止。
    scheduler.run_once().await;
    reconcile_coordinator.wait_for_idle().await;

    assert_eq!(
        session_manager.get_active_channels(),
        vec![HIGH.to_string(), MEDIUM.to_string()],
    );
    let events_after_second_tick = session_factory.events();
    assert_eq!(
        events_after_second_tick[4..].to_vec(),
        vec![
            SessionEvent::Stopped {
                channel: LOW.to_string(),
                generation: 1,
                reason: "inactive".to_string(),
            },
            SessionEvent::Created {
                channel: HIGH.to_string(),
                generation: 1,
            },
            SessionEvent::Started {
                channel: HIGH.to_string(),
                generation: 1,
            },
        ],
    );
    let events_before = events_after_second_tick;

    // 第 3 個 tick：503 → 保留現有 session、記 twitch_api_temporary_error。
    scheduler.run_once().await;
    reconcile_coordinator.wait_for_idle().await;

    assert_eq!(
        session_manager.get_active_channels(),
        vec![HIGH.to_string(), MEDIUM.to_string()],
    );
    assert_eq!(session_factory.events(), events_before);
    assert_eq!(transport.call_count(), 3);
    let temporary_error = recording_logger
        .records()
        .into_iter()
        .find(|record| {
            record.get("event") == Some(&serde_json::json!("twitch_api_temporary_error"))
        })
        .expect("需記錄 twitch_api_temporary_error");
    assert_eq!(
        temporary_error.get("reason"),
        Some(&serde_json::json!("server")),
    );
    assert_eq!(
        temporary_error.get("statusCode"),
        Some(&serde_json::json!(503)),
    );
    assert!(
        !recording_logger.serialized().contains(ACCESS_TOKEN),
        "access token 不得進日誌",
    );

    // 第 1 個請求的 user_login 照設定順序。
    let first_call_url = transport.call_url(0).expect("第 1 個請求應已記錄");
    let query = first_call_url.split('?').nth(1).unwrap_or("");
    let logins: Vec<&str> = query
        .split('&')
        .filter_map(|pair| pair.strip_prefix("user_login="))
        .collect();
    assert_eq!(
        logins,
        vec![HIGH, MEDIUM, LOW],
        "user_login 參數照設定的優先序：{first_call_url}",
    );
}
