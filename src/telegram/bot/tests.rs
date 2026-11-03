//! `DefaultTelegramBot` 的測試：移植舊版 `test/unit/telegram-bot.test.ts`
//! （17 個 `it`／`it.each` 標題；標題與 `doc/spec/ts-test-titles.tsv` 逐字相同）。
//!
//! 對應舊測試的 fake：
//!
//! - api（[`FakeTelegramApi`]）對應舊測試 mock 的 `TelegramApi`：`getUpdates`
//!   依腳本回覆，排空後走 fallback（長輪詢 hang 到 shutdown 回 `[]`；測試 3 的
//!   gated fallback 由測試端 `deliver`）；`sendMessage` 支援一次性的
//!   resolve／reject 腳本（測試 6／9）；共用的 `Notify` 供 `wait_for` 等待，
//!   絕不連真實 Telegram。
//! - [`FakeCommandContext`] 對應舊測試 fake 的 scheduler／sessionManager／
//!   runtimeConfigManager（經 `BotCommandContext` 的行為照舊 harness）；
//!   `getChannelPoints`／`captureScreenshot` 支援順序覆寫（`mockResolvedValueOnce`
//!   ／`mockRejectedValueOnce`）與測試 2／3 的 `pending` 停留（[`StallPlan`])。
//! - 時間用 `start_paused`：測試 3 的 30 秒查詢逾時計時以
//!   [`QUERY_TIMEOUT_MS`] 在虛擬時間結算；等處理完成的斷言以 `Notify` 等待，
//!   舊測試 `flushQueries` 以名額計數（[`DefaultTelegramBot::all_queries_settled`]）
//!   等價確認。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{Notify, watch};
use tokio_util::sync::CancellationToken;

use super::{
    DefaultTelegramBot, DefaultTelegramBotOptions, QUERY_TIMEOUT_MS, TelegramBot,
    TelegramBotLogger, TelegramSleep,
};
use crate::config::model::{
    AppConfig, BrowserConfig, BrowserEngine, BrowserRecoveryConfig, BrowserSessionStartConfig,
    DiscordConfig, ResourceGuardConfig, StreamQuality, TelegramConfig, TwitchApiConfig,
};
use crate::config::thresholds::{
    ResourceGuardAnchorThresholds, compute_effective_resource_guard_thresholds,
};
use crate::config::writeback::RuntimeWatchConfig;
use crate::logging::redact::SanitizedValue;
use crate::notifications::bot_command_context::{
    BotCommandContext, BotCommandError, ChannelSessionRefreshStatus, SchedulerChannelStatus,
    SessionChannelPointsResult, SessionChannelPointsUnavailableReason, SessionRefreshResult,
    SessionRefreshStatus, SessionScreenshot, StreamStatusChange, WatchdogSchedulerSnapshot,
};
use crate::notifications::bot_notifications::{
    BrowserRestartMode, BrowserRestartNotification, ContainerRestartNotification,
};
use crate::telegram::api::{
    TelegramApi, TelegramApiError, TelegramBotCommand, TelegramChat, TelegramChatId,
    TelegramErrorCategory, TelegramMessage, TelegramReplyKeyboardMarkup,
    TelegramSendMessageOptions, TelegramUpdate,
};

/// 舊測試 `createTestConfig` 的頻道預設。
const TEST_CHANNELS: [&str; 2] = ["first_channel", "second_channel"];
/// 舊測試的允許 Telegram chat id。
const ALLOWED_CHAT_ID: &str = "42";
/// 舊測試 harness 的 scheduler snapshot 頻道狀態（first 開台中、second 離線）。
const SNAPSHOT_STATUS: [(&str, bool); 2] = [("first", true), ("second", false)];
/// 舊測試 harness 的 `lastCheckedAt`。
const FIXED_LAST_CHECKED_AT: &str = "2026-06-14T00:00:00.000Z";

// -----------------------------------------------------------------------------
// 虛假的 Telegram API（對應舊測試 harness 的 mock `TelegramApi`）
// -----------------------------------------------------------------------------

/// 一筆 `sendMessage` 的錄製（chat id、內文、reply_markup）。
struct RecordedSend {
    chat_id: String,
    text: String,
    reply_markup: Option<TelegramReplyKeyboardMarkup>,
}

/// 一筆 `sendPhoto` 的錄製。
struct RecordedPhoto {
    chat_id: String,
    image: Vec<u8>,
    filename: String,
    caption: String,
}

/// 舊測試 `getUpdates` mock 的腳本項目（依序消耗；排空後走 fallback）。
enum GetUpdatesStep {
    /// `mockResolvedValueOnce(updates)`。
    Served(Vec<TelegramUpdate>),
    /// `mockRejectedValueOnce(error)`。
    Fail(TelegramApiError),
}

/// 舊測試 `getUpdates` mock 的 fallback 行為（腳本排空後）。
enum GetUpdatesFallback {
    /// 舊 harness 預設：hang 直到 shutdown（`addEventListener('abort', resolve)`），
    /// 回 `[]`。
    HangUntilAbort,
    /// 舊測試 3 的 `mockImplementation`：等待測試端 `deliver` 的長輪詢。
    Gated,
}

/// 測試端 `deliver` 的閘門（對應舊 mock 的 `deliver = resolve`）。
///
/// 版次以 watch 呈現、值以同步 cell 呈現：`deliver` 塞值並送版次變更；等待端
/// 先查 cell、無則等版次變更後再查（無 missed-wake）。
struct GatedUpdates {
    cell: Mutex<Option<Vec<TelegramUpdate>>>,
    version: watch::Sender<u64>,
}

impl GatedUpdates {
    fn new() -> Arc<Self> {
        let (version, _) = watch::channel(0);
        Arc::new(Self {
            cell: Mutex::new(None),
            version,
        })
    }

    /// 舊測試 `deliver?.(updates)`。
    fn deliver(&self, updates: Vec<TelegramUpdate>) {
        *self.cell.lock().expect("lock") = Some(updates);
        let current = *self.version.borrow();
        let _ = self.version.send_replace(current + 1);
    }

    /// 舊 mock 的 gated `getUpdates`：等 cell 有值就取走（一次一則）；
    /// shutdown 中止時照舊 mock 回 `[]`。
    async fn take(&self, cancel: &CancellationToken) -> Vec<TelegramUpdate> {
        let mut receiver = self.version.subscribe();
        loop {
            if let Some(updates) = self.cell.lock().expect("lock").take() {
                return updates;
            }
            tokio::select! {
                changed = receiver.changed() => {
                    if changed.is_err() {
                        return Vec::new();
                    }
                }
                () = cancel.cancelled() => return Vec::new(),
            }
        }
    }
}

struct FakeSharedApi {
    notify: Notify,
    /// 舊測試 `getUpdates.mock.calls`（offset、timeout）。
    get_updates_calls: Mutex<Vec<(Option<f64>, f64)>>,
    set_my_commands: Mutex<Vec<Vec<TelegramBotCommand>>>,
    send_message: Mutex<Vec<RecordedSend>>,
    send_photo: Mutex<Vec<RecordedPhoto>>,
}

struct FakeTelegramApi {
    shared: Arc<FakeSharedApi>,
    /// 舊測試 `getUpdates` 的順序腳本（排空後走 `fallback`）。
    get_updates_queue: Mutex<VecDeque<GetUpdatesStep>>,
    get_updates_fallback: GetUpdatesFallback,
    gate: Arc<GatedUpdates>,
    /// 舊測試 `sendMessage` 的一次性 mock 腳本（`None`＝成功、`Some`＝reject；
    /// 排空後成功）。
    send_message_script: Mutex<VecDeque<Option<TelegramApiError>>>,
}

impl FakeTelegramApi {
    /// 建立 fake api（`served` 依序餵 `getUpdates`；`failure` 排在腳本尾巴）。
    fn new(
        served: Vec<Vec<TelegramUpdate>>,
        failure: Option<TelegramApiError>,
        fallback: GetUpdatesFallback,
    ) -> Arc<Self> {
        let mut queue = VecDeque::new();
        for updates in served {
            queue.push_back(GetUpdatesStep::Served(updates));
        }
        if let Some(error) = failure {
            queue.push_back(GetUpdatesStep::Fail(error));
        }
        Arc::new(Self {
            shared: Arc::new(FakeSharedApi {
                notify: Notify::new(),
                get_updates_calls: Mutex::new(Vec::new()),
                set_my_commands: Mutex::new(Vec::new()),
                send_message: Mutex::new(Vec::new()),
                send_photo: Mutex::new(Vec::new()),
            }),
            get_updates_queue: Mutex::new(queue),
            get_updates_fallback: fallback,
            gate: GatedUpdates::new(),
            send_message_script: Mutex::new(VecDeque::new()),
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

    /// 舊測試：`deliver?.([update…])`（僅在 gated fallback 下有意義）。
    fn deliver(&self, updates: Vec<TelegramUpdate>) {
        self.gate.deliver(updates);
    }
}

#[async_trait]
impl TelegramApi for FakeTelegramApi {
    async fn get_updates(
        &self,
        offset: Option<f64>,
        timeout_seconds: f64,
        cancel: &CancellationToken,
    ) -> Result<Vec<TelegramUpdate>, TelegramApiError> {
        self.shared
            .get_updates_calls
            .lock()
            .expect("lock")
            .push((offset, timeout_seconds));
        self.shared.notify.notify_one();

        let step = self.get_updates_queue.lock().expect("lock").pop_front();
        match step {
            Some(GetUpdatesStep::Served(updates)) => Ok(updates),
            Some(GetUpdatesStep::Fail(error)) => Err(error),
            None => match self.get_updates_fallback {
                GetUpdatesFallback::HangUntilAbort => {
                    // 舊測試：等 shutdown 的 abort，resolve([])。
                    cancel.cancelled().await;
                    Ok(Vec::new())
                }
                GetUpdatesFallback::Gated => {
                    // 舊測試 3：等 deliver 或 shutdown（回 []）。
                    Ok(self.gate.take(cancel).await)
                }
            },
        }
    }

    async fn set_my_commands(
        &self,
        commands: &[TelegramBotCommand],
    ) -> Result<(), TelegramApiError> {
        self.shared
            .set_my_commands
            .lock()
            .expect("lock")
            .push(commands.to_vec());
        self.shared.notify.notify_one();
        Ok(())
    }

    async fn send_message(
        &self,
        chat_id: &str,
        text: &str,
        options: Option<&TelegramSendMessageOptions>,
    ) -> Result<(), TelegramApiError> {
        self.shared
            .send_message
            .lock()
            .expect("lock")
            .push(RecordedSend {
                chat_id: chat_id.to_string(),
                text: text.to_string(),
                reply_markup: options.and_then(|options| options.reply_markup.clone()),
            });
        self.shared.notify.notify_one();
        // 舊測試 mock 的 resolve／reject 腳本（`None`＝成功；排空後成功）。
        let failure = self
            .send_message_script
            .lock()
            .expect("lock")
            .pop_front()
            .flatten();
        if let Some(error) = failure {
            return Err(error);
        }
        Ok(())
    }

    async fn send_photo(
        &self,
        chat_id: &str,
        image: &[u8],
        filename: &str,
        caption: &str,
    ) -> Result<(), TelegramApiError> {
        self.shared
            .send_photo
            .lock()
            .expect("lock")
            .push(RecordedPhoto {
                chat_id: chat_id.to_string(),
                image: image.to_vec(),
                filename: filename.to_string(),
                caption: caption.to_string(),
            });
        self.shared.notify.notify_one();
        Ok(())
    }
}

/// 取出 Mutex 內容的長度（等待條件用）。
pub(crate) fn len<T>(lock: &Mutex<Vec<T>>) -> usize {
    lock.lock().expect("lock").len()
}

// -----------------------------------------------------------------------------
// 舊測試 2／3 的 `pending` 停留（watch 版 release 閘門）
// -----------------------------------------------------------------------------

/// 停留結束後兩種查詢的回覆設定（舊 harness 對 `pending` 的 mock 設定）。
#[derive(Clone, Default)]
struct StallPlan {
    /// `/points` 查詢（`getChannelPoints`）停留至 release；放行後的回覆。
    points_result: Option<Vec<SessionChannelPointsResult>>,
    /// `/screenshot` 查詢（`captureScreenshot`）停留至 release；`Some(None)`
    /// 對應舊 mock `pending` resolve 的 `undefined`、`Some(Some(…))` 為圖。
    capture_result: Option<Option<SessionScreenshot>>,
}

/// 舊測試 2／3 的 `pending`：`release()` 前查詢的等待（watch 版，
/// `wait` 不持鎖跨 await）。
struct StallGate {
    plan: StallPlan,
    released: watch::Sender<bool>,
}

impl StallGate {
    fn new(plan: StallPlan) -> Self {
        let (released, _) = watch::channel(false);
        Self { plan, released }
    }

    /// 舊測試 `await pending`：`finish()` 後放行。
    async fn wait(&self) {
        let mut receiver = self.released.subscribe();
        loop {
            if *receiver.borrow_and_update() {
                return;
            }
            let _ = receiver.changed().await;
        }
    }

    /// 舊測試 `finish?.()`。
    fn release(&self) {
        let _ = self.released.send_replace(true);
    }
}

// -----------------------------------------------------------------------------
// 虛假的 BotCommandContext（對應舊測試 fake 的 scheduler／sessionManager／
// runtimeConfigManager）
// -----------------------------------------------------------------------------

/// 舊測試 `captureScreenshot` 的一次性覆寫。
enum CaptureOverride {
    Ok(SessionScreenshot),
    Err(String),
}

struct FakeSharedContext {
    notify: Notify,
    /// 舊測試 scheduler.runOnce 的呼叫次數。
    run_once_count: Mutex<u32>,
    /// 舊測試 scheduler.stop（pauseChecks）呼叫次數。
    pause_count: Mutex<u32>,
    /// 舊測試 scheduler.start（resumeChecks）呼叫次數。
    resume_count: Mutex<u32>,
    /// 舊測試 harness 的 `activeChannels`（唯讀）。
    active_channels: Vec<String>,
    refresh_pages_calls: Mutex<Vec<Option<String>>>,
    capture_screenshot_calls: Mutex<Vec<String>>,
    get_channel_points_calls: Mutex<Vec<Option<String>>>,
    set_channels_calls: Mutex<Vec<Vec<String>>>,
    set_max_concurrent_streams_calls: Mutex<Vec<f64>>,
    /// 舊測試 harness 的 mutable `runtimeConfig`。
    runtime_config: Mutex<RuntimeWatchConfig>,
    /// 舊測試 `getChannelPoints.mockResolvedValueOnce` 的順序覆寫。
    points_results_queue: Mutex<VecDeque<Vec<SessionChannelPointsResult>>>,
    /// 舊測試 `getChannelPoints.mockRejectedValueOnce(new Error('…'))`。
    points_reject_once: Mutex<Option<String>>,
    /// 舊測試 `captureScreenshot` 的順序覆寫（Ok／Err 依次消耗）。
    capture_overrides: Mutex<VecDeque<CaptureOverride>>,
    /// 舊測試 2／3 的 `pending` 停留。
    stall: StallGate,
}

struct FakeCommandContext {
    shared: Arc<FakeSharedContext>,
}

impl FakeCommandContext {
    fn new(active_channels: Vec<String>, stall: StallPlan) -> Arc<Self> {
        let max_concurrent_streams = 2.0f64.min(active_channels.len() as f64);
        Arc::new(Self {
            shared: Arc::new(FakeSharedContext {
                notify: Notify::new(),
                run_once_count: Mutex::new(0),
                pause_count: Mutex::new(0),
                resume_count: Mutex::new(0),
                active_channels: active_channels.clone(),
                refresh_pages_calls: Mutex::new(Vec::new()),
                capture_screenshot_calls: Mutex::new(Vec::new()),
                get_channel_points_calls: Mutex::new(Vec::new()),
                set_channels_calls: Mutex::new(Vec::new()),
                set_max_concurrent_streams_calls: Mutex::new(Vec::new()),
                runtime_config: Mutex::new(RuntimeWatchConfig {
                    channels: active_channels,
                    max_concurrent_streams,
                }),
                points_results_queue: Mutex::new(VecDeque::new()),
                points_reject_once: Mutex::new(None),
                capture_overrides: Mutex::new(VecDeque::new()),
                stall: StallGate::new(stall),
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

    /// 舊測試 harness 的 scheduler snapshot。
    fn scheduler_snapshot() -> WatchdogSchedulerSnapshot {
        WatchdogSchedulerSnapshot {
            running: true,
            check_in_flight: false,
            retry_at: None,
            last_checked_at: Some(FIXED_LAST_CHECKED_AT.to_string()),
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
        // 舊測試 fake：回 harness 的 activeChannels。
        self.shared.active_channels.clone()
    }

    fn get_refresh_statuses(&self) -> Vec<ChannelSessionRefreshStatus> {
        // 舊測試 fake：enabled 全開、nextRefreshAt 依索引遞增、
        // secondsUntilRefresh 每個 +60 秒。
        self.shared
            .active_channels
            .iter()
            .enumerate()
            .map(|(index, channel)| ChannelSessionRefreshStatus {
                channel: channel.clone(),
                enabled: true,
                next_refresh_at: Some(format!("2026-06-14T00:0{}:00.000Z", index + 4)),
                seconds_until_refresh: Some(240.0 + index as f64 * 60.0),
            })
            .collect()
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
        // 舊測試 fake：過濾（不分大小寫）後各回 'refreshed'。
        let selected: Vec<String> = match channel {
            None => self.shared.active_channels.clone(),
            Some(channel) => self
                .shared
                .active_channels
                .iter()
                .filter(|active| active.to_lowercase() == channel.to_lowercase())
                .cloned()
                .collect(),
        };
        Ok(selected
            .into_iter()
            .map(|channel| SessionRefreshResult {
                channel,
                status: SessionRefreshStatus::Refreshed,
                error: None,
            })
            .collect())
    }

    async fn capture_screenshot(
        &self,
        channel: Option<&str>,
    ) -> Result<Option<SessionScreenshot>, BotCommandError> {
        // 舊測試 fake：`captureScreenshot(channel = 'first')`——undefined 以
        // 'first' 呼叫。
        let channel = channel.unwrap_or("first").to_string();
        self.shared
            .capture_screenshot_calls
            .lock()
            .expect("lock")
            .push(channel.clone());
        self.shared.notify.notify_one();

        // 舊測試 2／3 的 `pending`：先停留，release 後回設定的結果。
        if let Some(release_result) = &self.shared.stall.plan.capture_result {
            self.shared.stall.wait().await;
            return Ok(release_result.clone());
        }

        // 舊測試 mock 的順序覆寫（Ok／Err 依次消耗）。
        let override_result = {
            self.shared
                .capture_overrides
                .lock()
                .expect("lock")
                .remove(0)
        };
        match override_result {
            Some(CaptureOverride::Err(message)) => {
                return Err(BotCommandError { message });
            }
            Some(CaptureOverride::Ok(screenshot)) => {
                return Ok(Some(screenshot));
            }
            None => {}
        }

        // 舊測試 fake 的預設行為：查 activeChannels（不分大小寫），回
        // Buffer(`screenshot:<channel>`)；找不到回 undefined。
        if self
            .shared
            .active_channels
            .iter()
            .any(|active| active.to_lowercase() == channel.to_lowercase())
        {
            let selected = self
                .shared
                .active_channels
                .iter()
                .find(|active| active.to_lowercase() == channel.to_lowercase())
                .cloned()
                .expect("剛才的判定保證存在");
            Ok(Some(SessionScreenshot {
                channel: selected.clone(),
                image: format!("screenshot:{selected}").into_bytes(),
            }))
        } else {
            Ok(None)
        }
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

        // 舊測試 2／3 的 `pending`：先停留，release 後回設定的結果。
        if let Some(release_result) = &self.shared.stall.plan.points_result {
            self.shared.stall.wait().await;
            return Ok(release_result.clone());
        }

        // 舊測試 `mockRejectedValueOnce(new Error('…'))`。
        if let Some(rejected) = self.shared.points_reject_once.lock().expect("lock").take() {
            return Err(BotCommandError { message: rejected });
        }

        // 舊測試 fake：`mockResolvedValueOnce` 只覆寫下一次呼叫。
        let override_result = {
            self.shared
                .points_results_queue
                .lock()
                .expect("lock")
                .remove(0)
        };
        if let Some(overridden) = override_result {
            return Ok(overridden);
        }

        // 舊測試 fake 的預設行為：過濾活躍頻道後各回 available 1_000。
        let selected: Vec<String> = match channel {
            None => self.shared.active_channels.clone(),
            Some(channel) => self
                .shared
                .active_channels
                .iter()
                .filter(|active| active.to_lowercase() == channel.to_lowercase())
                .cloned()
                .collect(),
        };
        Ok(selected
            .into_iter()
            .map(|channel| SessionChannelPointsResult::Available {
                channel,
                balance: 1_000.0,
                display_value: "1K".to_string(),
            })
            .collect())
    }

    fn get_config(&self) -> RuntimeWatchConfig {
        self.shared.runtime_config.lock().expect("lock").clone()
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
        // 舊測試 fake：{ channels, maxConcurrentStreams: min(現值, 清單長度) }。
        let updated = {
            let mut runtime_config = self.shared.runtime_config.lock().expect("lock");
            runtime_config.channels = channels.to_vec();
            runtime_config.max_concurrent_streams = runtime_config
                .max_concurrent_streams
                .min(channels.len() as f64);
            runtime_config.clone()
        };
        Ok(updated)
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
        // 舊測試 fake：{ channels（不變）, maxConcurrentStreams: value }。
        let updated = {
            let mut runtime_config = self.shared.runtime_config.lock().expect("lock");
            runtime_config.max_concurrent_streams = value;
            runtime_config.clone()
        };
        Ok(updated)
    }
}

// -----------------------------------------------------------------------------
// 虛假的 logger（對應舊測試 harness 的 mocks.logger）；欄位以序列化字串錄製
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

    /// 舊測試 `expect(logger.warn).toHaveBeenCalledWith(event, fields)`：
    /// 以「事件＋欄位的 JSON 字串」呈現（欄位順序照插入序）。
    fn contains_warn_with_fields(&self, event: &str, fields: &str) -> bool {
        self.shared
            .warn_calls
            .lock()
            .expect("lock")
            .iter()
            .any(|record| record.event == event && record.fields.as_deref() == Some(fields))
    }

    /// 供除錯輸出的記錄清單。
    fn warn_records_debug(&self) -> String {
        self.shared
            .warn_calls
            .lock()
            .expect("lock")
            .iter()
            .map(|record| {
                format!(
                    "{}: {}",
                    record.event,
                    record.fields.as_deref().unwrap_or("")
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn record_log_call(record: &mut Vec<LogRecord>, event: &str, fields: Option<&SanitizedValue>) {
    record.push(LogRecord {
        event: event.to_string(),
        fields: fields.map(SanitizedValue::to_json_string),
    });
}

impl TelegramBotLogger for FakeLogger {
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
// 虛假的 sleep（對應舊測試 harness 的 `sleep` mock）
// -----------------------------------------------------------------------------

/// 舊測試 `sleep = vi.fn(async () => undefined)`：錄下 duration（訊號參數照
/// `expect.any(AbortSignal)` 的形，不斷言），立即返回。
struct FakeSleep {
    shared: Arc<FakeSleepShared>,
}

struct FakeSleepShared {
    notify: Notify,
    durations: Mutex<Vec<f64>>,
}

impl FakeSleep {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            shared: Arc::new(FakeSleepShared {
                notify: Notify::new(),
                durations: Mutex::new(Vec::new()),
            }),
        })
    }
}

#[async_trait]
impl TelegramSleep for FakeSleep {
    async fn sleep(&self, milliseconds: f64, _cancel: &CancellationToken) {
        self.shared
            .durations
            .lock()
            .expect("lock")
            .push(milliseconds);
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

/// 舊測試 `createTestConfig({telegram: {...}})`（其餘照預設）。
fn create_test_config(telegram: TelegramConfig) -> AppConfig {
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
        telegram,
        discord: DiscordConfig {
            enabled: false,
            bot_token: String::new(),
            application_id: String::new(),
            guild_id: String::new(),
            allowed_channel_ids: Vec::new(),
            allow_direct_messages: false,
            allowed_user_ids: Vec::new(),
        },
    }
}

// -----------------------------------------------------------------------------
// 測試 harness（對應舊測試 createHarness 的回傳）
// -----------------------------------------------------------------------------

struct Harness {
    bot: DefaultTelegramBot,
    api: Arc<FakeTelegramApi>,
    logger: Arc<FakeLogger>,
    context: Arc<FakeCommandContext>,
    sleep: Arc<FakeSleep>,
}

/// 建立測試 harness（對應舊測試 `createHarness(updates, activeChannels)`；
/// fake 的 `getUpdates` 排空後 hang 到 shutdown，`sendMessage`／送訊照預設成功）。
fn create_harness(served: Vec<Vec<TelegramUpdate>>, active_channels: Vec<&str>) -> Harness {
    create_harness_full(
        served,
        active_channels,
        GetUpdatesFallback::HangUntilAbort,
        StallPlan::default(),
    )
}

/// 帶 fallback 的版本（測試 3 用 gated）。
fn create_harness_with_fallback(
    served: Vec<Vec<TelegramUpdate>>,
    active_channels: Vec<&str>,
    fallback: GetUpdatesFallback,
) -> Harness {
    create_harness_full(served, active_channels, fallback, StallPlan::default())
}

/// 完整版本（測試 2 指定 stall 設定）。
fn create_harness_full(
    served: Vec<Vec<TelegramUpdate>>,
    active_channels: Vec<&str>,
    fallback: GetUpdatesFallback,
    stall: StallPlan,
) -> Harness {
    let api = FakeTelegramApi::new(served, None, fallback);
    let logger = FakeLogger::new();
    let context = FakeCommandContext::new(
        active_channels
            .iter()
            .map(|channel| (*channel).to_string())
            .collect(),
        stall,
    );
    let sleep = FakeSleep::new();
    let config = create_test_config(TelegramConfig {
        enabled: true,
        bot_token: "test-token".to_string(),
        allowed_chat_ids: vec![ALLOWED_CHAT_ID.to_string()],
        polling_timeout_seconds: 25.0,
    });
    let bot = DefaultTelegramBot::new(DefaultTelegramBotOptions {
        config,
        api: Arc::clone(&api) as Arc<dyn TelegramApi>,
        command_context: Arc::clone(&context) as Arc<dyn BotCommandContext>,
        logger: Arc::clone(&logger) as Arc<dyn TelegramBotLogger>,
        sleep: Some(Arc::clone(&sleep) as Arc<dyn TelegramSleep>),
    });
    Harness {
        bot,
        api,
        logger,
        context,
        sleep,
    }
}

impl Harness {
    /// 舊測試 flushQueries 的等價：等名額內查詢收尾（名額計為 0）。
    async fn wait_queries_settled(&self) {
        for _ in 0..400 {
            if self.bot.all_queries_settled() {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("名額內查詢應收尾（舊測試 flushQueries）");
    }

    /// 舊測試 `finish?.()`。
    fn release_stall(&self) {
        self.context.shared.stall.release();
    }
}

/// 舊測試 `update(updateId, chatId, text)`。
fn update(update_id: f64, chat_id: &str, text: &str) -> TelegramUpdate {
    TelegramUpdate {
        update_id,
        message: Some(TelegramMessage {
            message_id: update_id,
            text: Some(text.to_string()),
            chat: TelegramChat {
                id: TelegramChatId::String(chat_id.to_string()),
            },
        }),
    }
}

/// 送出訊息的文字列表（依錄製順序；供序列斷言）。
fn sent_texts(harness: &Harness) -> Vec<String> {
    harness
        .api
        .shared
        .send_message
        .lock()
        .expect("lock")
        .iter()
        .map(|recorded| recorded.text.clone())
        .collect()
}

// -----------------------------------------------------------------------------
// 移植的測試（標題與 doc/spec/ts-test-titles.tsv 逐字相同）
// -----------------------------------------------------------------------------

// 原測試：啟動、處理授權指令並可停止長輪詢
#[tokio::test(start_paused = true)]
async fn starts_handles_authorized_commands_and_can_stop_the_long_polling() {
    let harness = create_harness(
        vec![vec![
            update(1.0, ALLOWED_CHAT_ID, "/status"),
            update(2.0, ALLOWED_CHAT_ID, "/check"),
            update(3.0, ALLOWED_CHAT_ID, "/pause"),
            update(4.0, ALLOWED_CHAT_ID, "/resume"),
            update(5.0, ALLOWED_CHAT_ID, "/channels"),
            update(6.0, ALLOWED_CHAT_ID, "/refresh"),
            update(7.0, ALLOWED_CHAT_ID, "/refresh_now second"),
            update(8.0, ALLOWED_CHAT_ID, "/screenshot second"),
        ]],
        vec!["first", "second"],
    );

    harness.bot.start().await;

    // 舊測試：vi.waitFor 於 scheduler.runOnce／stop（pauseChecks）／start
    // （resumeChecks）各呼叫一次。
    harness
        .context
        .wait_for(|shared| {
            *shared.run_once_count.lock().expect("lock") == 1
                && *shared.pause_count.lock().expect("lock") == 1
                && *shared.resume_count.lock().expect("lock") == 1
        })
        .await;

    harness.bot.stop("test").await;

    let messages = sent_texts(&harness);
    assert!(
        messages
            .first()
            .expect("啟動訊息應存在")
            .contains("Twitch Watchdog 已啟動"),
        "{messages:?}"
    );
    {
        // 舊測試：setMyCommands（arrayContaining status／refresh 條目）。
        let runs = harness.api.shared.set_my_commands.lock().expect("lock");
        let commands = runs.last().expect("setMyCommands 應已呼叫");
        assert!(
            commands.contains(&TelegramBotCommand {
                command: "status".to_string(),
                description: "顯示服務與觀看狀態".to_string(),
            }) && commands.contains(&TelegramBotCommand {
                command: "refresh".to_string(),
                description: "顯示播放器重整倒數".to_string(),
            }),
            "{commands:?}"
        );
    }
    {
        // 舊測試：sendMessage 第 1 呼的第 3 參（reply_markup 帶 persistent／resize）。
        let sends = harness.api.shared.send_message.lock().expect("lock");
        let start = sends.first().expect("啟動訊息應存在");
        let markup = start.reply_markup.clone().expect("啟動訊息應帶鍵盤");
        assert_eq!(markup.is_persistent, Some(true));
        assert_eq!(markup.resize_keyboard, Some(true));
    }
    assert!(
        messages.contains(&"已完成一次狀態檢查。".to_string()),
        "{messages:?}"
    );
    assert!(
        messages.contains(&"自動檢查已暫停。".to_string()),
        "{messages:?}"
    );
    assert!(
        messages.contains(&"自動檢查已恢復。".to_string()),
        "{messages:?}"
    );
    assert!(
        messages.iter().any(|text| text.contains("正在觀看：first")),
        "{messages:?}"
    );
    assert!(
        messages.iter().any(|text| text.contains("1. 🔴 first")),
        "{messages:?}"
    );
    assert!(
        messages.iter().any(|text| text.contains("重整倒數")),
        "{messages:?}"
    );
    assert!(
        messages.iter().any(|text| text.contains("first：4 分鐘後")),
        "{messages:?}"
    );
    assert!(
        messages.iter().any(|text| text.contains("second：已重整")),
        "{messages:?}"
    );
    {
        // 舊測試：sendPhoto('42', Buffer('screenshot:second'), 'second.png',
        // 'second 目前瀏覽器畫面')。
        let photos = harness.api.shared.send_photo.lock().expect("lock");
        let photo = photos.last().expect("sendPhoto 應已呼叫");
        assert_eq!(photo.chat_id, ALLOWED_CHAT_ID);
        assert_eq!(photo.image, b"screenshot:second");
        assert_eq!(photo.filename, "second.png");
        assert_eq!(photo.caption, "second 目前瀏覽器畫面");
    }
    assert!(
        messages
            .last()
            .expect("停止訊息應存在")
            .contains("Twitch Watchdog 已停止"),
        "{messages:?}"
    );
    {
        // 舊測試：getUpdates.mock.calls 的前兩次 offset（undefined → 9）。
        let calls = harness.api.shared.get_updates_calls.lock().expect("lock");
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert_eq!(calls[0].0, None);
        assert_eq!(calls[1].0, Some(9.0));
    }
}

/// 舊測試 it.each 的案例表（`['/points', '/screenshot']`）。
enum StallCommand {
    Points,
    Screenshot,
}

// 原測試：%s 卡住時仍接收下一輪 /status 並可停止
#[tokio::test(start_paused = true)]
async fn stalled_commands_still_receive_the_next_status_round_and_can_stop() {
    for command in [StallCommand::Points, StallCommand::Screenshot] {
        let (command_text, stall) = match command {
            StallCommand::Points => (
                "/points",
                // getChannelPoints.mockImplementation：await pending 後回 []。
                StallPlan {
                    points_result: Some(Vec::new()),
                    capture_result: None,
                },
            ),
            StallCommand::Screenshot => (
                "/screenshot",
                // captureScreenshot.mockReturnValue(pending)：resolve undefined。
                StallPlan {
                    points_result: None,
                    capture_result: Some(None),
                },
            ),
        };
        let harness = create_harness_full(
            vec![vec![update(1.0, ALLOWED_CHAT_ID, command_text)]],
            vec!["first", "second"],
            GetUpdatesFallback::HangUntilAbort,
            stall,
        );
        // 舊測試 mock 的接續順序：第二輪 /status，其後 hang 到 abort。
        harness
            .api
            .get_updates_queue
            .lock()
            .expect("lock")
            .push_back(GetUpdatesStep::Served(vec![update(
                2.0,
                ALLOWED_CHAT_ID,
                "/status",
            )]));

        harness.bot.start().await;
        harness
            .api
            .wait_for(|shared| {
                len(&shared.send_message) >= 2
                    && shared
                        .send_message
                        .lock()
                        .expect("lock")
                        .iter()
                        .any(|send| send.text.contains("正在觀看：first、second"))
            })
            .await;
        harness.bot.stop("test").await;
        let message_count = len(&harness.api.shared.send_message);

        // 舊測試：finish?.(); await flushQueries(); → 名額內查詢收尾後無新訊息。
        harness.release_stall();
        harness.wait_queries_settled().await;
        assert_eq!(
            len(&harness.api.shared.send_message),
            message_count,
            "停止後不得有新訊息"
        );
        assert_eq!(len(&harness.api.shared.send_photo), 0);
    }
}

// 原測試：查詢逾時會回覆、保留名額，並抑制稍後完成的結果
#[tokio::test(start_paused = true)]
async fn query_timeout_replies_keeps_slot_and_suppresses_late_results() {
    let harness = create_harness_full(
        vec![vec![
            update(1.0, ALLOWED_CHAT_ID, "/points"),
            update(2.0, ALLOWED_CHAT_ID, "/screenshot"),
            update(3.0, ALLOWED_CHAT_ID, "/points"),
            update(4.0, ALLOWED_CHAT_ID, "/status"),
        ]],
        vec!["first", "second"],
        GetUpdatesFallback::Gated,
        // 舊測試 pending promise：查詢停在這裡（逾時計時器由虛擬時間結算）；
        // points 放行後回 first 1000、screenshot 放行後回圖。
        StallPlan {
            points_result: Some(vec![SessionChannelPointsResult::Available {
                channel: "first".to_string(),
                balance: 1_000.0,
                display_value: "1K".to_string(),
            }]),
            capture_result: Some(Some(SessionScreenshot {
                channel: "first".to_string(),
                image: b"png".to_vec(),
            })),
        },
    );

    harness.bot.start().await;
    harness
        .context
        .wait_for(|shared| len(&shared.get_channel_points_calls) >= 1)
        .await;
    {
        // 舊測試：查詢仍在處理中（第三個 update 的 /points 命名額滿）。
        assert!(
            sent_texts(&harness)
                .iter()
                .any(|text| text.contains("查詢仍在處理中")),
            "名額滿時應回覆忙線：{:?}",
            sent_texts(&harness)
        );
        // 舊測試：status 回覆含 active 頻道。
        assert!(
            sent_texts(&harness)
                .iter()
                .any(|text| text.contains("正在觀看：first、second")),
            "{:?}",
            sent_texts(&harness)
        );
    }

    // 舊測試：await vi.advanceTimersByTimeAsync(30_000)。
    tokio::time::advance(QUERY_TIMEOUT_MS).await;

    harness
        .logger
        .wait_for(|shared| {
            shared
                .warn_calls
                .lock()
                .expect("lock")
                .iter()
                .filter(|record| record.event == "telegram_query_timeout")
                .count()
                >= 2
        })
        .await;
    // 舊測試：telegram_query_timeout 欄位照 { command }。
    assert!(
        harness
            .logger
            .contains_warn_with_fields("telegram_query_timeout", r#"{"command":"points"}"#),
        "{:?}",
        harness.logger.warn_records_debug()
    );
    assert!(
        harness
            .logger
            .contains_warn_with_fields("telegram_query_timeout", r#"{"command":"screenshot"}"#),
        "{:?}",
        harness.logger.warn_records_debug()
    );
    // 舊測試：處理逾時回覆兩筆。
    {
        let replies = sent_texts(&harness)
            .into_iter()
            .filter(|text| text.contains("處理逾時"))
            .count();
        assert_eq!(replies, 2);
    }

    // 舊測試：deliver?.([update 5, /points]) → 名額仍滿（逾時後名額保留）。
    harness
        .api
        .deliver(vec![update(5.0, ALLOWED_CHAT_ID, "/points")]);
    {
        let busy_count_before = sent_texts(&harness)
            .into_iter()
            .filter(|text| text.contains("查詢仍在處理中"))
            .count();
        assert!(
            busy_count_before >= 1,
            "update 3 的 /points 應已命一次忙線回覆"
        );
        harness
            .api
            .wait_for(|shared| {
                len(&shared.send_message) > 1
                    && shared
                        .send_message
                        .lock()
                        .expect("lock")
                        .iter()
                        .filter(|send| send.text.contains("查詢仍在處理中"))
                        .count()
                        > busy_count_before
            })
            .await;
        // 舊測試：getChannelPoints 仍只被呼叫一次（名額保留到頁面工作結束）。
        assert_eq!(
            len(&harness.context.shared.get_channel_points_calls),
            1,
            "逾時後名額仍在，重試不得進入查詢"
        );
    }

    // 舊測試：finish?.(); await flushQueries(); → 抑制結果、名額釋放。
    harness.release_stall();
    harness.wait_queries_settled().await;
    assert_eq!(len(&harness.api.shared.send_photo), 0);
    assert!(
        !sent_texts(&harness)
            .iter()
            .any(|text| text.starts_with("忠誠點數：")),
        "逾時後結果應被抑制：{:?}",
        sent_texts(&harness)
    );

    // 舊測試：deliver?.([update 6, '/points') → 名額空、可再查。
    harness
        .api
        .deliver(vec![update(6.0, ALLOWED_CHAT_ID, "/points")]);
    harness
        .context
        .wait_for(|shared| len(&shared.get_channel_points_calls) == 2)
        .await;
    harness.wait_queries_settled().await;
    assert!(
        sent_texts(&harness)
            .iter()
            .any(|text| text.starts_with("忠誠點數：")),
        "{:?}",
        sent_texts(&harness)
    );

    harness.bot.stop("test").await;
}

// 原測試：背景查詢失敗會回覆且釋放名額
#[tokio::test(start_paused = true)]
async fn background_query_failure_replies_and_releases_the_slot() {
    let harness = create_harness(
        vec![vec![update(1.0, ALLOWED_CHAT_ID, "/points")]],
        vec!["first", "second"],
    );
    // 舊測試：getChannelPoints.mockRejectedValueOnce(new Error('page failed'))。
    *harness
        .context
        .shared
        .points_reject_once
        .lock()
        .expect("lock") = Some("page failed".to_string());

    harness.bot.start().await;

    // 舊測試：vi.waitFor 於 sendMessage('42', '/points 處理失敗，請稍後再試。')。
    harness
        .api
        .wait_for(|shared| {
            len(&shared.send_message) >= 1
                && shared
                    .send_message
                    .lock()
                    .expect("lock")
                    .iter()
                    .any(|send| {
                        send.chat_id == ALLOWED_CHAT_ID
                            && send.text == "/points 處理失敗，請稍後再試。"
                    })
        })
        .await;
    // 舊測試：await harness.bot.stop('test')（先等名額釋放完成）。
    harness.wait_queries_settled().await;
    harness.bot.stop("test").await;
}

// 原測試：忽略未授權 chat 且不執行管理指令
#[tokio::test(start_paused = true)]
async fn ignores_unauthorized_chat_and_does_not_run_admin_commands() {
    let harness = create_harness(
        vec![vec![update(1.0, "99", "/pause")]],
        vec!["first", "second"],
    );

    harness.bot.start().await;

    harness
        .logger
        .wait_for(|shared| {
            shared
                .warn_calls
                .lock()
                .expect("lock")
                .iter()
                .any(|record| record.event == "telegram_unauthorized_chat")
        })
        .await;
    harness.bot.stop("test").await;

    assert_eq!(*harness.context.shared.pause_count.lock().expect("lock"), 0);
    let sends = harness.api.shared.send_message.lock().expect("lock");
    assert!(
        sends.iter().all(|send| send.chat_id != "99"),
        "未授權 chat 不得收到訊息"
    );
}

// 原測試：回覆失敗時不重複執行同一個管理指令
#[tokio::test(start_paused = true)]
async fn failed_reply_does_not_repeat_the_same_admin_command() {
    let harness = create_harness(
        vec![vec![update(10.0, ALLOWED_CHAT_ID, "/check")]],
        vec!["first", "second"],
    );
    // 舊測試：sendMessage.mockResolvedValueOnce(undefined).mockRejectedValueOnce(
    // new Error('send failed'))。
    harness
        .api
        .send_message_script
        .lock()
        .expect("lock")
        .push_back(None);
    harness
        .api
        .send_message_script
        .lock()
        .expect("lock")
        .push_back(Some(TelegramApiError::generic("send failed")));

    harness.bot.start().await;

    harness
        .logger
        .wait_for(|shared| {
            shared
                .warn_calls
                .lock()
                .expect("lock")
                .iter()
                .any(|record| record.event == "telegram_command_failed")
        })
        .await;

    // 舊測試：await harness.bot.stop('test')（先等第二次 getUpdates 已錄製，
    // 避免時序差異）。
    harness
        .api
        .wait_for(|shared| len(&shared.get_updates_calls) >= 2)
        .await;
    harness.bot.stop("test").await;

    // 舊測試欄位：updateId 10、errorClass Error、httpStatus null、
    // errorCategory unknown、retryInMs 0。
    assert!(
        harness.logger.contains_warn_with_fields(
            "telegram_command_failed",
            r#"{"updateId":10,"errorClass":"Error","httpStatus":null,"errorCategory":"unknown","retryInMs":0}"#
        ),
        "{:?}",
        harness.logger.warn_records_debug()
    );

    // 舊測試：runOnce 呼叫一次；getUpdates 第二次的 offset 為 11。
    assert_eq!(
        *harness.context.shared.run_once_count.lock().expect("lock"),
        1
    );
    {
        let calls = harness.api.shared.get_updates_calls.lock().expect("lock");
        assert_eq!(calls[1].0, Some(11.0));
    }
}

// 原測試：推送開台、離線與領點通知，忽略 not_found
#[tokio::test(start_paused = true)]
async fn pushes_stream_and_reward_notifications_and_ignores_not_found() {
    let harness = create_harness(vec![], vec!["first", "second"]);

    harness
        .bot
        .notify_stream_status(&StreamStatusChange {
            channel: "first".to_string(),
            is_live: true,
        })
        .await;
    harness
        .bot
        .notify_stream_status(&StreamStatusChange {
            channel: "first".to_string(),
            is_live: false,
        })
        .await;
    harness
        .bot
        .notify_reward(
            &crate::browser::reward_claimer::RewardClaimResult::Claimed {
                channel: "first".to_string(),
                claimed_at: "2026-06-14T00:00:00.000Z".to_string(),
            },
        )
        .await;
    harness
        .bot
        .notify_reward(
            &crate::browser::reward_claimer::RewardClaimResult::NotFound {
                channel: "first".to_string(),
                checked_at: "2026-06-14T00:00:00.000Z".to_string(),
            },
        )
        .await;

    let messages = sent_texts(&harness);
    assert_eq!(
        messages,
        vec![
            "🔴 first 已開台".to_string(),
            "⚫ first 已離線".to_string(),
            "🎁 first 已領取忠誠點數".to_string(),
        ]
    );
}

// 原測試：poll failure 記錄安全分類、HTTP status 與 retry delay
#[tokio::test(start_paused = true)]
async fn poll_failure_logs_safe_classification_http_status_and_retry_delay() {
    let harness = create_harness(vec![], vec!["first", "second"]);
    // 舊測試：getUpdates.mockRejectedValueOnce(new TelegramApiError(
    // 'getUpdates', 'safe failure', { errorCategory: 'http', httpStatus: 429 }))。
    // （MutexGuard 不得跨 await，先在本區塊完成排程。）
    {
        let mut queue = harness.api.get_updates_queue.lock().expect("lock");
        queue.push_back(GetUpdatesStep::Fail(TelegramApiError::new(
            "getUpdates",
            "safe failure",
            TelegramErrorCategory::Http,
            Some(429),
        )));
    }

    harness.bot.start().await;

    // 舊測試欄位：TelegramApiError＋429＋http＋retryInMs 1000。
    harness
        .logger
        .wait_for(|shared| {
            shared
                .warn_calls
                .lock()
                .expect("lock")
                .iter()
                .any(|record| record.event == "telegram_poll_failed" && record.fields.as_deref() == Some(
                    r#"{"errorClass":"TelegramApiError","httpStatus":429,"errorCategory":"http","retryInMs":1000}"#,
                ))
        })
        .await;

    harness.bot.stop("test").await;

    // 舊測試：sleep 帶 1_000。
    let durations = harness.sleep.shared.durations.lock().expect("lock");
    assert_eq!(*durations, vec![1_000.0]);
}

// 原測試：send failure 不記錄 chat ID、endpoint 或 request body
#[tokio::test(start_paused = true)]
async fn send_failure_does_not_log_chat_id_endpoint_or_request_body() {
    let harness = create_harness(vec![], vec!["first", "second"]);
    // 舊測試：sendMessage.mockRejectedValueOnce(new TelegramApiError(
    // 'sendMessage', 'safe failure', { errorCategory: 'network' }))。
    harness
        .api
        .send_message_script
        .lock()
        .expect("lock")
        .push_back(Some(TelegramApiError::new(
            "sendMessage",
            "safe failure",
            TelegramErrorCategory::Network,
            None,
        )));

    harness
        .bot
        .notify_stream_status(&StreamStatusChange {
            channel: "first".to_string(),
            is_live: true,
        })
        .await;

    // 舊測試欄位：TelegramApiError＋null＋network＋retryInMs 0。
    harness
        .logger
        .wait_for(|shared| {
            shared
                .warn_calls
                .lock()
                .expect("lock")
                .iter()
                .any(|record| record.event == "telegram_send_failed" && record.fields.as_deref() == Some(
                    r#"{"errorClass":"TelegramApiError","httpStatus":null,"errorCategory":"network","retryInMs":0}"#,
                ))
        })
        .await;

    // 舊測試：serialized log 不含 '42'、'api.telegram.org'、'chat_id'。
    let serialized = harness.logger.warn_records_debug();
    assert!(!serialized.contains(ALLOWED_CHAT_ID), "{serialized}");
    assert!(!serialized.contains("api.telegram.org"), "{serialized}");
    assert!(!serialized.contains("chat_id"), "{serialized}");
}

// 原測試：推送 page crash、browser restart 與 container restart 通知
#[tokio::test(start_paused = true)]
async fn pushes_page_crash_browser_restart_and_container_restart_notifications() {
    let harness = create_harness(vec![], vec!["first", "second"]);

    harness.bot.notify_page_crash("first").await;
    harness
        .bot
        .notify_browser_restart(&BrowserRestartNotification {
            mode: BrowserRestartMode::Automatic,
        })
        .await;
    harness
        .bot
        .notify_browser_restart(&BrowserRestartNotification {
            mode: BrowserRestartMode::Manual,
        })
        .await;
    harness
        .bot
        .notify_container_restart(&ContainerRestartNotification {
            reason: "browser_crash_loop".to_string(),
            source: "browser_manager".to_string(),
            detail_reason: Some("channel_page_crash_loop".to_string()),
            channel: Some("first".to_string()),
        })
        .await;

    let messages = sent_texts(&harness);
    assert_eq!(
        messages,
        vec![
            "💥 first 觀看頁面崩潰".to_string(),
            "♻️ Firefox 瀏覽器已重啟（自動恢復）".to_string(),
            "♻️ Firefox 瀏覽器已重啟（完整回收）".to_string(),
            [
                "🚨 容器即將重啟".to_string(),
                "原因：browser_crash_loop（channel_page_crash_loop）".to_string(),
                "來源：browser_manager".to_string(),
                "頻道：first".to_string(),
            ]
            .join("\n"),
        ]
    );
}

// 原測試：尚未啟動時 stop 不傳送誤導通知
#[tokio::test(start_paused = true)]
async fn stop_before_start_does_not_send_misleading_notification() {
    let harness = create_harness(vec![], vec!["first", "second"]);

    harness.bot.stop("startup_failed:browser").await;

    assert_eq!(len(&harness.api.shared.send_message), 0);
    assert_eq!(len(&harness.api.shared.get_updates_calls), 0);
}

// 原測試：沒有 active session 時截圖指令回覆提示
#[tokio::test(start_paused = true)]
async fn screenshot_command_replies_with_hint_without_active_sessions() {
    let harness = create_harness(
        vec![vec![update(1.0, ALLOWED_CHAT_ID, "/screenshot")]],
        vec![],
    );

    harness.bot.start().await;

    harness
        .api
        .wait_for(|shared| {
            len(&shared.send_message) >= 2
                && shared
                    .send_message
                    .lock()
                    .expect("lock")
                    .iter()
                    .any(|send| send.text == "目前沒有正在觀看的頻道可供截圖。")
        })
        .await;
    // 舊測試：await harness.bot.stop('test')（先等名額釋放完成）。
    harness.wait_queries_settled().await;
    harness.bot.stop("test").await;

    assert_eq!(len(&harness.api.shared.send_photo), 0);
}

// 原測試：忠誠點數指令可查詢全部或指定頻道
#[tokio::test(start_paused = true)]
async fn points_command_queries_all_channels_or_specific_channel() {
    let harness = create_harness(
        vec![vec![
            update(1.0, ALLOWED_CHAT_ID, "/points"),
            update(2.0, ALLOWED_CHAT_ID, "/points SECOND"),
        ]],
        vec!["first", "second"],
    );
    // 舊測試：getChannelPoints.mockResolvedValueOnce（第一次兩列、第二次一列）。
    *harness
        .context
        .shared
        .points_results_queue
        .lock()
        .expect("lock") = VecDeque::from(vec![
        vec![
            SessionChannelPointsResult::Available {
                channel: "first".to_string(),
                balance: 1_234_567.0,
                display_value: "1.23M".to_string(),
            },
            SessionChannelPointsResult::Unavailable {
                channel: "second".to_string(),
                reason: SessionChannelPointsUnavailableReason::ParseFailed,
            },
        ],
        vec![SessionChannelPointsResult::Available {
            channel: "second".to_string(),
            balance: 9_876.0,
            display_value: "9.8K".to_string(),
        }],
    ]);

    harness.bot.start().await;

    harness
        .context
        .wait_for(|shared| len(&shared.get_channel_points_calls) == 2)
        .await;
    // 舊測試：await harness.bot.stop('test')（先等名額釋放完成）。
    harness.wait_queries_settled().await;
    harness.bot.stop("test").await;

    // 舊測試：getChannelPoints 的參數（undefined → 'SECOND'）。
    {
        let calls = harness
            .context
            .shared
            .get_channel_points_calls
            .lock()
            .expect("lock");
        assert_eq!(calls[0], None);
        assert_eq!(calls[1], Some("SECOND".to_string()));
    }
    let messages = sent_texts(&harness);
    assert!(
        messages.contains(&"忠誠點數：\nfirst：1,234,567 點\nsecond：無法取得".to_string()),
        "{messages:?}"
    );
    assert!(
        messages.contains(&"忠誠點數：\nsecond：9,876 點".to_string()),
        "{messages:?}"
    );
    {
        // 舊測試：setMyCommands 包含 points。
        let runs = harness.api.shared.set_my_commands.lock().expect("lock");
        let commands = runs.last().expect("setMyCommands 應已呼叫");
        assert!(
            commands.contains(&TelegramBotCommand {
                command: "points".to_string(),
                description: "顯示忠誠點數".to_string(),
            }),
            "{commands:?}"
        );
    }
}

// 原測試：沒有 active session 時忠誠點數指令回覆提示
#[tokio::test(start_paused = true)]
async fn points_command_replies_with_hint_without_active_sessions() {
    let harness = create_harness(vec![vec![update(1.0, ALLOWED_CHAT_ID, "/points")]], vec![]);

    harness.bot.start().await;

    harness
        .api
        .wait_for(|shared| {
            len(&shared.send_message) >= 2
                && shared
                    .send_message
                    .lock()
                    .expect("lock")
                    .iter()
                    .any(|send| send.text == "目前沒有正在觀看的頻道可查詢忠誠點數。")
        })
        .await;
    // 舊測試：await harness.bot.stop('test')（先等名額釋放完成）。
    harness.wait_queries_settled().await;
    harness.bot.stop("test").await;
}

// 原測試：未指定頻道時回傳所有 active session 截圖
#[tokio::test(start_paused = true)]
async fn screenshot_without_channel_returns_all_active_session_screenshots() {
    let harness = create_harness(
        vec![vec![update(1.0, ALLOWED_CHAT_ID, "/screenshot")]],
        vec!["first", "second"],
    );

    harness.bot.start().await;

    harness
        .api
        .wait_for(|shared| len(&shared.send_photo) >= 2)
        .await;
    // 舊測試：await harness.bot.stop('test')（先等名額釋放完成）。
    harness.wait_queries_settled().await;
    harness.bot.stop("test").await;

    // 舊測試：sendPhoto 兩呼叫的參數（first → second 順序）。
    {
        let photos = harness.api.shared.send_photo.lock().expect("lock");
        assert_eq!(photos.len(), 2);
        assert_eq!(photos[0].chat_id, ALLOWED_CHAT_ID);
        assert_eq!(photos[0].image, b"screenshot:first");
        assert_eq!(photos[0].filename, "first.png");
        assert_eq!(photos[0].caption, "first 目前瀏覽器畫面");
        assert_eq!(photos[1].chat_id, ALLOWED_CHAT_ID);
        assert_eq!(photos[1].image, b"screenshot:second");
        assert_eq!(photos[1].filename, "second.png");
        assert_eq!(photos[1].caption, "second 目前瀏覽器畫面");
    }
    // 舊測試：captureScreenshot 的兩次呼叫（first → second）。
    assert_eq!(
        *harness
            .context
            .shared
            .capture_screenshot_calls
            .lock()
            .expect("lock"),
        vec!["first".to_string(), "second".to_string()]
    );
}

// 原測試：未指定頻道截圖遇到 stale session 時會跳過並繼續回傳可用頻道
#[tokio::test(start_paused = true)]
async fn screenshot_skips_stale_sessions_and_keeps_available_channels() {
    let harness = create_harness(
        vec![vec![update(1.0, ALLOWED_CHAT_ID, "/screenshot")]],
        vec!["first", "second"],
    );
    // 舊測試：captureScreenshot.mockRejectedValueOnce(
    // new Error('Target page has been closed')).mockResolvedValueOnce(second)。
    *harness
        .context
        .shared
        .capture_overrides
        .lock()
        .expect("lock") = VecDeque::from(vec![
        CaptureOverride::Err("Target page has been closed".to_string()),
        CaptureOverride::Ok(SessionScreenshot {
            channel: "second".to_string(),
            image: b"screenshot:second".to_vec(),
        }),
    ]);

    harness.bot.start().await;

    harness
        .api
        .wait_for(|shared| len(&shared.send_photo) >= 1)
        .await;
    // 舊測試：await harness.bot.stop('test')（先等名額釋放完成）。
    harness.wait_queries_settled().await;
    harness.bot.stop("test").await;

    {
        let photos = harness.api.shared.send_photo.lock().expect("lock");
        let photo = photos.last().expect("sendPhoto 應已呼叫");
        assert_eq!(photo.chat_id, ALLOWED_CHAT_ID);
        assert_eq!(photo.image, b"screenshot:second");
        assert_eq!(photo.filename, "second.png");
        assert_eq!(photo.caption, "second 目前瀏覽器畫面");
    }
    // 舊測試：telegram_screenshot_failed 欄位 { channel, error }。
    assert!(
        harness.logger.contains_warn_with_fields(
            "telegram_screenshot_failed",
            r#"{"channel":"first","error":"Target page has been closed"}"#
        ),
        "{:?}",
        harness.logger.warn_records_debug()
    );
}

// 原測試：可由指令更新頻道清單與最大同時觀看
#[tokio::test(start_paused = true)]
async fn updates_channel_list_and_max_concurrent_streams_via_commands() {
    let harness = create_harness(
        vec![vec![
            update(1.0, ALLOWED_CHAT_ID, "/config"),
            update(2.0, ALLOWED_CHAT_ID, "/channel_add third"),
            update(3.0, ALLOWED_CHAT_ID, "/max_streams 2"),
            update(4.0, ALLOWED_CHAT_ID, "/channel_remove first"),
            update(5.0, ALLOWED_CHAT_ID, "/channels_set alpha,beta gamma"),
        ]],
        vec!["first", "second"],
    );

    harness.bot.start().await;

    harness
        .context
        .wait_for(|shared| len(&shared.set_channels_calls) == 3)
        .await;
    // 舊測試：await harness.bot.stop('test')。
    harness.bot.stop("test").await;

    let set_channels = harness
        .context
        .shared
        .set_channels_calls
        .lock()
        .expect("lock");
    assert_eq!(
        set_channels[0],
        vec![
            "first".to_string(),
            "second".to_string(),
            "third".to_string()
        ]
    );
    let max_streams = harness
        .context
        .shared
        .set_max_concurrent_streams_calls
        .lock()
        .expect("lock");
    assert_eq!(*max_streams, vec![2.0]);
    drop(max_streams);
    assert_eq!(
        set_channels[1],
        vec!["second".to_string(), "third".to_string()]
    );
    assert_eq!(
        set_channels[2],
        vec!["alpha".to_string(), "beta".to_string(), "gamma".to_string()]
    );
    // 舊測試：/config 的回覆含監控頻道（harness 的 runtimeConfig）。
    assert!(
        sent_texts(&harness)
            .iter()
            .any(|text| text.contains("監控頻道：first、second")),
        "{:?}",
        sent_texts(&harness)
    );
}
