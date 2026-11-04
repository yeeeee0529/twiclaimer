//! `DefaultAppRunner` 的單元測試：移植舊版 `test/unit/app-runner.test.ts` 的
//! 11 個案例（標題依 `doc/spec/ts-test-titles.tsv` 第 32–44 列逐字；第 43 列
//! 「未處理 rejection」依 X6 不移植，見 `doc/test-port.md`；第 44 列的
//! `預設 RuntimeFactory` 在 `src/app/create/tests.rs`）。
//!
//! fake 照舊測試 harness 逐一對應（`createHarness`／`createTestLogger`／
//! fake config loader／credential validator／browser manager／session
//! manager／scheduler／`FakeProcessTarget`）；`vi.waitFor` 以 [`eventually`]
//! 輪詢對應（單元測試不用真實睡眠）。
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures_util::future::join_all;

use super::{
    AppRunner, ApplicationIntegration, ApplicationLoggerFactory, ApplicationRuntime,
    DefaultAppRunner, DefaultAppRunnerOptions, RunnerBrowserManager, RunnerConfigLoader,
    RunnerCredentialValidator, RunnerLogger, RunnerScheduler, RunnerSessionManager, RuntimeFactory,
    StartupDiagnosticsProvider,
};
use crate::app::process::{
    InstallProcessHandlersOptions, ProcessHandlerTarget, ProcessSignal, ProcessSignalReceiver,
    SignalDelivery, install_process_handlers,
};
use crate::config::loader::ProcessEnv;
use crate::config::model::{
    AppConfig, BrowserConfig, BrowserEngine, BrowserRecoveryConfig, BrowserSessionStartConfig,
    DiscordConfig, ResourceGuardConfig, ResourceGuardEffectiveThresholds, StreamQuality,
    TelegramConfig, TwitchApiConfig,
};
use crate::credentials::validator::CredentialValidationResult;
use crate::logging::logger::LogLevel;
use crate::logging::redact::SanitizedValue;

/// 舊測試常數。
const CONFIG_PATH: &str = "/test/config.yml";
const STORAGE_STATE_PATH: &str = "/private/storage-state.json";
const ACCESS_TOKEN: &str = "highly-sensitive-access-token";

// ---------------------------------------------------------------------------
// 斷言輔助（同 sessions/manager/tests.rs 的寫法）
// ---------------------------------------------------------------------------

/// 斷言側的值：[`SanitizedValue`] → JSON 值（與 logger 儲存的形狀相同）。
fn to_json(value: &SanitizedValue) -> serde_json::Value {
    serde_json::from_str(&value.to_json_string()).unwrap_or(serde_json::Value::Null)
}

/// 對應 `expect.objectContaining({...})`：expected 的每個鍵值都要出現。
fn object_contains(
    expected: &[(&'static str, serde_json::Value)],
    actual: &serde_json::Value,
) -> bool {
    let Some(entries) = actual.as_object() else {
        return false;
    };
    expected
        .iter()
        .all(|(key, value)| entries.get(*key) == Some(value))
}

/// 對應舊測試的 `vi.waitFor`：輪詢條件（單元測試不等真實時間）。
async fn eventually(condition: impl Fn() -> bool) -> bool {
    for _ in 0..1_000 {
        if condition() {
            return true;
        }
        tokio::task::yield_now().await;
    }
    false
}

/// runId 的舊版形狀：`/^[A-Za-z0-9-]{8,64}$/u`。
fn is_run_id(value: &serde_json::Value) -> bool {
    let Some(text) = value.as_str() else {
        return false;
    };
    (8..=64).contains(&text.len()) && text.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// safeConfigFingerprint 的舊版形狀：64 個小寫十六進位字元。
fn is_fingerprint(value: &serde_json::Value) -> bool {
    let Some(text) = value.as_str() else {
        return false;
    };
    text.len() == 64
        && text
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

// ---------------------------------------------------------------------------
// 測試常數與 CONFIG（對應舊測試的 CONFIG 物件）
// ---------------------------------------------------------------------------

fn default_resource_guard() -> ResourceGuardConfig {
    ResourceGuardConfig {
        enabled: false,
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
        effective: ResourceGuardEffectiveThresholds {
            max_concurrent_streams: 2.0,
            warning_memory_mib: 2_901.0,
            warning_reset_memory_mib: 2_731.0,
            browser_recycle_memory_mib: 3_243.0,
            emergency_memory_mib: 3_755.0,
            post_recycle_target_memory_mib: 2_901.0,
        },
    }
}

fn default_recovery() -> BrowserRecoveryConfig {
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

fn default_session_start() -> BrowserSessionStartConfig {
    BrowserSessionStartConfig {
        failure_backoff_seconds: vec![30.0, 60.0, 120.0],
        maximum_cooldown_seconds: 900.0,
    }
}

/// 對應舊測試的 `CONFIG`（AppConfig 完整形；fingerprint 只驗輸出形狀）。
fn test_config() -> AppConfig {
    AppConfig {
        channels: vec!["first".to_string(), "second".to_string()],
        check_interval_seconds: 60.0,
        max_concurrent_streams: 2.0,
        headless: true,
        storage_state_path: STORAGE_STATE_PATH.to_string(),
        log_level: LogLevel::Info,
        twitch_api: TwitchApiConfig {
            client_id: "test-client".to_string(),
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
            recovery: default_recovery(),
            session_start: default_session_start(),
            resource_guard: default_resource_guard(),
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

/// 舊測試的 `CREDENTIAL_RESULT`。
fn credential_result() -> CredentialValidationResult {
    CredentialValidationResult {
        storage_state_path: STORAGE_STATE_PATH.to_string(),
        has_cookies: true,
        twitch_api_configured: true,
    }
}

// ---------------------------------------------------------------------------
// fake seams（舊測試 harness 對映；fake一律放測試模組內）
// ---------------------------------------------------------------------------

type Lifecycle = Arc<Mutex<Vec<String>>>;

fn lifecycle_push(lifecycle: &Lifecycle, event: &str) {
    lifecycle
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(event.to_string());
}

fn lifecycle_events(lifecycle: &Lifecycle) -> Vec<String> {
    lifecycle
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

fn lifecycle_clear(lifecycle: &Lifecycle) {
    lifecycle
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
}

/// 對應舊測試 `createTestLogger(lifecycleEvents, name)`：各層級記錄事件與
/// 欄位（消毒前的 [`SanitizedValue`]），flush 記進 lifecycle。
struct FakeRunnerLogger {
    name: String,
    lifecycle: Lifecycle,
    entries: Mutex<Vec<(LogLevel, String, serde_json::Value)>>,
    flush_calls: AtomicUsize,
}

impl FakeRunnerLogger {
    fn new(lifecycle: Lifecycle, name: &str) -> Arc<Self> {
        Arc::new(Self {
            name: name.to_string(),
            lifecycle,
            entries: Mutex::new(Vec::new()),
            flush_calls: AtomicUsize::new(0),
        })
    }

    fn record(&self, level: LogLevel, event: &str, fields: Option<&SanitizedValue>) {
        let value = match fields {
            Some(fields) => to_json(fields),
            None => serde_json::Value::Object(serde_json::Map::new()),
        };
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push((level, event.to_string(), value));
    }

    /// 對應 `logger[level].mock.calls.map(([event]) => event)`。
    fn events_of(&self, level: LogLevel) -> Vec<String> {
        self.calls_of(level)
            .into_iter()
            .map(|(event, _)| event)
            .collect()
    }

    /// 對應 `logger[level].mock.calls`（事件與欄位）。
    fn calls_of(&self, level: LogLevel) -> Vec<(String, serde_json::Value)> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .filter(|(entry_level, _, _)| *entry_level == level)
            .map(|(_, event, value)| (event.clone(), value.clone()))
            .collect()
    }

    fn flush_count(&self) -> usize {
        self.flush_calls.load(Ordering::SeqCst)
    }

    /// 對應 `JSON.stringify(logger[level].mock.calls)`：全部記錄的序列化。
    fn serialized(&self) -> String {
        let entries = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        serde_json::to_string(
            &entries
                .iter()
                .map(|(_, event, value)| (event.clone(), value.clone()))
                .collect::<Vec<_>>(),
        )
        .unwrap_or_default()
    }
}

#[async_trait]
impl RunnerLogger for FakeRunnerLogger {
    fn debug(&self, event: &str, fields: Option<&SanitizedValue>) {
        self.record(LogLevel::Debug, event, fields);
    }

    fn info(&self, event: &str, fields: Option<&SanitizedValue>) {
        self.record(LogLevel::Info, event, fields);
    }

    fn warn(&self, event: &str, fields: Option<&SanitizedValue>) {
        self.record(LogLevel::Warn, event, fields);
    }

    fn error(&self, event: &str, fields: Option<&SanitizedValue>) {
        self.record(LogLevel::Error, event, fields);
    }

    async fn flush(&self) -> Result<(), String> {
        self.flush_calls.fetch_add(1, Ordering::SeqCst);
        lifecycle_push(&self.lifecycle, &format!("{}.flush", self.name));
        Ok(())
    }
}

/// 對應舊測試的 fake `ConfigLoader`：記下呼叫參數，錯誤時以訊息失敗。
struct FakeConfigLoader {
    lifecycle: Lifecycle,
    load_calls: Mutex<Vec<(String, ProcessEnv)>>,
    load_error: Option<String>,
}

#[async_trait]
impl RunnerConfigLoader for FakeConfigLoader {
    async fn load(&self, config_path: &str, env: &ProcessEnv) -> Result<AppConfig, String> {
        lifecycle_push(&self.lifecycle, "config.load");
        self.load_calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push((config_path.to_string(), env.clone()));
        match &self.load_error {
            Some(message) => Err(message.clone()),
            None => Ok(test_config()),
        }
    }
}

/// 對應舊測試的 fake `CredentialValidator`。
struct FakeCredentialValidator {
    lifecycle: Lifecycle,
    validate_calls: AtomicUsize,
    validate_error: Option<String>,
}

#[async_trait]
impl RunnerCredentialValidator for FakeCredentialValidator {
    async fn validate(&self, _config: &AppConfig) -> Result<CredentialValidationResult, String> {
        lifecycle_push(&self.lifecycle, "credential.validate");
        self.validate_calls.fetch_add(1, Ordering::SeqCst);
        match &self.validate_error {
            Some(message) => Err(message.clone()),
            None => Ok(credential_result()),
        }
    }
}

/// 對應舊測試的 fake `loggerFactory`：回傳 harness 的 configured logger。
struct FakeLoggerFactory {
    lifecycle: Lifecycle,
    create_calls: AtomicUsize,
    logger: Arc<FakeRunnerLogger>,
}

impl ApplicationLoggerFactory for FakeLoggerFactory {
    fn create_logger(&self, _config: &AppConfig) -> Arc<dyn RunnerLogger> {
        lifecycle_push(&self.lifecycle, "logger.create");
        self.create_calls.fetch_add(1, Ordering::SeqCst);
        Arc::clone(&self.logger) as Arc<dyn RunnerLogger>
    }
}

/// 對應舊測試 `createBrowserManager`。
struct FakeRunnerBrowserManager {
    lifecycle: Lifecycle,
    start_calls: AtomicUsize,
    start_error: Option<String>,
    stop_calls: AtomicUsize,
}

#[async_trait]
impl RunnerBrowserManager for FakeRunnerBrowserManager {
    async fn start(&self) -> Result<(), String> {
        lifecycle_push(&self.lifecycle, "browser.start");
        self.start_calls.fetch_add(1, Ordering::SeqCst);
        match &self.start_error {
            Some(message) => Err(message.clone()),
            None => Ok(()),
        }
    }

    async fn stop(&self) -> Result<(), String> {
        lifecycle_push(&self.lifecycle, "browser.stop");
        self.stop_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn get_browser_generation(&self) -> f64 {
        1.0
    }

    fn get_browser_version(&self) -> Option<String> {
        Some("test-browser-1.0".to_string())
    }
}

/// 對應舊測試 `createSessionManager`。
struct FakeRunnerSessionManager {
    lifecycle: Lifecycle,
    stop_all_calls: Mutex<Vec<String>>,
}

#[async_trait]
impl RunnerSessionManager for FakeRunnerSessionManager {
    async fn stop_all(&self, reason: &str) -> Result<(), String> {
        lifecycle_push(&self.lifecycle, &format!("sessionManager.stopAll:{reason}"));
        self.stop_all_calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(reason.to_string());
        Ok(())
    }
}

impl FakeRunnerSessionManager {
    fn stop_all_reasons(&self) -> Vec<String> {
        self.stop_all_calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

/// 對應舊測試 `createScheduler`。
struct FakeRunnerScheduler {
    lifecycle: Lifecycle,
    start_calls: AtomicUsize,
    stop_calls: AtomicUsize,
}

#[async_trait]
impl RunnerScheduler for FakeRunnerScheduler {
    fn start(&self) {
        lifecycle_push(&self.lifecycle, "scheduler.start");
        self.start_calls.fetch_add(1, Ordering::SeqCst);
    }

    async fn stop(&self) -> Result<(), String> {
        lifecycle_push(&self.lifecycle, "scheduler.stop");
        self.stop_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// 對應舊測試第 8 案的 fake integration。
struct FakeIntegration {
    lifecycle: Lifecycle,
}

#[async_trait]
impl ApplicationIntegration for FakeIntegration {
    async fn start(&self) -> Result<(), String> {
        lifecycle_push(&self.lifecycle, "integration.start");
        Ok(())
    }

    async fn stop(&self, reason: &str) -> Result<(), String> {
        lifecycle_push(&self.lifecycle, &format!("integration.stop:{reason}"));
        Ok(())
    }
}

/// 對應舊測試 fake 的 `startupDiagnostics` provider（canned 或 Err）。
struct FakeStartupDiagnostics {
    /// `Err` 的訊息；`None` 時回 canned 結果（舊測試的 provider 物件）。
    error: Option<String>,
}

#[async_trait]
impl StartupDiagnosticsProvider for FakeStartupDiagnostics {
    async fn diagnostics(&self) -> Result<super::RuntimeStartupDiagnostics, String> {
        match &self.error {
            Some(message) => Err(message.clone()),
            None => Ok(super::RuntimeStartupDiagnostics {
                run_id: "provider-run-1234".to_string(),
                application_version: Some("9.9.9".to_string()),
                git_commit: Some("abcdef0".to_string()),
                node_version: "v99.0.0".to_string(),
                playwright_version: Some("1.62.1".to_string()),
                browser_engine: "firefox".to_string(),
                browser_version: Some("provider-browser".to_string()),
                browser_generation: 42.0,
                memory_max_bytes: super::DiagnosticLimitValue::Number(6_442_450_944.0),
                pids_max: super::DiagnosticLimitValue::Number(512.0),
                safe_config_fingerprint: "f".repeat(64),
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// harness（對應舊測試 createHarness／fake RuntimeFactory 的可 mock 形狀）
// ---------------------------------------------------------------------------

fn mock_runtime(
    factory: &FakeRuntimeFactory,
    integrations: Vec<Arc<dyn ApplicationIntegration>>,
    startup_diagnostics: Option<Arc<dyn StartupDiagnosticsProvider>>,
) {
    factory.mock_runtime(integrations, startup_diagnostics);
}
/// 對應舊測試 fake 的 `RuntimeFactory`（可換 integrations／startupDiagnostics，
/// 對映 `runtimeFactory.mockResolvedValue`）。
struct FakeRuntimeFactory {
    lifecycle: Lifecycle,
    create_calls: AtomicUsize,
    integrations: Mutex<Option<Vec<Arc<dyn ApplicationIntegration>>>>,
    startup_diagnostics: Mutex<Option<Arc<dyn StartupDiagnosticsProvider>>>,
    browser_manager: Arc<FakeRunnerBrowserManager>,
    session_manager: Arc<FakeRunnerSessionManager>,
    scheduler: Arc<FakeRunnerScheduler>,
}

impl FakeRuntimeFactory {
    fn mock_runtime(
        &self,
        integrations: Vec<Arc<dyn ApplicationIntegration>>,
        startup_diagnostics: Option<Arc<dyn StartupDiagnosticsProvider>>,
    ) {
        *self
            .integrations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(integrations);
        *self
            .startup_diagnostics
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = startup_diagnostics;
    }
}

#[async_trait]
impl RuntimeFactory for FakeRuntimeFactory {
    async fn create_runtime(
        &self,
        _config: &AppConfig,
        _logger: Arc<dyn RunnerLogger>,
    ) -> Result<ApplicationRuntime, String> {
        lifecycle_push(&self.lifecycle, "runtime.create");
        self.create_calls.fetch_add(1, Ordering::SeqCst);
        let integrations = self
            .integrations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
            .unwrap_or_default();
        let startup_diagnostics = self
            .startup_diagnostics
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        Ok(ApplicationRuntime {
            browser_manager: Arc::clone(&self.browser_manager) as Arc<dyn RunnerBrowserManager>,
            session_manager: Arc::clone(&self.session_manager) as Arc<dyn RunnerSessionManager>,
            reconcile_coordinator: None,
            scheduler: Arc::clone(&self.scheduler) as Arc<dyn RunnerScheduler>,
            integrations,
            startup_diagnostics,
        })
    }
}

#[derive(Default)]
struct HarnessOptions {
    config_error: Option<String>,
    credential_error: Option<String>,
    browser_start_error: Option<String>,
    startup_diagnostics: Option<FakeStartupDiagnostics>,
}

/// 對應舊測試 `createHarness`。
struct Harness {
    app: Arc<DefaultAppRunner>,
    lifecycle: Lifecycle,
    bootstrap_logger: Arc<FakeRunnerLogger>,
    logger: Arc<FakeRunnerLogger>,
    config_loader: Arc<FakeConfigLoader>,
    credential_validator: Arc<FakeCredentialValidator>,
    logger_factory: Arc<FakeLoggerFactory>,
    runtime_factory: Arc<FakeRuntimeFactory>,
    browser_manager: Arc<FakeRunnerBrowserManager>,
    session_manager: Arc<FakeRunnerSessionManager>,
    scheduler: Arc<FakeRunnerScheduler>,
}

fn create_harness_with(options: HarnessOptions) -> Harness {
    let lifecycle: Lifecycle = Arc::new(Mutex::new(Vec::new()));
    let bootstrap_logger = FakeRunnerLogger::new(Arc::clone(&lifecycle), "bootstrap");
    let logger = FakeRunnerLogger::new(Arc::clone(&lifecycle), "configured");
    let config_loader = Arc::new(FakeConfigLoader {
        lifecycle: Arc::clone(&lifecycle),
        load_calls: Mutex::new(Vec::new()),
        load_error: options.config_error,
    });
    let credential_validator = Arc::new(FakeCredentialValidator {
        lifecycle: Arc::clone(&lifecycle),
        validate_calls: AtomicUsize::new(0),
        validate_error: options.credential_error,
    });
    let browser_manager = Arc::new(FakeRunnerBrowserManager {
        lifecycle: Arc::clone(&lifecycle),
        start_calls: AtomicUsize::new(0),
        start_error: options.browser_start_error,
        stop_calls: AtomicUsize::new(0),
    });
    let session_manager = Arc::new(FakeRunnerSessionManager {
        lifecycle: Arc::clone(&lifecycle),
        stop_all_calls: Mutex::new(Vec::new()),
    });
    let scheduler = Arc::new(FakeRunnerScheduler {
        lifecycle: Arc::clone(&lifecycle),
        start_calls: AtomicUsize::new(0),
        stop_calls: AtomicUsize::new(0),
    });
    let logger_factory = Arc::new(FakeLoggerFactory {
        lifecycle: Arc::clone(&lifecycle),
        create_calls: AtomicUsize::new(0),
        logger: Arc::clone(&logger),
    });
    let startup_diagnostics = options
        .startup_diagnostics
        .map(|provider| Arc::new(provider) as Arc<dyn StartupDiagnosticsProvider>);
    let runtime_factory = Arc::new(FakeRuntimeFactory {
        lifecycle: Arc::clone(&lifecycle),
        create_calls: AtomicUsize::new(0),
        integrations: Mutex::new(None),
        startup_diagnostics: Mutex::new(startup_diagnostics),
        browser_manager: Arc::clone(&browser_manager),
        session_manager: Arc::clone(&session_manager),
        scheduler: Arc::clone(&scheduler),
    });
    let app = Arc::new(DefaultAppRunner::new(DefaultAppRunnerOptions {
        config_path: CONFIG_PATH.to_string(),
        env: ProcessEnv::new(),
        config_loader: config_loader.clone(),
        credential_validator: credential_validator.clone(),
        bootstrap_logger: bootstrap_logger.clone(),
        logger_factory: logger_factory.clone(),
        runtime_factory: runtime_factory.clone(),
    }));
    Harness {
        app,
        lifecycle,
        bootstrap_logger,
        logger,
        config_loader,
        credential_validator,
        logger_factory,
        runtime_factory,
        browser_manager,
        session_manager,
        scheduler,
    }
}

fn create_harness() -> Harness {
    create_harness_with(HarnessOptions::default())
}

// ---------------------------------------------------------------------------
// 測試本體（順序照舊測試檔）
// ---------------------------------------------------------------------------

// 原測試：依 Config、Logger、Credential、Runtime、Browser、Scheduler 順序啟動
#[tokio::test]
async fn starts_in_config_logger_credential_runtime_browser_scheduler_order() {
    let harness = create_harness();

    let result = harness.app.start().await;
    assert!(result.is_ok(), "start 失敗：{result:?}");

    assert_eq!(
        lifecycle_events(&harness.lifecycle),
        vec![
            "config.load",
            "logger.create",
            "credential.validate",
            "runtime.create",
            "browser.start",
            "scheduler.start",
        ]
    );
    assert_eq!(
        harness
            .config_loader
            .load_calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone(),
        vec![(CONFIG_PATH.to_string(), ProcessEnv::new())]
    );
    assert_eq!(
        harness.logger.events_of(LogLevel::Info),
        vec!["config_loaded", "credential_checked", "service_started"]
    );

    // service_started 欄位（objectContaining；nodeVersion／playwrightVersion
    // 的 Rust 值為 'unknown'／null——diagnostics 模組註記的環境差異）。
    let calls = harness.logger.calls_of(LogLevel::Info);
    let (_, fields) = calls
        .iter()
        .find(|(event, _)| event == "service_started")
        .unwrap();
    assert!(object_contains(
        &[
            ("channelCount", serde_json::json!(2)),
            ("applicationVersion", serde_json::json!("0.1.0")),
            ("gitCommit", serde_json::json!(null)),
            ("nodeVersion", serde_json::json!("unknown")),
            ("playwrightVersion", serde_json::json!(null)),
            ("browserEngine", serde_json::json!("firefox")),
            ("browserVersion", serde_json::json!("test-browser-1.0")),
            ("browserGeneration", serde_json::json!(1)),
            ("memoryMaxBytes", serde_json::json!(null)),
            ("pidsMax", serde_json::json!(null)),
        ],
        fields
    ));
    assert!(
        is_run_id(fields.get("runId").unwrap_or(&serde_json::Value::Null)),
        "runId 需符合舊版形狀：{fields:?}"
    );
    assert!(
        is_fingerprint(
            fields
                .get("safeConfigFingerprint")
                .unwrap_or(&serde_json::Value::Null)
        ),
        "safeConfigFingerprint 需為 64 hex：{fields:?}"
    );
    let serialized = harness.logger.serialized();
    assert!(
        !serialized.contains(ACCESS_TOKEN),
        "info 呼叫不得帶 access token"
    );
    assert!(
        !serialized.contains(STORAGE_STATE_PATH),
        "info 呼叫不得帶 storageState 路徑"
    );
}

// 原測試：runtime 提供 startupDiagnostics 時以 provider 結果發出 service_started
#[tokio::test]
async fn uses_startup_diagnostics_provider_result_for_service_started() {
    let harness = create_harness_with(HarnessOptions {
        startup_diagnostics: Some(FakeStartupDiagnostics { error: None }),
        ..HarnessOptions::default()
    });

    let result = harness.app.start().await;
    assert!(result.is_ok(), "start 失敗：{result:?}");

    let calls = harness.logger.calls_of(LogLevel::Info);
    let (_, fields) = calls
        .iter()
        .find(|(event, _)| event == "service_started")
        .unwrap();
    assert!(object_contains(
        &[
            ("runId", serde_json::json!("provider-run-1234")),
            (
                "memoryMaxBytes",
                serde_json::Number::from(6_442_450_944i64).into()
            ),
            ("pidsMax", serde_json::json!(512)),
            ("browserGeneration", serde_json::json!(42)),
        ],
        fields
    ));
}

// 原測試：startupDiagnostics 拋錯時改用預設 diagnostics 並記錄警告
#[tokio::test]
async fn falls_back_to_default_diagnostics_and_warns_when_provider_fails() {
    let harness = create_harness_with(HarnessOptions {
        startup_diagnostics: Some(FakeStartupDiagnostics {
            error: Some("diagnostics exploded".to_string()),
        }),
        ..HarnessOptions::default()
    });

    let result = harness.app.start().await;
    assert!(result.is_ok(), "start 失敗：{result:?}");

    assert_eq!(
        harness.logger.calls_of(LogLevel::Warn),
        vec![(
            "runtime_diagnostics_failed".to_string(),
            serde_json::json!({"error": "diagnostics exploded"}),
        )]
    );
    let calls = harness.logger.calls_of(LogLevel::Info);
    let (_, fields) = calls
        .iter()
        .find(|(event, _)| event == "service_started")
        .unwrap();
    assert!(object_contains(
        &[
            ("memoryMaxBytes", serde_json::json!(null)),
            ("pidsMax", serde_json::json!(null)),
        ],
        fields
    ));
    assert!(
        is_run_id(fields.get("runId").unwrap_or(&serde_json::Value::Null)),
        "fallback runId 需符合舊版形狀：{fields:?}"
    );
}

// 原測試：設定失敗時不建立 logger、驗證 credential 或 runtime
#[tokio::test]
async fn config_failure_skips_logger_credential_and_runtime() {
    let harness = create_harness_with(HarnessOptions {
        config_error: Some("invalid config".to_string()),
        ..HarnessOptions::default()
    });

    let result = harness.app.start().await;
    assert_eq!(result.unwrap_err(), "invalid config");

    assert_eq!(
        harness.logger_factory.create_calls.load(Ordering::SeqCst),
        0
    );
    assert_eq!(
        harness
            .credential_validator
            .validate_calls
            .load(Ordering::SeqCst),
        0
    );
    assert_eq!(
        harness.runtime_factory.create_calls.load(Ordering::SeqCst),
        0
    );
    assert_eq!(
        harness.bootstrap_logger.calls_of(LogLevel::Error),
        vec![(
            "config_error".to_string(),
            serde_json::json!({"phase": "config", "error": "invalid config"}),
        )]
    );
}

// 原測試：credential 失敗時不建立或啟動 runtime
#[tokio::test]
async fn credential_failure_skips_runtime_start() {
    let harness = create_harness_with(HarnessOptions {
        credential_error: Some("credential unavailable".to_string()),
        ..HarnessOptions::default()
    });

    let result = harness.app.start().await;
    assert_eq!(result.unwrap_err(), "credential unavailable");

    assert_eq!(
        harness.runtime_factory.create_calls.load(Ordering::SeqCst),
        0
    );
    assert_eq!(
        harness.browser_manager.start_calls.load(Ordering::SeqCst),
        0
    );
    assert_eq!(harness.scheduler.start_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        harness.logger.calls_of(LogLevel::Error),
        vec![(
            "service_start_failed".to_string(),
            serde_json::json!({"phase": "credential", "error": "credential unavailable"}),
        )]
    );
}

// 原測試：Browser 啟動失敗時清理完整 runtime，且不啟動 Scheduler
#[tokio::test]
async fn browser_start_failure_cleans_up_runtime_without_scheduler() {
    let harness = create_harness_with(HarnessOptions {
        browser_start_error: Some("browser unavailable".to_string()),
        ..HarnessOptions::default()
    });

    let result = harness.app.start().await;
    assert_eq!(result.unwrap_err(), "browser unavailable");

    assert_eq!(harness.scheduler.start_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        lifecycle_events(&harness.lifecycle),
        vec![
            "config.load",
            "logger.create",
            "credential.validate",
            "runtime.create",
            "browser.start",
            "scheduler.stop",
            "sessionManager.stopAll:startup_failed:browser",
            "browser.stop",
            "configured.flush",
        ]
    );
}

// 原測試：停止順序為 Scheduler、SessionManager、Browser、logger flush
#[tokio::test]
async fn stop_order_is_scheduler_session_manager_browser_flush() {
    let harness = create_harness();
    assert!(harness.app.start().await.is_ok());
    lifecycle_clear(&harness.lifecycle);

    let result = harness.app.stop("SIGTERM").await;
    assert!(result.is_ok(), "stop 失敗：{result:?}");

    assert_eq!(
        lifecycle_events(&harness.lifecycle),
        vec![
            "scheduler.stop",
            "sessionManager.stopAll:SIGTERM",
            "browser.stop",
            "configured.flush",
        ]
    );
}

// 原測試：整合元件在 Browser 後、Scheduler 前啟動，並於 Browser 後停止
#[tokio::test]
async fn integrations_start_after_browser_and_stop_after_browser() {
    let harness = create_harness();
    let integration = Arc::new(FakeIntegration {
        lifecycle: Arc::clone(&harness.lifecycle),
    });
    mock_runtime(
        &harness.runtime_factory,
        vec![Arc::clone(&integration) as Arc<dyn ApplicationIntegration>],
        None,
    );

    assert!(harness.app.start().await.is_ok());
    let events = lifecycle_events(&harness.lifecycle);
    assert!(events.contains(&"integration.start".to_string()));
    let browser_index = events
        .iter()
        .position(|event| event == "browser.start")
        .unwrap();
    let integration_index = events
        .iter()
        .position(|event| event == "integration.start")
        .unwrap();
    let scheduler_index = events
        .iter()
        .position(|event| event == "scheduler.start")
        .unwrap();
    assert!(browser_index < integration_index);
    assert!(integration_index < scheduler_index);

    lifecycle_clear(&harness.lifecycle);
    let result = harness.app.stop("SIGTERM").await;
    assert!(result.is_ok(), "stop 失敗：{result:?}");
    assert_eq!(
        lifecycle_events(&harness.lifecycle),
        vec![
            "scheduler.stop",
            "sessionManager.stopAll:SIGTERM",
            "browser.stop",
            "integration.stop:SIGTERM",
            "configured.flush",
        ]
    );
}

// 原測試：並發及重複 stop 只釋放一次資源
#[tokio::test]
async fn concurrent_and_repeated_stop_release_resources_once() {
    let harness = create_harness();
    assert!(harness.app.start().await.is_ok());

    let results = join_all(vec![
        harness.app.stop("SIGINT"),
        harness.app.stop("SIGTERM"),
        harness.app.stop("duplicate"),
    ])
    .await;
    assert_eq!(results, vec![Ok(()), Ok(()), Ok(())]);
    assert_eq!(harness.app.stop("after_complete").await, Ok(()));

    assert_eq!(harness.scheduler.stop_calls.load(Ordering::SeqCst), 1);
    assert_eq!(harness.session_manager.stop_all_reasons(), vec!["SIGINT"]);
    assert_eq!(harness.browser_manager.stop_calls.load(Ordering::SeqCst), 1);
    assert_eq!(harness.logger.flush_count(), 1);
}

// 原測試：啟動後收到 signal 會觸發優雅停止
#[tokio::test]
async fn signal_after_start_triggers_graceful_stop() {
    let harness = create_harness();
    let target = FakeProcessTarget::new();
    let remove_handlers = install_process_handlers(InstallProcessHandlersOptions {
        app: Arc::clone(&harness.app) as Arc<dyn AppRunner>,
        logger: harness.bootstrap_logger.clone(),
        target: Some(Arc::clone(&target) as Arc<dyn ProcessHandlerTarget>),
    });
    assert!(harness.app.start().await.is_ok());
    lifecycle_clear(&harness.lifecycle);

    target.emit(ProcessSignal::Sigterm);

    assert!(
        eventually(|| harness.logger.flush_count() == 1).await,
        "flush 未在輪詢窗口內完成"
    );
    assert_eq!(
        lifecycle_events(&harness.lifecycle),
        vec![
            "scheduler.stop",
            "sessionManager.stopAll:SIGTERM",
            "browser.stop",
            "configured.flush",
        ]
    );

    remove_handlers();
    assert_eq!(target.listener_count(ProcessSignal::Sigterm), 0);
}

// 原測試：啟動錯誤日誌不洩漏 token 或 storageState 路徑
#[tokio::test]
async fn startup_error_log_does_not_leak_token_or_storage_state_path() {
    let harness = create_harness_with(HarnessOptions {
        credential_error: Some(format!(
            "Authorization: Bearer {ACCESS_TOKEN}; access_token={ACCESS_TOKEN}; path={STORAGE_STATE_PATH}"
        )),
        ..HarnessOptions::default()
    });

    let result = harness.app.start().await;
    let message = result.unwrap_err();
    assert!(message.contains(ACCESS_TOKEN), "原始錯誤照舊版保留 token");

    let logged = harness.logger.serialized();
    assert!(!logged.contains(ACCESS_TOKEN), "記錄不得帶 token：{logged}");
    assert!(
        !logged.contains(STORAGE_STATE_PATH),
        "記錄不得帶 storageState 路徑：{logged}"
    );
    assert!(
        logged.contains("[REDACTED]"),
        "記錄需帶 [REDACTED]：{logged}"
    );
}

// ---------------------------------------------------------------------------
// 舊 app-runner.test.ts 的 FakeProcessTarget（signal 測試用）
// ---------------------------------------------------------------------------

/// 舊測試 `FakeProcessTarget`：`emit` 把訊號送進已註冊的接聽；count 反映
/// 註冊／撤銷（對映舊 `listenerCount`）。
struct FakeProcessTarget {
    exit_code: Mutex<Option<i32>>,
    subscriptions: Mutex<Vec<tokio::sync::mpsc::UnboundedSender<ProcessSignal>>>,
    counts: Mutex<[usize; 2]>,
}

impl FakeProcessTarget {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            exit_code: Mutex::new(None),
            subscriptions: Mutex::new(Vec::new()),
            counts: Mutex::new([0, 0]),
        })
    }

    /// 對應 `target.emit('SIGTERM')`：把訊號交給現有接聽（一次註冊各一份）。
    fn emit(&self, signal: ProcessSignal) {
        let subscriptions = self
            .subscriptions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let signal_debug = signal.as_str().to_string();
        for sender in subscriptions.iter() {
            let _ = sender.send(signal);
        }
        let _ = signal_debug;
    }

    fn listener_count(&self, signal: ProcessSignal) -> usize {
        self.counts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())[signal.index()]
    }
}

#[async_trait]
impl ProcessHandlerTarget for FakeProcessTarget {
    fn subscribe(&self) -> ProcessSignalReceiver {
        // 舊版兩個 listener 各別掛載；Rust 版一次註冊兩種訊號的接聽。
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        self.subscriptions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(sender);
        {
            let mut counts = self
                .counts
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            counts[0] += 1;
            counts[1] += 1;
        }
        ProcessSignalReceiver::new(Arc::new(ChannelDelivery {
            receiver: tokio::sync::Mutex::new(receiver),
        }) as Arc<dyn SignalDelivery>)
    }

    fn remove(&self) {
        self.subscriptions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        let mut counts = self
            .counts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        counts[0] = 0;
        counts[1] = 0;
    }

    fn set_exit_code(&self, code: i32) {
        *self
            .exit_code
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(code);
    }
}

/// fake 的訊號來源：channel 的接收端（註冊者把訊號 push 進來）。
struct ChannelDelivery {
    receiver: tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<ProcessSignal>>,
}

#[async_trait]
impl SignalDelivery for ChannelDelivery {
    async fn next_signal(&self) -> Option<ProcessSignal> {
        self.receiver.lock().await.recv().await
    }
}
