//! `app/AppRunner.ts` 逐函式移植（§4.2，S3.6；§7.9 的啟動、停止與退出）。
//!
//! 行為照舊版（§7.9）：啟動順序 `config` → `logger` → `credential` → `runtime`
//! → `browser` → `integration`（`reconcile` coordinator 於 runtime 內）→
//! `scheduler` 並記 `service_started`；失敗時該階段記 `config_error`／
//! `service_start_failed {phase, error}`，執行 cleanup（reason
//! `startup_failed:<phase>`）後把原始錯誤丟回呼叫端。停止順序
//! `scheduler.stop` → `reconcile_coordinator.stop(reason)` →
//! `session_manager.stop_all(reason)` → `browser_manager.stop` → integration
//! 依啟動相反順序 `stop(reason)` → 記 `service_stopped {reason}` →
//! `logger.flush`（flush 失敗記 `logger_flush_failed`，用 bootstrap logger）。
//!
//! flight 語意照舊版三個 promise 欄位（`startFlight`／`stopFlight`／
//! `cleanupFlight`）：第一次呼叫登記 flight，其後的呼叫沿用同一 flight 的
//! 結果；flight 完成後**不清除**欄位——`stop` 後再 `start` 沿用原始
//! start flight，`startFlight` 為空但 `cleanupFlight` 已存在（stop 先於任何
//! start）時 `start` 拒絕 `Application cannot start after cleanup has begun`。
//!
//! 依賴以窄介面接縫注入（§4.2、DEC-020 note 1 同類）：
//! - [`RunnerConfigLoader`]／[`RunnerCredentialValidator`]／
//!   [`ApplicationLoggerFactory`]／[`RuntimeFactory`] 對映舊版 options 的四個
//!   成員（`load`／`validate`／logger 工廠／runtime 工廠）。
//! - [`RunnerBrowserManager`]／[`RunnerScheduler`]／[`RunnerSessionManager`]
//!   分別對映 `Pick<BrowserManager, 'start' | 'stop' | 診斷 getter>`、
//!   `Pick<WatchdogScheduler, 'start' | 'stop'>`、`Pick<SessionManager,
//!   'stopAll'>`（正式端以 blanket impl 轉接既有 trait／具體型別，測試端直接
//!   實作 fake，對應舊測試 harness）。
//! - [`ApplicationIntegration`] 對映舊版同名介面；[`StartupDiagnosticsProvider`]
//!   對映舊版 `RuntimeRuntime.startupDiagnostics` 的 provider 型別。
//!
//! 接縫的錯誤以訊息字串呈現（舊版 promise rejection 只用 `Error.message`：
//! `safeErrorMessage` 消毒後記 log，重新丟出的也只有同一訊息；正式端由
//! blanket impl 從各錯誤型別的 `message()`／`Display` 轉換，測試 fake 直接
//! 給舊測試的字面值）。
//! `safeErrorMessage`（[`safe_error_message`]）照舊版 AppRunner 的實作：
//! 先 [`redact_sensitive_string`]，再把 config 的六個敏感值逐字以
//! `[REDACTED]` 取代（`storageStatePath`／`twitchApi` 三值／兩個 bot token）。
//!
//! `#![allow(dead_code)]` 保留至 main 的 no-arg 啟動接線（AppRunner 與
//! `createApplication` 的正式呼叫端；下一段）：先以測試鎖定 flight 語意與
//! 各接縫形狀。
#![allow(dead_code)]
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use super::diagnostics::{
    DiagnosticLimitValue, RuntimeDiagnosticsOptions, RuntimeStartupDiagnostics,
    create_runtime_startup_diagnostics,
};
use crate::config::model::AppConfig;
use crate::logging::logger::LogEvents;
use crate::logging::redact::{SanitizedValue, redact_sensitive_string};

/// 舊版選項的 `env: NodeJS.ProcessEnv`（鍵存在即為已定義；loader 的型別）。
use crate::config::loader::ProcessEnv;

/// 舊版 `REDACTED_VALUE`（AppRunner 的 `'[REDACTED]'` 字面；功能上同
/// [`crate::logging::redact::REDACTED_VALUE`]）。
const APP_REDACTED_VALUE: &str = "[REDACTED]";

/// 舊版 `safeErrorMessage` 在 `error` 為 `undefined`／`null` 時的預設訊息。
pub(crate) const UNKNOWN_APPLICATION_FAILURE: &str = "Unknown application failure";

/// 舊版 cleanup 已開始後 `start` 的拒絕訊息。
const START_AFTER_CLEANUP_MESSAGE: &str = "Application cannot start after cleanup has begun";

// ---------------------------------------------------------------------------
// flight 共用槽位（對映舊版三個 promise 欄位；多等待者沿用同一結果）
// ---------------------------------------------------------------------------

/// 舊版 promise flight 的完成信號：第一個呼叫端登記，結果由單一 leader
/// 填入，其餘等待者等到同一結果（watch；`reconcile.rs` 的 FlightSignal
/// 前例，另帶結果值）。完成後**不清除**，沿用舊版欄位語意。
struct FlightCell<T: Clone + Send + Sync + 'static> {
    done: tokio::sync::watch::Sender<Option<T>>,
}

impl<T: Clone + Send + Sync + 'static> FlightCell<T> {
    fn new() -> Self {
        Self {
            done: tokio::sync::watch::channel(None).0,
        }
    }

    /// 等待完成並取得結果（多等待者各自拿到 clone）。
    async fn wait(&self) -> T {
        let mut receiver = self.done.subscribe();
        loop {
            // borrow_and_update 記住看過的版本；`None` 表示尚未完成。
            if let Some(value) = receiver.borrow_and_update().clone() {
                return value;
            }
            // sender 消失（runner 已 drop、runtime 結束）：等待端自持 Arc，
            // sender 至少活到等待結束；`Err` 照 `FlightSignal::wait` 前例繼續輪詢。
            if receiver.changed().await.is_err() {
                continue;
            }
        }
    }

    /// leader 完成時填入結果（`send_replace` 同時喚醒既有訂閱者）。
    fn complete(&self, value: T) {
        let _ = self.done.send_replace(Some(value));
    }
}

// ---------------------------------------------------------------------------
// 窄介面接縫
// ---------------------------------------------------------------------------

/// 舊版 `ConfigLoader` 的接縫：`load(configPath, env)`。
#[async_trait]
pub(crate) trait RunnerConfigLoader: Send + Sync {
    async fn load(&self, config_path: &str, env: &ProcessEnv) -> Result<AppConfig, String>;
}

/// 正式設定載入器的轉接：錯誤以 [`ConfigLoadError::message`] 對映（舊版
/// rejection 只有 `Error.message` 進 `safeErrorMessage`）。
#[async_trait]
impl RunnerConfigLoader for crate::config::loader::YamlConfigLoader {
    async fn load(&self, config_path: &str, env: &ProcessEnv) -> Result<AppConfig, String> {
        crate::config::loader::YamlConfigLoader::load(self, config_path, env)
            .await
            .map_err(|error| error.message().to_string())
    }
}

/// 舊版 `CredentialValidationResult`（validator 的回傳形；credentials 模組同伴型）。
pub(crate) use crate::credentials::validator::CredentialValidationResult;

/// 舊版 `CredentialValidator` 的接縫：`validate(config)`。
#[async_trait]
pub(crate) trait RunnerCredentialValidator: Send + Sync {
    async fn validate(&self, config: &AppConfig) -> Result<CredentialValidationResult, String>;
}

/// 正式憑證驗證器的轉接（同 [`RunnerConfigLoader`] 的訊息對映）。
#[async_trait]
impl RunnerCredentialValidator for crate::credentials::validator::FileCredentialValidator {
    async fn validate(&self, config: &AppConfig) -> Result<CredentialValidationResult, String> {
        crate::credentials::validator::FileCredentialValidator::validate(self, config)
            .await
            .map_err(|error| error.message().to_string())
    }
}

/// 舊版 `Logger` 用到的五個成員（`Pick<Logger, 'debug' | 'info' | 'warn' |
/// 'error' | 'flush'>`；欄位值為消毒前的 [`SanitizedValue`]）。
#[async_trait]
pub(crate) trait RunnerLogger: Send + Sync {
    fn debug(&self, event: &str, fields: Option<&SanitizedValue>);
    fn info(&self, event: &str, fields: Option<&SanitizedValue>);
    fn warn(&self, event: &str, fields: Option<&SanitizedValue>);
    fn error(&self, event: &str, fields: Option<&SanitizedValue>);

    /// 舊版 `flush(): Promise<void>`；失敗以 `Err(message)` 呈現
    ///（對映 `io::Error` 的顯示；消费者記 `logger_flush_failed`）。
    async fn flush(&self) -> Result<(), String>;
}

#[async_trait]
impl RunnerLogger for crate::logging::logger::JsonLineLogger {
    fn debug(&self, event: &str, fields: Option<&SanitizedValue>) {
        crate::logging::logger::JsonLineLogger::debug(self, event, fields);
    }

    fn info(&self, event: &str, fields: Option<&SanitizedValue>) {
        crate::logging::logger::JsonLineLogger::info(self, event, fields);
    }

    fn warn(&self, event: &str, fields: Option<&SanitizedValue>) {
        crate::logging::logger::JsonLineLogger::warn(self, event, fields);
    }

    fn error(&self, event: &str, fields: Option<&SanitizedValue>) {
        crate::logging::logger::JsonLineLogger::error(self, event, fields);
    }

    async fn flush(&self) -> Result<(), String> {
        // 舊版 flush 的 rejection 只有 message 進 log；`io::Error` 顯示即訊息。
        crate::logging::logger::JsonLineLogger::flush(self)
            .await
            .map_err(|error| error.to_string())
    }
}

/// 舊版 `ApplicationLoggerFactory`：`(config) => Logger`（同步、不可拋錯）。
pub(crate) trait ApplicationLoggerFactory: Send + Sync {
    fn create_logger(&self, config: &AppConfig) -> Arc<dyn RunnerLogger>;
}

/// 舊版 `RuntimeFactory`：`(config, logger) => ApplicationRuntime | Promise<…>`。
///
/// 舊版 rejection 的 `Error.message` 以 [`String`] 呈現；正式工廠把建構期的
/// `TwitchApiClientInitError`／`reqwest::Error` 顯示轉換進來（舊版 TypeError
/// 等在 Rust 為不可能，但介面保持可拋錯）。
#[async_trait]
pub(crate) trait RuntimeFactory: Send + Sync {
    async fn create_runtime(
        &self,
        config: &AppConfig,
        logger: Arc<dyn RunnerLogger>,
    ) -> Result<ApplicationRuntime, String>;
}

/// 舊版 `Pick<BrowserManager, 'start' | 'stop' | 診斷 getter>`：runner 啟動／
/// 清理與 diagnostics fallback 用到的成員（`createPage` 等不屬於 runner）。
#[async_trait]
pub(crate) trait RunnerBrowserManager: Send + Sync {
    /// 舊版 `start(): Promise<void>`。
    async fn start(&self) -> Result<(), String>;

    /// 舊版 `stop(): Promise<void>`。
    async fn stop(&self) -> Result<(), String>;

    /// 舊版 `getBrowserGeneration(): number`（diagnostics fallback 用）。
    fn get_browser_generation(&self) -> f64;

    /// 舊版 `getBrowserVersion(): string | null`。
    fn get_browser_version(&self) -> Option<String>;
}

#[async_trait]
impl RunnerBrowserManager for crate::browser::manager::DefaultBrowserManager {
    async fn start(&self) -> Result<(), String> {
        crate::browser::manager::BrowserManager::start(self)
            .await
            .map_err(|error| error.message)
    }

    async fn stop(&self) -> Result<(), String> {
        crate::browser::manager::BrowserManager::stop(self)
            .await
            .map_err(|error| error.message)
    }

    fn get_browser_generation(&self) -> f64 {
        crate::browser::manager::BrowserManager::get_browser_generation(self) as f64
    }

    fn get_browser_version(&self) -> Option<String> {
        crate::browser::manager::BrowserManager::get_browser_version(self)
    }
}

#[async_trait]
impl RunnerBrowserManager for Arc<dyn crate::browser::manager::BrowserManager> {
    async fn start(&self) -> Result<(), String> {
        crate::browser::manager::BrowserManager::start(self.as_ref())
            .await
            .map_err(|error| error.message)
    }

    async fn stop(&self) -> Result<(), String> {
        crate::browser::manager::BrowserManager::stop(self.as_ref())
            .await
            .map_err(|error| error.message)
    }

    fn get_browser_generation(&self) -> f64 {
        crate::browser::manager::BrowserManager::get_browser_generation(self.as_ref()) as f64
    }

    fn get_browser_version(&self) -> Option<String> {
        crate::browser::manager::BrowserManager::get_browser_version(self.as_ref())
    }
}

/// 舊版 `Pick<WatchdogScheduler, 'start' | 'stop'>`。
#[async_trait]
pub(crate) trait RunnerScheduler: Send + Sync {
    /// 舊版 `start()`（同步）。
    fn start(&self);

    /// 舊版 `stop(): Promise<void>`。
    async fn stop(&self) -> Result<(), String>;
}

#[async_trait]
impl RunnerScheduler for crate::scheduler::watchdog::DefaultWatchdogScheduler {
    fn start(&self) {
        crate::scheduler::watchdog::WatchdogScheduler::start(self);
    }

    async fn stop(&self) -> Result<(), String> {
        // 舊版 promise 可 reject；正式 scheduler 的 trait 為無失敗型別
        //（DEC-024 note 6 同款），防衛形 `Ok`。
        crate::scheduler::watchdog::WatchdogScheduler::stop(self).await;
        Ok(())
    }
}

#[async_trait]
impl RunnerScheduler for Arc<dyn crate::scheduler::watchdog::WatchdogScheduler> {
    fn start(&self) {
        crate::scheduler::watchdog::WatchdogScheduler::start(self.as_ref());
    }

    async fn stop(&self) -> Result<(), String> {
        // 同 [`DefaultWatchdogScheduler`] 的註解。
        crate::scheduler::watchdog::WatchdogScheduler::stop(self.as_ref()).await;
        Ok(())
    }
}

/// 舊版 `Pick<SessionManager, 'stopAll'>`。
#[async_trait]
pub(crate) trait RunnerSessionManager: Send + Sync {
    async fn stop_all(&self, reason: &str) -> Result<(), String>;
}

#[async_trait]
impl RunnerSessionManager for crate::sessions::manager::DefaultSessionManager {
    async fn stop_all(&self, reason: &str) -> Result<(), String> {
        // 正式 SessionManager 的 stopAll 為無失敗型別（所有清理都在內部吞錯）；
        // 舊版 promise 可 reject，防衛形 `Ok`（§7.9 的 cleanupStep 仍保同形）。
        crate::sessions::manager::SessionManager::stop_all(self, reason).await;
        Ok(())
    }
}

#[async_trait]
impl RunnerSessionManager for Arc<dyn crate::sessions::manager::SessionManager> {
    async fn stop_all(&self, reason: &str) -> Result<(), String> {
        // 同 [`DefaultSessionManager`] 的註解。
        crate::sessions::manager::SessionManager::stop_all(self.as_ref(), reason).await;
        Ok(())
    }
}

/// 舊版 `ApplicationIntegration`：整合元件（`RuntimeResourceMonitor`、
/// `SchedulerStallWatchdog`、Telegram／Discord bot）。
#[async_trait]
pub(crate) trait ApplicationIntegration: Send + Sync {
    async fn start(&self) -> Result<(), String>;
    async fn stop(&self, reason: &str) -> Result<(), String>;
}

/// 舊版 `RuntimeStartupDiagnostics` 的 provider（`runtime.startupDiagnostics?`）。
/// 舊版可為 sync 或 async、可拋錯；非同步 trait 統一，錯誤以訊息字串呈現。
#[async_trait]
pub(crate) trait StartupDiagnosticsProvider: Send + Sync {
    async fn diagnostics(&self) -> Result<RuntimeStartupDiagnostics, String>;
}

/// 舊版 `ApplicationRuntime`。`reconcileCoordinator`／`startupDiagnostics`
/// 以 [`Option`] 對映省略（舊測試 harness 不提供 integrations 時為空陣列
/// ／無 tuple）。
#[derive(Clone)]
pub(crate) struct ApplicationRuntime {
    pub(crate) browser_manager: Arc<dyn RunnerBrowserManager>,
    pub(crate) session_manager: Arc<dyn RunnerSessionManager>,
    pub(crate) reconcile_coordinator:
        Option<Arc<dyn crate::sessions::reconcile::ReconcileCoordinator>>,
    pub(crate) scheduler: Arc<dyn RunnerScheduler>,
    pub(crate) integrations: Vec<Arc<dyn ApplicationIntegration>>,
    pub(crate) startup_diagnostics: Option<Arc<dyn StartupDiagnosticsProvider>>,
}

// ---------------------------------------------------------------------------
// StartupPhase
// ---------------------------------------------------------------------------

/// 舊版 `StartupPhase` union。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StartupPhase {
    Config,
    Logger,
    Credential,
    Runtime,
    Browser,
    Integration,
    Scheduler,
}

impl StartupPhase {
    /// 舊版 union 的字面值（日誌欄位與 cleanup reason 用）。
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            StartupPhase::Config => "config",
            StartupPhase::Logger => "logger",
            StartupPhase::Credential => "credential",
            StartupPhase::Runtime => "runtime",
            StartupPhase::Browser => "browser",
            StartupPhase::Integration => "integration",
            StartupPhase::Scheduler => "scheduler",
        }
    }
}

// ---------------------------------------------------------------------------
// safeErrorMessage 的移植
// ---------------------------------------------------------------------------

/// 對應舊版 AppRunner 的 `safeErrorMessage(error, config?)`：app 模組的消毒
/// （與 `browser/safe_logging.rs` 的 channel-session 同款分層）。
///
/// - `None`：對映 `error === undefined || error === null`（default message）。
/// - `Some(message)`：對映 `Error.message`／字串／`String(error)`；舊版
///   stringify 的 try/catch 在 Rust 不會失敗，分支不存在。
/// - `config`：把 config 的六個敏感值逐字以 `[REDACTED]` 取代（空值不換）。
pub(crate) fn safe_error_message(message: Option<&str>, config: Option<&AppConfig>) -> String {
    let Some(message) = message else {
        return UNKNOWN_APPLICATION_FAILURE.to_string();
    };
    let mut safe = redact_sensitive_string(message);
    let Some(config) = config else {
        return safe;
    };

    let sensitive = [
        Some(config.twitch_api.access_token.as_str()),
        Some(config.twitch_api.client_id.as_str()),
        Some(config.twitch_api.client_secret.as_str()),
        Some(config.storage_state_path.as_str()),
        Some(config.telegram.bot_token.as_str()),
        Some(config.discord.bot_token.as_str()),
    ];
    for value in sensitive.into_iter().flatten() {
        if !value.is_empty() {
            safe = safe.replace(value, APP_REDACTED_VALUE);
        }
    }
    safe
}

// ---------------------------------------------------------------------------
// DefaultAppRunner
// ---------------------------------------------------------------------------

/// 舊版 `DefaultAppRunnerOptions`。
pub(crate) struct DefaultAppRunnerOptions {
    pub(crate) config_path: String,
    pub(crate) env: ProcessEnv,
    pub(crate) config_loader: Arc<dyn RunnerConfigLoader>,
    pub(crate) credential_validator: Arc<dyn RunnerCredentialValidator>,
    pub(crate) bootstrap_logger: Arc<dyn RunnerLogger>,
    pub(crate) logger_factory: Arc<dyn ApplicationLoggerFactory>,
    pub(crate) runtime_factory: Arc<dyn RuntimeFactory>,
}

/// runner 的共享狀態（舊版私有欄位群）；std 鎖、鎖內無 await。
struct RunnerState {
    config: Option<Arc<AppConfig>>,
    logger: Option<Arc<dyn RunnerLogger>>,
    runtime: Option<ApplicationRuntime>,
    start_flight: Option<Arc<FlightCell<Result<(), String>>>>,
    stop_flight: Option<Arc<FlightCell<Result<(), String>>>>,
    cleanup_flight: Option<Arc<FlightCell<()>>>,
}

/// 舊版 `AppRunner` 介面。
///
/// `start` 的 rejection 以 [`String`] 呈現（原始錯誤的 `Error.message`；
/// 舊版呼叫端 `index.ts` 只把 `exitCode` 設 1，不讀內容）。`stop` 的
/// rejection 對映 `service_stop_failed {reason, error}` 的防衛路徑（§7.9）。
#[async_trait]
pub(crate) trait AppRunner: Send + Sync {
    async fn start(&self) -> Result<(), String>;
    async fn stop(&self, reason: &str) -> Result<(), String>;
}

/// 舊版 `DefaultAppRunner`。
pub(crate) struct DefaultAppRunner {
    options: DefaultAppRunnerOptions,
    state: Mutex<RunnerState>,
}

impl DefaultAppRunner {
    pub(crate) fn new(options: DefaultAppRunnerOptions) -> Self {
        Self {
            options,
            state: Mutex::new(RunnerState {
                config: None,
                logger: None,
                runtime: None,
                start_flight: None,
                stop_flight: None,
                cleanup_flight: None,
            }),
        }
    }

    /// 舊版 `currentLogger()`。
    fn current_logger(&self) -> Arc<dyn RunnerLogger> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .logger
            .clone()
            .unwrap_or_else(|| Arc::clone(&self.options.bootstrap_logger))
    }

    /// 舊版 catch 路徑的 `this.config`（cleanupStep 的消毒輸入）。
    fn current_config(&self) -> Option<Arc<AppConfig>> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .config
            .clone()
    }

    /// 舊版 `startApplication()` 的 catch 分支：記 `config_error`／
    /// `service_start_failed {phase, error}`、cleanup、丟回原始錯誤。
    async fn handle_start_failure(&self, phase: StartupPhase, message: String) -> String {
        let logger = self.current_logger();
        let config = self.current_config();
        let error = safe_error_message(Some(&message), config.as_deref());
        let event = if phase == StartupPhase::Config {
            LogEvents::CONFIG_ERROR
        } else {
            "service_start_failed"
        };
        log_runner(
            logger.as_ref(),
            crate::logging::logger::LogLevel::Error,
            event,
            Some(&SanitizedValue::Object(vec![
                (
                    "phase".to_string(),
                    SanitizedValue::String(phase.as_str().to_string()),
                ),
                ("error".to_string(), SanitizedValue::String(error)),
            ])),
        );
        self.cleanup(&format!("startup_failed:{}", phase.as_str()))
            .await;
        message
    }

    /// 舊版 `startApplication()` 的 try 本體（階段順照 §7.9）。
    async fn start_phases(&self, phase: &mut StartupPhase) -> Result<(), String> {
        // config
        let config = Arc::new(
            self.options
                .config_loader
                .load(&self.options.config_path, &self.options.env)
                .await?,
        );
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .config = Some(Arc::clone(&config));

        // logger
        *phase = StartupPhase::Logger;
        let logger = self.options.logger_factory.create_logger(&config);
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .logger = Some(Arc::clone(&logger));
        log_runner(
            logger.as_ref(),
            crate::logging::logger::LogLevel::Info,
            LogEvents::CONFIG_LOADED,
            Some(&SanitizedValue::Object(vec![
                (
                    "channelCount".to_string(),
                    SanitizedValue::Number(config.channels.len() as f64),
                ),
                (
                    "maxConcurrentStreams".to_string(),
                    SanitizedValue::Number(config.max_concurrent_streams),
                ),
            ])),
        );

        // credential
        *phase = StartupPhase::Credential;
        let credential = self.options.credential_validator.validate(&config).await?;
        if !credential.has_cookies {
            log_runner(
                logger.as_ref(),
                crate::logging::logger::LogLevel::Warn,
                "credential_storage_state_empty_cookies",
                Some(&SanitizedValue::Object(vec![])),
            );
        }
        log_runner(
            logger.as_ref(),
            crate::logging::logger::LogLevel::Info,
            LogEvents::CREDENTIAL_CHECKED,
            Some(&SanitizedValue::Object(vec![
                (
                    "hasCookies".to_string(),
                    SanitizedValue::Bool(credential.has_cookies),
                ),
                (
                    "twitchApiConfigured".to_string(),
                    SanitizedValue::Bool(credential.twitch_api_configured),
                ),
            ])),
        );

        // runtime
        *phase = StartupPhase::Runtime;
        let runtime = self
            .options
            .runtime_factory
            .create_runtime(&config, Arc::clone(&logger))
            .await?;
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .runtime = Some(runtime.clone());

        // browser
        *phase = StartupPhase::Browser;
        runtime.browser_manager.start().await?;

        // integration
        *phase = StartupPhase::Integration;
        for integration in &runtime.integrations {
            integration.start().await?;
        }

        // scheduler
        *phase = StartupPhase::Scheduler;
        runtime.scheduler.start();

        let startup_diagnostics = self
            .resolve_startup_diagnostics(&config, &runtime, logger.as_ref())
            .await;
        let mut fields = vec![(
            "channelCount".to_string(),
            SanitizedValue::Number(config.channels.len() as f64),
        )];
        fields.extend(diagnostics_fields(&startup_diagnostics));
        log_runner(
            logger.as_ref(),
            crate::logging::logger::LogLevel::Info,
            LogEvents::SERVICE_STARTED,
            Some(&SanitizedValue::Object(fields)),
        );
        Ok(())
    }

    /// 舊版 `startApplication()`；catch 的 cleanup 與丟出在本方法收尾。
    async fn start_application(&self) -> Result<(), String> {
        let mut phase = StartupPhase::Config;
        match self.start_phases(&mut phase).await {
            Ok(()) => Ok(()),
            Err(message) => Err(self.handle_start_failure(phase, message).await),
        }
    }

    /// 舊版 `stopApplication(reason)`：等 in-flight start（吞錯）後 cleanup。
    async fn stop_application(&self, reason: &str) -> Result<(), String> {
        // 舊版 `await this.startFlight?.catch(() => undefined)`：沿用既有
        // start flight 的最終結果並丟棄。
        let start_flight = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .start_flight
            .clone();
        if let Some(flight) = start_flight {
            let _ = flight.wait().await;
        }
        // 舊版 cleanup 永遠 resolve（元件失敗都吞成 component_failed 記點）；
        // Err 形為 §7.9 `service_stop_failed` 的防衛路徑。
        self.cleanup(reason).await;
        Ok(())
    }

    /// 舊版 `cleanup(reason)`：cleanupFlight 只登記一次（首個 reason 為準）。
    async fn cleanup(&self, reason: &str) {
        enum Leader {
            Initiate(Arc<FlightCell<()>>),
            Follow(Arc<FlightCell<()>>),
        }
        let choice = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(flight) = state.cleanup_flight.clone() {
                Leader::Follow(flight)
            } else {
                let flight = Arc::new(FlightCell::<()>::new());
                state.cleanup_flight = Some(Arc::clone(&flight));
                Leader::Initiate(flight)
            }
        };
        match choice {
            Leader::Initiate(flight) => {
                self.perform_cleanup(reason).await;
                flight.complete(());
            }
            // 舊版回傳既有 flight（reason 忽略）。
            Leader::Follow(flight) => {
                flight.wait().await;
            }
        }
    }

    /// 舊版 `performCleanup(reason)`。
    async fn perform_cleanup(&self, reason: &str) {
        let runtime = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .runtime
            .clone();
        let logger = self.current_logger();

        if let Some(runtime) = runtime {
            let scheduler = runtime.scheduler.clone();
            cleanup_step(
                logger.as_ref(),
                "scheduler",
                self.current_config().as_deref(),
                async { scheduler.stop().await },
            )
            .await;
            if let Some(reconcile_coordinator) = &runtime.reconcile_coordinator {
                // 舊版 `reconcileCoordinator.stop(reason)` 可 reject；正式
                // trait 為無失敗型別（DEC-024 note 6），防衛形 `Ok`。
                let coordinator = Arc::clone(reconcile_coordinator);
                let stop_reason = reason.to_string();
                cleanup_step(
                    logger.as_ref(),
                    "reconcile_coordinator",
                    self.current_config().as_deref(),
                    async move {
                        crate::sessions::reconcile::ReconcileCoordinator::stop(
                            coordinator.as_ref(),
                            &stop_reason,
                        )
                        .await;
                        Ok(())
                    },
                )
                .await;
            }
            let session_manager = runtime.session_manager.clone();
            let stop_reason = reason.to_string();
            cleanup_step(
                logger.as_ref(),
                "session_manager",
                self.current_config().as_deref(),
                async move { session_manager.stop_all(&stop_reason).await },
            )
            .await;
            let browser_manager = runtime.browser_manager.clone();
            cleanup_step(
                logger.as_ref(),
                "browser",
                self.current_config().as_deref(),
                async move { browser_manager.stop().await },
            )
            .await;
            for integration in runtime.integrations.iter().rev() {
                let integration = Arc::clone(integration);
                let stop_reason = reason.to_string();
                cleanup_step(
                    logger.as_ref(),
                    "integration",
                    self.current_config().as_deref(),
                    async move { integration.stop(&stop_reason).await },
                )
                .await;
            }
        }

        log_runner(
            logger.as_ref(),
            crate::logging::logger::LogLevel::Info,
            LogEvents::SERVICE_STOPPED,
            Some(&SanitizedValue::Object(vec![(
                "reason".to_string(),
                SanitizedValue::String(reason.to_string()),
            )])),
        );

        // 舊版 try { await logger.flush() } catch → bootstrapLogger 記
        // `logger_flush_failed`（§7.9）。
        let config = self.current_config();
        if let Err(message) = logger.flush().await {
            let error = safe_error_message(Some(&message), config.as_deref());
            log_runner(
                self.options.bootstrap_logger.as_ref(),
                crate::logging::logger::LogLevel::Error,
                "logger_flush_failed",
                Some(&SanitizedValue::Object(vec![(
                    "error".to_string(),
                    SanitizedValue::String(error),
                )])),
            );
        }
    }

    /// 舊版 `resolveStartupDiagnostics(config, runtime, logger)`。
    async fn resolve_startup_diagnostics(
        &self,
        config: &AppConfig,
        runtime: &ApplicationRuntime,
        logger: &dyn RunnerLogger,
    ) -> RuntimeStartupDiagnostics {
        let browser_bridge = RunnerBrowserManagerBridge(runtime.browser_manager.as_ref());
        let fallback = create_runtime_startup_diagnostics(&RuntimeDiagnosticsOptions {
            config,
            browser_manager: &browser_bridge,
            resource_limits: None,
            env: Some(&self.options.env),
            browser_engine: Some(config.browser.engine.as_str()),
            run_id: None,
            node_version: None,
        });
        let Some(provider) = &runtime.startup_diagnostics else {
            return fallback;
        };
        match provider.diagnostics().await {
            Ok(diagnostics) => diagnostics,
            Err(message) => {
                let error = safe_error_message(Some(&message), Some(config));
                log_runner(
                    logger,
                    crate::logging::logger::LogLevel::Warn,
                    "runtime_diagnostics_failed",
                    Some(&SanitizedValue::Object(vec![(
                        "error".to_string(),
                        SanitizedValue::String(error),
                    )])),
                );
                fallback
            }
        }
    }
}

/// 把 [`RunnerBrowserManager`]（trait 物件）轉成 diagnostics 需要的
/// `&dyn RuntimeDiagnosticsBrowser`（兩個 getter 的形狀一致；借跟本呼叫）。
struct RunnerBrowserManagerBridge<'a>(&'a dyn RunnerBrowserManager);

impl super::diagnostics::RuntimeDiagnosticsBrowser for RunnerBrowserManagerBridge<'_> {
    fn get_browser_generation(&self) -> f64 {
        RunnerBrowserManager::get_browser_generation(self.0)
    }

    fn get_browser_version(&self) -> Option<String> {
        RunnerBrowserManager::get_browser_version(self.0)
    }
}

/// 舊版 `cleanupStep(logger, component, operation, config)`：單一元件的
///停止失敗記 `service_stop_component_failed` 後繼續（§7.9）。`operation`
/// 已在呼叫端展開（對映 promise 當下接上尾巴）。
async fn cleanup_step(
    logger: &dyn RunnerLogger,
    component: &str,
    config: Option<&AppConfig>,
    operation: impl std::future::Future<Output = Result<(), String>>,
) {
    if let Err(message) = operation.await {
        let error = safe_error_message(Some(&message), config);
        log_runner(
            logger,
            crate::logging::logger::LogLevel::Error,
            "service_stop_component_failed",
            Some(&SanitizedValue::Object(vec![
                (
                    "component".to_string(),
                    SanitizedValue::String(component.to_string()),
                ),
                ("error".to_string(), SanitizedValue::String(error)),
            ])),
        );
    }
}

/// 舊版 AppRunner 的 `safeLog`：日誌不得中斷啟動或清理。正式 logger 的各層級
/// 呼叫不會失敗（寫入錯誤照舊版忽略），try/catch 的對應即「不回錯」。
fn log_runner(
    logger: &dyn RunnerLogger,
    level: crate::logging::logger::LogLevel,
    event: &str,
    fields: Option<&SanitizedValue>,
) {
    match level {
        crate::logging::logger::LogLevel::Debug => logger.debug(event, fields),
        crate::logging::logger::LogLevel::Info => logger.info(event, fields),
        crate::logging::logger::LogLevel::Warn => logger.warn(event, fields),
        crate::logging::logger::LogLevel::Error => logger.error(event, fields),
    }
}

/// `RuntimeStartupDiagnostics` → `service_started` 的欄位（順序照舊版物件：
/// `channelCount` 先行，其餘為 diagnostics 的欄位順序；省略值為舊版 null）。
fn diagnostics_fields(diagnostics: &RuntimeStartupDiagnostics) -> Vec<(String, SanitizedValue)> {
    let limit_value = |value: &DiagnosticLimitValue| match value {
        DiagnosticLimitValue::Number(number) => SanitizedValue::Number(*number),
        DiagnosticLimitValue::Text(text) => SanitizedValue::String(text.clone()),
        DiagnosticLimitValue::Null => SanitizedValue::Null,
    };
    let optional = |value: &Option<String>| match value {
        Some(text) => SanitizedValue::String(text.clone()),
        None => SanitizedValue::Null,
    };
    vec![
        (
            "runId".to_string(),
            SanitizedValue::String(diagnostics.run_id.clone()),
        ),
        (
            "applicationVersion".to_string(),
            optional(&diagnostics.application_version),
        ),
        ("gitCommit".to_string(), optional(&diagnostics.git_commit)),
        (
            "nodeVersion".to_string(),
            SanitizedValue::String(diagnostics.node_version.clone()),
        ),
        (
            "playwrightVersion".to_string(),
            optional(&diagnostics.playwright_version),
        ),
        (
            "browserEngine".to_string(),
            SanitizedValue::String(diagnostics.browser_engine.clone()),
        ),
        (
            "browserVersion".to_string(),
            optional(&diagnostics.browser_version),
        ),
        (
            "browserGeneration".to_string(),
            SanitizedValue::Number(diagnostics.browser_generation),
        ),
        (
            "memoryMaxBytes".to_string(),
            limit_value(&diagnostics.memory_max_bytes),
        ),
        ("pidsMax".to_string(), limit_value(&diagnostics.pids_max)),
        (
            "safeConfigFingerprint".to_string(),
            SanitizedValue::String(diagnostics.safe_config_fingerprint.clone()),
        ),
    ]
}

#[async_trait]
impl AppRunner for DefaultAppRunner {
    /// 舊版 `start()`：`startFlight` 已存在時沿用原始 flight（含完成後的
    /// 結果）；cleanup 已開始而無 startFlight 時拒絕。
    async fn start(&self) -> Result<(), String> {
        enum Steady {
            Leader(Arc<FlightCell<Result<(), String>>>),
            Follow(Arc<FlightCell<Result<(), String>>>),
        }
        let choice = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(flight) = state.start_flight.clone() {
                Steady::Follow(flight)
            } else if state.cleanup_flight.is_some() {
                drop(state);
                return Err(START_AFTER_CLEANUP_MESSAGE.to_string());
            } else {
                let flight = Arc::new(FlightCell::<Result<(), String>>::new());
                state.start_flight = Some(Arc::clone(&flight));
                Steady::Leader(flight)
            }
        };
        match choice {
            Steady::Leader(flight) => {
                let result = self.start_application().await;
                flight.complete(result.clone());
                result
            }
            Steady::Follow(flight) => flight.wait().await,
        }
    }

    /// 舊版 `stop(reason)`：`stopFlight` 已存在時沿用原始 flight（含完成後
    /// 的結果）；stop 只釋放一次資源。
    async fn stop(&self, reason: &str) -> Result<(), String> {
        enum Steady {
            Leader(Arc<FlightCell<Result<(), String>>>),
            Follow(Arc<FlightCell<Result<(), String>>>),
        }
        let choice = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(flight) = state.stop_flight.clone() {
                Steady::Follow(flight)
            } else {
                let flight = Arc::new(FlightCell::<Result<(), String>>::new());
                state.stop_flight = Some(Arc::clone(&flight));
                Steady::Leader(flight)
            }
        };
        match choice {
            Steady::Leader(flight) => {
                let result = self.stop_application(reason).await;
                flight.complete(result.clone());
                result
            }
            Steady::Follow(flight) => flight.wait().await,
        }
    }
}

#[cfg(test)]
mod tests;
