//! `app/createApplication.ts` 逐函式移植（§4.2，S3.6）：`createApplication`
//! 的預設組裝（config 載入器、憑證驗證器、logger 工廠、runtime 工廠）與
//! `createDefaultRuntime` 的真實接線：
//!
//! `DefaultBrowserManager`（S3.3）→ `RewardClaimer`（S1.7）→
//! session 工廠＋`DefaultSessionManager`（S3.4／S3.5）→
//! `DefaultReconcileCoordinator`（S3.5）→ `TwitchApiClient`（S3.1）→
//! `DefaultWatchdogScheduler`（S3.5）→ `YamlRuntimeConfigManager`（S2.3）→
//! Bot 指令上下文（S4.1）→ `RuntimeResourceMonitor`（S3.2）→
//! `SchedulerStallWatchdog`（S3.2）→ Telegram／Discord bot（S4.2／S4.3）。
//!
//! 舊版組裝細節照抄：
//! - session 工廠的 `config.channels` 是 runtime watch config 的 getter：
//!   `updateConfig` 換 channels 之後，尚未建立的 session 會用新清單；Rust 以
//!   每次 `create` 當下重建 options 的 [`RuntimeChannelSessionFactory`] 對應。
//! - `notify` helper：telegram／discord 的 bot 都在呼叫當下讀取，尚未建立時
//!   無作用；各接點的 fire-and-forget（`void notify(...)`）以 `tokio::spawn`
//!   外派。
//! - `sessionManagerReference`／`runtimeResourceMonitorReference` 是建立
//!   順序上的參照 cell（browser manager 建構於 session manager 之前）；以
//!   [`OnceLock`] 對映（設定後不再變動）。
//! - `startupDiagnostics` 的 provider 直接引用 runtimeResourceMonitor（舊版
//!   在整合陣列建立後回傳 closure；`AppRunner` 的 fallback 無 resource
//!   limits，由此 provider 補上）。
//!
//! 接縫差異（Rust 對應）：
//! - 同一個 logger 物件分發到各模組的窄 logger trait，以 [`SharedLogger`]
//!   對映（轉呼 runner 的 [`RunnerLogger`]；正式端為 [`JsonLineLogger`]，
//!   測試端為 fake 的記錄器）。
//! - `TelegramApiClient`／`DiscordApiClient`／`TwitchApiClient` 建構失敗
//!   （Rust 新增的 reqwest／選項驗證）以訊息字串回 `Err`；舊版
//!   `globalThis.fetch` 必在（無此路徑）。`create_default_runtime` 因此為
//!   `Result`，`service_start_failed` 的對象（runner 的 runtime 階段）。
//! - `ContainerRestartRequest` 的欄位值以 Send 的 [`ControllerFieldValue`]
//!   呈現（`SanitizedValue` 的 `Shared` 不是 Send；見 restart 模組註記）。
//!
//! `#![allow(dead_code)]` 保留至 main 的 no-arg 啟動接線（舊 `index.ts` 的
//! `main()`；下一段）；本檔以 `src/tests/*` 的整合測試與
//! `app/create/tests.rs` 的舊測試移植（`預設 RuntimeFactory`）鎖定行為。
#![allow(dead_code)]

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use futures_util::future::join_all;

use super::diagnostics::{
    RuntimeDiagnosticsBrowser, RuntimeDiagnosticsOptions, RuntimeResourceLimitsProvider,
    RuntimeResourceLimitsSnapshot, RuntimeStartupDiagnostics, collect_process_env,
    create_runtime_startup_diagnostics,
};
use super::resource_monitor::{
    MonitorBrowser, MonitorLogger, MonitorSessions, RuntimeResourceMonitor,
    RuntimeResourceMonitorOptions,
};
use super::restart::{
    ContainerRestartController, ContainerRestartControllerOptions, ContainerRestartNotifyObserver,
    ContainerRestartRequest, ControllerFieldValue, NotifyOutcomeFuture, RestartLogger,
};
use super::runner::{
    AppRunner, ApplicationIntegration, ApplicationLoggerFactory, ApplicationRuntime,
    DefaultAppRunner, DefaultAppRunnerOptions, RunnerBrowserManager, RunnerConfigLoader,
    RunnerCredentialValidator, RunnerLogger, RunnerScheduler, RunnerSessionManager, RuntimeFactory,
    StartupDiagnosticsProvider,
};
use super::stall_watchdog::{
    SchedulerSnapshot, SchedulerStallWatchdog, SchedulerStallWatchdogOptions,
    StallRestartRequester, StallSchedulerSource, StallWatchdogLogger,
};
use crate::browser::cdp::context::DriverError;
use crate::browser::manager::{BrowserManager, DefaultBrowserManager};
use crate::browser::reward_claimer::{
    RewardClaimer, RewardClaimerClock, RewardClaimerLogger, RewardClaimerOptions,
};
use crate::browser::session::{
    ChannelSession, ChannelSessionConfig, ChannelSessionInvalidationObserver,
    ContainerRestartRequest as SessionContainerRestartRequest, ContainerRestartRequestObserver,
    DefaultChannelSessionFactory, DefaultChannelSessionFactoryOptions,
    ManagedSessionBrowserManager, SessionBrowserManager, SessionRewardClaimer,
};
use crate::browser::types::{
    BrowserFatalRecoveryObserver as FatalRecoveryObserver, BrowserInvalidation,
    BrowserInvalidationObserver, BrowserInvalidationReason, BrowserLauncher,
    BrowserManagerBrowserConfig, BrowserManagerConfig, BrowserManagerDependencies,
    BrowserManagerLogger, BrowserManagerRecoveryConfig, BrowserRestartedEvent,
    BrowserRestartedMode, BrowserRestartedObserver, CdpLauncherAdapter, FatalRecoveryFieldValue,
    FatalRecoveryRequest,
};
use crate::config::loader::{ConfigLogger, DEFAULT_CONFIG_PATH, ProcessEnv, YamlConfigLoader};
use crate::config::model::AppConfig;
use crate::config::writeback::{
    ConfigWriteError, RuntimeWatchConfig, RuntimeWatchConfigTarget, TokioConfigFileIo,
    YamlRuntimeConfigManager,
};
use crate::credentials::validator::FileCredentialValidator;
use crate::discord::api::{DiscordApi, DiscordApiClient, DiscordApiClientOptions};
use crate::discord::bot::{
    DefaultDiscordBot, DefaultDiscordBotOptions, DiscordBot, DiscordBotLogger,
};
use crate::logging::logger::{JsonLineLogger, JsonLineLoggerOptions, LogLevel, create_logger};
use crate::logging::redact::SanitizedValue;
use crate::notifications::bot_command_context::{BotCommandContext, BotCommandError};
use crate::notifications::bot_notifications::{
    BrowserRestartMode, BrowserRestartNotification, ContainerRestartNotification,
};
use crate::scheduler::watchdog::{
    DefaultSchedulerStreamSelector, DefaultWatchdogScheduler, DefaultWatchdogSchedulerOptions,
    SchedulerLiveStatusProvider, SchedulerReconcileRequest, StreamStatusChange,
    StreamStatusObserver, WatchdogScheduler, WatchdogSchedulerConfig, WatchdogSchedulerLogger,
};
use crate::sessions::manager::{
    DefaultSessionManager, SessionChannelSessionFactory, SessionManager,
    SessionManagerDependencies, SessionNavigationOutcomeObserver,
};
use crate::sessions::reconcile::{
    DefaultReconcileCoordinator, ReconcileCoordinator, ReconcileCoordinatorOptions,
    ReconcileLogger, ReconcileSessionManager,
};
use crate::sessions::recovery::{ChannelRecoveryPolicy, ChannelRecoveryPolicyOptions};
use crate::telegram::api::{TelegramApi, TelegramApiClient, TelegramApiClientOptions};
use crate::telegram::bot::{
    DefaultTelegramBot, DefaultTelegramBotOptions, TelegramBot, TelegramBotLogger,
};
use crate::twitch::api::{TwitchApiClient, TwitchApiClientOptions};

// ---------------------------------------------------------------------------
// createApplication 與預設工廠
// ---------------------------------------------------------------------------

/// 對應舊版 `CreateApplicationOptions`；`None` 用預設（與舊版 `??` 鏈相同）。
pub(crate) struct CreateApplicationOptions {
    /// 舊版 `configPath?`。
    pub(crate) config_path: Option<String>,
    /// 舊版 `env?`；`None` 用真實程序的環境（非 UTF-8 鍵值略過）。
    pub(crate) env: Option<ProcessEnv>,
    /// 舊版 `bootstrapLogger?`。
    pub(crate) bootstrap_logger: Option<Arc<JsonLineLogger>>,
    /// 舊版 `configLoader?`。
    pub(crate) config_loader: Option<Arc<dyn RunnerConfigLoader>>,
    /// 舊版 `credentialValidator?`。
    pub(crate) credential_validator: Option<Arc<dyn RunnerCredentialValidator>>,
    /// 舊版 `loggerFactory?`。
    pub(crate) logger_factory: Option<Arc<dyn ApplicationLoggerFactory>>,
    /// 舊版 `runtimeFactory?`；`None` 用預設 [`ConfiguredRuntimeFactory`]
    ///（closure 恆以建構時的 configPath 呼叫 `createDefaultRuntime`）。
    pub(crate) runtime_factory: Option<Arc<dyn RuntimeFactory>>,
}

/// 對應舊版 `createApplication(options = {})`：回傳 [`AppRunner`]。
pub(crate) fn create_application(options: CreateApplicationOptions) -> Arc<dyn AppRunner> {
    // 舊版 `const env = options.env ?? process.env`。
    let env = options.env.unwrap_or_else(collect_process_env);
    // 舊版 `options.configPath ?? env.CONFIG_PATH ?? DEFAULT_CONFIG_PATH`。
    let config_path = options
        .config_path
        .or_else(|| env.get("CONFIG_PATH").cloned())
        .unwrap_or_else(|| DEFAULT_CONFIG_PATH.to_string());
    // 舊版 `createLogger({ level: 'info' })`。
    let bootstrap_logger = options.bootstrap_logger.unwrap_or_else(|| {
        Arc::new(create_logger(JsonLineLoggerOptions {
            level: Some(LogLevel::Info),
            ..JsonLineLoggerOptions::default()
        }))
    });
    // 舊版 `options.configLoader ?? new YamlConfigLoader(bootstrapLogger)`。
    let config_loader = options.config_loader.unwrap_or_else(|| {
        Arc::new(YamlConfigLoader::new(
            Arc::clone(&bootstrap_logger) as Arc<dyn ConfigLogger>
        )) as Arc<dyn RunnerConfigLoader>
    });
    // 舊版 `options.credentialValidator ?? new FileCredentialValidator()`
    //（validator 的預設 logger 為 no-op、讀檔為 tokio）。
    let credential_validator = options.credential_validator.unwrap_or_else(|| {
        Arc::new(FileCredentialValidator::default()) as Arc<dyn RunnerCredentialValidator>
    });
    // 舊版 `((config) => createLogger({ level: config.logLevel }))`。
    let logger_factory = options
        .logger_factory
        .unwrap_or_else(|| Arc::new(ConfiguredLoggerFactory) as Arc<dyn ApplicationLoggerFactory>);
    // 舊版 `((config, logger) => createDefaultRuntime(config, logger, configPath))`。
    let runtime_factory = options.runtime_factory.unwrap_or_else(|| {
        Arc::new(ConfiguredRuntimeFactory {
            config_path: config_path.clone(),
        }) as Arc<dyn RuntimeFactory>
    });

    Arc::new(DefaultAppRunner::new(DefaultAppRunnerOptions {
        config_path,
        env,
        config_loader,
        credential_validator,
        bootstrap_logger: bootstrap_logger as Arc<dyn RunnerLogger>,
        logger_factory,
        runtime_factory,
    }))
}

/// 舊版預設 `loggerFactory`：以 `config.logLevel` 建 [`JsonLineLogger`]。
struct ConfiguredLoggerFactory;

impl ApplicationLoggerFactory for ConfiguredLoggerFactory {
    fn create_logger(&self, config: &AppConfig) -> Arc<dyn RunnerLogger> {
        Arc::new(create_logger(JsonLineLoggerOptions {
            level: Some(config.log_level),
            ..JsonLineLoggerOptions::default()
        }))
    }
}

/// 舊版預設 `runtimeFactory`（closure 恆以建構時的 configPath 呼叫
/// `createDefaultRuntime`）。
struct ConfiguredRuntimeFactory {
    config_path: String,
}

#[async_trait]
impl RuntimeFactory for ConfiguredRuntimeFactory {
    async fn create_runtime(
        &self,
        config: &AppConfig,
        logger: Arc<dyn RunnerLogger>,
    ) -> Result<ApplicationRuntime, String> {
        create_default_runtime(config, logger, &self.config_path).await
    }
}

// ---------------------------------------------------------------------------
// SharedLogger：同一 logger 物件的窄介面分發
// ---------------------------------------------------------------------------

/// 舊版「同一 Logger 物件」的分發：`create_default_runtime` 把 runner 的
/// [`RunnerLogger`] 包成本 wrapper，實作各模組的窄 logger trait（轉呼）。
/// 正式端 `.inner` 為 [`JsonLineLogger`]；測試端為 fake 記錄器（同一物件身分
/// 貫穿每個元件，對映舊碼把同一 logger 傳進 BrowserManager／SessionManager／
/// scheduler／monitor／bot）。
struct SharedLogger {
    inner: Arc<dyn RunnerLogger>,
}

impl SharedLogger {
    fn new(inner: Arc<dyn RunnerLogger>) -> Arc<Self> {
        Arc::new(Self { inner })
    }

    async fn flush_io(&self) -> std::io::Result<()> {
        // 舊版 flush 的 rejection 進 `logger_flush_failed` 的訊息欄位。
        self.inner.flush().await.map_err(std::io::Error::other)
    }
}

impl BrowserManagerLogger for SharedLogger {
    fn debug(&self, event: &str, fields: Option<&SanitizedValue>) {
        self.inner.debug(event, fields);
    }

    fn info(&self, event: &str, fields: Option<&SanitizedValue>) {
        self.inner.info(event, fields);
    }

    fn warn(&self, event: &str, fields: Option<&SanitizedValue>) {
        self.inner.warn(event, fields);
    }

    fn error(&self, event: &str, fields: Option<&SanitizedValue>) {
        self.inner.error(event, fields);
    }
}

impl MonitorLogger for SharedLogger {
    fn debug(&self, event: &str, fields: Option<&SanitizedValue>) {
        self.inner.debug(event, fields);
    }

    fn info(&self, event: &str, fields: Option<&SanitizedValue>) {
        self.inner.info(event, fields);
    }

    fn warn(&self, event: &str, fields: Option<&SanitizedValue>) {
        self.inner.warn(event, fields);
    }

    fn error(&self, event: &str, fields: Option<&SanitizedValue>) {
        self.inner.error(event, fields);
    }
}

#[async_trait]
impl StallWatchdogLogger for SharedLogger {
    fn info(&self, event: &str, fields: Option<&SanitizedValue>) {
        self.inner.info(event, fields);
    }

    fn error(&self, event: &str, fields: Option<&SanitizedValue>) {
        self.inner.error(event, fields);
    }

    async fn flush(&self) -> std::io::Result<()> {
        self.flush_io().await
    }
}

#[async_trait]
impl RestartLogger for SharedLogger {
    fn error(&self, event: &str, fields: Option<&SanitizedValue>) {
        self.inner.error(event, fields);
    }

    async fn flush(&self) -> std::io::Result<()> {
        self.flush_io().await
    }
}

impl crate::twitch::api::TwitchApiLogger for SharedLogger {
    fn debug(&self, event: &str, fields: Option<&SanitizedValue>) {
        self.inner.debug(event, fields);
    }

    fn info(&self, event: &str, fields: Option<&SanitizedValue>) {
        self.inner.info(event, fields);
    }

    fn warn(&self, event: &str, fields: Option<&SanitizedValue>) {
        self.inner.warn(event, fields);
    }

    fn error(&self, event: &str, fields: Option<&SanitizedValue>) {
        self.inner.error(event, fields);
    }
}

impl WatchdogSchedulerLogger for SharedLogger {
    fn debug(&self, event: &str, fields: Option<&SanitizedValue>) {
        self.inner.debug(event, fields);
    }

    fn info(&self, event: &str, fields: Option<&SanitizedValue>) {
        self.inner.info(event, fields);
    }

    fn warn(&self, event: &str, fields: Option<&SanitizedValue>) {
        self.inner.warn(event, fields);
    }

    fn error(&self, event: &str, fields: Option<&SanitizedValue>) {
        self.inner.error(event, fields);
    }
}

impl ReconcileLogger for SharedLogger {
    fn debug(&self, event: &str, fields: Option<&SanitizedValue>) {
        self.inner.debug(event, fields);
    }

    fn error(&self, event: &str, fields: Option<&SanitizedValue>) {
        self.inner.error(event, fields);
    }
}

impl RewardClaimerLogger for SharedLogger {
    fn info(&self, event: &str, fields: &SanitizedValue) -> Result<(), String> {
        self.inner.info(event, Some(fields));
        Ok(())
    }

    fn warn(&self, event: &str, fields: &SanitizedValue) -> Result<(), String> {
        self.inner.warn(event, Some(fields));
        Ok(())
    }
}

impl TelegramBotLogger for SharedLogger {
    fn info(&self, event: &str, fields: Option<&SanitizedValue>) {
        self.inner.info(event, fields);
    }

    fn warn(&self, event: &str, fields: Option<&SanitizedValue>) {
        self.inner.warn(event, fields);
    }
}

impl DiscordBotLogger for SharedLogger {
    fn info(&self, event: &str, fields: Option<&SanitizedValue>) {
        self.inner.info(event, fields);
    }

    fn warn(&self, event: &str, fields: Option<&SanitizedValue>) {
        self.inner.warn(event, fields);
    }
}

// ---------------------------------------------------------------------------
// BotsCell 與 notify helper（舊版 `notify(operation)`）
// ---------------------------------------------------------------------------

/// 舊版 `notify` helper 的 bot cell（telegram／discord bot 在整合階段建立；
/// 通知端讀取呼叫當下的值，尚未建立時為無作用）。
#[derive(Default)]
struct BotsCell {
    bots: Mutex<Bots>,
}

#[derive(Default)]
struct Bots {
    telegram: Option<Arc<dyn TelegramBot>>,
    discord: Option<Arc<dyn DiscordBot>>,
}

/// [`BotsCell::snapshot`] 的回傳型別（clippy type_complexity 的別名）。
type BotsSnapshot = (Option<Arc<dyn TelegramBot>>, Option<Arc<dyn DiscordBot>>);

impl BotsCell {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            bots: Mutex::new(Bots::default()),
        })
    }

    /// 舊版 `[telegramBot, discordBot].filter((bot) => bot !== undefined)`。
    fn snapshot(&self) -> BotsSnapshot {
        let bots = self
            .bots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        (bots.telegram.clone(), bots.discord.clone())
    }

    fn set_telegram(&self, bot: Arc<dyn TelegramBot>) {
        self.bots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .telegram = Some(bot);
    }

    fn set_discord(&self, bot: Arc<dyn DiscordBot>) {
        self.bots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .discord = Some(bot);
    }
}

/// 對應舊版 `notify((bot) => bot.notifyContainerRestart(notification))`。
async fn notify_container_restart(bots: &BotsCell, event: &ContainerRestartNotification) {
    let (telegram, discord) = bots.snapshot();
    let mut futures: Vec<Pin<Box<dyn Future<Output = ()> + Send>>> = Vec::new();
    if let Some(bot) = telegram {
        let event = event.clone();
        futures.push(Box::pin(async move {
            bot.notify_container_restart(&event).await
        }));
    }
    if let Some(bot) = discord {
        let event = event.clone();
        futures.push(Box::pin(async move {
            bot.notify_container_restart(&event).await
        }));
    }
    // 舊版 `Promise.all().map(...catch(() => undefined))`：bot 通知為無失敗
    // 介面，catch 的對應即「不會失敗」。
    join_all(futures).await;
}

/// 對應舊版 `notify((bot) => bot.notifyPageCrash(channel))`。
async fn notify_page_crash(bots: &BotsCell, channel: &str) {
    let (telegram, discord) = bots.snapshot();
    let mut futures: Vec<Pin<Box<dyn Future<Output = ()> + Send>>> = Vec::new();
    if let Some(bot) = telegram {
        let channel = channel.to_string();
        futures.push(Box::pin(
            async move { bot.notify_page_crash(&channel).await },
        ));
    }
    if let Some(bot) = discord {
        let channel = channel.to_string();
        futures.push(Box::pin(
            async move { bot.notify_page_crash(&channel).await },
        ));
    }
    join_all(futures).await;
}

/// 對應舊版 `notify((bot) => bot.notifyBrowserRestart(event))`。
async fn notify_browser_restart(bots: &BotsCell, event: &BrowserRestartNotification) {
    let (telegram, discord) = bots.snapshot();
    let mut futures: Vec<Pin<Box<dyn Future<Output = ()> + Send>>> = Vec::new();
    if let Some(bot) = telegram {
        let event = event.clone();
        futures.push(Box::pin(
            async move { bot.notify_browser_restart(&event).await },
        ));
    }
    if let Some(bot) = discord {
        let event = event.clone();
        futures.push(Box::pin(
            async move { bot.notify_browser_restart(&event).await },
        ));
    }
    join_all(futures).await;
}

/// 對應舊版 `notify((bot) => bot.notifyReward(result))`。
async fn notify_reward(
    bots: &BotsCell,
    result: &crate::browser::reward_claimer::RewardClaimResult,
) {
    let (telegram, discord) = bots.snapshot();
    let mut futures: Vec<Pin<Box<dyn Future<Output = ()> + Send>>> = Vec::new();
    if let Some(bot) = telegram {
        let result = result.clone();
        futures.push(Box::pin(async move { bot.notify_reward(&result).await }));
    }
    if let Some(bot) = discord {
        let result = result.clone();
        futures.push(Box::pin(async move { bot.notify_reward(&result).await }));
    }
    join_all(futures).await;
}

/// 對應舊版 `notify((bot) => bot.notifyStreamStatus(change))`（scheduler 的
/// 觀察端以 spawn 呼叫，本 helper await 兩個 bot）。
async fn notify_stream_status(bots: &BotsCell, change: &StreamStatusChange) {
    let (telegram, discord) = bots.snapshot();
    let mut futures: Vec<Pin<Box<dyn Future<Output = ()> + Send>>> = Vec::new();
    if let Some(bot) = telegram {
        let change = change.clone();
        futures.push(Box::pin(
            async move { bot.notify_stream_status(&change).await },
        ));
    }
    if let Some(bot) = discord {
        let change = change.clone();
        futures.push(Box::pin(
            async move { bot.notify_stream_status(&change).await },
        ));
    }
    join_all(futures).await;
}

// ---------------------------------------------------------------------------
// 觀察者與各接縫的實作（bridge；本檔集中，不動各模組）
// ---------------------------------------------------------------------------

/// 對應舊版 `ContainerRestartController` 的 `onNotify`：
/// `notify((bot) => bot.notifyContainerRestart(...))`。
fn container_restart_notify_observer(bots: Arc<BotsCell>) -> ContainerRestartNotifyObserver {
    Arc::new(move |request: ContainerRestartRequest| {
        // 舊版 `(request) => notify(bot => bot.notifyContainerRestart(...))`；
        // 欄位取樣在呼叫端當下（request 的欄位不進未來，保持 Send）。
        let notification = to_container_restart_notification(&request);
        let bots = Arc::clone(&bots);
        Box::pin(async move {
            notify_container_restart(&bots, &notification).await;
            Ok(())
        }) as NotifyOutcomeFuture
    })
}

/// 舊版 `toContainerRestartNotification(request)`：`fields.reason`（字串且
/// 不等於 reason）→ `detailReason`；`fields.channel`（非空字串）→ `channel`。
fn to_container_restart_notification(
    request: &ContainerRestartRequest,
) -> ContainerRestartNotification {
    let fields = request.fields.as_deref().unwrap_or(&[]);
    let find_string = |name: &str| {
        // 舊版物件屬性查詢（fields 為單一值時）；Rust 版取最後一筆同名鍵。
        fields
            .iter()
            .rev()
            .find(|(key, _)| key == name)
            .and_then(|(_, value)| match value {
                ControllerFieldValue::Str(text) => Some(text.clone()),
                _ => None,
            })
    };
    let detail_reason = find_string("reason").filter(|value| *value != request.reason);
    let channel = find_string("channel").filter(|value| !value.is_empty());

    ContainerRestartNotification {
        reason: request.reason.clone(),
        source: request.source.clone(),
        channel,
        detail_reason,
    }
}

/// 舊版 browser manager 的 `onInvalidated`：page_crash 帶 fire-and-forget 的
/// bot 通知，其後轉送 session manager 的 invalidate（參照 cell 對映建立順序）。
struct BrowserInvalidationRouter {
    bots: Arc<BotsCell>,
    session_manager: Arc<OnceLock<Arc<DefaultSessionManager>>>,
}

#[async_trait]
impl BrowserInvalidationObserver for BrowserInvalidationRouter {
    async fn on_invalidated(&self, invalidation: BrowserInvalidation) -> Result<(), DriverError> {
        if invalidation.reason == BrowserInvalidationReason::PageCrashed {
            // 舊版 `void notify((bot) => bot.notifyPageCrash(channel))`。
            let bots = Arc::clone(&self.bots);
            let channel = invalidation.channel.clone();
            tokio::spawn(async move {
                notify_page_crash(&bots, &channel).await;
            });
        }
        // 舊版 `sessionManagerReference.current?.invalidate(channel, reason)`：
        // reason 字串原樣轉送（DEC-023 的接線備註）。
        if let Some(manager) = self.session_manager.get() {
            SessionManager::invalidate(
                manager.as_ref(),
                &invalidation.channel,
                invalidation.reason.as_str(),
            )
            .await;
        }
        Ok(())
    }
}

/// 舊版 browser manager 的 `onFatalRecovery`：轉送 containerRestartController
///（source `browser_manager`）。
struct BrowserFatalRecoveryRouter {
    controller: Arc<ContainerRestartController>,
}

#[async_trait]
impl FatalRecoveryObserver for BrowserFatalRecoveryRouter {
    async fn on_fatal_recovery(&self, request: FatalRecoveryRequest) -> Result<(), DriverError> {
        // 舊版：`containerRestartController.request({
        //   reason, source: 'browser_manager', fields? })`；
        // FatalRecoveryFieldValue 的值域是限制型（本層 Send 需求的取捨），
        // 在此轉回消毒值的純量形。
        self.controller
            .request(ContainerRestartRequest {
                reason: request.reason,
                source: "browser_manager".to_string(),
                fields: request.fields.map(|fields| {
                    fields
                        .into_iter()
                        .map(|(name, value)| match value {
                            FatalRecoveryFieldValue::Str(text) => {
                                (name, ControllerFieldValue::Str(text))
                            }
                            FatalRecoveryFieldValue::Num(number) => {
                                (name, ControllerFieldValue::Num(number))
                            }
                        })
                        .collect()
                }),
            })
            .await;
        Ok(())
    }
}

/// 舊版 browser manager 的 `onBrowserRestarted`：通知 monitor（前置；同步
/// 段）與 fire-and-forget 的 bot 通知。
struct BrowserRestartedRouter {
    bots: Arc<BotsCell>,
    monitor: Arc<OnceLock<Arc<RuntimeResourceMonitor>>>,
}

impl BrowserRestartedObserver for BrowserRestartedRouter {
    fn on_browser_restarted(&self, event: BrowserRestartedEvent) -> Result<(), DriverError> {
        // 舊版 `runtimeResourceMonitorReference.current?.notifyBrowserRestarted()`；
        // Rust monitor 的方法是 async（狀態在 tokio Mutex），以 spawn 對映。
        if let Some(monitor) = self.monitor.get() {
            let monitor = Arc::clone(monitor);
            tokio::spawn(async move {
                monitor.notify_browser_restarted().await;
            });
        }
        // 舊版 `void notify((bot) => bot.notifyBrowserRestart(event))`。
        let bots = Arc::clone(&self.bots);
        let notification = BrowserRestartNotification {
            mode: match event.mode {
                BrowserRestartedMode::Automatic => BrowserRestartMode::Automatic,
                BrowserRestartedMode::Manual => BrowserRestartMode::Manual,
            },
        };
        tokio::spawn(async move {
            notify_browser_restart(&bots, &notification).await;
        });
        Ok(())
    }
}

/// 舊版 session 工廠的 `onInvalidated`：轉送 session manager invalidate
///（session 的接縫把 reason 固定為 `health_failure_threshold`，見
/// `ChannelSessionInvalidationObserver` 的註記）。
struct SessionInvalidationRouter {
    session_manager: Arc<OnceLock<Arc<DefaultSessionManager>>>,
}

#[async_trait]
impl ChannelSessionInvalidationObserver for SessionInvalidationRouter {
    async fn on_invalidated(&self, channel: &str) -> Result<(), DriverError> {
        // 舊版 `(channel, reason) => sessionManagerReference.current?.invalidate(...)`。
        if let Some(manager) = self.session_manager.get() {
            SessionManager::invalidate(manager.as_ref(), channel, "health_failure_threshold").await;
        }
        Ok(())
    }
}

/// 舊版 session 工廠的 `onContainerRestartRequested`：轉送
/// containerRestartController（source `channel_session_reward`；fields 照舊）。
struct SessionRestartRequestRouter {
    controller: Arc<ContainerRestartController>,
}

#[async_trait]
impl ContainerRestartRequestObserver for SessionRestartRequestRouter {
    async fn on_container_restart_requested(
        &self,
        request: &SessionContainerRestartRequest,
    ) -> Result<(), DriverError> {
        // 舊版 fields：`{ channel, consecutiveFailures, requestedAt }`。
        self.controller
            .request(ContainerRestartRequest {
                reason: request.reason.to_string(),
                source: "channel_session_reward".to_string(),
                fields: Some(vec![
                    (
                        "channel".to_string(),
                        ControllerFieldValue::Str(request.channel.clone()),
                    ),
                    (
                        "consecutiveFailures".to_string(),
                        ControllerFieldValue::Num(request.consecutive_failures as f64),
                    ),
                    (
                        "requestedAt".to_string(),
                        ControllerFieldValue::Str(request.requested_at.clone()),
                    ),
                ]),
            })
            .await;
        Ok(())
    }
}

/// 舊版 `RewardClaimer` 的 `onResult`：fire-and-forget 的 bot 通知
///（claimer 的 observer 是 sync 函式，內部自行外派）。
fn reward_claim_observer(
    bots: Arc<BotsCell>,
) -> crate::browser::reward_claimer::RewardClaimObserver {
    Box::new(
        move |result: &crate::browser::reward_claimer::RewardClaimResult| {
            let bots = Arc::clone(&bots);
            let result = result.clone();
            tokio::spawn(async move {
                notify_reward(&bots, &result).await;
            });
            Ok(())
        },
    )
}

/// 舊版 `onStreamStatusChanged: (change) => notify(...)` 的接縫本體
///（scheduler 以 spawn 呼叫；DEC-024 的接線備註）。
struct BotStreamStatusObserver {
    bots: Arc<BotsCell>,
}

#[async_trait]
impl StreamStatusObserver for BotStreamStatusObserver {
    async fn on_stream_status_changed(&self, change: &StreamStatusChange) {
        notify_stream_status(&self.bots, change).await;
    }
}

// 各模組窄介面的型別轉接（對映舊版把同一物件當不同的 Pick<…> 傳遞）。

/// 舊版 `RuntimeResourceMonitorOptions.browserManager`：`Pick<BrowserManager,
/// 'getPageCount' | 'restart'>`。
#[async_trait]
impl MonitorBrowser for DefaultBrowserManager {
    fn get_page_count(&self) -> u64 {
        BrowserManager::get_page_count(self) as u64
    }

    async fn restart(&self) -> Result<(), String> {
        BrowserManager::restart(self)
            .await
            .map_err(|error| error.message)
    }
}

/// 舊版 `RuntimeResourceMonitorOptions.sessionManager`：`getActiveChannels`。
impl MonitorSessions for DefaultSessionManager {
    fn get_active_channels(&self) -> Vec<String> {
        SessionManager::get_active_channels(self)
    }
}

/// 舊版 `SchedulerStallWatchdogOptions.scheduler`：`getSnapshot` 的窄版。
impl StallSchedulerSource for DefaultWatchdogScheduler {
    fn get_snapshot(&self) -> SchedulerSnapshot {
        let snapshot = WatchdogScheduler::get_snapshot(self);
        SchedulerSnapshot {
            running: snapshot.running,
            check_in_flight: snapshot.check_in_flight,
            retry_at: snapshot.retry_at,
            last_checked_at: snapshot.last_checked_at,
        }
    }
}

/// 舊版 stall watchdog 取得的 `containerRestartController`：request 是 sync、
/// fire-and-forget（舊版 `void controller.request(...)`）；controller 的
/// request 是 async 的單飛，因此以 spawn 外派。
struct ContainerRestartRequester {
    controller: Arc<ContainerRestartController>,
}

impl StallRestartRequester for ContainerRestartRequester {
    fn request(&self, request: ContainerRestartRequest) {
        let controller = Arc::clone(&self.controller);
        tokio::spawn(async move {
            controller.request(request).await;
        });
    }
}

/// diagnostics fallback 的 `RuntimeDiagnosticsBrowser` 轉接（BrowserManager
/// 介面的兩個 getter；JS number 以 f64 呈現）。
impl RuntimeDiagnosticsBrowser for DefaultBrowserManager {
    fn get_browser_generation(&self) -> f64 {
        BrowserManager::get_browser_generation(self) as f64
    }

    fn get_browser_version(&self) -> Option<String> {
        BrowserManager::get_browser_version(self)
    }
}

/// 舊版 `SessionManagerOptions.onNavigationOutcomes`：
/// `(outcomes) => browserManager.reportNavigationOutcomes(outcomes)`。
#[async_trait]
impl SessionNavigationOutcomeObserver for DefaultBrowserManager {
    async fn on_navigation_outcomes(
        &self,
        outcomes: &[crate::browser::types::BrowserNavigationOutcome],
    ) -> Result<(), DriverError> {
        self.report_navigation_outcomes(outcomes.to_vec()).await;
        Ok(())
    }
}

/// `resourceLimits` 的同步快照（provider 內先取得監視器的快照，再供同步的
/// diagnostics 組裝讀取；對映舊版同步 `getLatestResourceLimits`）。
struct SnapshotLimitsProvider(RuntimeResourceLimitsSnapshot);

impl RuntimeResourceLimitsProvider for SnapshotLimitsProvider {
    fn get_latest_resource_limits(&self) -> RuntimeResourceLimitsSnapshot {
        self.0.clone()
    }
}

/// 舊版 `startupDiagnostics` 的 provider：`createRuntimeStartupDiagnostics({
/// config, browserManager, resourceLimits: runtimeResourceMonitor, env, … })`。
struct ConfiguredStartupDiagnostics {
    config: AppConfig,
    browser_manager: Arc<DefaultBrowserManager>,
    monitor: Arc<RuntimeResourceMonitor>,
}

#[async_trait]
impl StartupDiagnosticsProvider for ConfiguredStartupDiagnostics {
    async fn diagnostics(&self) -> Result<RuntimeStartupDiagnostics, String> {
        // 舊版 `env: process.env`：呼叫當下的程序環境。
        let limits = self.monitor.get_latest_resource_limits().await;
        let limits_provider = SnapshotLimitsProvider(limits);
        Ok(create_runtime_startup_diagnostics(
            &RuntimeDiagnosticsOptions {
                config: &self.config,
                browser_manager: self.browser_manager.as_ref(),
                resource_limits: Some(&limits_provider),
                env: None,
                browser_engine: Some(self.config.browser.engine.as_str()),
                run_id: None,
                node_version: None,
            },
        ))
    }
}

// ---------------------------------------------------------------------------
// runtime watch config 的 target 與 bot 指令上下文
// ---------------------------------------------------------------------------

/// 舊版 `target: { async updateConfig(nextConfig) { … } }` 與 runtime watch
/// config 的 cell（session 工廠的 channels getter 讀取）。
struct RuntimeConfigApplier {
    runtime_watch_config: Arc<Mutex<RuntimeWatchConfig>>,
    session_manager: Arc<DefaultSessionManager>,
    scheduler: Arc<DefaultWatchdogScheduler>,
}

impl RuntimeWatchConfigTarget for RuntimeConfigApplier {
    async fn update_config(&self, next: RuntimeWatchConfig) -> Result<(), ConfigWriteError> {
        // 舊版順序：`runtimeWatchConfig = nextConfig` →
        // `sessionManager.updateConfiguredChannels(nextConfig.channels)` →
        // `await scheduler.updateConfig(nextConfig)`（兩者皆無失敗型別；
        // Promise 的對應不 reject）。
        *self
            .runtime_watch_config
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = next.clone();
        SessionManager::update_configured_channels(self.session_manager.as_ref(), &next.channels);
        WatchdogScheduler::update_config(self.scheduler.as_ref(), next).await;
        Ok(())
    }
}

/// 舊版 `commandContext` 物件字面（S4.1 的介面；指向 scheduler／
/// session manager／runtime config manager）。
struct DefaultBotCommandContext {
    scheduler: Arc<DefaultWatchdogScheduler>,
    session_manager: Arc<DefaultSessionManager>,
    runtime_config_manager: Arc<YamlRuntimeConfigManager<RuntimeConfigApplier, TokioConfigFileIo>>,
}

#[async_trait]
impl BotCommandContext for DefaultBotCommandContext {
    async fn run_check(&self) -> Result<(), BotCommandError> {
        // 舊版 `() => scheduler.runOnce()`（無失敗型別）。
        WatchdogScheduler::run_once(self.scheduler.as_ref()).await;
        Ok(())
    }

    async fn pause_checks(&self) -> Result<(), BotCommandError> {
        // 舊版 `() => scheduler.stop()`（無失敗型別）。
        WatchdogScheduler::stop(self.scheduler.as_ref()).await;
        Ok(())
    }

    fn resume_checks(&self) {
        // 舊版 `() => scheduler.start()`（同步）。
        WatchdogScheduler::start(self.scheduler.as_ref());
    }

    fn get_scheduler_snapshot(&self) -> crate::scheduler::watchdog::WatchdogSchedulerSnapshot {
        WatchdogScheduler::get_snapshot(self.scheduler.as_ref())
    }

    fn get_active_channels(&self) -> Vec<String> {
        SessionManager::get_active_channels(self.session_manager.as_ref())
    }

    fn get_refresh_statuses(&self) -> Vec<crate::browser::session::ChannelSessionRefreshStatus> {
        SessionManager::get_refresh_statuses(self.session_manager.as_ref())
    }

    async fn refresh_pages(
        &self,
        channel: Option<&str>,
    ) -> Result<Vec<crate::sessions::manager::SessionRefreshResult>, BotCommandError> {
        // 舊版 `(channel) => sessionManager.refreshPages(channel)`（無失敗型別）。
        Ok(SessionManager::refresh_pages(self.session_manager.as_ref(), channel).await)
    }

    async fn capture_screenshot(
        &self,
        channel: Option<&str>,
    ) -> Result<Option<crate::sessions::manager::SessionScreenshot>, BotCommandError> {
        // 同上。
        Ok(SessionManager::capture_screenshot(self.session_manager.as_ref(), channel).await)
    }

    async fn get_channel_points(
        &self,
        channel: Option<&str>,
    ) -> Result<Vec<crate::sessions::manager::SessionChannelPointsResult>, BotCommandError> {
        Ok(SessionManager::get_channel_points(self.session_manager.as_ref(), channel).await)
    }

    fn get_config(&self) -> RuntimeWatchConfig {
        self.runtime_config_manager.get_config()
    }

    async fn set_channels(
        &self,
        channels: &[String],
    ) -> Result<RuntimeWatchConfig, ConfigWriteError> {
        self.runtime_config_manager.set_channels(channels).await
    }

    async fn set_max_concurrent_streams(
        &self,
        value: f64,
    ) -> Result<RuntimeWatchConfig, ConfigWriteError> {
        self.runtime_config_manager
            .set_max_concurrent_streams(value)
            .await
    }
}

// ---------------------------------------------------------------------------
// ApplicationIntegration 的四個預設元件
// ---------------------------------------------------------------------------

#[async_trait]
impl ApplicationIntegration for RuntimeResourceMonitor {
    async fn start(&self) -> Result<(), String> {
        // 舊版 monitor 的 start 不拋錯。
        RuntimeResourceMonitor::start(self).await;
        Ok(())
    }

    async fn stop(&self, reason: &str) -> Result<(), String> {
        let _ = reason;
        // 舊版 monitor 的 stop 不拋錯。
        RuntimeResourceMonitor::stop(self).await;
        Ok(())
    }
}

#[async_trait]
impl ApplicationIntegration for SchedulerStallWatchdog {
    async fn start(&self) -> Result<(), String> {
        // 舊版 monitor 的 start 不拋錯。
        SchedulerStallWatchdog::start(self).await;
        Ok(())
    }

    async fn stop(&self, reason: &str) -> Result<(), String> {
        let _ = reason;
        // 同上。
        SchedulerStallWatchdog::stop(self).await;
        Ok(())
    }
}

#[async_trait]
impl ApplicationIntegration for DefaultTelegramBot {
    async fn start(&self) -> Result<(), String> {
        // 舊版 TelegramBot.start() 不 reject。
        TelegramBot::start(self).await;
        Ok(())
    }

    async fn stop(&self, reason: &str) -> Result<(), String> {
        // 舊版 stop(reason) 不 reject（對未啟動的 stop 為 no-op，照舊）。
        TelegramBot::stop(self, reason).await;
        Ok(())
    }
}

#[async_trait]
impl ApplicationIntegration for DefaultDiscordBot {
    async fn start(&self) -> Result<(), String> {
        // 舊版 DiscordBot.start() 可 reject；整合階段失敗記
        // `service_start_failed {phase: 'integration'}`。
        DiscordBot::start(self)
            .await
            .map_err(|error| error.message().to_string())
    }

    async fn stop(&self, reason: &str) -> Result<(), String> {
        // 舊版 stop(reason) 不 reject。
        DiscordBot::stop(self, reason).await;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 動態 session 工廠（runtime watch config 的 channels getter）
// ---------------------------------------------------------------------------

/// 舊版 `DefaultChannelSessionFactory` 的合成：`config.channels` 讀 runtime
/// watch config 的 getter；Rust 版在每次 `create` 當下以目前的 watch config
/// 重建 options（session 的設定在建立當下凍結，跟舊版一樣）。
struct RuntimeChannelSessionFactory {
    browser: crate::config::model::BrowserConfig,
    runtime_watch_config: Arc<Mutex<RuntimeWatchConfig>>,
    browser_manager: Arc<ManagedSessionBrowserManager>,
    reward_claimer: Arc<RewardClaimer>,
    logger: Arc<SharedLogger>,
    on_invalidated: Arc<dyn ChannelSessionInvalidationObserver>,
    on_container_restart_requested: Arc<dyn ContainerRestartRequestObserver>,
}

impl SessionChannelSessionFactory for RuntimeChannelSessionFactory {
    fn create(&self, channel: &str) -> Result<Arc<dyn ChannelSession>, DriverError> {
        // 舊版 config 的 `get channels()`：每次建立用當下的 watch config。
        let channels = self
            .runtime_watch_config
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .channels
            .clone();
        let factory = DefaultChannelSessionFactory::new(DefaultChannelSessionFactoryOptions {
            config: ChannelSessionConfig {
                channels,
                browser: self.browser.clone(),
            },
            browser_manager: Arc::clone(&self.browser_manager) as Arc<dyn SessionBrowserManager>,
            reward_claimer: Arc::clone(&self.reward_claimer) as Arc<dyn SessionRewardClaimer>,
            playback_optimizer: None,
            logger: Some(Arc::clone(&self.logger) as Arc<dyn BrowserManagerLogger>),
            health_failure_threshold: None,
            health_evaluator: None,
            on_invalidated: Some(Arc::clone(&self.on_invalidated)),
            on_page_refresh: None,
            on_container_restart_requested: Some(Arc::clone(&self.on_container_restart_requested)),
            page_operation_drain_timeout_ms: None,
            points_claim_retry_attempts: None,
            points_claim_retry_delay_ms: None,
            now: None,
        });
        let session = factory.create(channel)?;
        Ok(Arc::new(session) as Arc<dyn ChannelSession>)
    }
}

// ---------------------------------------------------------------------------
// createDefaultRuntime
// ---------------------------------------------------------------------------

/// 對應舊版 `createDefaultRuntime(config, logger, configPath = …)`；建構失敗
/// 以訊息字串回報（對應舊版建構時的 TypeError／RangeError／fetch 缺失）。
pub(crate) async fn create_default_runtime(
    config: &AppConfig,
    logger: Arc<dyn RunnerLogger>,
    config_path: &str,
) -> Result<ApplicationRuntime, String> {
    let shared_logger = SharedLogger::new(Arc::clone(&logger));

    // 舊版 `sessionManagerReference: { current?: DefaultSessionManager }`。
    let session_manager_reference: Arc<OnceLock<Arc<DefaultSessionManager>>> =
        Arc::new(OnceLock::new());
    // 舊版 `runtimeWatchConfig = { channels …, maxConcurrentStreams … }`。
    let runtime_watch_config: Arc<Mutex<RuntimeWatchConfig>> =
        Arc::new(Mutex::new(RuntimeWatchConfig {
            channels: config.channels.clone(),
            max_concurrent_streams: config.max_concurrent_streams,
        }));
    let bots = BotsCell::new();
    // 舊版 `runtimeResourceMonitorReference: { current? … }`。
    let runtime_resource_monitor_reference: Arc<OnceLock<Arc<RuntimeResourceMonitor>>> =
        Arc::new(OnceLock::new());

    // 舊版 `const containerRestartController = new ContainerRestartController({ logger, onNotify })`。
    let container_restart_controller = Arc::new(ContainerRestartController::new(
        ContainerRestartControllerOptions {
            logger: Arc::clone(&shared_logger) as Arc<dyn RestartLogger>,
            exit: None,
            flush_timeout_ms: None,
            on_notify: Some(container_restart_notify_observer(Arc::clone(&bots))),
            notify_timeout_ms: None,
        },
    ));

    // 舊版 `const browserManager = new DefaultBrowserManager(config, { … })`。
    let browser_manager = Arc::new(DefaultBrowserManager::new(
        browser_manager_config(config),
        BrowserManagerDependencies {
            launcher: Some(Arc::new(CdpLauncherAdapter::new()) as Arc<dyn BrowserLauncher>),
            logger: Some(Arc::clone(&shared_logger) as Arc<dyn BrowserManagerLogger>),
            on_invalidated: Some(Arc::new(BrowserInvalidationRouter {
                bots: Arc::clone(&bots),
                session_manager: Arc::clone(&session_manager_reference),
            }) as Arc<dyn BrowserInvalidationObserver>),
            on_fatal_recovery: Some(Arc::new(BrowserFatalRecoveryRouter {
                controller: Arc::clone(&container_restart_controller),
            }) as Arc<dyn FatalRecoveryObserver>),
            on_browser_restarted: Some(Arc::new(BrowserRestartedRouter {
                bots: Arc::clone(&bots),
                monitor: Arc::clone(&runtime_resource_monitor_reference),
            }) as Arc<dyn BrowserRestartedObserver>),
            ..BrowserManagerDependencies::default()
        },
    ));

    // 舊版 `const rewardClaimer = new RewardClaimer({ logger, onResult })`。
    let reward_claimer = Arc::new(RewardClaimer::new(RewardClaimerOptions {
        logger: Arc::clone(&shared_logger) as Arc<dyn RewardClaimerLogger>,
        clock: Arc::new(crate::browser::reward_claimer::SystemRewardClock)
            as Arc<dyn RewardClaimerClock>,
        on_result: Some(reward_claim_observer(Arc::clone(&bots))),
    }));

    // 舊版 `const sessionFactory = new DefaultChannelSessionFactory({ … })`；
    // `config.channels` 的 getter 以每次 create 重建 options 對應。
    let session_factory = Arc::new(RuntimeChannelSessionFactory {
        browser: config.browser.clone(),
        runtime_watch_config: Arc::clone(&runtime_watch_config),
        browser_manager: Arc::new(ManagedSessionBrowserManager::new(
            Arc::clone(&browser_manager) as Arc<dyn BrowserManager>,
        )),
        reward_claimer: Arc::clone(&reward_claimer),
        logger: Arc::clone(&shared_logger),
        on_invalidated: Arc::new(SessionInvalidationRouter {
            session_manager: Arc::clone(&session_manager_reference),
        }) as Arc<dyn ChannelSessionInvalidationObserver>,
        on_container_restart_requested: Arc::new(SessionRestartRequestRouter {
            controller: Arc::clone(&container_restart_controller),
        }) as Arc<dyn ContainerRestartRequestObserver>,
    });

    // 舊版 `const sessionManager = new DefaultSessionManager(sessionFactory, { … })`。
    let session_manager = Arc::new(DefaultSessionManager::new(
        Arc::clone(&session_factory) as Arc<dyn SessionChannelSessionFactory>,
        SessionManagerDependencies {
            logger: Some(Arc::clone(&shared_logger) as Arc<dyn BrowserManagerLogger>),
            sleep: None,
            start_retry_attempts: Some(1),
            start_retry_delay_ms: Some(2_000),
            start_stagger_ms: Some(1_000),
            // 舊版 `Math.max(60_000, config.browser.navigationTimeoutMs * 2)`。
            session_operation_timeout_ms: Some(
                (config.browser.navigation_timeout_ms * 2.0).max(60_000.0) as u64,
            ),
            on_navigation_outcomes: Some(
                Arc::clone(&browser_manager) as Arc<dyn SessionNavigationOutcomeObserver>
            ),
            recovery_policy: Some(Arc::new(std::sync::Mutex::new(ChannelRecoveryPolicy::new(
                ChannelRecoveryPolicyOptions {
                    recovery: config.browser.recovery.clone(),
                    session_start: config.browser.session_start.clone(),
                    now: None,
                },
            )))),
            configured_channels: config.channels.clone(),
        },
    ));
    // 舊版 `sessionManagerReference.current = sessionManager`（首次設定）。
    let _ = session_manager_reference.set(Arc::clone(&session_manager));

    // 舊版 `const reconcileCoordinator = new DefaultReconcileCoordinator({ … })`。
    let reconcile_coordinator = Arc::new(DefaultReconcileCoordinator::new(
        ReconcileCoordinatorOptions {
            session_manager: Arc::clone(&session_manager) as Arc<dyn ReconcileSessionManager>,
            logger: Arc::clone(&shared_logger) as Arc<dyn ReconcileLogger>,
            now: None,
            shutdown_timeout_ms: None,
        },
    ));

    // 舊版 `const liveStatusProvider = new TwitchApiClient({ … })`（建構失敗視
    // 為 runtime 階段的啟動失敗；Rust 對應的選項驗證在舊版無此路徑）。
    let live_status_provider = Arc::new(
        TwitchApiClient::new(TwitchApiClientOptions {
            client_id: config.twitch_api.client_id.clone(),
            access_token: Some(config.twitch_api.access_token.clone()),
            client_secret: Some(config.twitch_api.client_secret.clone()),
            check_interval_seconds: config.check_interval_seconds,
            logger: Arc::clone(&shared_logger) as Arc<dyn crate::twitch::api::TwitchApiLogger>,
            transport: None,
            base_url: None,
            oauth_base_url: None,
            request_timeout_ms: None,
            token_validation_interval_ms: None,
            token_refresh_threshold_seconds: None,
            now: None,
        })
        .map_err(|error| error.to_string())?,
    );

    // 舊版 `const scheduler = new DefaultWatchdogScheduler({ … })`（不帶
    // sessionManager；reconcile 一律經 coordinator）。
    let scheduler = Arc::new(DefaultWatchdogScheduler::new(
        DefaultWatchdogSchedulerOptions {
            config: WatchdogSchedulerConfig {
                channels: config.channels.clone(),
                check_interval_seconds: config.check_interval_seconds,
                max_concurrent_streams: config.max_concurrent_streams,
            },
            live_status_provider: Arc::clone(&live_status_provider)
                as Arc<dyn SchedulerLiveStatusProvider>,
            stream_selector: Arc::new(DefaultSchedulerStreamSelector)
                as Arc<dyn crate::scheduler::watchdog::SchedulerStreamSelector>,
            session_manager: None,
            reconcile_coordinator: Some(
                Arc::clone(&reconcile_coordinator) as Arc<dyn SchedulerReconcileRequest>
            ),
            logger: Arc::clone(&shared_logger) as Arc<dyn WatchdogSchedulerLogger>,
            timer: None,
            now: None,
            on_stream_status_changed: Some(Arc::new(BotStreamStatusObserver {
                bots: Arc::clone(&bots),
            }) as Arc<dyn StreamStatusObserver>),
        },
    ));

    // 舊版 `const runtimeConfigManager = new YamlRuntimeConfigManager({ … })`。
    let runtime_config_manager = Arc::new(YamlRuntimeConfigManager::new(
        config_path.to_string(),
        RuntimeWatchConfig {
            channels: config.channels.clone(),
            max_concurrent_streams: config.max_concurrent_streams,
        },
        RuntimeConfigApplier {
            runtime_watch_config: Arc::clone(&runtime_watch_config),
            session_manager: Arc::clone(&session_manager),
            scheduler: Arc::clone(&scheduler),
        },
        TokioConfigFileIo,
    ));

    // 舊版 `const runtimeResourceMonitor = new RuntimeResourceMonitor({ … })`。
    let runtime_resource_monitor = Arc::new(RuntimeResourceMonitor::new(
        RuntimeResourceMonitorOptions {
            browser: Arc::clone(&browser_manager) as Arc<dyn MonitorBrowser>,
            sessions: Arc::clone(&session_manager) as Arc<dyn MonitorSessions>,
            logger: Arc::clone(&shared_logger) as Arc<dyn MonitorLogger>,
            interval_seconds: config.browser.resource_telemetry_interval_seconds,
            resource_guard: Some(config.browser.resource_guard.clone()),
            cgroup_reader: None,
            process_metrics: None,
            now: None,
            // 舊版 `(request) => containerRestartController.request(request)`。
            on_container_restart_requested: Some({
                let controller = Arc::clone(&container_restart_controller);
                Arc::new(move |request: ContainerRestartRequest| {
                    let controller = Arc::clone(&controller);
                    Box::pin(async move {
                        controller.request(request).await;
                        Ok(())
                    }) as NotifyOutcomeFuture
                }) as ContainerRestartNotifyObserver
            }),
        },
    )?);
    // 舊版 `runtimeResourceMonitorReference.current = runtimeResourceMonitor`。
    let _ = runtime_resource_monitor_reference.set(Arc::clone(&runtime_resource_monitor));
    // 舊版 `const integrations = [monitor, stallWatchdog, telegram?, discord?]`。
    let mut integrations: Vec<Arc<dyn ApplicationIntegration>> = Vec::new();
    integrations.push(Arc::clone(&runtime_resource_monitor) as Arc<dyn ApplicationIntegration>);
    // 舊版 `new SchedulerStallWatchdog({ scheduler, logger, intervalSeconds:
    // Math.min(60, checkIntervalSeconds), stallThresholdMs: Math.max(600_000,
    // checkIntervalSeconds * 5_000), containerRestartController })`。
    integrations.push(Arc::new(SchedulerStallWatchdog::new(
        SchedulerStallWatchdogOptions {
            scheduler: Arc::clone(&scheduler) as Arc<dyn StallSchedulerSource>,
            logger: Arc::clone(&shared_logger) as Arc<dyn StallWatchdogLogger>,
            interval_seconds: config.check_interval_seconds.min(60.0),
            stall_threshold_ms: 600_000.0f64.max(config.check_interval_seconds * 5_000.0),
            now: None,
            container_restart_controller: Some(Arc::new(ContainerRestartRequester {
                controller: Arc::clone(&container_restart_controller),
            }) as Arc<dyn StallRestartRequester>),
            exit: None,
        },
    )?) as Arc<dyn ApplicationIntegration>);

    // 舊版 `config.telegram.enabled ? [{ telegramBot = new DefaultTelegramBot({...}) }] : []`。
    if config.telegram.enabled {
        let api = Arc::new(
            TelegramApiClient::new(TelegramApiClientOptions {
                bot_token: config.telegram.bot_token.clone(),
                base_url: None,
            })
            .map_err(|error| error.to_string())?,
        ) as Arc<dyn TelegramApi>;
        let telegram_bot = Arc::new(DefaultTelegramBot::new(DefaultTelegramBotOptions {
            config: config.clone(),
            api,
            command_context: Arc::new(DefaultBotCommandContext {
                scheduler: Arc::clone(&scheduler),
                session_manager: Arc::clone(&session_manager),
                runtime_config_manager: Arc::clone(&runtime_config_manager),
            }) as Arc<dyn BotCommandContext>,
            logger: Arc::clone(&shared_logger) as Arc<dyn TelegramBotLogger>,
            sleep: None,
        }));
        bots.set_telegram(Arc::clone(&telegram_bot) as Arc<dyn TelegramBot>);
        integrations.push(Arc::clone(&telegram_bot) as Arc<dyn ApplicationIntegration>);
    }

    // 舊版 `config.discord.enabled ? [{ discordBot = new DefaultDiscordBot({...}) }] : []`。
    if config.discord.enabled {
        let api = Arc::new(
            DiscordApiClient::new(DiscordApiClientOptions {
                bot_token: config.discord.bot_token.clone(),
                base_url: None,
            })
            .map_err(|error| error.to_string())?,
        ) as Arc<dyn DiscordApi>;
        let discord_bot = Arc::new(DefaultDiscordBot::new(DefaultDiscordBotOptions {
            config: config.clone(),
            api,
            command_context: Arc::new(DefaultBotCommandContext {
                scheduler: Arc::clone(&scheduler),
                session_manager: Arc::clone(&session_manager),
                runtime_config_manager: Arc::clone(&runtime_config_manager),
            }) as Arc<dyn BotCommandContext>,
            logger: Arc::clone(&shared_logger) as Arc<dyn DiscordBotLogger>,
            gateway_factory: None,
        }));
        bots.set_discord(Arc::clone(&discord_bot) as Arc<dyn DiscordBot>);
        integrations.push(Arc::clone(&discord_bot) as Arc<dyn ApplicationIntegration>);
    }

    // 舊版 `return { browserManager, sessionManager, reconcileCoordinator,
    // scheduler, integrations, startupDiagnostics }`。
    Ok(ApplicationRuntime {
        browser_manager: Arc::clone(&browser_manager) as Arc<dyn RunnerBrowserManager>,
        session_manager: Arc::clone(&session_manager) as Arc<dyn RunnerSessionManager>,
        reconcile_coordinator: Some(
            Arc::clone(&reconcile_coordinator) as Arc<dyn ReconcileCoordinator>
        ),
        scheduler: Arc::clone(&scheduler) as Arc<dyn RunnerScheduler>,
        integrations,
        startup_diagnostics: Some(Arc::new(ConfiguredStartupDiagnostics {
            config: config.clone(),
            browser_manager: Arc::clone(&browser_manager),
            monitor: Arc::clone(&runtime_resource_monitor),
        }) as Arc<dyn StartupDiagnosticsProvider>),
    })
}

/// 舊版 `DefaultBrowserManager(config, …)` 第一參數的設定轉換
///（`Pick<AppConfig, 'headless' | 'storageStatePath' | 'browser 選項'>`）。
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

#[cfg(test)]
mod tests;
