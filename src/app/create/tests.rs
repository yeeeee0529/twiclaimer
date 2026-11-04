//! `create.rs` 的測試：移植舊版 `test/unit/app-runner.test.ts` 第 44 列的
//! `預設 RuntimeFactory 可完成建構且不會呼叫 Twitch`。
//!
//! 舊測試斷言 `createDefaultRuntime` 的三個元件都是預設實作且建構期間不碰
//! 網路；Rust 版的 runtime 欄位是 trait 物件（無 downcast 接縫），以組裝形狀
//! 對應：建構回 `Ok`、integrations 為 monitor＋stall watchdog 兩個（telegram
//! ／discord 未啟用）、`reconcile_coordinator`／`startup_diagnostics` 皆在。
//! 「不會呼叫 Twitch」由建構路徑保證：`TwitchApiClient` 只建立 client（
//! `transport: None` → reqwest client 的建構無 I/O），token 維護要到
//! `get_live_statuses` 才會發請求。
use std::sync::Arc;

use super::create_default_runtime;
use crate::app::runner::RunnerLogger;
use crate::config::model::{
    AppConfig, BrowserConfig, BrowserEngine, BrowserRecoveryConfig, BrowserSessionStartConfig,
    DiscordConfig, ResourceGuardConfig, ResourceGuardEffectiveThresholds, StreamQuality,
    TelegramConfig, TwitchApiConfig,
};
use crate::logging::logger::{JsonLineLogger, JsonLineLoggerOptions, LogLevel, LogWriter};

/// 舊測試常數（同 runner 測試的 `CONFIG_PATH`）。
const CONFIG_PATH: &str = "/test/config.yml";
/// 舊測試常數（同 runner 測試的 `STORAGE_STATE_PATH`）。
const STORAGE_STATE_PATH: &str = "/private/storage-state.json";

/// 捨去輸出的 writer（舊 `createTestLogger([])` 不檢查內容）。
struct DiscardWriter;

impl LogWriter for DiscardWriter {
    fn write_line(&self, _line: &str) {}
    fn flush(&self) -> std::io::Result<()> {
        Ok(())
    }
}

/// 舊測試的 `logger`：真實 JsonLineLogger（同 `create_default_runtime` 的
/// 正式 logger 型別）。
fn test_logger() -> Arc<JsonLineLogger> {
    Arc::new(JsonLineLogger::new(JsonLineLoggerOptions {
        level: Some(LogLevel::Info),
        output: Some(Box::new(DiscardWriter)),
        clock: None,
    }))
}

/// 舊測試的 `CONFIG`（同 runner 測試的形狀；telegram／discord 未啟用）。
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
            access_token: "highly-sensitive-access-token".to_string(),
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
            resource_guard: ResourceGuardConfig {
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

// 原測試：預設 RuntimeFactory 可完成建構且不會呼叫 Twitch
#[tokio::test]
async fn default_runtime_factory_builds_without_calling_twitch() {
    let logger = test_logger() as Arc<dyn RunnerLogger>;

    let runtime = create_default_runtime(&test_config(), logger, CONFIG_PATH)
        .await
        .expect("runtime 建構應成功");

    // 舊斷言的三個元件都在（trait 物件；形狀以組裝結果鎖定）。
    // integrations：RuntimeResourceMonitor＋SchedulerStallWatchdog（bot 未啟用）。
    assert_eq!(runtime.integrations.len(), 2);
    assert!(runtime.reconcile_coordinator.is_some());
    assert!(runtime.startup_diagnostics.is_some());
}
