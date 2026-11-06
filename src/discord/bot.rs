//! Discord Bot：`discord/DiscordBot.ts` 逐函式移植（§4.2／§7.8，S4.3）。
//!
//! 指令、授權檢查、回覆與通知文字逐字照舊版（含全形標點與換行）；點數以
//! `toLocaleString('en-US')` 的千分位逗號（[`format_balance`]）。與舊版的
//! 結構差異（由 §7.8 指定，細節見各段）：
//!
//! - Gateway 以 twilight `Shard` 取代手寫 websocket（舊版 `DiscordGatewaySocket`
//!   ＋`DiscordBotTimer`＋sequence／heartbeat 權杖）：identify 設定在
//!   [`build_shard_config`]、HELLO／heartbeat／可恢復斷線的重連與 resume（X4）
//!   由 twilight 內部處理；不開 cache（未含 cache crate）。
//! - [`Gateway`] trait 包住 shard，正式實作 [`TwilightShardGateway`] 以
//!   `StreamExt::next_event(EventTypeFlags::INTERACTION_CREATE)` 把 gateway
//!   訊息窄化成 interaction／close／錯誤；重大關閉（如 token 無效）以串流
//!   結束呈現，接收迴圈記 `discord_gateway_error` 後結束（不影響常駐服務）。
//!   舊測試對「identify／heartbeat op 框」的斷言改以 [`build_shard_config`]
//!   的設定值驗證（twilight 內部行為不可經 fake 觀察）。
//! - `stop` 先（經接收迴圈）送 close code 1000（理由照舊版寫進 close frame），
//!   再廣播 `twiclaimer 已停止\n原因：<reason>`。
#![allow(dead_code)]

use std::borrow::Cow;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use twilight_gateway::{
    CloseFrame, Config, EventTypeFlags, Intents, Shard, ShardId, StreamExt as _,
};
use twilight_model::application::interaction::application_command::CommandOptionValue;
use twilight_model::application::interaction::{Interaction, InteractionData, InteractionType};
use twilight_model::gateway::event::Event;
use twilight_model::gateway::payload::outgoing::identify::IdentifyProperties;

use crate::browser::reward_claimer::RewardClaimResult;
use crate::config::model::{AppConfig, DiscordConfig};
use crate::config::writeback::{ConfigWriteError, RuntimeWatchConfig};
use crate::discord::api::{
    DiscordApi, DiscordApiError, DiscordApplicationCommand, DiscordApplicationCommandOption,
};
use crate::logging::redact::{SanitizedValue, js_number_to_string};
use crate::notifications::bot_command_context::{
    BotCommandContext, BotCommandError, SessionChannelPointsResult, SessionRefreshResult,
    SessionRefreshStatus, SessionScreenshot, StreamStatusChange,
};
use crate::notifications::bot_notifications::{
    BrowserRestartNotification, ContainerRestartNotification, format_browser_restart_message,
    format_container_restart_message, format_page_crash_message,
};

/// 舊版 `DiscordBot.start` 的 `getGatewayBot(AbortSignal.timeout(10_000))` 逾時。
const GATEWAY_BOT_TIMEOUT: Duration = Duration::from_millis(10_000);
/// 對應舊版 `DISCORD_APPLICATION_COMMAND_INTERACTION`（`type: 2`）。
const DISCORD_APPLICATION_COMMAND_INTERACTION: InteractionType =
    InteractionType::ApplicationCommand;

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

/// 對應舊版 `BOT_COMMANDS`（`registerCommands` 的內容；option `type` 照舊版
/// 3＝string、4＝integer）。
fn bot_commands() -> Vec<DiscordApplicationCommand> {
    let channel_option = |required: bool| DiscordApplicationCommandOption {
        kind: 3,
        name: "channel".to_string(),
        description: "Twitch 頻道名稱".to_string(),
        required: required.then_some(true),
    };
    vec![
        DiscordApplicationCommand::plain("status", "顯示服務與觀看狀態"),
        DiscordApplicationCommand::plain("channels", "顯示監控頻道"),
        DiscordApplicationCommand::plain("refresh", "顯示播放器重整倒數"),
        DiscordApplicationCommand {
            name: "refresh_now".to_string(),
            description: "立即重整觀看頁".to_string(),
            options: Some(vec![channel_option(false)]),
        },
        DiscordApplicationCommand {
            name: "points".to_string(),
            description: "顯示忠誠點數".to_string(),
            options: Some(vec![channel_option(false)]),
        },
        DiscordApplicationCommand::plain("config", "顯示頻道與同時觀看設定"),
        DiscordApplicationCommand {
            name: "channel_add".to_string(),
            description: "新增監控頻道".to_string(),
            options: Some(vec![channel_option(true)]),
        },
        DiscordApplicationCommand {
            name: "channel_remove".to_string(),
            description: "移除監控頻道".to_string(),
            options: Some(vec![channel_option(true)]),
        },
        DiscordApplicationCommand {
            name: "channels_set".to_string(),
            description: "取代監控頻道清單".to_string(),
            options: Some(vec![DiscordApplicationCommandOption {
                kind: 3,
                name: "channels".to_string(),
                description: "以逗號或空白分隔的 Twitch 頻道名稱".to_string(),
                required: Some(true),
            }]),
        },
        DiscordApplicationCommand {
            name: "max_streams".to_string(),
            description: "設定最大同時觀看".to_string(),
            options: Some(vec![DiscordApplicationCommandOption {
                kind: 4,
                name: "count".to_string(),
                description: "最大同時觀看數量".to_string(),
                required: Some(true),
            }]),
        },
        DiscordApplicationCommand::plain("check", "立即檢查一次"),
        DiscordApplicationCommand::plain("pause", "暫停自動檢查"),
        DiscordApplicationCommand::plain("resume", "恢復自動檢查"),
        DiscordApplicationCommand {
            name: "screenshot".to_string(),
            description: "擷取目前觀看畫面".to_string(),
            options: Some(vec![channel_option(false)]),
        },
        DiscordApplicationCommand::plain("help", "顯示指令說明"),
    ]
}

/// 對應舊版 `DiscordGatewaySocket` 的介面（§7.8：以 trait 包住 twilight Shard；
/// 單元測試用 fake shard 送 interaction，不連真實 Discord）。
#[async_trait]
pub(crate) trait Gateway: Send {
    /// 接收下一則 gateway 事件。twilight 內部處理 hello／heartbeat 與可恢復
    /// 斷線，此層只會見到 [`GatewayItem`] 與串流結束（重大關閉，如 token 無效）。
    async fn next(&mut self) -> Option<GatewayItem>;

    /// 對應舊版 `socket?.close(1000, reason)`：以 close code 1000 關閉，
    /// 理由寫進 close frame。
    fn close(&self, reason: &str);
}

/// 對應舊版 `DiscordGatewayEvent`／舊版 bot 會處理的 gateway 事件。
#[derive(Debug)]
pub(crate) enum GatewayItem {
    /// 被處理的 dispatch：INTERACTION_CREATE。
    Interaction(Box<Interaction>),
    /// 接收層錯誤（舊版 `error` 事件；可恢復的斷線已由 twilight 自行重連）
    TransportIssue,
    /// 連線的 close（舊版 `close` 事件；含 close code，可為 [`None`]）。
    Closed(Option<u16>),
}

/// 建立正式的 gateway 連線（舊版 `createWebSocket`；連線 URL 用 twilight
/// 的預設值，§7.8）。
pub(crate) fn create_shard_gateway(bot_token: &str) -> TwilightShardGateway {
    TwilightShardGateway {
        shard: Shard::with_config(ShardId::ONE, build_shard_config(bot_token)),
    }
}

/// 建立 twilight shard 的識別設定（§7.8：intents `Intents::empty()`
/// （舊版 `intents: 0`）、identify properties 為 `twitch-watchdog`／
/// `twitch-watchdog`／`std::env::consts::OS`（同 Node 的 `process.platform`，
/// Linux 都是 `linux`））。
pub(crate) fn build_shard_config(bot_token: &str) -> Config {
    twilight_gateway::ConfigBuilder::new(bot_token.to_string(), Intents::empty())
        .identify_properties(IdentifyProperties::new(
            // 舊版 identify payload 的順序：os: process.platform、
            // browser: 'twitch-watchdog'、device: 'twitch-watchdog'。
            "twitch-watchdog",
            "twitch-watchdog",
            std::env::consts::OS,
        ))
        .build()
}

/// 正式的 gateway 實作：以 twilight `Shard` 提供，事件窄化為需要的型別。
pub(crate) struct TwilightShardGateway {
    shard: Shard,
}

#[async_trait]
impl Gateway for TwilightShardGateway {
    async fn next(&mut self) -> Option<GatewayItem> {
        loop {
            match self
                .shard
                .next_event(EventTypeFlags::INTERACTION_CREATE)
                .await
            {
                Some(Ok(Event::InteractionCreate(payload))) => {
                    return Some(GatewayItem::Interaction(Box::new(payload.0)));
                }
                // close 訊息永遠會被 next_event 呈現（twilight StreamExt 的行為）。
                Some(Ok(Event::GatewayClose(frame))) => {
                    return Some(GatewayItem::Closed(frame.map(|frame| frame.code)));
                }
                Some(Err(_)) => return Some(GatewayItem::TransportIssue),
                // EventTypeFlags 只挑 INTERACTION_CREATE；其他事件（理論上不會
                // 出現）防衛性略過。
                Some(_) => continue,
                None => return None,
            }
        }
    }

    fn close(&self, reason: &str) {
        // 舊版 `this.socket?.close(1000, reason)`；twilight close 的字串需
        // 'static，以 owned Cow 帶入（reason 為短字串，只在 stop 呼叫一次）。
        self.shard.close(CloseFrame {
            code: 1000,
            reason: Cow::Owned(reason.to_string()),
        });
    }
}

/// 對應舊版 `DiscordBotOptions`：config、api、commandContext、logger、
/// gatewayFactory。舊版 `timer`（heartbeat 計時注入）在 twilight 架構下移除。
pub(crate) struct DefaultDiscordBotOptions {
    pub(crate) config: AppConfig,
    pub(crate) api: Arc<dyn DiscordApi>,
    pub(crate) command_context: Arc<dyn BotCommandContext>,
    pub(crate) logger: Arc<dyn DiscordBotLogger>,
    /// 對應舊版 `gatewayFactory`；`None` 用 [`create_shard_gateway`]。
    pub(crate) gateway_factory: Option<GatewayFactory>,
}

/// 對應舊版 `DiscordGatewayFactory`。舊版參數是 gateway URL；本移植連線 URL
/// 用 twilight 的預設值（§7.8），工廠無參數。
pub(crate) type GatewayFactory = Arc<dyn Fn() -> Box<dyn Gateway> + Send + Sync>;

/// 對應舊版 `DiscordBot` 介面。
#[async_trait]
pub(crate) trait DiscordBot: Send + Sync {
    /// 舊版 `start()`：registerCommands（失敗只記 warn）→ getGatewayBot
    /// （10 秒；失敗連帶 start 失敗，與舊版相同）→ 建立 twilight Shard →
    /// 廣播啟動訊息。
    async fn start(&self) -> Result<(), DiscordApiError>;

    /// 舊版 `stop(reason)`：先關閉 gateway（close code 1000）再廣播
    /// `twiclaimer 已停止\n原因：<reason>`；重複呼叫無作用。
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

/// 舊版 `handleCommand` 內 `await` 失敗會丟出的值（舊版任何 Error 皆落入
/// message listener 的 catch，記 `discord_gateway_message_failed`；config 類
/// 先在 [`BotCore::run_config_command`] 化為回覆）。
enum BotFailure {
    /// api 呼叫失敗（舊版 `Error.name` 為 `DiscordApiError`）。
    Api(DiscordApiError),
    /// commandContext 的 reject（舊版 reject 一般 Error；`Error.name` 為
    /// `Error`）。
    Command(BotCommandError),
    /// config 寫回失敗（舊版 `ConfigValidationError` 或其他 Error；
    /// [`ConfigWriteError::name`] 為舊版 `Error.name`）。
    Config(ConfigWriteError),
}

impl From<DiscordApiError> for BotFailure {
    fn from(error: DiscordApiError) -> Self {
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

/// 舊版 `authorizeInteraction` 的拒絕結果。log 欄位名與值以單一欄位呈現
/// （舊版欄位物件 `{ <field>: value }` 的形狀；值可以是 undefined → 省略）。
/// [`SanitizedValue`] 含非 Send 的共用節點，不跨 await，由呼叫端組建。
struct InteractionDenial {
    event: &'static str,
    /// 舊版欄位名（`userId`／`channelId`）。
    field_name: &'static str,
    /// 舊版欄位值（`undefined` → [`None`]）。
    field_value: Option<String>,
    message: String,
}

/// 對應舊版 `Logger` 介面（bot 用到的 info／warn；欄位值為消毒前的
/// [`SanitizedValue`]，消毒在 Logger 本體進行）。
pub(crate) trait DiscordBotLogger: Send + Sync {
    fn info(&self, event: &str, fields: Option<&SanitizedValue>);
    fn warn(&self, event: &str, fields: Option<&SanitizedValue>);
}

impl DiscordBotLogger for crate::logging::logger::JsonLineLogger {
    fn info(&self, event: &str, fields: Option<&SanitizedValue>) {
        // inherent 的 info 同簽名；以完整路徑呼叫避免誤呼本 impl 造成遞迴。
        crate::logging::logger::JsonLineLogger::info(self, event, fields);
    }

    fn warn(&self, event: &str, fields: Option<&SanitizedValue>) {
        crate::logging::logger::JsonLineLogger::warn(self, event, fields);
    }
}

/// 接收迴圈的 `started` 狀態與停止旗標（與 bot 主體共享）。
struct BotState {
    /// 舊版 `started`；`start` 在 getGatewayBot 之前置 true（失敗不復歸，照
    /// 舊版其後的 stop 仍會廣播停止訊息）；`stop` 置 false。
    started: bool,
    /// 接收迴圈的 close 要求；`Some(reason)` 由 `stop()` 置入，reason 給
    /// close frame。
    stop_tx: tokio::sync::watch::Sender<Option<String>>,
}

/// Bot 的共享核心（接收迴圈任務持有 Arc；只有 [`BotState`] 有鎖）。
struct BotCore {
    api: Arc<dyn DiscordApi>,
    command_context: Arc<dyn BotCommandContext>,
    logger: Arc<dyn DiscordBotLogger>,
    gateway_factory: GatewayFactory,
    /// 舊版 options.config（`channels`／`maxConcurrentStreams` 供啟動訊息）。
    config: AppConfig,
    /// 舊版 `allowedChannelIds`（`Set` 語意：去重、保插入順序）。
    allowed_channel_ids: Vec<String>,
    /// 舊版 `allowedUserIds`（`Set` 語意）。
    allowed_user_ids: Vec<String>,
    discord: DiscordConfig,
    state: tokio::sync::Mutex<BotState>,
}

impl BotCore {
    /// 舊版 `start()` 的前半（registerCommands 失敗只記 warn；getGatewayBot
    /// 失敗連帶啟動失敗）。回傳的 `DiscordApiError` 來自 `get_gateway_bot`。
    async fn begin(&self) -> Result<(), DiscordApiError> {
        {
            let mut state = self.state.lock().await;
            if state.started {
                return Ok(());
            }
            state.started = true;
        }

        // 舊版：registerCommands 失敗只記 warn（欄位照舊版：無）。
        if self
            .api
            .register_commands(
                &self.discord.application_id,
                &self.discord.guild_id,
                &bot_commands(),
            )
            .await
            .is_err()
        {
            self.logger
                .warn("discord_command_registration_failed", None);
        }

        // 舊版：getGatewayBot(AbortSignal.timeout(10_000))；失敗連帶 start 失敗。
        self.api.get_gateway_bot(GATEWAY_BOT_TIMEOUT).await?;
        Ok(())
    }

    /// 舊版 `start()` 的後半：建 gateway（twilight shard）並外派接收迴圈，
    /// 廣播啟動訊息。需要 `Arc<BotCore>` 供接收迴圈持有。
    async fn finish_start(self: &Arc<Self>) {
        let stop_rx = self.state.lock().await.stop_tx.subscribe();
        let core = Arc::clone(self);
        tokio::spawn(run_receive_loop(core, stop_rx));

        // 舊版最後一步：廣播啟動訊息。
        self.broadcast(&self.format_service_started()).await;
    }

    /// 舊版 `stop(reason)` 的本體。`started` 保護照舊版（重複呼叫無作用）。
    async fn stop(&self, reason: &str) {
        let should_close = {
            let mut state = self.state.lock().await;
            if !state.started {
                false
            } else {
                state.started = false;
                // 舊版 socket?.close(1000, reason)：由接收迴圈（唯一 ownership）
                // 以 stop 旗標遞交；理由照舊版寫進 close frame。
                state.stop_tx.send_replace(Some(reason.to_string()));
                true
            }
        };
        if !should_close {
            return;
        }
        self.broadcast(&format!("twiclaimer 已停止\n原因：{reason}"))
            .await;
    }

    /// 舊版 `notifyStreamStatus(change)`。
    async fn notify_stream_status(&self, change: &StreamStatusChange) {
        let text = if change.is_live {
            format!("{} 已開台", change.channel)
        } else {
            format!("{} 已離線", change.channel)
        };
        self.broadcast(&text).await;
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
        self.broadcast(&text).await;
    }

    /// 舊版 `notifyPageCrash(channel)`。
    async fn notify_page_crash(&self, channel: &str) {
        self.broadcast(&format_page_crash_message(channel)).await;
    }

    /// 舊版 `notifyBrowserRestart(event)`。
    async fn notify_browser_restart(&self, event: &BrowserRestartNotification) {
        self.broadcast(&format_browser_restart_message(event)).await;
    }

    /// 舊版 `notifyContainerRestart(event)`。
    async fn notify_container_restart(&self, event: &ContainerRestartNotification) {
        self.broadcast(&format_container_restart_message(event))
            .await;
    }

    /// 舊版 `broadcast`：逐 allowedChannelId 送訊；送訊失敗記
    /// `discord_send_failed`（欄位 `channelId`），不會中斷迴圈。
    async fn broadcast(&self, text: &str) {
        for channel_id in &self.allowed_channel_ids {
            if self.api.send_message(channel_id, text).await.is_err() {
                self.logger.warn(
                    "discord_send_failed",
                    Some(&SanitizedValue::Object(vec![(
                        "channelId".to_string(),
                        SanitizedValue::String(channel_id.clone()),
                    )])),
                );
            }
        }
    }

    /// 舊版 `handleGatewayMessage` 分支的承接：處理一則 interaction。
    async fn handle_interaction(&self, interaction: &Interaction) -> Result<(), BotFailure> {
        if interaction.kind != DISCORD_APPLICATION_COMMAND_INTERACTION {
            return Ok(());
        }
        if let Err(denial) = self.authorize_interaction(interaction) {
            self.logger.warn(
                denial.event,
                Some(&id_field(denial.field_name, denial.field_value)),
            );
            self.reply(interaction, &denial.message).await?;
            return Ok(());
        }

        let Some(parsed) = parse_interaction_command(interaction) else {
            return Ok(());
        };
        self.handle_command(
            interaction,
            parsed.command.as_str(),
            parsed.argument.as_deref(),
        )
        .await?;
        Ok(())
    }

    /// 對應舊版 `authorizeInteraction`。
    fn authorize_interaction(&self, interaction: &Interaction) -> Result<(), InteractionDenial> {
        // 舊版 `interaction.guild_id === undefined` → 直接訊息（DM）路徑；
        // userId 取 `interaction.user?.id ?? interaction.member?.user?.id`。
        if interaction.guild_id.is_none() {
            let user_id = dm_user_id_of(interaction);
            let allowed = user_id
                .as_ref()
                .is_some_and(|id| self.allowed_user_ids.iter().any(|entry| entry == id));
            if self.discord.allow_direct_messages && allowed {
                return Ok(());
            }
            return Err(InteractionDenial {
                event: "discord_unauthorized_dm_user",
                field_name: "userId",
                field_value: user_id,
                message: "這個 Discord 使用者未被允許透過私訊控制服務".to_string(),
            });
        }

        // 舊版讀 `interaction.channel_id` 驗權；twilight 已把欄位標 deprecated
        // （Discord 未來改用 channel），本移植照舊版行為保留 channel_id。
        #[allow(deprecated)]
        let channel_id = interaction.channel_id.map(|id| id.get().to_string());
        if let Some(channel_id) = channel_id.as_deref()
            && self
                .allowed_channel_ids
                .iter()
                .any(|entry| entry == channel_id)
        {
            return Ok(());
        }
        Err(InteractionDenial {
            event: "discord_unauthorized_channel",
            field_name: "channelId",
            field_value: channel_id,
            message: "這個 Discord channel 未被允許使用".to_string(),
        })
    }

    /// 對應舊版 `handleCommand`：每個分支的 await 失敗照舊版拋回 message
    /// listener（config 類由 [`BotCore::run_config_command`] 化為回覆）。
    async fn handle_command(
        &self,
        interaction: &Interaction,
        command: &str,
        argument: Option<&str>,
    ) -> Result<(), BotFailure> {
        match command {
            "help" => self.reply(interaction, &help_text()).await?,
            "status" => self.reply(interaction, &self.format_status()).await?,
            "channels" => self.reply(interaction, &self.format_channels()).await?,
            "refresh" => {
                self.reply(interaction, &self.format_refresh_countdown())
                    .await?
            }
            "refresh_now" => {
                self.defer(interaction).await?;
                self.refresh_pages(interaction, argument).await?;
            }
            "points" => {
                self.defer(interaction).await?;
                self.send_channel_points(interaction, argument).await?;
            }
            "config" => {
                self.reply(interaction, &self.format_runtime_config())
                    .await?
            }
            "channel_add" => {
                self.defer(interaction).await?;
                let fut = self.add_channel(interaction, argument);
                self.run_config_command(interaction, fut).await?;
            }
            "channel_remove" => {
                self.defer(interaction).await?;
                let fut = self.remove_channel(interaction, argument);
                self.run_config_command(interaction, fut).await?;
            }
            "channels_set" => {
                self.defer(interaction).await?;
                let fut = self.set_channels_command(interaction, argument);
                self.run_config_command(interaction, fut).await?;
            }
            "max_streams" => {
                self.defer(interaction).await?;
                let fut = self.set_max_streams(interaction, argument);
                self.run_config_command(interaction, fut).await?;
            }
            "check" => {
                self.defer(interaction).await?;
                self.command_context.run_check().await?;
                self.edit_reply(interaction, "已完成一次狀態檢查").await?;
            }
            "pause" => {
                self.command_context.pause_checks().await?;
                self.reply(interaction, "自動檢查已暫停").await?;
            }
            "resume" => {
                // 舊版 resumeChecks() 是同步 void（不會 reject）。
                self.command_context.resume_checks();
                self.reply(interaction, "自動檢查已恢復").await?;
            }
            "screenshot" => {
                self.defer(interaction).await?;
                self.send_screenshot(interaction, argument).await?;
            }
            _ => {
                self.reply(
                    interaction,
                    &format!("不支援的指令：/{command}\n\n{}", help_text()),
                )
                .await?;
            }
        }
        Ok(())
    }

    /// 對應舊版 `sendScreenshot`（已先 defer；進度以 editReply 更新）。
    async fn send_screenshot(
        &self,
        interaction: &Interaction,
        requested_channel: Option<&str>,
    ) -> Result<(), BotFailure> {
        let Some(requested_channel) = requested_channel else {
            let active_channels = self.command_context.get_active_channels();
            if active_channels.is_empty() {
                self.edit_reply(interaction, "目前沒有正在觀看的頻道可供截圖")
                    .await?;
                return Ok(());
            }

            self.edit_reply(interaction, "正在傳送截圖").await?;
            let mut sent_count = 0usize;
            for channel in active_channels {
                let screenshot = self
                    .command_context
                    .capture_screenshot(Some(channel.as_str()))
                    .await?;
                let Some(screenshot) = screenshot else {
                    continue;
                };
                self.send_interaction_screenshot(interaction, &screenshot)
                    .await?;
                sent_count += 1;
            }
            if sent_count == 0 {
                self.edit_reply(interaction, "目前沒有正在觀看的頻道可供截圖")
                    .await?;
            }
            return Ok(());
        };

        let screenshot = self
            .command_context
            .capture_screenshot(Some(requested_channel))
            .await?;
        let Some(screenshot) = screenshot else {
            let active_channels = self.command_context.get_active_channels();
            let text = if active_channels.is_empty() {
                "目前沒有正在觀看的頻道可供截圖".to_string()
            } else {
                [
                    format!("找不到正在觀看的頻道：{requested_channel}"),
                    format!("可用頻道：{}", format_list(&active_channels)),
                ]
                .join("\n")
            };
            self.edit_reply(interaction, &text).await?;
            return Ok(());
        };

        self.edit_reply(interaction, "正在傳送截圖").await?;
        self.send_interaction_screenshot(interaction, &screenshot)
            .await
    }

    /// 對應舊版 `refreshPages`。
    async fn refresh_pages(
        &self,
        interaction: &Interaction,
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
            self.edit_reply(interaction, &text).await?;
            return Ok(());
        }

        let mut lines = vec!["手動重整結果：".to_string()];
        lines.extend(format_refresh_results(&results));
        self.edit_reply(interaction, &lines.join("\n")).await
    }

    /// 對應舊版 `sendChannelPoints`。
    async fn send_channel_points(
        &self,
        interaction: &Interaction,
        requested_channel: Option<&str>,
    ) -> Result<(), BotFailure> {
        let results = self
            .command_context
            .get_channel_points(requested_channel)
            .await?;
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
            self.edit_reply(interaction, &text).await?;
            return Ok(());
        }

        self.edit_reply(interaction, &format_channel_points(&results))
            .await
    }

    /// 對應舊版 `sendInteractionScreenshot`。
    async fn send_interaction_screenshot(
        &self,
        interaction: &Interaction,
        screenshot: &SessionScreenshot,
    ) -> Result<(), BotFailure> {
        self.api
            .send_interaction_photo(
                &self.discord.application_id,
                &interaction.token,
                &screenshot.image,
                &format!("{}.png", screenshot.channel),
                &format!("{} 目前瀏覽器畫面", screenshot.channel),
            )
            .await?;
        self.logger.info(
            "discord_screenshot_sent",
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

    /// 對應舊版 `addChannel`（在 `run_config_command` 內執行）。
    async fn add_channel(
        &self,
        interaction: &Interaction,
        argument: Option<&str>,
    ) -> Result<(), BotFailure> {
        let Some(argument) = argument else {
            self.edit_reply(interaction, "用法：/channel_add 頻道名稱")
                .await?;
            return Ok(());
        };
        let current = self.command_context.get_config();
        let normalized_argument = js_lower(argument);
        if current
            .channels
            .iter()
            .any(|channel| js_lower(channel) == normalized_argument)
        {
            self.edit_reply(interaction, &format!("頻道已在監控清單中：{argument}"))
                .await?;
            return Ok(());
        }
        let mut channels = current.channels.clone();
        channels.push(argument.to_string());
        let updated = self.command_context.set_channels(&channels).await?;
        self.edit_reply(
            interaction,
            &format!(
                "已加入頻道：{argument}\n{}",
                format_runtime_config(&updated)
            ),
        )
        .await
    }

    /// 對應舊版 `runConfigCommand`：config 類操作失敗時記
    /// `discord_config_update_failed`（欄位 `errorType` 為舊版 `Error.name`）；
    /// 回覆為 `ConfigValidationError` 的訊息或
    /// `設定更新失敗，請確認 config.yml 可寫入。`。必要時回覆自身的失敗照舊版
    /// 上拋（落入 message listener 的 catch）。
    async fn run_config_command(
        &self,
        interaction: &Interaction,
        operation: impl std::future::Future<Output = Result<(), BotFailure>>,
    ) -> Result<(), BotFailure> {
        if let Err(error) = operation.await {
            let error_name = match &error {
                BotFailure::Api(_) => "DiscordApiError",
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
                "discord_config_update_failed",
                Some(&SanitizedValue::Object(vec![(
                    "errorType".to_string(),
                    SanitizedValue::String(error_name.to_string()),
                )])),
            );
            self.edit_reply(interaction, &message).await?;
        }
        Ok(())
    }

    /// 對應舊版 `removeChannel`（在 `run_config_command` 內執行）。
    async fn remove_channel(
        &self,
        interaction: &Interaction,
        argument: Option<&str>,
    ) -> Result<(), BotFailure> {
        let Some(argument) = argument else {
            self.edit_reply(interaction, "用法：/channel_remove 頻道名稱")
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
            self.edit_reply(interaction, &format!("找不到監控頻道：{argument}"))
                .await?;
            return Ok(());
        }
        let updated = self.command_context.set_channels(&channels).await?;
        self.edit_reply(
            interaction,
            &format!(
                "已移除頻道：{argument}\n{}",
                format_runtime_config(&updated)
            ),
        )
        .await
    }

    /// 對應舊版 `setChannels`（`/channels_set`；在 `run_config_command` 內執行）。
    async fn set_channels_command(
        &self,
        interaction: &Interaction,
        argument: Option<&str>,
    ) -> Result<(), BotFailure> {
        let channels = parse_channels_argument(argument);
        if channels.is_empty() {
            self.edit_reply(interaction, "用法：/channels_set 頻道一,頻道二")
                .await?;
            return Ok(());
        }
        let updated = self.command_context.set_channels(&channels).await?;
        self.edit_reply(
            interaction,
            &format!("頻道清單已更新\n{}", format_runtime_config(&updated)),
        )
        .await
    }

    /// 對應舊版 `setMaxStreams`（在 `run_config_command` 內執行）。
    async fn set_max_streams(
        &self,
        interaction: &Interaction,
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
            self.edit_reply(interaction, "用法：/max_streams 正整數")
                .await?;
            return Ok(());
        };
        let updated = self
            .command_context
            .set_max_concurrent_streams(value)
            .await?;
        self.edit_reply(
            interaction,
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

    /// 對應舊版 `reply`。
    async fn reply(&self, interaction: &Interaction, text: &str) -> Result<(), BotFailure> {
        self.api
            .send_interaction_response(
                &interaction_id_string(interaction),
                &interaction.token,
                text,
            )
            .await?;
        Ok(())
    }

    /// 對應舊版 `defer`。
    async fn defer(&self, interaction: &Interaction) -> Result<(), BotFailure> {
        self.api
            .defer_interaction_response(&interaction_id_string(interaction), &interaction.token)
            .await?;
        Ok(())
    }

    /// 對應舊版 `editReply`。
    async fn edit_reply(&self, interaction: &Interaction, text: &str) -> Result<(), BotFailure> {
        self.api
            .edit_interaction_response(&self.discord.application_id, &interaction.token, text)
            .await?;
        Ok(())
    }
}

/// 對應舊版 `DefaultDiscordBot` 的公開門面（狀態與邏輯在 core；接收迴圈持有
/// `Arc<BotCore>`）。
pub(crate) struct DefaultDiscordBot {
    core: Arc<BotCore>,
}

impl DefaultDiscordBot {
    /// 對應舊版建構子。
    pub(crate) fn new(options: DefaultDiscordBotOptions) -> Self {
        let allowed_channel_ids =
            dedupe_preserving_order(&options.config.discord.allowed_channel_ids);
        let allowed_user_ids = dedupe_preserving_order(&options.config.discord.allowed_user_ids);
        let gateway_factory = options.gateway_factory.unwrap_or_else(|| {
            // 舊版 createWebSocket 的對應：以 discord.botToken 建 twilight shard。
            let bot_token = options.config.discord.bot_token.clone();
            Arc::new(move || -> Box<dyn Gateway> { Box::new(create_shard_gateway(&bot_token)) })
        });
        Self {
            core: Arc::new(BotCore {
                api: options.api,
                command_context: options.command_context,
                logger: options.logger,
                gateway_factory,
                config: options.config.clone(),
                allowed_channel_ids,
                allowed_user_ids,
                discord: options.config.discord.clone(),
                state: tokio::sync::Mutex::new(BotState {
                    started: false,
                    stop_tx: tokio::sync::watch::channel(None).0,
                }),
            }),
        }
    }
}

#[async_trait]
impl DiscordBot for DefaultDiscordBot {
    async fn start(&self) -> Result<(), DiscordApiError> {
        self.core.begin().await?;
        self.core.finish_start().await;
        Ok(())
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

/// 對應舊版 `DiscordBot.start` 在 `start()` 註冊 listeners 的部分：啟動接收
/// 迴圈（唯一持有 gateway；`stop` 經 watch 旗標要求 close code 1000）。
///
/// 迴圈結束條件（§7.8：重大錯誤結束接收迴圈並記 `discord_gateway_error`，
/// 不影響常駐服務）：串流結束（重大關閉）或自己要求的 close 已呈現。
async fn run_receive_loop(
    core: Arc<BotCore>,
    mut stop_rx: tokio::sync::watch::Receiver<Option<String>>,
) {
    let mut gateway = (core.gateway_factory)();
    // 我們自己要求的 close（stop 旗標）成立後才可能在這個旗標下結束。
    let mut closing = false;
    loop {
        tokio::select! {
            changed = stop_rx.changed(), if !closing => {
                if changed.is_ok() {
                    closing = true;
                    let reason = stop_rx
                        .borrow_and_update()
                        .clone()
                        .unwrap_or_default();
                    // 舊版 stop 的 `socket?.close(1000, reason)`；close 之後
                    // 偵測到呈現的 close 即結束迴圈（twilight 文件的模式）。
                    gateway.close(&reason);
                }
            }
            item = gateway.next() => match item {
                None => {
                    // 重大關閉（如 token 無效）：記 discord_gateway_error 後結束
                    // 接收迴圈（§7.8）。移植到的行為差異：舊版 socket 沒有此
                    // 觀察點（斷線只觸發 close listener，twilight 會替我們記收）。
                    if !closing {
                        core.logger.warn("discord_gateway_error", None);
                    }
                    break;
                }
                Some(GatewayItem::TransportIssue) => {
                    // 可恢復的接收層錯誤：記 warn 後繼續（twilight 會重連，
                    // X4）。
                    core.logger.warn("discord_gateway_error", None);
                }
                Some(GatewayItem::Closed(_)) => {
                    // 舊版 close listener：只清 heartbeat（twilight 自行處理）。
                    // 我們自己要求關閉時偵測 close 即結束迴圈。
                    if closing {
                        break;
                    }
                }
                Some(GatewayItem::Interaction(interaction)) => {
                    // 舊版 message listener 的 catch：處理失敗記
                    // discord_gateway_message_failed。
                    if core.handle_interaction(&interaction).await.is_err() {
                        core.logger.warn("discord_gateway_message_failed", None);
                    }
                }
            }
        }
    }
}

/// 供 API 呼叫的 interaction id（舊版 `interaction.id`，twilight `Id` 的
/// 字串形）。
fn interaction_id_string(interaction: &Interaction) -> String {
    interaction.id.get().to_string()
}

/// 舊版 `interaction.user?.id ?? interaction.member?.user?.id`（user 優先，
/// 與 twilight `author_id` 的 member 優先不同）。
fn dm_user_id_of(interaction: &Interaction) -> Option<String> {
    interaction
        .user
        .as_ref()
        .map(|user| user.id.get().to_string())
        .or_else(|| {
            interaction
                .member
                .as_ref()
                .and_then(|member| member.user.as_ref())
                .map(|user| user.id.get().to_string())
        })
}

/// 舊版 log 欄位 `{ <name>: value }`；value 為 undefined 時照 `JSON.stringify`
/// 的行為省略屬性（以 [`SanitizedValue::Undefined`] 建模）。
fn id_field(name: &str, value: Option<String>) -> SanitizedValue {
    SanitizedValue::Object(vec![(
        name.to_string(),
        value.map_or(SanitizedValue::Undefined, SanitizedValue::String),
    )])
}

/// 對應舊版 `parseInteractionCommand` 的結果。
struct ParsedCommand {
    /// 舊版 `interaction.data?.name?.toLocaleLowerCase('en-US')`。
    command: String,
    /// 舊版 `data?.options?.[0]?.value` 經 `String(...).trim()` 的字串；
    /// 沒有 value 時為 [`None`]（照舊版屬性不存在），空字串仍為 [`Some`]。
    argument: Option<String>,
}

/// 對應舊版 `parseInteractionCommand`：取指令名稱與第一個 option 的 value。
/// 舊版 `toLocaleLowerCase('en-US')` 以 Rust `to_lowercase` 呈現——Discord
/// 指令名只允許 ASCII（a-z0-9_-），永遠等同。
fn parse_interaction_command(interaction: &Interaction) -> Option<ParsedCommand> {
    let Some(InteractionData::ApplicationCommand(data)) = interaction.data.as_ref() else {
        // 舊版 data?.name undefined → parsed undefined（其他 interaction 型別
        // 照舊版視為無指令）。
        return None;
    };
    let command = data.name.to_lowercase();
    if command.is_empty() {
        return None;
    }
    let argument = data
        .options
        .first()
        .and_then(|option| stringify_command_option_value(&option.value))
        .map(|value| js_trim(&value).to_string());
    Some(ParsedCommand { command, argument })
}

/// 舊版 `String(firstValue)` 的 typed 對應：Discord 原始 JSON 的 value 是
/// 字串或數字（我們只宣告 type 3／4 的選項）；其他 option 型別的值舊版會以
/// `String()` 轉字串，這裡按型別對應（id 型別轉十進位字串）。
fn stringify_command_option_value(value: &CommandOptionValue) -> Option<String> {
    Some(match value {
        CommandOptionValue::String(value) => value.clone(),
        CommandOptionValue::Integer(value) => value.to_string(),
        CommandOptionValue::Number(value) => js_number_to_string(*value),
        CommandOptionValue::Boolean(value) => value.to_string(),
        CommandOptionValue::Focused(value, _) => value.clone(),
        CommandOptionValue::Attachment(id) => id.get().to_string(),
        CommandOptionValue::Channel(id) => id.get().to_string(),
        CommandOptionValue::User(id) => id.get().to_string(),
        CommandOptionValue::Role(id) => id.get().to_string(),
        CommandOptionValue::Mentionable(id) => id.get().to_string(),
        // 舊版 value 的型別是 JSON 值（字串或數字）；SubCommand 表示第一個
        // option 不是葉節點（實際等同 Discord 的 `value` 缺席），回傳 None
        // 讓 argument 為 none（與舊版 value undefined 的行為相同）。
        CommandOptionValue::SubCommand(_) | CommandOptionValue::SubCommandGroup(_) => return None,
    })
}

/// 對應舊版 `formatList`。
fn format_list<T: AsRef<str>>(values: &[T]) -> String {
    if values.is_empty() {
        return "無".to_string();
    }
    values
        .iter()
        .map(|value| value.as_ref())
        .collect::<Vec<_>>()
        .join("、")
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

/// 對應舊版 `refreshPages` 回覆的結果行（`手動重整結果：` 之後）。
fn format_refresh_results(results: &[SessionRefreshResult]) -> Vec<String> {
    results
        .iter()
        .enumerate()
        .map(|(index, result)| {
            let status = match result.status {
                SessionRefreshStatus::Refreshed => "已重整",
                SessionRefreshStatus::Unavailable => "目前無法重整",
                SessionRefreshStatus::Failed => "重整失敗",
            };
            format!("{}. {}：{}", index + 1, result.channel, status)
        })
        .collect()
}

/// 對應舊版 `sendChannelPoints` 回覆的內文（首行為 `忠誠點數：`）。
fn format_channel_points(results: &[SessionChannelPointsResult]) -> String {
    let mut lines = vec!["忠誠點數：".to_string()];
    lines.extend(results.iter().map(|result| match result {
        SessionChannelPointsResult::Available {
            channel, balance, ..
        } => format!("{channel}：{} 點", format_balance(*balance)),
        SessionChannelPointsResult::Unavailable { channel, .. } => {
            format!("{channel}：無法取得")
        }
    }));
    lines.join("\n")
}

/// 對應舊版 `balance.toLocaleString('en-US')`：以 JS `Number#toString` 的
/// 短表示為基礎，整數部分加千分位逗號。
///
/// 已知差異：`toLocaleString('en-US')` 對 ≥1e21 的數值會展開十進位數字，
/// 本移植沿用 toString 的指數形；忠誠點數不會達到該量級，A/B 比對若出現
/// 再補處理（DEC 未記，僅在此註明）。
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
        format!("監控頻道：{}", format_list(&config.channels)),
        format!(
            "最大同時觀看：{}",
            js_number_to_string(config.max_concurrent_streams)
        ),
    ]
    .join("\n")
}

/// 舊版 `toLocaleLowerCase('en-US')` 的對應。Discord 的頻道名是 ASCII 為主，
/// 但兩者其餘 Unicode 字元的映射不同（如土耳其 I）；不特殊處理（DEC-009 的
/// 同類取捨）。
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

/// 舊版 `String.prototype.trim` 會移除的字元：JS 的 WhiteSpace 加行終止符
/// （U+0085 不移除、U+FEFF 移除，與 Rust `str::trim` 不同；DEC-013 的
/// 同類處理——各模組自帶一份，統一時合併）。
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
