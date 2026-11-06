//! Telegram Bot：`telegram/TelegramBot.ts` 逐函式移植（§4.2／§7.8，S4.2）。
//!
//! 指令、授權檢查、回覆與通知文字逐字照舊版（含全形標點與換行）；點數以
//! `toLocaleString('en-US')` 的千分位逗號（[`format_balance`]）。與舊版的
//! 結構差異（細節見各段）：
//!
//! - 長輪詢：`start()` 依舊版順序（`setMyCommands` 失敗只記
//!   `telegram_command_menu_failed` → 廣播啟動訊息 → 外派 poll 迴圈）。
//!   `stop()` 取消訊號（舊版 `abortController` 由 [`CancellationToken`] 呈現）、
//!   等待 poll 迴圈結束後廣播 `twiclaimer 已停止\n原因：<reason>`。
//! - 舊版 `306b4ba`「查詢停頓時保持回應」逐項移植：`/points`／`/screenshot`
//!   以 `start_query` 外派（名額 [`MAX_ACTIVE_QUERIES`]）
//!   保留到頁面工作結束；30 秒查詢逾時計時器（`telegram_query_timeout`）；
//!   失敗回覆與 `telegram_query_failed` 的 catch 鏈。訊號組合
//!   （`AbortSignal.any`）以 [`QueryAbortSignal`] 呈現；計時器的
//!   `clearTimeout`／shutdown listener 以 `shutdown` 的 child token 呈現。
//! - `sleep` 照舊版 `options.sleep` 注入（預設 [`TokioTelegramSleep`]，同舊版
//!   `DEFAULT_SLEEP` 的 setTimeout／abort listener 語意）。
#![allow(dead_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Mutex as StateMutex;
use tokio_util::sync::CancellationToken;

use crate::browser::reward_claimer::RewardClaimResult;
use crate::config::model::AppConfig;
use crate::config::writeback::{ConfigWriteError, RuntimeWatchConfig};
use crate::logging::redact::{SanitizedValue, js_number_to_string, redact_sensitive_string};
use crate::notifications::bot_command_context::{
    BotCommandContext, BotCommandError, SessionChannelPointsResult, SessionRefreshStatus,
    SessionScreenshot, StreamStatusChange,
};
use crate::notifications::bot_notifications::{
    BrowserRestartNotification, ContainerRestartNotification, format_browser_restart_message,
    format_container_restart_message, format_page_crash_message,
};
use crate::telegram::api::{
    TelegramApi, TelegramApiError, TelegramBotCommand, TelegramErrorCategory,
    TelegramErrorDescriptor, TelegramKeyboardButton, TelegramReplyKeyboardMarkup,
    TelegramSendMessageOptions, TelegramUpdate,
};

/// 舊版 `QUERY_TIMEOUT_MS`（查詢逾時計時器的毫秒數）。
const QUERY_TIMEOUT_MS: Duration = Duration::from_millis(30_000);
/// 舊版 `MAX_ACTIVE_QUERIES`（名額內查詢數上限）。
const MAX_ACTIVE_QUERIES: usize = 2;
/// 舊版 poll 失敗後的重試上限（`Math.min(30_000, 1_000 * 2 ** (n − 1))`）。
const MAX_RETRY_MS: f64 = 30_000.0;
/// 舊版 poll 失敗重試的基數（`1_000 * 2^(failureCount − 1)`）。
const RETRY_BASE_MS: f64 = 1_000.0;

/// 對應舊版 `HELP_TEXT` 的行內容（逐行照舊版陣列）。
const HELP_TEXT_LINES: [&str; 16] = [
    "可用指令：",
    "/status - 顯示服務與觀看狀態",
    "/channels - 顯示監控頻道",
    "/refresh - 顯示 Twitch 播放器重整倒數",
    "/refresh_now [頻道] - 立即重整全部或指定觀看頁",
    "/points [頻道] - 顯示全部或指定頻道忠誠點數",
    "/config - 顯示可調整的設定",
    "/channel_add 頻道 - 新增監控頻道",
    "/channel_remove 頻道 - 移除監控頻道",
    "/channels_set 頻道一,頻道二 - 取代頻道清單",
    "/max_streams 數量 - 設定最大同時觀看",
    "/check - 立即檢查一次",
    "/pause - 暫停自動檢查",
    "/resume - 恢復自動檢查",
    "/screenshot [頻道] - 回傳全部或指定頻道畫面",
    "/help - 顯示說明",
];

/// 對應舊版 `HELP_TEXT`（`"\n".join`）。
fn help_text() -> String {
    HELP_TEXT_LINES.join("\n")
}

/// 對應舊版 `BOT_COMMANDS`（`setMyCommands` 的內容）。
pub(crate) fn bot_commands() -> Vec<TelegramBotCommand> {
    let command = |name: &str, description: &str| TelegramBotCommand {
        command: name.to_string(),
        description: description.to_string(),
    };
    vec![
        command("status", "顯示服務與觀看狀態"),
        command("channels", "顯示監控頻道"),
        command("refresh", "顯示播放器重整倒數"),
        command("refresh_now", "立即重整觀看頁"),
        command("points", "顯示忠誠點數"),
        command("config", "顯示頻道與同時觀看設定"),
        command("channel_add", "新增監控頻道"),
        command("channel_remove", "移除監控頻道"),
        command("channels_set", "取代監控頻道清單"),
        command("max_streams", "設定最大同時觀看"),
        command("check", "立即檢查一次"),
        command("pause", "暫停自動檢查"),
        command("resume", "恢復自動檢查"),
        command("screenshot", "擷取目前觀看畫面"),
        command("help", "顯示指令說明"),
    ]
}

/// 對應舊版 `COMMAND_KEYBOARD`（`reply_markup` 物件字面；欄位順序照建立順序）。
fn command_keyboard() -> TelegramReplyKeyboardMarkup {
    let button = |text: &str| TelegramKeyboardButton {
        text: text.to_string(),
    };
    TelegramReplyKeyboardMarkup {
        keyboard: vec![
            vec![button("/status"), button("/channels")],
            vec![button("/refresh"), button("/refresh_now")],
            vec![button("/points")],
            vec![button("/config")],
            vec![button("/check"), button("/screenshot")],
            vec![button("/pause"), button("/resume")],
            vec![button("/help")],
        ],
        is_persistent: Some(true),
        resize_keyboard: Some(true),
    }
}

/// 對應舊版 `TelegramBot` 介面。
#[async_trait]
pub(crate) trait TelegramBot: Send + Sync {
    /// 舊版 `start()`（不可失敗：`setMyCommands` 與廣播的失敗都只記 log）。
    async fn start(&self);

    /// 舊版 `stop(reason)`：取消訊號、等 poll 迴圈結束後廣播停止訊息；
    /// 重複呼叫無作用（`started` 下旗標前先檢查）。
    async fn stop(&self, reason: &str);

    /// 舊版 `notifyStreamStatus(change)`。
    async fn notify_stream_status(&self, change: &StreamStatusChange);

    /// 舊版 `notifyReward(result)`。
    async fn notify_reward(&self, result: &RewardClaimResult);

    /// 舊版 `notifyPageCrash(channel)`。
    async fn notify_page_crash(&self, channel: &str);

    /// 舊版 `notifyBrowserRestart(event)`。
    async fn notify_browser_restart(&self, event: &BrowserRestartNotification);

    /// 舊版 `notifyContainerRestart(event)`。
    async fn notify_container_restart(&self, event: &ContainerRestartNotification);
}

/// 對應舊版 `DefaultTelegramBotOptions`：config、api、commandContext、logger、
/// sleep。
pub(crate) struct DefaultTelegramBotOptions {
    pub(crate) config: AppConfig,
    pub(crate) api: Arc<dyn TelegramApi>,
    pub(crate) command_context: Arc<dyn BotCommandContext>,
    pub(crate) logger: Arc<dyn TelegramBotLogger>,
    /// 舊版 `sleep?`；[`None`] 用 [`TokioTelegramSleep`]（[`DEFAULT_SLEEP`] 對應）。
    pub(crate) sleep: Option<Arc<dyn TelegramSleep>>,
}

/// 對應舊版 `sleep?: (milliseconds, signal) => Promise<void>` 的注入。
#[async_trait]
pub(crate) trait TelegramSleep: Send + Sync {
    /// 舊版 `sleep(milliseconds, signal)`：訊號中止時提前結束。
    async fn sleep(&self, milliseconds: f64, cancel: &CancellationToken);
}

/// 舊版 `DEFAULT_SLEEP`：setTimeout 與 abort listener 先到先結束。
pub(crate) struct TokioTelegramSleep;

#[async_trait]
impl TelegramSleep for TokioTelegramSleep {
    async fn sleep(&self, milliseconds: f64, cancel: &CancellationToken) {
        let milliseconds = if milliseconds.is_finite() && milliseconds > 0.0 {
            milliseconds
        } else {
            0.0
        };
        tokio::select! {
            () = tokio::time::sleep(Duration::from_millis(milliseconds as u64)) => {}
            () = cancel.cancelled() => {}
        }
    }
}

/// 對應舊版 `Logger` 介面（bot 用到的 info／warn；欄位值為消毒前的
/// [`SanitizedValue`]，消毒在 Logger 本體進行）。
pub(crate) trait TelegramBotLogger: Send + Sync {
    fn info(&self, event: &str, fields: Option<&SanitizedValue>);
    fn warn(&self, event: &str, fields: Option<&SanitizedValue>);
}

impl TelegramBotLogger for crate::logging::logger::JsonLineLogger {
    fn info(&self, event: &str, fields: Option<&SanitizedValue>) {
        // inherent 的 info 同簽名；以完整路徑呼叫避免誤呼本 impl 造成遞迴。
        crate::logging::logger::JsonLineLogger::info(self, event, fields);
    }

    fn warn(&self, event: &str, fields: Option<&SanitizedValue>) {
        crate::logging::logger::JsonLineLogger::warn(self, event, fields);
    }
}

/// 舊版 `handleCommand` 內 `await` 失敗會丟出的值（舊版任何 Error 皆落入 poll
/// 迴圈的 update catch，記 `telegram_command_failed`；config 類先在
/// [`BotCore::run_config_command`] 化為回覆）。
enum BotFailure {
    /// api 呼叫失敗（舊版 `Error.name` 為 `TelegramApiError`）。
    Api(TelegramApiError),
    /// commandContext 的 reject（舊版 reject 一般 Error；`Error.name` 為 `Error`）。
    Command(BotCommandError),
    /// config 寫回失敗（舊版 `ConfigValidationError` 或其他 Error；
    /// [`ConfigWriteError::name`] 為舊版 `Error.name`）。
    Config(ConfigWriteError),
}

impl From<TelegramApiError> for BotFailure {
    fn from(error: TelegramApiError) -> Self {
        BotFailure::Api(error)
    }
}

impl From<BotCommandError> for BotFailure {
    fn from(error: BotCommandError) -> Self {
        BotFailure::Command(error)
    }
}

impl From<ConfigWriteError> for BotFailure {
    fn from(error: ConfigWriteError) -> Self {
        BotFailure::Config(error)
    }
}

impl BotFailure {
    /// 舊版 `describeTelegramError(error)` 的對應（`TelegramApiError` 實例 →
    /// 自身欄位；其餘 generic Error → `{errorClass: 'Error', httpStatus: null,
    /// errorCategory: 'unknown'}`）。
    fn describe(&self) -> TelegramErrorDescriptor {
        match self {
            BotFailure::Api(error) => error.describe(),
            // 舊版 generic Error：errorClass 'Error'、無 httpStatus、unknown。
            BotFailure::Command(_) | BotFailure::Config(_) => TelegramErrorDescriptor {
                error_class: "Error",
                http_status: None,
                error_category: TelegramErrorCategory::Unknown,
            },
        }
    }
}

/// 舊版舊 `306b4ba` 的查詢 operation 鏈錯誤：`signal.throwIfAborted()` 的
/// 中止（[`OperationFailure::Aborted`]）與 operation 內 api／context 的
/// reject（分別呈現舊版 `TelegramApiError`／generic `Error`）。
#[derive(Debug)]
enum OperationFailure {
    Aborted,
    Api(TelegramApiError),
    Command(BotCommandError),
}

impl From<TelegramApiError> for OperationFailure {
    fn from(error: TelegramApiError) -> Self {
        OperationFailure::Api(error)
    }
}

impl From<BotCommandError> for OperationFailure {
    fn from(error: BotCommandError) -> Self {
        OperationFailure::Command(error)
    }
}

impl From<BotFailure> for OperationFailure {
    fn from(error: BotFailure) -> Self {
        match error {
            BotFailure::Api(api) => OperationFailure::Api(api),
            BotFailure::Command(command) => OperationFailure::Command(command),
            // config 錯誤在 operation 內不會造成（runConfigCommand 前已化為
            // 回覆）；防衛性以訊息化為 generic（照舊版的 Error 通用分支）。
            BotFailure::Config(config) => OperationFailure::Command(BotCommandError {
                message: config.message(),
            }),
        }
    }
}

impl OperationFailure {
    /// 舊版 `describeTelegramError(error)` 的對應（同 [`BotFailure::describe`]；
    /// [`OperationFailure::Aborted`] 不應進入失敗描述，防衛性以 generic 呈現）。
    fn describe(&self) -> TelegramErrorDescriptor {
        match self {
            OperationFailure::Aborted => TelegramErrorDescriptor {
                error_class: "Error",
                http_status: None,
                error_category: TelegramErrorCategory::Unknown,
            },
            OperationFailure::Api(error) => error.describe(),
            OperationFailure::Command(_) => TelegramErrorDescriptor {
                error_class: "Error",
                http_status: None,
                error_category: TelegramErrorCategory::Unknown,
            },
        }
    }
}

/// 對應舊版 `AbortSignal.any([shutdownSignal, controller.signal])`：名額內
/// 查詢的「是否中止」由 shutdown 取消或 30 秒 deadline 到期任一成立決定；
/// 只用於流程輪詢（舊版 `signal.throwIfAborted()`／catch 的 `signal.aborted`
/// 檢查點），不進入 api 呼叫。
#[derive(Clone)]
pub(crate) struct QueryAbortSignal {
    shutdown: CancellationToken,
    deadline: CancellationToken,
}

impl QueryAbortSignal {
    fn new(shutdown: CancellationToken, deadline: CancellationToken) -> Self {
        Self { shutdown, deadline }
    }

    /// 舊版 `signal.aborted`。
    fn is_aborted(&self) -> bool {
        self.shutdown.is_cancelled() || self.deadline.is_cancelled()
    }

    /// 舊版 `signal.throwIfAborted()`：中止時丟出（由 startQuery 的第一段
    /// catch 以 `signal.aborted` 安靜處理）。
    fn ensure_active(&self) -> Result<(), OperationFailure> {
        if self.is_aborted() {
            Err(OperationFailure::Aborted)
        } else {
            Ok(())
        }
    }
}

/// 舊版 startQuery 的 operation 別名（/points／/screenshot 的既有函式）。
#[derive(Clone, Copy)]
enum QueryCommandKind {
    Points,
    Screenshot,
}

/// 接收 poll 迴圈的 `started` 狀態與停止控制（與 bot 主體共享）。
struct BotState {
    /// 舊版 `started`；`start` 置 true、`stop` 置 false（同時取走訊號與 flight）。
    started: bool,
    /// 舊版 `abortController.signal`（start 建立；stop 取消）。
    shutdown: Option<CancellationToken>,
    /// 舊版 `pollingFlight`：stop 等待其結束。
    polling_flight: Option<tokio::task::JoinHandle<()>>,
}

/// Bot 的共享核心（poll 迴圈與查詢 flight 任務持有 Arc；只有 [`BotState`] 有鎖）。
struct BotCore {
    api: Arc<dyn TelegramApi>,
    command_context: Arc<dyn BotCommandContext>,
    logger: Arc<dyn TelegramBotLogger>,
    sleep: Arc<dyn TelegramSleep>,
    /// 舊版 options.config（`channels`／`maxConcurrentStreams` 供啟動訊息、
    /// `telegram.polling_timeout_seconds` 供長輪詢）。
    config: AppConfig,
    /// 舊版 `allowedChatIds`（`Set` 語意：去重、保插入順序）。
    allowed_chat_ids: Vec<String>,
    /// 舊版 `this.offset`（poll 迴圈是唯一寫入者；std Mutex，不跨 await 持有）。
    offset: std::sync::Mutex<Option<f64>>,
    /// 舊版 `queryFlights`（只計數：>= MAX_ACTIVE_QUERIES 時回覆忙線）。
    active_queries: AtomicUsize,
    state: StateMutex<BotState>,
}

impl BotCore {
    /// 舊版 `start()`：`abortController` 建立 → `setMyCommands`（失敗只記
    /// warn）→ 廣播啟動訊息 → 外派 poll 迴圈。
    async fn begin_start(self: &Arc<Self>) {
        let shutdown = {
            let mut state = self.state.lock().await;
            if state.started {
                return;
            }
            state.started = true;
            let shutdown = CancellationToken::new();
            state.shutdown = Some(shutdown.clone());
            shutdown
        };

        // 舊版：setMyCommands 失敗只記 warn（欄位 ...describe + retryInMs: 0）。
        if let Err(error) = self.api.set_my_commands(&bot_commands()).await {
            self.logger.warn(
                "telegram_command_menu_failed",
                Some(&retry_ms_fields(&error.describe(), 0.0)),
            );
        }

        // 舊版：帶 COMMAND_KEYBOARD 的啟動訊息（先廣播、再外派 poll）。
        self.broadcast(
            &self.format_service_started(),
            Some(TelegramSendMessageOptions {
                reply_markup: Some(command_keyboard()),
            }),
        )
        .await;

        let core = Arc::clone(self);
        let flight = tokio::spawn(async move { run_poll(core, shutdown).await });
        self.state.lock().await.polling_flight = Some(flight);
    }

    /// 舊版 `stop(reason)` 本體；`started` 保護照舊版（重複呼叫無作用）。
    async fn stop(&self, reason: &str) {
        let (shutdown, flight) = {
            let mut state = self.state.lock().await;
            if !state.started {
                return;
            }
            state.started = false;
            (state.shutdown.take(), state.polling_flight.take())
        };
        // 舊版順序：abort 訊號 → await pollingFlight → 廣播停止訊息。
        if let Some(cancel) = shutdown {
            cancel.cancel();
        }
        if let Some(flight) = flight {
            let _ = flight.await;
        }
        self.broadcast(&format!("twiclaimer 已停止\n原因：{reason}"), None)
            .await;
    }

    /// 舊版 `notifyStreamStatus(change)`。
    async fn notify_stream_status(&self, change: &StreamStatusChange) {
        let text = if change.is_live {
            format!("{} 已開台", change.channel)
        } else {
            format!("{} 已離線", change.channel)
        };
        self.broadcast(&text, None).await;
    }

    /// 舊版 `notifyReward(result)`：`claimed`／`click_failed` 會通知，其他
    /// （`not_found` 等）照舊版直接返回。
    async fn notify_reward(&self, result: &RewardClaimResult) {
        let text = match result {
            RewardClaimResult::Claimed { channel, .. } => {
                format!("{channel} 已領取忠誠點數")
            }
            RewardClaimResult::ClickFailed { channel, .. } => {
                format!("{channel} 忠誠點數領取失敗")
            }
            RewardClaimResult::NotFound { .. } => return,
        };
        self.broadcast(&text, None).await;
    }

    /// 舊版 `notifyPageCrash(channel)`。
    async fn notify_page_crash(&self, channel: &str) {
        self.broadcast(&format_page_crash_message(channel), None)
            .await;
    }

    /// 舊版 `notifyBrowserRestart(event)`。
    async fn notify_browser_restart(&self, event: &BrowserRestartNotification) {
        self.broadcast(&format_browser_restart_message(event), None)
            .await;
    }

    /// 舊版 `notifyContainerRestart(event)`。
    async fn notify_container_restart(&self, event: &ContainerRestartNotification) {
        self.broadcast(&format_container_restart_message(event), None)
            .await;
    }

    /// 舊版 `broadcast`：逐 allowedChatId 送訊；送訊失敗記
    /// `telegram_send_failed`（欄位 ...describe、retryInMs 0），不會中斷迴圈。
    async fn broadcast(&self, text: &str, options: Option<TelegramSendMessageOptions>) {
        for chat_id in &self.allowed_chat_ids {
            if let Err(error) = self.api.send_message(chat_id, text, options.as_ref()).await {
                self.logger.warn(
                    "telegram_send_failed",
                    Some(&retry_ms_fields(&error.describe(), 0.0)),
                );
            }
        }
    }

    /// 舊版 `handleUpdate` 的承接：有 message 與 text 才處理（`message?.text
    /// === undefined` 合併），未授權 chat 照舊版記 `telegram_unauthorized_chat`
    /// 後返回，其餘交給 [`Self::handle_command`]。
    async fn handle_update(self: &Arc<Self>, update: &TelegramUpdate) -> Result<(), BotFailure> {
        let Some(message) = update.message.as_ref() else {
            return Ok(());
        };
        let Some(text) = message.text.as_ref() else {
            return Ok(());
        };

        let chat_id = message.chat.id.to_chat_id_string();
        if !self
            .allowed_chat_ids
            .iter()
            .any(|allowed| allowed == &chat_id)
        {
            // 舊版：無欄位（單一參數）。
            self.logger.warn("telegram_unauthorized_chat", None);
            return Ok(());
        }

        let Some(parsed) = parse_command(text) else {
            return Ok(());
        };
        self.handle_command(&chat_id, &parsed.command, parsed.argument.as_deref())
            .await
    }

    /// 對應舊版 `handleCommand`：每個分支的 await 失敗照舊版拋回 poll 迴圈
    /// （config 類由 [`BotCore::run_config_command`] 化為回覆）。
    async fn handle_command(
        self: &Arc<Self>,
        chat_id: &str,
        command: &str,
        argument: Option<&str>,
    ) -> Result<(), BotFailure> {
        match command {
            "start" | "help" => {
                self.send_with_markup(chat_id, &help_text(), Some(command_keyboard()))
                    .await?
            }
            "status" => self.reply(chat_id, &self.format_status()).await?,
            "channels" => self.reply(chat_id, &self.format_channels()).await?,
            "refresh" => {
                self.reply(chat_id, &self.format_refresh_countdown())
                    .await?
            }
            "refresh_now" => self.refresh_pages(chat_id, argument).await?,
            "points" => {
                self.start_query(chat_id, "points", QueryCommandKind::Points, argument)
                    .await?
            }
            "config" => self.reply(chat_id, &self.format_runtime_config()).await?,
            "channel_add" => {
                self.run_config_command(chat_id, self.add_channel(chat_id, argument))
                    .await?
            }
            "channel_remove" => {
                self.run_config_command(chat_id, self.remove_channel(chat_id, argument))
                    .await?
            }
            "channels_set" => {
                self.run_config_command(chat_id, self.set_channels_command(chat_id, argument))
                    .await?
            }
            "max_streams" => {
                self.run_config_command(chat_id, self.set_max_streams(chat_id, argument))
                    .await?
            }
            "check" => {
                self.command_context.run_check().await?;
                self.reply(chat_id, "已完成一次狀態檢查").await?;
            }
            "pause" => {
                self.command_context.pause_checks().await?;
                self.reply(chat_id, "自動檢查已暫停").await?;
            }
            "resume" => {
                // 舊版 resumeChecks() 是同步 void（不會 reject）。
                self.command_context.resume_checks();
                self.reply(chat_id, "自動檢查已恢復").await?;
            }
            "screenshot" => {
                self.start_query(
                    chat_id,
                    "screenshot",
                    QueryCommandKind::Screenshot,
                    argument,
                )
                .await?
            }
            _ => {
                self.reply(
                    chat_id,
                    &format!("不支援的指令：/{command}\n\n{}", help_text()),
                )
                .await?
            }
        }
        Ok(())
    }

    /// 舊版 `handleCommand` 內一般分支的 `sendMessage(chatId, text)`。
    async fn reply(&self, chat_id: &str, text: &str) -> Result<(), BotFailure> {
        self.api.send_message(chat_id, text, None).await?;
        Ok(())
    }

    /// 舊版 start／help 分支帶 `reply_markup` 的 sendMessage。
    async fn send_with_markup(
        &self,
        chat_id: &str,
        text: &str,
        markup: Option<TelegramReplyKeyboardMarkup>,
    ) -> Result<(), BotFailure> {
        self.api
            .send_message(
                chat_id,
                text,
                Some(&TelegramSendMessageOptions {
                    reply_markup: markup,
                }),
            )
            .await?;
        Ok(())
    }

    /// 對應舊版 `runConfigCommand`：config 類操作失敗時記
    /// `telegram_config_update_failed`（欄位 `errorType` 為舊版 `Error.name`）；
    /// 回覆為 `ConfigValidationError` 的訊息或
    /// `設定更新失敗，請確認 config.yml 可寫入。`。回覆自身的失敗照舊版
    /// 上拋（落入 poll 迴圈的 update catch）。
    async fn run_config_command(
        &self,
        chat_id: &str,
        operation: impl core::future::Future<Output = Result<(), BotFailure>>,
    ) -> Result<(), BotFailure> {
        if let Err(error) = operation.await {
            let error_name = match &error {
                BotFailure::Api(api_error) => api_error.name(),
                BotFailure::Command(_) => "Error",
                BotFailure::Config(config_error) => config_error.name(),
            };
            let message = match &error {
                BotFailure::Config(ConfigWriteError::Validation(validation)) => {
                    validation.message().to_string()
                }
                _ => "設定更新失敗，請確認 config.yml 可寫入".to_string(),
            };
            self.logger.warn(
                "telegram_config_update_failed",
                Some(&SanitizedValue::Object(vec![(
                    "errorType".to_string(),
                    SanitizedValue::String(error_name.to_string()),
                )])),
            );
            self.reply(chat_id, &message).await?;
        }
        Ok(())
    }

    /// 對應舊版 `startQuery(chatId, command, operation)`（`306b4ba`）：
    /// shutdown 已停或名額滿時先行返回／回覆忙線；30 秒計時器外派；
    /// 查詢 flight 外派（catch 鏈照舊版）。`self: &Arc<Self>` 供 flight
    /// 持有 Arc。
    async fn start_query(
        self: &Arc<Self>,
        chat_id: &str,
        command: &'static str,
        kind: QueryCommandKind,
        argument: Option<&str>,
    ) -> Result<(), BotFailure> {
        // 舊版 `this.abortController?.signal`（呼叫當下的最新 controller）。
        let shutdown = self.state.lock().await.shutdown.clone();
        let Some(shutdown) = shutdown else {
            return Ok(());
        };
        if shutdown.is_cancelled() {
            // 舊版 shutdownSignal.aborted
            return Ok(());
        }
        if self.active_queries.load(Ordering::Relaxed) >= MAX_ACTIVE_QUERIES {
            self.reply(
                chat_id,
                "查詢仍在處理中，請稍後再試；可用 /status 查看服務狀態",
            )
            .await?;
            return Ok(());
        }

        // 底層頁面工作結束前保留名額（照舊版註解：避免逾時後重試造成無限排隊）。
        self.active_queries.fetch_add(1, Ordering::Relaxed);

        // 舊版 30 秒計時器：`setTimeout(QUERY_TIMEOUT_MS)` 的 callback 由
        // finally 的 clearTimeout 與 shutdown 的 abort listener 取消——
        // 二者以 `shutdown` 的 child token 呈現（shutdown 取消或 finally
        // 都會解除計時器）。
        let deadline = shutdown.child_token();
        let timer = tokio::spawn({
            let core = Arc::clone(self);
            let chat_id = chat_id.to_string();
            let deadline = deadline.clone();
            async move { run_query_timer(&core, &chat_id, command, deadline).await }
        });

        // 舊版 flight：operation(signal).catch(...).catch(...).finally(...)。
        let signal = QueryAbortSignal::new(shutdown, deadline.clone());
        let flight_core = Arc::clone(self);
        let chat_id = chat_id.to_string();
        let argument = argument.map(str::to_string);
        tokio::spawn(async move {
            let operation_result = match kind {
                QueryCommandKind::Points => {
                    flight_core
                        .send_channel_points(&chat_id, argument.as_deref(), &signal)
                        .await
                }
                QueryCommandKind::Screenshot => {
                    flight_core
                        .send_screenshot(&chat_id, argument.as_deref(), &signal)
                        .await
                }
            };
            // 舊版 catch：signal.aborted → 安靜返回；否則記
            // telegram_query_failed（{command, ...describe}）後送失敗回覆；
            // 回覆失敗記 telegram_send_failed（{...describe, retryInMs: 0}）。
            // （log 欄位的 SanitizedValue 含非 Send 的節點，不跨 await，因此
            //  先記錄、區塊結束後才進入 send_message 的 await。）
            if let Err(failure) = operation_result
                && !signal.is_aborted()
            {
                {
                    let mut fields = vec![(
                        "command".to_string(),
                        SanitizedValue::String(command.to_string()),
                    )];
                    fields.extend(descriptor_fields(&failure.describe()));
                    flight_core.logger.warn(
                        "telegram_query_failed",
                        Some(&SanitizedValue::Object(fields)),
                    );
                }
                let reply = format!("/{command} 處理失敗，請稍後再試");
                if let Err(send_error) = flight_core.api.send_message(&chat_id, &reply, None).await
                {
                    flight_core.logger.warn(
                        "telegram_send_failed",
                        Some(&retry_ms_fields(&send_error.describe(), 0.0)),
                    );
                }
            }
            // finally：clearTimeout（deadline 的解除由 child token 的 cancel 短路
            // timer task）→ 跳脫 timer task → 釋放名額。
            deadline.cancel();
            drop(timer);
            flight_core.active_queries.fetch_sub(1, Ordering::Relaxed);
        });
        Ok(())
    }

    /// 對應舊版 `sendScreenshot`（/screenshot 的 operation；含 signal 檢查點）。
    async fn send_screenshot(
        &self,
        chat_id: &str,
        requested_channel: Option<&str>,
        signal: &QueryAbortSignal,
    ) -> Result<(), OperationFailure> {
        let Some(requested_channel) = requested_channel else {
            let active_channels = self.command_context.get_active_channels();
            if active_channels.is_empty() {
                self.reply(chat_id, "目前沒有正在觀看的頻道可供截圖")
                    .await?;
                return Ok(());
            }

            let mut sent_count = 0usize;
            for channel in &active_channels {
                signal.ensure_active()?;
                let screenshot = self.capture_screenshot_safely(channel).await;
                signal.ensure_active()?;
                let Some(screenshot) = screenshot else {
                    continue;
                };
                self.send_session_screenshot(chat_id, &screenshot).await?;
                sent_count += 1;
            }
            if sent_count == 0 {
                self.reply(chat_id, "目前沒有正在觀看的頻道可供截圖")
                    .await?;
            }
            return Ok(());
        };

        let screenshot = self.capture_screenshot_safely(requested_channel).await;
        signal.ensure_active()?;
        let Some(screenshot) = screenshot else {
            let active_channels = self.command_context.get_active_channels();
            let requested_is_active = active_channels
                .iter()
                .any(|channel| js_lower(channel) == js_lower(requested_channel));
            let text = if active_channels.is_empty() {
                "目前沒有正在觀看的頻道可供截圖".to_string()
            } else if requested_is_active {
                format!("頻道目前無法截圖：{requested_channel}")
            } else {
                [
                    format!("找不到正在觀看的頻道：{requested_channel}"),
                    format!("可用頻道：{}", format_list(&active_channels)),
                ]
                .join("\n")
            };
            self.reply(chat_id, &text).await?;
            return Ok(());
        };

        self.send_session_screenshot(chat_id, &screenshot).await
    }

    /// 對應舊版 `captureScreenshotSafely`：失敗記 `telegram_screenshot_failed`
    /// （欄位 channel、error 為消毒後的 safeErrorMessage）並回 [`None`] 使
    /// 迴圈繼續下一頻道。
    async fn capture_screenshot_safely(&self, channel: &str) -> Option<SessionScreenshot> {
        match self.command_context.capture_screenshot(Some(channel)).await {
            Ok(screenshot) => screenshot,
            Err(error) => {
                self.logger.warn(
                    "telegram_screenshot_failed",
                    Some(&SanitizedValue::Object(vec![
                        (
                            "channel".to_string(),
                            SanitizedValue::String(channel.to_string()),
                        ),
                        (
                            "error".to_string(),
                            SanitizedValue::String(safe_error_message(&error.message)),
                        ),
                    ])),
                );
                None
            }
        }
    }

    /// 對應舊版 `sendSessionScreenshot`。
    async fn send_session_screenshot(
        &self,
        chat_id: &str,
        screenshot: &SessionScreenshot,
    ) -> Result<(), OperationFailure> {
        self.api
            .send_photo(
                chat_id,
                &screenshot.image,
                &format!("{}.png", screenshot.channel),
                &format!("{} 目前瀏覽器畫面", screenshot.channel),
            )
            .await?;
        self.logger.info(
            "telegram_screenshot_sent",
            Some(&SanitizedValue::Object(vec![
                (
                    "channel".to_string(),
                    SanitizedValue::String(screenshot.channel.clone()),
                ),
                (
                    "bytes".to_string(),
                    SanitizedValue::Number(screenshot.image.len() as f64),
                ),
            ])),
        );
        Ok(())
    }

    /// 對應舊版 `refreshPages`（command 的 manage 分支）。
    async fn refresh_pages(
        &self,
        chat_id: &str,
        requested_channel: Option<&str>,
    ) -> Result<(), BotFailure> {
        let results = self
            .command_context
            .refresh_pages(requested_channel)
            .await?;
        if results.is_empty() {
            let active_channels = self.command_context.get_active_channels();
            let text = if active_channels.is_empty() {
                "目前沒有正在觀看的頻道可重整".to_string()
            } else {
                [
                    format!("找不到正在觀看的頻道：{}", requested_channel.unwrap_or("")),
                    format!("可用頻道：{}", format_list(&active_channels)),
                ]
                .join("\n")
            };
            self.reply(chat_id, &text).await?;
            return Ok(());
        }

        let mut lines = vec!["手動重整結果：".to_string()];
        lines.extend(results.iter().enumerate().map(|(index, result)| {
            let status = match result.status {
                SessionRefreshStatus::Refreshed => "已重整",
                SessionRefreshStatus::Unavailable => "目前無法重整",
                SessionRefreshStatus::Failed => "重整失敗",
            };
            format!("{}. {}：{}", index + 1, result.channel, status)
        }));
        self.reply(chat_id, &lines.join("\n")).await?;
        Ok(())
    }

    /// 對應舊版 `sendChannelPoints`（/points 的 operation；查詢完成後照舊版
    /// `signal.throwIfAborted()`）。
    async fn send_channel_points(
        &self,
        chat_id: &str,
        requested_channel: Option<&str>,
        signal: &QueryAbortSignal,
    ) -> Result<(), OperationFailure> {
        let results = self
            .command_context
            .get_channel_points(requested_channel)
            .await?;
        signal.ensure_active()?;
        if results.is_empty() {
            let active_channels = self.command_context.get_active_channels();
            let text = if active_channels.is_empty() {
                "目前沒有正在觀看的頻道可查詢忠誠點數".to_string()
            } else {
                [
                    format!("找不到正在觀看的頻道：{}", requested_channel.unwrap_or("")),
                    format!("可用頻道：{}", format_list(&active_channels)),
                ]
                .join("\n")
            };
            self.reply(chat_id, &text).await?;
            return Ok(());
        }

        let mut lines = vec!["忠誠點數：".to_string()];
        lines.extend(results.iter().map(|result| match result {
            SessionChannelPointsResult::Available {
                channel, balance, ..
            } => format!("{channel}：{} 點", format_balance(*balance)),
            SessionChannelPointsResult::Unavailable { channel, .. } => {
                format!("{channel}：無法取得")
            }
        }));
        self.reply(chat_id, &lines.join("\n")).await?;
        Ok(())
    }

    /// 對應舊版 `addChannel`（在 `run_config_command` 內執行）。
    async fn add_channel(&self, chat_id: &str, argument: Option<&str>) -> Result<(), BotFailure> {
        let Some(argument) = argument else {
            self.reply(chat_id, "用法：/channel_add 頻道名稱").await?;
            return Ok(());
        };
        let current = self.command_context.get_config();
        let normalized_argument = js_lower(argument);
        if current
            .channels
            .iter()
            .any(|channel| js_lower(channel) == normalized_argument)
        {
            self.reply(chat_id, &format!("頻道已在監控清單中：{argument}"))
                .await?;
            return Ok(());
        }
        let mut channels = current.channels.clone();
        channels.push(argument.to_string());
        let updated = self.command_context.set_channels(&channels).await?;
        self.reply(
            chat_id,
            &format!(
                "已加入頻道：{argument}\n{}",
                format_runtime_config(&updated)
            ),
        )
        .await
    }

    /// 對應舊版 `removeChannel`（在 `run_config_command` 內執行）。
    async fn remove_channel(
        &self,
        chat_id: &str,
        argument: Option<&str>,
    ) -> Result<(), BotFailure> {
        let Some(argument) = argument else {
            self.reply(chat_id, "用法：/channel_remove 頻道名稱")
                .await?;
            return Ok(());
        };
        let current = self.command_context.get_config();
        let normalized = js_lower(argument);
        let channels: Vec<String> = current
            .channels
            .iter()
            .filter(|channel| js_lower(channel) != normalized)
            .cloned()
            .collect();
        if channels.len() == current.channels.len() {
            self.reply(chat_id, &format!("找不到監控頻道：{argument}"))
                .await?;
            return Ok(());
        }
        let updated = self.command_context.set_channels(&channels).await?;
        self.reply(
            chat_id,
            &format!(
                "已移除頻道：{argument}\n{}",
                format_runtime_config(&updated)
            ),
        )
        .await
    }

    /// 對應舊版 `setChannels`（/channels_set；在 `run_config_command` 內執行）。
    async fn set_channels_command(
        &self,
        chat_id: &str,
        argument: Option<&str>,
    ) -> Result<(), BotFailure> {
        let channels = parse_channels_argument(argument);
        if channels.is_empty() {
            self.reply(chat_id, "用法：/channels_set 頻道一,頻道二")
                .await?;
            return Ok(());
        }
        let updated = self.command_context.set_channels(&channels).await?;
        self.reply(
            chat_id,
            &format!("頻道清單已更新\n{}", format_runtime_config(&updated)),
        )
        .await
    }

    /// 對應舊版 `setMaxStreams`（在 `run_config_command` 內執行）。
    async fn set_max_streams(
        &self,
        chat_id: &str,
        argument: Option<&str>,
    ) -> Result<(), BotFailure> {
        // 舊版：
        // `argument === undefined || !/^\d+$/u.test(argument)
        //  || !Number.isSafeInteger(Number(argument))`。
        let value = argument
            .filter(|argument| is_ascii_digits(argument))
            .and_then(|argument| argument.parse::<f64>().ok())
            .filter(|value| is_safe_integer(*value));
        let Some(value) = value else {
            self.reply(chat_id, "用法：/max_streams 正整數").await?;
            return Ok(());
        };
        let updated = self
            .command_context
            .set_max_concurrent_streams(value)
            .await?;
        self.reply(
            chat_id,
            &format!(
                "最大同時觀看已更新為 {}\n{}",
                js_number_to_string(value),
                format_runtime_config(&updated)
            ),
        )
        .await
    }

    /// 對應舊版 `formatServiceStarted`。
    fn format_service_started(&self) -> String {
        [
            "twiclaimer 已啟動".to_string(),
            format!("監控頻道：{}", self.config.channels.len()),
            format!(
                "最大同時觀看：{}",
                js_number_to_string(self.config.max_concurrent_streams)
            ),
        ]
        .join("\n")
    }

    /// 對應舊版 `formatStatus`。
    fn format_status(&self) -> String {
        let snapshot = self.command_context.get_scheduler_snapshot();
        let active_channels = self.command_context.get_active_channels();
        let live_channels: Vec<String> = snapshot
            .channels
            .iter()
            .filter(|channel| channel.is_live == Some(true))
            .map(|channel| channel.channel.clone())
            .collect();

        [
            format!(
                "自動檢查：{}",
                if snapshot.running {
                    "執行中"
                } else {
                    "已暫停"
                }
            ),
            format!(
                "檢查進行中：{}",
                if snapshot.check_in_flight {
                    "是"
                } else {
                    "否"
                }
            ),
            format!(
                "上次完成：{}",
                snapshot.last_checked_at.as_deref().unwrap_or("尚未完成")
            ),
            format!("限流重試：{}", snapshot.retry_at.as_deref().unwrap_or("無")),
            format!("目前開台：{}", format_list(&live_channels)),
            format!("正在觀看：{}", format_list(&active_channels)),
        ]
        .join("\n")
    }

    /// 對應舊版 `formatChannels`。
    fn format_channels(&self) -> String {
        let snapshot = self.command_context.get_scheduler_snapshot();
        snapshot
            .channels
            .iter()
            .enumerate()
            .map(|(index, status)| {
                let marker = if status.is_live == Some(true) {
                    "🔴"
                } else if status.is_live == Some(false) {
                    "⚫"
                } else {
                    "❔"
                };
                format!("{}. {} {}", index + 1, marker, status.channel)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 對應舊版 `formatRefreshCountdown`。
    fn format_refresh_countdown(&self) -> String {
        let statuses = self.command_context.get_refresh_statuses();
        if statuses.is_empty() {
            return "目前沒有正在觀看的頻道".to_string();
        }

        let mut lines = vec!["重整倒數：".to_string()];
        lines.extend(
            statuses
                .iter()
                .enumerate()
                .map(|(index, status)| {
                    if !status.enabled {
                        return format!("{}. {}：已關閉定時重整", index + 1, status.channel);
                    }
                    let (Some(next_refresh_at), Some(seconds_until_refresh)) =
                        (&status.next_refresh_at, &status.seconds_until_refresh)
                    else {
                        return format!("{}. {}：重整排程準備中", index + 1, status.channel);
                    };
                    format!(
                        "{}. {}：{}後（下次：{}）",
                        index + 1,
                        status.channel,
                        format_duration((*seconds_until_refresh) as f64),
                        next_refresh_at
                    )
                })
                .collect::<Vec<_>>(),
        );
        lines.join("\n")
    }

    /// 對應舊版 `formatRuntimeConfig`（方法版：讀 `getConfig`）。
    fn format_runtime_config(&self) -> String {
        format_runtime_config(&self.command_context.get_config())
    }
}

/// 對應舊版 `DefaultTelegramBot` 的公開門面（狀態與 poll 的邏輯在 core；
/// poll 迴圈與查詢 flight 任務持有 `Arc<BotCore>`）。
pub(crate) struct DefaultTelegramBot {
    core: Arc<BotCore>,
}

impl DefaultTelegramBot {
    /// 對應舊版建構子。
    pub(crate) fn new(options: DefaultTelegramBotOptions) -> Self {
        let allowed_chat_ids = dedupe_preserving_order(&options.config.telegram.allowed_chat_ids);
        let sleep = options
            .sleep
            .unwrap_or_else(|| Arc::new(TokioTelegramSleep));
        Self {
            core: Arc::new(BotCore {
                api: options.api,
                command_context: options.command_context,
                logger: options.logger,
                sleep,
                config: options.config.clone(),
                allowed_chat_ids,
                offset: std::sync::Mutex::new(None),
                active_queries: AtomicUsize::new(0),
                state: StateMutex::new(BotState {
                    started: false,
                    shutdown: None,
                    polling_flight: None,
                }),
            }),
        }
    }

    /// 測試接縫（對應舊測試 flushQueries 後的「名額內查詢已收尾」確認）：
    /// true 表示目前沒有名額內查詢（`queryFlights.size === 0` 的對應）。
    pub(crate) fn all_queries_settled(&self) -> bool {
        self.core.active_queries.load(Ordering::Relaxed) == 0
    }
}

#[async_trait]
impl TelegramBot for DefaultTelegramBot {
    async fn start(&self) {
        self.core.begin_start().await;
    }

    async fn stop(&self, reason: &str) {
        self.core.stop(reason).await;
    }

    async fn notify_stream_status(&self, change: &StreamStatusChange) {
        self.core.notify_stream_status(change).await;
    }

    async fn notify_reward(&self, result: &RewardClaimResult) {
        self.core.notify_reward(result).await;
    }

    async fn notify_page_crash(&self, channel: &str) {
        self.core.notify_page_crash(channel).await;
    }

    async fn notify_browser_restart(&self, event: &BrowserRestartNotification) {
        self.core.notify_browser_restart(event).await;
    }

    async fn notify_container_restart(&self, event: &ContainerRestartNotification) {
        self.core.notify_container_restart(event).await;
    }
}

/// 對應舊版 `DefaultTelegramBot.start` 在 `start()` 外派 `this.poll(signal)`
/// 的部分：長輪詢迴圈（唯一 offset 寫入者）。
///
/// 失敗記 `telegram_poll_failed`（欄位 ...describe、retryInMs＝
/// `min(30_000, 1_000 * 2^(failureCount − 1))`）後 `sleep(retryInMs, signal)`；
/// 訊號中止時照舊版直接結束（不記 log）。每個 update 的 `handleUpdate` 失敗
/// 照舊版記 `telegram_command_failed`（欄位 updateId、...describe、retryInMs: 0）。
async fn run_poll(core: Arc<BotCore>, cancel: CancellationToken) {
    let mut failure_count: u32 = 0;
    while !cancel.is_cancelled() {
        let timeout_seconds = core.config.telegram.polling_timeout_seconds;
        let offset = *core.offset.lock().expect("offset lock");
        match core.api.get_updates(offset, timeout_seconds, &cancel).await {
            Ok(updates) => {
                failure_count = 0;
                for update in &updates {
                    // 舊版：this.offset = Math.max(this.offset ?? 0, update.update_id + 1)
                    // （在 handleUpdate 之前完成）。
                    let next = {
                        let guard = core.offset.lock().expect("offset lock");
                        guard.unwrap_or(0.0).max(update.update_id + 1.0)
                    };
                    *core.offset.lock().expect("offset lock") = Some(next);
                    if let Err(error) = core.handle_update(update).await {
                        let mut fields = vec![(
                            "updateId".to_string(),
                            SanitizedValue::Number(update.update_id),
                        )];
                        fields.extend(descriptor_fields(&error.describe()));
                        fields.push(("retryInMs".to_string(), SanitizedValue::Number(0.0)));
                        core.logger.warn(
                            "telegram_command_failed",
                            Some(&SanitizedValue::Object(fields)),
                        );
                    }
                }
            }
            Err(error) => {
                if cancel.is_cancelled() {
                    return;
                }
                failure_count += 1;
                let retry_in_ms =
                    (RETRY_BASE_MS * 2f64.powi(failure_count as i32 - 1)).min(MAX_RETRY_MS);
                core.logger.warn(
                    "telegram_poll_failed",
                    Some(&retry_ms_fields(&error.describe(), retry_in_ms)),
                );
                core.sleep.sleep(retry_in_ms, &cancel).await;
            }
        }
    }
}

/// 對應舊版 `startQuery` 的 `setTimeout(QUERY_TIMEOUT_MS)` callback（`306b4ba`）：
/// deadline 到期 → abort controller（`deadline.cancel()`，使名額內查詢照
/// `signal.throwIfAborted()` 停止）→ 記 `telegram_query_timeout` → 送逾時
/// 回覆（不等待，失敗記 `telegram_send_failed`）。
async fn run_query_timer(
    core: &Arc<BotCore>,
    chat_id: &str,
    command: &'static str,
    deadline: CancellationToken,
) {
    tokio::select! {
        () = tokio::time::sleep(QUERY_TIMEOUT_MS) => {
            deadline.cancel();
            core.logger
                .warn("telegram_query_timeout", Some(&SanitizedValue::Object(vec![(
                    "command".to_string(),
                    SanitizedValue::String(command.to_string()),
                )])));
            let reply = format!("/{command} 處理逾時，請稍後再試；可用 /status 查看服務狀態");
            if let Err(error) = core.api.send_message(chat_id, &reply, None).await {
                core.logger
                    .warn("telegram_send_failed", Some(&retry_ms_fields(&error.describe(), 0.0)));
            }
        }
        () = deadline.cancelled() => {}
    }
}

/// 舊版 `{...describe(error), retryInMs}`（spread 順序：errorClass、
/// httpStatus、errorCategory，其後 retryInMs）。
fn retry_ms_fields(descriptor: &TelegramErrorDescriptor, retry_in_ms: f64) -> SanitizedValue {
    let mut fields = descriptor_fields(descriptor);
    fields.push(("retryInMs".to_string(), SanitizedValue::Number(retry_in_ms)));
    SanitizedValue::Object(fields)
}

/// 舊版 `describeTelegramError` 的欄位（物件字面順序）。
fn descriptor_fields(descriptor: &TelegramErrorDescriptor) -> Vec<(String, SanitizedValue)> {
    vec![
        (
            "errorClass".to_string(),
            SanitizedValue::String(descriptor.error_class.to_string()),
        ),
        (
            "httpStatus".to_string(),
            match descriptor.http_status {
                // 舊版描述物件把缺值放 `null`（JSON.stringify 保留 null）。
                None => SanitizedValue::Null,
                Some(status) => SanitizedValue::Number(status as f64),
            },
        ),
        (
            "errorCategory".to_string(),
            SanitizedValue::String(descriptor.error_category.as_str().to_string()),
        ),
    ]
}

/// 供 log 與紅線消毒的舊版 `safeErrorMessage`：訊息照 `Error.message`、
/// 後再 `redactSensitiveString`（context 與 api 的錯誤在此都僅有訊息欄位）。
fn safe_error_message(message: &str) -> String {
    redact_sensitive_string(message)
}

/// 對應舊版 `parseCommand(text)` 的結果。
struct ParsedCommand {
    /// 舊版 `firstToken.slice(1).split('@', 1)[0].toLocaleLowerCase('en-US')`。
    command: String,
    /// 舊版 `trimmed.slice(separatorIndex).trim()`；空白或無分隔時為
    /// [`None`]（照舊版屬性不存在），非空字串仍為 [`Some`]。
    argument: Option<String>,
}

/// 對應舊版 `parseCommand`：第一個非空白 token 以 `/` 開頭才算指令；
/// `toLocaleLowerCase('en-US')` 以 Rust `to_lowercase` 呈現（DEC-019 的
/// 同類取捨）。
fn parse_command(text: &str) -> Option<ParsedCommand> {
    let trimmed = js_trim(text);
    // 舊版 `trimmed.search(/\s/u)`：第一個 JS 空白字元的索引（以字元迭代對應
    // UTF-16 搜尋；分隔點是單一字元邊界，切片等價）。
    let separator = trimmed
        .char_indices()
        .find(|(_, character)| is_js_whitespace(*character))
        .map(|(index, _)| index);
    let (first_token, argument) = match separator {
        None => (trimmed.to_string(), None),
        Some(index) => {
            let rest = js_trim(&trimmed[index..]);
            (
                trimmed[..index].to_string(),
                if rest.is_empty() {
                    None
                } else {
                    Some(rest.to_string())
                },
            )
        }
    };
    if !first_token.starts_with('/') {
        // 舊版：firstToken 非空字串且以 '/' 開頭才有效（空白文字亦為 None）。
        return None;
    }
    let command = first_token[1..]
        .split('@')
        .next()
        .unwrap_or("")
        .to_lowercase();
    if command.is_empty() {
        return None;
    }
    Some(ParsedCommand { command, argument })
}

/// 對應舊版 `formatList`。
fn format_list<T: AsRef<str>>(values: &[T]) -> String {
    let strings: Vec<&str> = values.iter().map(|value| value.as_ref()).collect();
    if strings.is_empty() {
        return "無".to_string();
    }
    strings.join("、")
}

/// 對應舊版 `formatDuration`（不會有小時粒度；負值照舊版 clamp 到 0）。
fn format_duration(total_seconds: f64) -> String {
    let seconds = total_seconds.clamp(0.0, f64::INFINITY).floor();
    let seconds = if seconds.is_finite() { seconds } else { 0.0 } as u64;
    let minutes = seconds / 60;
    let remaining_seconds = seconds % 60;
    if minutes == 0 {
        return format!("{remaining_seconds} 秒");
    }
    if remaining_seconds == 0 {
        return format!("{minutes} 分鐘");
    }
    format!("{minutes} 分 {remaining_seconds} 秒")
}

/// 對應舊版 `balance.toLocaleString('en-US')`：以 JS `Number#toString` 的
/// 短表示為基礎，整數部分加千分位逗號（請參見 discord `format_balance` 的
/// 同款註解：≥1e21 照 toString 指數形，點數不會到該量級）。
fn format_balance(balance: f64) -> String {
    let number_string = js_number_to_string(balance);
    let (sign, rest) = match number_string.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", number_string.as_str()),
    };
    // toLocaleString('en-US') 的表現至 1e21 皆為定點（無指數）；到此處的值
    // 若為指數形（MANTISSA e+NN）直接照 toString 呈現。
    if rest.contains('e') {
        return number_string;
    }
    let (integer, fractional) = match rest.split_once('.') {
        Some((integer, fractional)) => (integer, Some(fractional)),
        None => (rest, None),
    };
    let mut grouped = String::with_capacity(integer.len() + integer.len() / 3);
    for (index, character) in integer.chars().enumerate() {
        if index > 0 && (integer.len() - index) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(character);
    }
    match fractional {
        Some(fractional) => format!("{sign}{grouped}.{fractional}"),
        None => format!("{sign}{grouped}"),
    }
}

/// 對應舊版 `parseChannelsArgument`：`undefined` 回空清單；字串以
/// `/[\s,]+/u` 切開並過濾空白片段。
fn parse_channels_argument(argument: Option<&str>) -> Vec<String> {
    let Some(argument) = argument else {
        return Vec::new();
    };
    let mut channels = Vec::new();
    let mut current = String::new();
    for character in argument.chars() {
        if character == ',' || is_js_whitespace(character) {
            if !current.is_empty() {
                channels.push(std::mem::take(&mut current));
            }
        } else {
            current.push(character);
        }
    }
    if !current.is_empty() {
        channels.push(current);
    }
    channels
}

/// 對應舊版 `formatRuntimeConfig`（自由函式版；欄位照舊版）。
fn format_runtime_config(config: &RuntimeWatchConfig) -> String {
    [
        format!("監控頻道：{}", config.channels.join("、")),
        format!(
            "最大同時觀看：{}",
            js_number_to_string(config.max_concurrent_streams)
        ),
    ]
    .join("\n")
}

/// 舊版 `toLocaleLowerCase('en-US')` 的對應（DEC-009／DEC-019 的同類取捨）。
fn js_lower(value: &str) -> String {
    value.to_lowercase()
}

/// 舊版 `Number.MAX_SAFE_INTEGER`（2^53 − 1）。
const MAX_SAFE_INTEGER_F64: f64 = 9_007_199_254_740_991.0;

/// 舊版 `Number.isSafeInteger` 的對應：有限、整數、絕對值 ≤ 2^53 − 1。
fn is_safe_integer(value: f64) -> bool {
    value.is_finite() && value == value.trunc() && value.abs() <= MAX_SAFE_INTEGER_F64
}

/// 舊版 `/^\d+$/u` 的判定：非空且全是 ASCII 數字。
fn is_ascii_digits(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}

/// 舊版 `String.prototype.trim`／`/\s/u` 會移除／比對的字元：JS 的 WhiteSpace
/// 加行終止符（U+0085 不移除、U+FEFF 移除，與 Rust `str::trim` 不同；
/// DEC-013 的同類處理——各模組自帶一份，統一時合併）。
fn is_js_whitespace(character: char) -> bool {
    matches!(
        character,
        '\t' | '\n' | '\u{0b}' | '\u{0c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
    )
}

/// 對應舊版 `value.trim()`（去頭尾的 JS 空白）。
fn js_trim(value: &str) -> &str {
    value.trim_matches(is_js_whitespace)
}

/// 去重串列，保留首次出現的順序（對應舊版 `new Set(values)` 的迭代順序）。
fn dedupe_preserving_order(values: &[String]) -> Vec<String> {
    let mut result: Vec<String> = Vec::with_capacity(values.len());
    for value in values {
        if !result.iter().any(|entry| entry == value) {
            result.push(value.clone());
        }
    }
    result
}

#[cfg(test)]
mod tests;
