//! `DefaultDiscordBot` 的測試：移植舊版 `test/unit/discord-bot.test.ts`（7 個
//! `it`；標題與 `doc/spec/ts-test-titles.tsv` 逐字相同）。
//!
//! 對應舊測試的 fake：
//!
//! - 事件以 fake shard（[`FakeGatewayUnit`]）的有限 channel 餵入，絕不連真實
//!   Discord。傳給 bot 的 interaction 由 fixture JSON 反序列化成 twilight 的
//!   typed 事件。與舊 fixture 的差異皆出自 twilight 的型別要求：id／guild／
//!   user／command id 用數字字串（舊測試的 `interaction-2` 在此為 `2`），並補
//!   上 twilight 目前必填的 `application_id`／`authorizing_integration_owners`
//!   ／`entitlements`、first option 的 `type` 與 command data 的 `type`
//!   （真實 payload 亦帶這些欄位）。
//! - 舊測試對 identify／heartbeat op 框（`{op:2,…}`／`{op:1,d:null}`）的斷言
//!   屬 twilight 內部；移植測試改以 [`build_shard_config`]
//!   的設定值斷言 identify 內容（token、intents、properties），heartbeat 由
//!   twilight 依 HELLO 的 interval 自行送出。
//! - 舊測試的 `timer` 注入（heartbeat 計時）在 twilight 架構下不存在。
//! - 時間用 `start_paused`；等處理完成的斷言以 fake api／fake shard／
//!   fake logger 的 `Notify` 等待，不等真實時間。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{Notify, mpsc};
use twilight_gateway::Intents;
use twilight_model::application::interaction::Interaction;

use super::{
    DefaultDiscordBot, DefaultDiscordBotOptions, DiscordBot, Gateway, GatewayItem,
    build_shard_config,
};
use crate::config::model::{
    AppConfig, BrowserConfig, BrowserEngine, BrowserRecoveryConfig, BrowserSessionStartConfig,
    DiscordConfig, ResourceGuardConfig, StreamQuality, TelegramConfig, TwitchApiConfig,
};
use crate::config::thresholds::{
    ResourceGuardAnchorThresholds, compute_effective_resource_guard_thresholds,
};
use crate::config::writeback::RuntimeWatchConfig;
use crate::discord::api::{
    DiscordApi, DiscordApiError, DiscordApplicationCommand, DiscordGatewayResponse,
};
use crate::logging::redact::SanitizedValue;
use crate::notifications::bot_command_context::{
    BotCommandContext, BotCommandError, ChannelSessionRefreshStatus, SchedulerChannelStatus,
    SessionChannelPointsResult, SessionChannelPointsUnavailableReason, SessionRefreshResult,
    SessionRefreshStatus, SessionScreenshot, StreamStatusChange, WatchdogSchedulerSnapshot,
};
use crate::notifications::bot_notifications::{
    BrowserRestartMode, BrowserRestartNotification, ContainerRestartNotification,
};

/// 舊測試 `createTestConfig` 的頻道預設。
const TEST_CHANNELS: [&str; 2] = ["first_channel", "second_channel"];
/// 舊測試的允許 Discord channel id。
const ALLOWED_CHANNEL_ID: &str = "111111111111111111";
/// 舊測試 scheduler snapshot 的頻道狀態（first 開台中、second 離線）。
const SNAPSHOT_STATUS: [(&str, bool); 2] = [("first", true), ("second", false)];
/// 舊測試 getRefreshStatuses 的固定 `nextRefreshAt`。
const FIXED_NEXT_REFRESH: &str = "2026-06-27T00:00:00.000Z";

// -----------------------------------------------------------------------------
// 虛假的 Discord API（對應舊測試的 mock `DiscordApi`）
// -----------------------------------------------------------------------------

struct FakeSharedApi {
    notify: Notify,
    get_gateway_bot: Mutex<Vec<DiscordGatewayResponse>>,
    register_commands: Mutex<Vec<RecordedRegister>>,
    send_message: Mutex<Vec<(String, String)>>,
    send_interaction_response: Mutex<Vec<(String, String, String)>>,
    defer_interaction_response: Mutex<Vec<(String, String)>>,
    edit_interaction_response: Mutex<Vec<(String, String, String)>>,
    send_interaction_photo: Mutex<Vec<RecordedPhoto>>,
}

/// `registerCommands` 的錄製（application id、guild id 與 commands 內容）。
struct RecordedRegister {
    application_id: String,
    guild_id: String,
    commands: Vec<DiscordApplicationCommand>,
}

/// `sendInteractionPhoto` 的錄製。
struct RecordedPhoto {
    application_id: String,
    interaction_token: String,
    image: Vec<u8>,
    filename: String,
    content: String,
}

struct FakeDiscordApi {
    shared: Arc<FakeSharedApi>,
}

impl FakeDiscordApi {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            shared: Arc::new(FakeSharedApi {
                notify: Notify::new(),
                get_gateway_bot: Mutex::new(Vec::new()),
                register_commands: Mutex::new(Vec::new()),
                send_message: Mutex::new(Vec::new()),
                send_interaction_response: Mutex::new(Vec::new()),
                defer_interaction_response: Mutex::new(Vec::new()),
                edit_interaction_response: Mutex::new(Vec::new()),
                send_interaction_photo: Mutex::new(Vec::new()),
            }),
        })
    }

    /// 等到條件成立（虛擬時間上限 60 秒，預防條件永不成立時測試掛住）。
    async fn wait_for(&self, predicate: impl Fn(&FakeSharedApi) -> bool) {
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                if predicate(&self.shared) {
                    return;
                }
                self.shared.notify.notified().await;
            }
        })
        .await
        .expect("等待條件應成立");
    }
}

/// 取出 Mutex 內容的長度（等待條件用）。
pub(crate) fn len<T>(lock: &Mutex<Vec<T>>) -> usize {
    lock.lock().expect("lock").len()
}

#[async_trait]
impl DiscordApi for FakeDiscordApi {
    async fn get_gateway_bot(
        &self,
        _timeout: Duration,
    ) -> Result<DiscordGatewayResponse, DiscordApiError> {
        let mut calls = self.shared.get_gateway_bot.lock().expect("lock");
        calls.push(DiscordGatewayResponse {
            url: Some("wss://gateway.discord.gg".to_string()),
        });
        drop(calls);
        self.shared.notify.notify_one();
        // 舊測試 fake：回傳固定 gateway URL。
        Ok(DiscordGatewayResponse {
            url: Some("wss://gateway.discord.gg".to_string()),
        })
    }

    async fn register_commands(
        &self,
        application_id: &str,
        guild_id: &str,
        commands: &[DiscordApplicationCommand],
    ) -> Result<(), DiscordApiError> {
        self.shared
            .register_commands
            .lock()
            .expect("lock")
            .push(RecordedRegister {
                application_id: application_id.to_string(),
                guild_id: guild_id.to_string(),
                commands: commands.to_vec(),
            });
        self.shared.notify.notify_one();
        Ok(())
    }

    async fn send_message(&self, channel_id: &str, content: &str) -> Result<(), DiscordApiError> {
        self.shared
            .send_message
            .lock()
            .expect("lock")
            .push((channel_id.to_string(), content.to_string()));
        self.shared.notify.notify_one();
        Ok(())
    }

    async fn send_interaction_response(
        &self,
        interaction_id: &str,
        interaction_token: &str,
        content: &str,
    ) -> Result<(), DiscordApiError> {
        self.shared
            .send_interaction_response
            .lock()
            .expect("lock")
            .push((
                interaction_id.to_string(),
                interaction_token.to_string(),
                content.to_string(),
            ));
        self.shared.notify.notify_one();
        Ok(())
    }

    async fn defer_interaction_response(
        &self,
        interaction_id: &str,
        interaction_token: &str,
    ) -> Result<(), DiscordApiError> {
        self.shared
            .defer_interaction_response
            .lock()
            .expect("lock")
            .push((interaction_id.to_string(), interaction_token.to_string()));
        self.shared.notify.notify_one();
        Ok(())
    }

    async fn edit_interaction_response(
        &self,
        application_id: &str,
        interaction_token: &str,
        content: &str,
    ) -> Result<(), DiscordApiError> {
        self.shared
            .edit_interaction_response
            .lock()
            .expect("lock")
            .push((
                application_id.to_string(),
                interaction_token.to_string(),
                content.to_string(),
            ));
        self.shared.notify.notify_one();
        Ok(())
    }

    async fn send_interaction_photo(
        &self,
        application_id: &str,
        interaction_token: &str,
        image: &[u8],
        filename: &str,
        content: &str,
    ) -> Result<(), DiscordApiError> {
        self.shared
            .send_interaction_photo
            .lock()
            .expect("lock")
            .push(RecordedPhoto {
                application_id: application_id.to_string(),
                interaction_token: interaction_token.to_string(),
                image: image.to_vec(),
                filename: filename.to_string(),
                content: content.to_string(),
            });
        self.shared.notify.notify_one();
        Ok(())
    }
}

// -----------------------------------------------------------------------------
// 虛假的 gateway（對應舊測試的 MockGatewaySocket）
// -----------------------------------------------------------------------------

/// fake shard 的共享測試端（餵事件、錄 close、等通知）。
struct FakeGatewayHub {
    notify: Notify,
    /// 工廠被呼叫的次數（對應舊測試斷言 gatewayFactory 收到 URL）。
    created_count: Mutex<u32>,
    /// 測試端餵事件的發送端（bounded，對應舊測試事件逐筆送入）。
    feed: mpsc::Sender<GatewayItem>,
    /// 只建立一次的接收端（工廠第一次被呼叫時取走）。
    pending_receiver: Mutex<Option<mpsc::Receiver<GatewayItem>>>,
    /// close（code 1000）的錄製。
    closed: Mutex<Option<(u16, String)>>,
}

impl FakeGatewayHub {
    fn new() -> Arc<Self> {
        let (feed, receiver) = mpsc::channel(32);
        Arc::new(Self {
            notify: Notify::new(),
            created_count: Mutex::new(0),
            feed,
            pending_receiver: Mutex::new(Some(receiver)),
            closed: Mutex::new(None),
        })
    }

    /// 等到 fake shard 被工廠建立（start 的接收迴圈啟動）。
    async fn wait_created(&self) {
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                if *self.created_count.lock().expect("lock") > 0 {
                    return;
                }
                self.notify.notified().await;
            }
        })
        .await
        .expect("fake shard 應被建立");
    }

    /// 餵一則 gateway 事件（對應舊測試 `socket.emit('message', …)`）。
    async fn feed(&self, item: GatewayItem) {
        self.feed
            .send(item)
            .await
            .expect("fake gateway 的 feed channel 應有容量");
    }

    /// 等到 close（code 1000）被錄製（`stop()` 由接收迴圈遞交，時序在此收斂）。
    async fn wait_closed(&self) {
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                if self.closed.lock().expect("lock").is_some() {
                    return;
                }
                self.notify.notified().await;
            }
        })
        .await
        .expect("close 應被錄製");
    }
}

/// fake shard 的實體；由工廠建立（一次）。
struct FakeGatewayUnit {
    feed: mpsc::Receiver<GatewayItem>,
    hub: Arc<FakeGatewayHub>,
}

#[async_trait]
impl Gateway for FakeGatewayUnit {
    async fn next(&mut self) -> Option<GatewayItem> {
        self.feed.recv().await
    }

    fn close(&self, reason: &str) {
        *self.hub.closed.lock().expect("lock") = Some((1000, reason.to_string()));
        self.hub.notify.notify_one();
    }
}

/// 對應舊測試的 `gatewayFactory`（hub 上建置）。
fn gateway_factory(hub: &Arc<FakeGatewayHub>) -> super::GatewayFactory {
    let hub = Arc::clone(hub);
    Arc::new(move || {
        let receiver = hub
            .pending_receiver
            .lock()
            .expect("lock")
            .take()
            .expect("fake gateway 只應被建立一次");
        *hub.created_count.lock().expect("lock") += 1;
        hub.notify.notify_one();
        Box::new(FakeGatewayUnit {
            feed: receiver,
            hub: Arc::clone(&hub),
        }) as Box<dyn Gateway>
    })
}

// -----------------------------------------------------------------------------
// 虛假的 BotCommandContext（對應舊測試的 fake scheduler／sessionManager／
// runtimeConfigManager）
// -----------------------------------------------------------------------------

struct FakeSharedContext {
    notify: Notify,
    /// 舊測試 scheduler.runOnce 的呼叫次數。
    run_once_count: Mutex<u32>,
    /// 舊測試 scheduler.stop（pauseChecks）呼叫次數。
    pause_count: Mutex<u32>,
    /// 舊測試 scheduler.start（resumeChecks）呼叫次數。
    resume_count: Mutex<u32>,
    refresh_pages_calls: Mutex<Vec<Option<String>>>,
    capture_screenshot_calls: Mutex<Vec<String>>,
    get_channel_points_calls: Mutex<Vec<Option<String>>>,
    /// 舊測試 `getChannelPoints` 的 `mockResolvedValueOnce` 尾端覆寫。
    points_override: Mutex<Option<Vec<SessionChannelPointsResult>>>,
    set_channels_calls: Mutex<Vec<Vec<String>>>,
    set_max_concurrent_streams_calls: Mutex<Vec<f64>>,
    /// 設定時 `capture_screenshot` 先等到取得一個 permit（模擬慢速截圖）。
    screenshot_gate: Mutex<Option<Arc<tokio::sync::Semaphore>>>,
}

/// fake 的 `BotCommandContext`（行為照舊測試 harness）。
struct FakeCommandContext {
    shared: Arc<FakeSharedContext>,
}

impl FakeCommandContext {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            shared: Arc::new(FakeSharedContext {
                notify: Notify::new(),
                run_once_count: Mutex::new(0),
                pause_count: Mutex::new(0),
                resume_count: Mutex::new(0),
                refresh_pages_calls: Mutex::new(Vec::new()),
                capture_screenshot_calls: Mutex::new(Vec::new()),
                get_channel_points_calls: Mutex::new(Vec::new()),
                points_override: Mutex::new(None),
                set_channels_calls: Mutex::new(Vec::new()),
                set_max_concurrent_streams_calls: Mutex::new(Vec::new()),
                screenshot_gate: Mutex::new(None),
            }),
        })
    }

    async fn wait_for(&self, predicate: impl Fn(&FakeSharedContext) -> bool) {
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                if predicate(&self.shared) {
                    return;
                }
                self.shared.notify.notified().await;
            }
        })
        .await
        .expect("等待條件應成立");
    }

    /// 舊測試的 scheduler snapshot。
    fn scheduler_snapshot() -> WatchdogSchedulerSnapshot {
        WatchdogSchedulerSnapshot {
            running: true,
            check_in_flight: false,
            retry_at: None,
            last_checked_at: None,
            channels: SNAPSHOT_STATUS
                .iter()
                .map(|(channel, is_live)| SchedulerChannelStatus {
                    channel: (*channel).to_string(),
                    is_live: Some(*is_live),
                })
                .collect(),
        }
    }
}

#[async_trait]
impl BotCommandContext for FakeCommandContext {
    async fn run_check(&self) -> Result<(), BotCommandError> {
        *self.shared.run_once_count.lock().expect("lock") += 1;
        self.shared.notify.notify_one();
        Ok(())
    }

    async fn pause_checks(&self) -> Result<(), BotCommandError> {
        *self.shared.pause_count.lock().expect("lock") += 1;
        self.shared.notify.notify_one();
        Ok(())
    }

    fn resume_checks(&self) {
        *self.shared.resume_count.lock().expect("lock") += 1;
        self.shared.notify.notify_one();
    }

    fn get_scheduler_snapshot(&self) -> WatchdogSchedulerSnapshot {
        Self::scheduler_snapshot()
    }

    fn get_active_channels(&self) -> Vec<String> {
        // 舊測試 fake：固定 ['first', 'second']。
        vec!["first".to_string(), "second".to_string()]
    }

    fn get_refresh_statuses(&self) -> Vec<ChannelSessionRefreshStatus> {
        vec![ChannelSessionRefreshStatus {
            channel: "first".to_string(),
            enabled: true,
            next_refresh_at: Some(FIXED_NEXT_REFRESH.to_string()),
            seconds_until_refresh: Some(4),
        }]
    }

    async fn refresh_pages(
        &self,
        channel: Option<&str>,
    ) -> Result<Vec<SessionRefreshResult>, BotCommandError> {
        self.shared
            .refresh_pages_calls
            .lock()
            .expect("lock")
            .push(channel.map(str::to_string));
        self.shared.notify.notify_one();
        // 舊測試 fake：固定回傳 ['second' 已重整]。
        Ok(vec![SessionRefreshResult {
            channel: "second".to_string(),
            status: SessionRefreshStatus::Refreshed,
            error: None,
        }])
    }

    async fn capture_screenshot(
        &self,
        channel: Option<&str>,
    ) -> Result<Option<SessionScreenshot>, BotCommandError> {
        // 舊測試 fake：`captureScreenshot(channel = 'first')`——undefined 以
        // 'first' 呼叫；回傳 Buffer(`screenshot:<channel>`)。
        let channel = channel.unwrap_or("first").to_string();
        self.shared
            .capture_screenshot_calls
            .lock()
            .expect("lock")
            .push(channel.clone());
        self.shared.notify.notify_one();
        let gate = self.shared.screenshot_gate.lock().expect("lock").clone();
        if let Some(gate) = gate {
            gate.acquire().await.expect("gate open").forget();
        }
        Ok(Some(SessionScreenshot {
            image: format!("screenshot:{channel}").into_bytes(),
            channel,
        }))
    }

    async fn get_channel_points(
        &self,
        channel: Option<&str>,
    ) -> Result<Vec<SessionChannelPointsResult>, BotCommandError> {
        self.shared
            .get_channel_points_calls
            .lock()
            .expect("lock")
            .push(channel.map(str::to_string));
        self.shared.notify.notify_one();

        // 舊測試 fake：`mockResolvedValueOnce` 只覆寫第一次呼叫。
        if let Some(overridden) = self.shared.points_override.lock().expect("lock").take() {
            return Ok(overridden);
        }
        // 舊測試 fake 的預設行為：過濾活躍頻道後各回 available 1_000。
        let active_channels = ["first", "second"];
        let selected: Vec<String> = match channel {
            None => active_channels
                .iter()
                .map(|channel| channel.to_string())
                .collect(),
            Some(channel) => active_channels
                .iter()
                .filter(|active| active.to_lowercase() == channel.to_lowercase())
                .map(|channel| channel.to_string())
                .collect(),
        };
        Ok(selected
            .into_iter()
            .map(|channel| SessionChannelPointsResult::Available {
                channel,
                balance: 1_000.0,
            })
            .collect())
    }

    fn get_config(&self) -> RuntimeWatchConfig {
        // 舊測試 fake：channels ['first','second']、maxConcurrentStreams 2。
        RuntimeWatchConfig {
            channels: vec!["first".to_string(), "second".to_string()],
            max_concurrent_streams: 2.0,
        }
    }

    async fn set_channels(
        &self,
        channels: &[String],
    ) -> Result<RuntimeWatchConfig, crate::config::writeback::ConfigWriteError> {
        self.shared
            .set_channels_calls
            .lock()
            .expect("lock")
            .push(channels.to_vec());
        self.shared.notify.notify_one();
        // 舊測試 fake：回傳 { channels, maxConcurrentStreams: 2 }。
        Ok(RuntimeWatchConfig {
            channels: channels.to_vec(),
            max_concurrent_streams: 2.0,
        })
    }

    async fn set_max_concurrent_streams(
        &self,
        value: f64,
    ) -> Result<RuntimeWatchConfig, crate::config::writeback::ConfigWriteError> {
        self.shared
            .set_max_concurrent_streams_calls
            .lock()
            .expect("lock")
            .push(value);
        self.shared.notify.notify_one();
        // 舊測試 fake：回傳 { channels ['first','second'], maxConcurrentStreams }。
        Ok(RuntimeWatchConfig {
            channels: vec!["first".to_string(), "second".to_string()],
            max_concurrent_streams: value,
        })
    }
}

// -----------------------------------------------------------------------------
// 虛假的 logger（對應舊測試的 mocks.logger）；欄位以序列化字串錄製
// （SanitizedValue 含非 Send 的共用節點）
// -----------------------------------------------------------------------------

struct FakeSharedLogger {
    notify: Notify,
    warn_calls: Mutex<Vec<LogRecord>>,
    info_calls: Mutex<Vec<LogRecord>>,
}

/// 一筆 log（欄位以 `[SanitizedValue::to_json_string]` 呈現，不含保留欄位）。
struct LogRecord {
    event: String,
    fields: Option<String>,
}

/// fake logger。
struct FakeLogger {
    shared: Arc<FakeSharedLogger>,
}

impl FakeLogger {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            shared: Arc::new(FakeSharedLogger {
                notify: Notify::new(),
                warn_calls: Mutex::new(Vec::new()),
                info_calls: Mutex::new(Vec::new()),
            }),
        })
    }

    async fn wait_for(&self, predicate: impl Fn(&FakeSharedLogger) -> bool) {
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                if predicate(&self.shared) {
                    return;
                }
                self.shared.notify.notified().await;
            }
        })
        .await
        .expect("等待條件應成立");
    }
}

fn record_log_call(record: &mut Vec<LogRecord>, event: &str, fields: Option<&SanitizedValue>) {
    record.push(LogRecord {
        event: event.to_string(),
        fields: fields.map(SanitizedValue::to_json_string),
    });
}

impl crate::discord::bot::DiscordBotLogger for FakeLogger {
    fn info(&self, event: &str, fields: Option<&SanitizedValue>) {
        record_log_call(
            &mut self.shared.info_calls.lock().expect("lock"),
            event,
            fields,
        );
        self.shared.notify.notify_one();
    }

    fn warn(&self, event: &str, fields: Option<&SanitizedValue>) {
        record_log_call(
            &mut self.shared.warn_calls.lock().expect("lock"),
            event,
            fields,
        );
        self.shared.notify.notify_one();
    }
}

// -----------------------------------------------------------------------------
// 測試設定（對應舊測試 test/helpers/test-config.ts）
// -----------------------------------------------------------------------------

/// 舊測試 `createDefaultBrowserRecovery`。
fn create_default_browser_recovery() -> BrowserRecoveryConfig {
    BrowserRecoveryConfig {
        page_crash_backoff_seconds: vec![30.0, 60.0, 120.0],
        channel_crash_window_seconds: 600.0,
        channel_quarantine_threshold: 4.0,
        channel_quarantine_seconds: 900.0,
        stable_reset_seconds: 1_800.0,
        multi_channel_crash_window_seconds: 15.0,
        multi_channel_crash_threshold: 2.0,
        browser_failure_window_seconds: 600.0,
        browser_failure_container_threshold: 3.0,
    }
}

/// 舊測試 `createDefaultBrowserSessionStart`。
fn create_default_browser_session_start() -> BrowserSessionStartConfig {
    BrowserSessionStartConfig {
        failure_backoff_seconds: vec![30.0, 60.0, 120.0],
        maximum_cooldown_seconds: 900.0,
    }
}

/// 舊測試 `createDefaultResourceGuard`（effective 依 Rust 版 thresholds 計算）。
fn create_default_resource_guard(max_concurrent_streams: f64) -> ResourceGuardConfig {
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
        effective: compute_effective_resource_guard_thresholds(&anchors, max_concurrent_streams),
    }
}

/// 舊測試 `createTestConfig`（discord 以參數覆寫，其餘照預設）。
fn create_test_config(discord: DiscordConfig) -> AppConfig {
    AppConfig {
        channels: TEST_CHANNELS
            .iter()
            .map(|channel| channel.to_string())
            .collect(),
        check_interval_seconds: 60.0,
        max_concurrent_streams: 2.0,
        headless: true,
        storage_state_path: "/tmp/test-storage-state.json".to_string(),
        log_level: crate::logging::logger::LogLevel::Debug,
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
            restart_on_crash: true,
            stream_quality: StreamQuality::P160,
            enforce_stream_quality_seconds: 120.0,
            viewport_width: 1_280.0,
            viewport_height: 720.0,
            mute_audio: true,
            block_images: false,
            block_fonts: false,
            block_known_tracking: false,
            disable_chat: true,
            resource_telemetry_interval_seconds: 300.0,
            recovery: create_default_browser_recovery(),
            session_start: create_default_browser_session_start(),
            resource_guard: create_default_resource_guard(2.0),
        },
        telegram: TelegramConfig {
            enabled: false,
            bot_token: String::new(),
            allowed_chat_ids: Vec::new(),
            polling_timeout_seconds: 25.0,
        },
        discord,
    }
}

/// 舊測試 harness 的 discord 覆寫選項。
struct HarnessDiscordOptions {
    allow_direct_messages: bool,
    allowed_user_ids: Vec<String>,
}

/// 舊測試 `createHarness` 的 discord 預設。
impl Default for HarnessDiscordOptions {
    fn default() -> Self {
        Self {
            allow_direct_messages: false,
            allowed_user_ids: Vec::new(),
        }
    }
}

/// 測試 harness（對應舊測試 createHarness 的回傳）。
struct Harness {
    bot: DefaultDiscordBot,
    hub: Arc<FakeGatewayHub>,
    api: Arc<FakeDiscordApi>,
    logger: Arc<FakeLogger>,
    context: Arc<FakeCommandContext>,
}

/// 建立測試 harness。
fn create_harness(options: HarnessDiscordOptions) -> Harness {
    let hub = FakeGatewayHub::new();
    let api = FakeDiscordApi::new();
    let logger = FakeLogger::new();
    let context = FakeCommandContext::new();
    let config = create_test_config(DiscordConfig {
        enabled: true,
        bot_token: "discord-token".to_string(),
        application_id: "app-id".to_string(),
        guild_id: "guild-id".to_string(),
        allowed_channel_ids: vec![ALLOWED_CHANNEL_ID.to_string()],
        allow_direct_messages: options.allow_direct_messages,
        allowed_user_ids: options.allowed_user_ids,
    });
    let bot = DefaultDiscordBot::new(DefaultDiscordBotOptions {
        config,
        api: Arc::clone(&api) as Arc<dyn DiscordApi>,
        command_context: Arc::clone(&context) as Arc<dyn BotCommandContext>,
        logger: Arc::clone(&logger) as Arc<dyn crate::discord::bot::DiscordBotLogger>,
        gateway_factory: Some(gateway_factory(&hub)),
    });
    Harness {
        bot,
        hub,
        api,
        logger,
        context,
    }
}

// -----------------------------------------------------------------------------
// interaction fixture（對應舊測試的 interaction() helper；twilight 型別需求
// 見模組文件）
// -----------------------------------------------------------------------------

/// interaction fixture 的可選欄位（舊測試的 options 物件）。
#[derive(Clone, Copy, Default)]
struct InteractionFixture {
    /// 舊測試 options.guildId（undefined 時不帶 guild_id = DM）。
    guild_id: Option<&'static str>,
    /// 舊測試 options.userId。
    user_id: Option<&'static str>,
}

/// 舊測試 interaction() 的預設 options（guildId: 'guild-id'）。
const GUILD_FIXTURE: InteractionFixture = InteractionFixture {
    guild_id: Some("121212121212121212"),
    user_id: None,
};

/// 舊測試 helper 的 user 物件（twilight `User` 必填欄位以最小集補齊）。
fn fixture_user_json(id: &str) -> String {
    format!(r#"{{"id": "{id}", "username": "tester", "discriminator": "0"}}"#)
}

/// 產生一則 interaction 的 `d` JSON（舊測試 interaction()）：twilight 的
/// `Id` 欄位以數字字串表示，required 欄位（application_id、
/// authorizing_integration_owners、entitlements、data.type、option.type）補齊。
fn fixture_interaction_json(
    id: u64,
    name: &str,
    value: Option<&str>,
    channel_id: Option<&str>,
    options: InteractionFixture,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    parts.push(format!(r#""id": "{id}""#));
    parts.push(format!(r#""token": "token-{id}""#));
    if let Some(v) = channel_id {
        parts.push(format!(r#""channel_id": "{v}""#));
    }
    if let Some(v) = options.guild_id {
        parts.push(format!(r#""guild_id": "{v}""#));
    }
    if let Some(v) = options.user_id {
        parts.push(format!(r#""user": {}"#, fixture_user_json(v)));
    }
    parts.push(r#""type": 2"#.to_string());
    parts.push(r#""application_id": "999999999999999999""#.to_string());
    parts.push(r#""authorizing_integration_owners": {}"#.to_string());
    parts.push(r#""entitlements": []"#.to_string());
    let options_json = match value {
        None => "[]".to_string(),
        Some(v) => {
            format!(r#"[{{"name": "channel", "type": 3, "value": "{v}"}}]"#)
        }
    };
    parts.push(format!(
        r#""data": {{"id": "1", "type": 1, "name": "{name}", "options": {options_json}}}"#
    ));
    format!("{{{}}}", parts.join(", "))
}

/// 產生 fake shard 要餵的 interaction 事件（與舊測試 interaction() 同語意）。
fn interaction_item(
    id: u64,
    name: &str,
    value: Option<&str>,
    channel_id: Option<&str>,
    options: InteractionFixture,
) -> GatewayItem {
    let json = fixture_interaction_json(id, name, value, channel_id, options);
    let interaction: Interaction =
        serde_json::from_str(&json).expect("interaction fixture 應可解析");
    GatewayItem::Interaction(Box::new(interaction))
}

impl Harness {
    /// 餵一則 interaction（old socket.emit('message', interaction(...))）。
    async fn feed_interaction(&self, id: u64, name: &str, value: Option<&str>) {
        self.hub
            .feed(interaction_item(
                id,
                name,
                value,
                Some(ALLOWED_CHANNEL_ID),
                GUILD_FIXTURE,
            ))
            .await;
    }
}

// -----------------------------------------------------------------------------
// 移植的測試（標題與 doc/spec/ts-test-titles.tsv 逐字相同）
// -----------------------------------------------------------------------------

/// 舊測試 1 的 identify 斷言屬 twilight 內部（op 框）；移植改以
/// [`build_shard_config`] 的設定值驗證（token、intents、properties）。
#[tokio::test]
async fn shard_config_identifies_with_watchdog_properties_and_empty_intents() {
    let config = build_shard_config("discord-token");
    // twilight 的 builder 會補 "Bot " 前綴（舊版 identify payload 用原始
    // token；兩種 Discord 都接受）。intents 照舊版 `intents: 0`。
    assert_eq!(config.token(), "Bot discord-token");
    assert_eq!(config.intents(), Intents::empty());
    let properties = config
        .identify_properties()
        .expect("identify properties 應已設定");
    assert_eq!(properties.browser, "twitch-watchdog");
    assert_eq!(properties.device, "twitch-watchdog");
    assert_eq!(properties.os, std::env::consts::OS);
}

// 原測試：registers commands, identifies to the gateway, and handles allowed slash commands
#[tokio::test(start_paused = true)]
async fn registers_commands_identifies_to_the_gateway_and_handles_allowed_slash_commands() {
    let harness = create_harness(HarnessDiscordOptions::default());

    harness.bot.start().await.expect("start 應成功");
    // 舊測試：gatewayFactory 收到 gateway URL（此處驗證 gateway 已建立）。
    harness.hub.wait_created().await;
    assert_eq!(
        *harness.hub.created_count.lock().expect("lock"),
        1,
        "fake shard 應建立一次"
    );

    // 舊測試的互動序列（status／check／pause／resume／refresh_now／screenshot）。
    for id in 1..=6 {
        let (name, value): (&str, Option<&str>) = match id {
            1 => ("status", None),
            2 => ("check", None),
            3 => ("pause", None),
            4 => ("resume", None),
            5 => ("refresh_now", Some("second")),
            _ => ("screenshot", Some("second")),
        };
        harness.feed_interaction(id, name, value).await;
    }

    harness
        .api
        .wait_for(|shared| len(&shared.send_interaction_photo) == 1)
        .await;

    // 舊測試：runOnce／stop（pause）／start（resume）各呼叫一次。
    harness
        .context
        .wait_for(|shared| {
            *shared.run_once_count.lock().expect("lock") == 1
                && *shared.pause_count.lock().expect("lock") == 1
                && *shared.resume_count.lock().expect("lock") == 1
        })
        .await;

    {
        // 舊測試對 registerCommands 的參數斷言。
        let registers = harness.api.shared.register_commands.lock().expect("lock");
        let register = registers.last().expect("registerCommands 應已呼叫");
        assert_eq!(register.application_id, "app-id");
        assert_eq!(register.guild_id, "guild-id");
        assert_eq!(register.commands.len(), 15);
        assert!(
            register
                .commands
                .iter()
                .any(|command| command.name == "status")
                && register
                    .commands
                    .iter()
                    .any(|command| command.name == "screenshot"),
            "commands 應包含 status 與 screenshot"
        );
    }

    {
        assert_eq!(
            *harness
                .context
                .shared
                .refresh_pages_calls
                .lock()
                .expect("lock"),
            vec![Some("second".to_string())]
        );
        assert_eq!(
            *harness
                .context
                .shared
                .capture_screenshot_calls
                .lock()
                .expect("lock"),
            vec!["second".to_string()]
        );
    }

    // 舊測試：sendInteractionPhoto 的參數（id/token/image/檔名/內文）。
    {
        let photos = harness
            .api
            .shared
            .send_interaction_photo
            .lock()
            .expect("lock");
        let photo = photos.last().expect("sendInteractionPhoto 應已呼叫");
        assert_eq!(photo.application_id, "app-id");
        assert_eq!(photo.interaction_token, "token-6");
        assert_eq!(photo.image, b"screenshot:second");
        assert_eq!(photo.filename, "second.png");
        assert_eq!(photo.content, "second 目前瀏覽器畫面");
    }
    // 舊測試：回覆包含狀態文字、暫停／恢復回覆，與手動重整結果。
    {
        let replies = harness
            .api
            .shared
            .send_interaction_response
            .lock()
            .expect("lock");
        let contents: Vec<&str> = replies.iter().map(|(.., text)| text.as_str()).collect();
        assert!(contents.iter().any(|text| text.contains("自動檢查")));
        assert!(contents.contains(&"自動檢查已暫停"));
        assert!(contents.contains(&"自動檢查已恢復"));
    }
    {
        let edits = harness
            .api
            .shared
            .edit_interaction_response
            .lock()
            .expect("lock");
        let contents: Vec<&str> = edits.iter().map(|(.., text)| text.as_str()).collect();
        assert!(contents.contains(&"已完成一次狀態檢查"));
        assert!(contents.iter().any(|text| text.contains("手動重整結果")));
    }
    {
        // 舊測試：deferInteractionResponse ('interaction-2','token-2')；twilight 的
        // Id 需要數字字串，fixture id 改用數值。
        let defers = harness
            .api
            .shared
            .defer_interaction_response
            .lock()
            .expect("lock");
        assert!(
            defers.contains(&("2".to_string(), "token-2".to_string())),
            "{defers:?}"
        );
    }

    // 舊測試：stop('test') → socket.closed == { code: 1000, reason: 'test' }。
    harness.bot.stop("test").await;
    harness.hub.wait_closed().await;
    let closed = harness.hub.closed.lock().expect("lock");
    assert_eq!(
        closed
            .as_ref()
            .map(|(code, reason)| (*code, reason.as_str())),
        Some((1000, "test"))
    );
}

// 原測試：rejects interactions from unauthorized channels
#[tokio::test(start_paused = true)]
async fn rejects_interactions_from_unauthorized_channels() {
    let harness = create_harness(HarnessDiscordOptions::default());

    harness.bot.start().await.expect("start 應成功");
    harness
        .hub
        .feed(interaction_item(
            9,
            "pause",
            None,
            Some("999999999999999999"),
            GUILD_FIXTURE,
        ))
        .await;

    harness
        .logger
        .wait_for(|shared| {
            shared
                .warn_calls
                .lock()
                .expect("lock")
                .iter()
                .any(|record| record.event == "discord_unauthorized_channel")
        })
        .await;

    // 舊測試：log 欄位 { channelId: '999999999999999999' }。
    let warns = harness.logger.shared.warn_calls.lock().expect("lock");
    let denial = warns
        .iter()
        .find(|record| record.event == "discord_unauthorized_channel")
        .expect("warn 應已呼叫");
    assert_eq!(
        denial.fields.as_deref(),
        Some(r#"{"channelId":"999999999999999999"}"#)
    );
    assert_eq!(
        *harness.context.shared.pause_count.lock().expect("lock"),
        0,
        "pause 不應被呼叫"
    );

    // 舊測試：回覆 '這個 Discord channel 未被允許使用。'。
    let replies = harness
        .api
        .shared
        .send_interaction_response
        .lock()
        .expect("lock");
    assert!(
        replies.contains(&(
            "9".to_string(),
            "token-9".to_string(),
            "這個 Discord channel 未被允許使用".to_string()
        )),
        "{replies:?}"
    );
}

// 原測試：allows direct message interactions from configured Discord users
#[tokio::test(start_paused = true)]
async fn allows_direct_message_interactions_from_configured_discord_users() {
    let harness = create_harness(HarnessDiscordOptions {
        allow_direct_messages: true,
        allowed_user_ids: vec!["333333333333333333".to_string()],
    });

    harness.bot.start().await.expect("start 應成功");
    harness
        .hub
        .feed(interaction_item(
            10,
            "status",
            None,
            None,
            InteractionFixture {
                guild_id: None,
                user_id: Some("333333333333333333"),
            },
        ))
        .await;

    harness
        .api
        .wait_for(|shared| {
            shared
                .send_interaction_response
                .lock()
                .expect("lock")
                .iter()
                .any(|(.., text)| text.contains("自動檢查"))
        })
        .await;
}

// 原測試：rejects direct message interactions from unauthorized Discord users
#[tokio::test(start_paused = true)]
async fn rejects_direct_message_interactions_from_unauthorized_discord_users() {
    let harness = create_harness(HarnessDiscordOptions {
        allow_direct_messages: true,
        allowed_user_ids: vec!["333333333333333333".to_string()],
    });

    harness.bot.start().await.expect("start 應成功");
    harness
        .hub
        .feed(interaction_item(
            11,
            "pause",
            None,
            None,
            InteractionFixture {
                guild_id: None,
                user_id: Some("444444444444444444"),
            },
        ))
        .await;

    harness
        .logger
        .wait_for(|shared| {
            shared
                .warn_calls
                .lock()
                .expect("lock")
                .iter()
                .any(|record| record.event == "discord_unauthorized_dm_user")
        })
        .await;

    // 舊測試：log 欄位 { userId: '444444444444444444' }。
    let warns = harness.logger.shared.warn_calls.lock().expect("lock");
    let denial = warns
        .iter()
        .find(|record| record.event == "discord_unauthorized_dm_user")
        .expect("warn 應已呼叫");
    assert_eq!(
        denial.fields.as_deref(),
        Some(r#"{"userId":"444444444444444444"}"#)
    );
    assert_eq!(
        *harness.context.shared.pause_count.lock().expect("lock"),
        0,
        "pause 不應被呼叫"
    );

    let replies = harness
        .api
        .shared
        .send_interaction_response
        .lock()
        .expect("lock");
    assert!(
        replies.contains(&(
            "11".to_string(),
            "token-11".to_string(),
            "這個 Discord 使用者未被允許透過私訊控制服務".to_string()
        )),
        "{replies:?}"
    );
}

// 原測試：broadcasts service notifications to allowed channels
#[tokio::test(start_paused = true)]
async fn broadcasts_service_notifications_to_allowed_channels() {
    let harness = create_harness(HarnessDiscordOptions::default());

    harness.bot.start().await.expect("start 應成功");

    harness
        .bot
        .notify_stream_status(&StreamStatusChange {
            channel: "first".to_string(),
            is_live: true,
        })
        .await;
    harness
        .bot
        .notify_reward(
            &crate::browser::reward_claimer::RewardClaimResult::Claimed {
                channel: "first".to_string(),
                claimed_at: "2026-06-27T00:00:00.000Z".to_string(),
            },
        )
        .await;
    harness.bot.notify_page_crash("first").await;
    harness
        .bot
        .notify_browser_restart(&BrowserRestartNotification {
            mode: BrowserRestartMode::Manual,
        })
        .await;
    harness
        .bot
        .notify_container_restart(&ContainerRestartNotification {
            reason: "emergency_memory".to_string(),
            source: "resource_guard".to_string(),
            channel: None,
            detail_reason: None,
        })
        .await;

    harness
        .api
        .wait_for(|shared| shared.send_message.lock().expect("lock").len() >= 6)
        .await;

    // 啟動訊息 + 5 則通知。
    let messages: Vec<String> = harness
        .api
        .shared
        .send_message
        .lock()
        .expect("lock")
        .iter()
        .map(|(channel_id, content)| format!("{channel_id}:{content}"))
        .collect();
    assert!(
        messages
            .iter()
            .any(|m| m == &format!("{ALLOWED_CHANNEL_ID}:first 已開台")),
        "{messages:?}"
    );
    assert!(
        messages
            .iter()
            .any(|m| m == &format!("{ALLOWED_CHANNEL_ID}:first 已領取忠誠點數")),
        "{messages:?}"
    );
    assert!(
        messages
            .iter()
            .any(|m| m == &format!("{ALLOWED_CHANNEL_ID}:💥 first 觀看頁面崩潰")),
        "{messages:?}"
    );
    assert!(
        messages
            .iter()
            .any(|m| m == &format!("{ALLOWED_CHANNEL_ID}:♻️ Firefox 瀏覽器已重啟（完整回收）")),
        "{messages:?}"
    );
    assert!(
        messages.iter().any(|m| m
            == &format!(
                "{ALLOWED_CHANNEL_ID}:🚨 容器即將重啟\n原因：emergency_memory\n來源：resource_guard"
            )),
        "{messages:?}"
    );
}

// 原測試：忠誠點數指令會 defer 並格式化全部頻道結果
#[tokio::test(start_paused = true)]
async fn points_command_defers_and_formats_all_channel_results() {
    let harness = create_harness(HarnessDiscordOptions::default());
    // 舊測試 mockResolvedValueOnce：first available 1,234,567／second
    // unavailable（page_unavailable）。
    *harness.context.shared.points_override.lock().expect("lock") = Some(vec![
        SessionChannelPointsResult::Available {
            channel: "first".to_string(),
            balance: 1_234_567.0,
        },
        SessionChannelPointsResult::Unavailable {
            channel: "second".to_string(),
            reason: SessionChannelPointsUnavailableReason::PageUnavailable,
        },
    ]);

    harness.bot.start().await.expect("start 應成功");
    harness.feed_interaction(12, "points", None).await;

    harness
        .api
        .wait_for(|shared| {
            shared
                .edit_interaction_response
                .lock()
                .expect("lock")
                .iter()
                .any(|(.., text)| text.contains("忠誠點數"))
        })
        .await;

    // 舊測試：registerCommands 包含 points 指令（options 第一個 channel、type 3）。
    {
        let registers = harness.api.shared.register_commands.lock().expect("lock");
        let register = registers.last().expect("registerCommands 應已呼叫");
        let points = register
            .commands
            .iter()
            .find(|command| command.name == "points")
            .expect("points 指令應在清單內");
        let options = points.options.as_ref().expect("points 應有 options");
        assert_eq!(options[0].name, "channel");
        assert_eq!(options[0].kind, 3);
    }

    // 舊測試：deferInteractionResponse ('12','token-12')。
    {
        let defers = harness
            .api
            .shared
            .defer_interaction_response
            .lock()
            .expect("lock");
        assert!(
            defers.contains(&("12".to_string(), "token-12".to_string())),
            "{defers:?}"
        );
    }

    // 舊測試：getChannelPoints 收到 undefined。
    {
        assert_eq!(
            harness
                .context
                .shared
                .get_channel_points_calls
                .lock()
                .expect("lock")
                .last()
                .expect("getChannelPoints 應已呼叫")
                .as_deref(),
            None
        );
    }

    // 舊測試：editReply '忠誠點數：\nfirst：1,234,567 點\nsecond：無法取得'。
    {
        let edits = harness
            .api
            .shared
            .edit_interaction_response
            .lock()
            .expect("lock");
        assert!(
            edits.contains(&(
                "app-id".to_string(),
                "token-12".to_string(),
                "忠誠點數：\nfirst：1,234,567 點\nsecond：無法取得".to_string()
            )),
            "{edits:?}"
        );
    }
}

// 原測試：找不到指定頻道時回覆 active 頻道清單
#[tokio::test(start_paused = true)]
async fn replies_with_the_active_channel_list_when_the_requested_channel_is_missing() {
    let harness = create_harness(HarnessDiscordOptions::default());

    harness.bot.start().await.expect("start 應成功");
    harness
        .feed_interaction(13, "points", Some("missing"))
        .await;

    harness
        .api
        .wait_for(|shared| {
            shared
                .edit_interaction_response
                .lock()
                .expect("lock")
                .iter()
                .any(|(.., text)| text.contains("可用頻道"))
        })
        .await;

    // 舊測試：getChannelPoints('missing')。
    assert_eq!(
        harness
            .context
            .shared
            .get_channel_points_calls
            .lock()
            .expect("lock")
            .last()
            .expect("getChannelPoints 應已呼叫"),
        &Some("missing".to_string())
    );

    // 舊測試：editReply '找不到正在觀看的頻道：missing\n可用頻道：first、second'。
    let edits = harness
        .api
        .shared
        .edit_interaction_response
        .lock()
        .expect("lock");
    assert!(
        edits.contains(&(
            "app-id".to_string(),
            "token-13".to_string(),
            "找不到正在觀看的頻道：missing\n可用頻道：first、second".to_string()
        )),
        "{edits:?}"
    );
}

// X4（允許差異，不需記 decisions）：fake shard 斷線後恢復，Bot 繼續處理指令。
#[tokio::test(start_paused = true)]
async fn continues_to_handle_interactions_after_transport_issue() {
    let harness = create_harness(HarnessDiscordOptions::default());

    harness.bot.start().await.expect("start 應成功");

    // 斷線（接收層錯誤）：twilight 會自行重連；接收迴圈記 warn 後繼續收。
    harness.hub.feed(GatewayItem::TransportIssue).await;
    harness
        .logger
        .wait_for(|shared| {
            shared
                .warn_calls
                .lock()
                .expect("lock")
                .iter()
                .any(|record| record.event == "discord_gateway_error")
        })
        .await;

    // 之後的指令照常處理（斷線恢復後仍在服務）。
    harness.feed_interaction(14, "pause", None).await;
    harness
        .api
        .wait_for(|shared| {
            shared
                .send_interaction_response
                .lock()
                .expect("lock")
                .iter()
                .any(|(.., text)| text == "自動檢查已暫停")
        })
        .await;
    assert_eq!(*harness.context.shared.pause_count.lock().expect("lock"), 1);

    // 第二次斷線後仍能處理（resilience 的持續性）。
    harness.hub.feed(GatewayItem::TransportIssue).await;
    harness
        .logger
        .wait_for(|shared| {
            shared
                .warn_calls
                .lock()
                .expect("lock")
                .iter()
                .filter(|record| record.event == "discord_gateway_error")
                .count()
                >= 2
        })
        .await;
    harness.feed_interaction(15, "resume", None).await;
    harness
        .api
        .wait_for(|shared| {
            shared
                .send_interaction_response
                .lock()
                .expect("lock")
                .iter()
                .any(|(.., text)| text == "自動檢查已恢復")
        })
        .await;
    assert_eq!(
        *harness.context.shared.resume_count.lock().expect("lock"),
        1
    );
}

// 回歸測試：慢速指令執行中，接收迴圈仍立即處理後續指令；慢速指令同時執行數
// 已滿時，新的慢速指令立即收到忙碌回覆；釋放後慢速指令照常完成。
#[tokio::test]
async fn slow_commands_do_not_block_other_interactions_and_overflow_gets_busy_reply() {
    let harness = create_harness(HarnessDiscordOptions::default());
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    *harness.context.shared.screenshot_gate.lock().expect("lock") = Some(Arc::clone(&gate));

    harness.bot.start().await.expect("start 應成功");

    // 佔滿 4 個慢速名額（截圖卡在 gate 上）。
    for id in 0..4u64 {
        harness
            .feed_interaction(100 + id, "screenshot", Some("first"))
            .await;
    }
    harness
        .context
        .wait_for(|shared| len(&shared.capture_screenshot_calls) == 4)
        .await;

    // 後續的一般指令不被卡住。
    harness.feed_interaction(200, "pause", None).await;
    harness
        .api
        .wait_for(|shared| {
            shared
                .send_interaction_response
                .lock()
                .expect("lock")
                .iter()
                .any(|(.., text)| text == "自動檢查已暫停")
        })
        .await;

    // 第 5 個慢速指令：立即回覆忙碌，不呼叫截圖。
    harness
        .feed_interaction(300, "screenshot", Some("first"))
        .await;
    harness
        .api
        .wait_for(|shared| {
            shared
                .send_interaction_response
                .lock()
                .expect("lock")
                .iter()
                .any(|(id, _, text)| id == "300" && text == "目前處理中的指令過多，請稍後再試")
        })
        .await;
    assert_eq!(len(&harness.context.shared.capture_screenshot_calls), 4);

    // 放行：卡住的 4 個截圖照常送出。
    gate.add_permits(4);
    harness
        .api
        .wait_for(|shared| len(&shared.send_interaction_photo) == 4)
        .await;
}
