//! 設定模型：對應舊版 `config/AppConfig.ts` 的完整形狀（§7.1 下半）。
//!
//! - 欄位名稱照舊版 loader 的 snake_case → camelCase 轉換（YAML 的
//!   `navigation_timeout_ms` 對應 [`BrowserConfig::navigation_timeout_ms`]；
//!   鍵名 `allowed_chat_ids` 對應 [`TelegramConfig::allowed_chat_ids`] 等）。
//! - 數字一律 f64（照 JS number；驗證與門檻縮放的語意都以 JS number 為準）。
//! - 欄位宣告順序照舊版物件建立的順序，[`AppConfig::to_json`] 也以相同順序輸出，
//!   供 golden 比對與 S2.6 的 `createSafeConfigFingerprint` 使用。
//! - 舊版 `deepFreeze` 的不可變由 Rust 的所有權模型保證（透過參照無法修改），
//!   不需要對應實作。
//!
//! 從 [`JsValue`](crate::config::yaml::JsValue) 建構時的型別檢查與錯誤訊息
//! 在 [`crate::config::loader`]（照舊版 `ConfigLoader.ts` 逐函式移植）。
#![allow(dead_code)]

use crate::logging::logger::LogLevel;

/// 舊版 `STREAM_QUALITIES`（錯誤訊息的允許清單；`StreamQuality::parse` 的分支與其一對一）。
pub(crate) const STREAM_QUALITIES: [&str; 4] = ["auto", "160p", "360p", "480p"];

/// 舊版 `BROWSER_ENGINES`（錯誤訊息的允許清單；`BrowserEngine::parse` 的分支與其一對一）。
pub(crate) const BROWSER_ENGINES: [&str; 2] = ["firefox", "chromium"];

/// 對應舊版 `StreamQuality`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StreamQuality {
    Auto,
    P160,
    P360,
    P480,
}

impl StreamQuality {
    /// 字串值（照舊版 union 的字面）。
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            StreamQuality::Auto => "auto",
            StreamQuality::P160 => "160p",
            StreamQuality::P360 => "360p",
            StreamQuality::P480 => "480p",
        }
    }

    /// 對應舊版 `STREAM_QUALITIES.includes(value)` 的判定。
    pub(crate) fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "auto" => StreamQuality::Auto,
            "160p" => StreamQuality::P160,
            "360p" => StreamQuality::P360,
            "480p" => StreamQuality::P480,
            _ => return None,
        })
    }
}

/// 對應舊版 `BrowserEngine`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BrowserEngine {
    Firefox,
    Chromium,
}

impl BrowserEngine {
    /// 字串值（照舊版 union 的字面）。
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            BrowserEngine::Firefox => "firefox",
            BrowserEngine::Chromium => "chromium",
        }
    }

    /// 對應舊版 `BROWSER_ENGINES.includes(value)` 的判定。
    pub(crate) fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "firefox" => BrowserEngine::Firefox,
            "chromium" => BrowserEngine::Chromium,
            _ => return None,
        })
    }
}

/// 對應舊版 `TwitchApiConfig`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TwitchApiConfig {
    pub(crate) client_id: String,
    pub(crate) access_token: String,
    pub(crate) client_secret: String,
}

/// 對應舊版 `ResourceGuardEffectiveThresholds`：依串流數縮放後的絕對門檻（binary MiB）。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ResourceGuardEffectiveThresholds {
    pub(crate) max_concurrent_streams: f64,
    pub(crate) warning_memory_mib: f64,
    pub(crate) warning_reset_memory_mib: f64,
    pub(crate) browser_recycle_memory_mib: f64,
    pub(crate) emergency_memory_mib: f64,
    pub(crate) post_recycle_target_memory_mib: f64,
}

/// 對應舊版 `ResourceGuardConfig`。
///
/// YAML 的數值是 `baseline_streams` 的錨點；[`ResourceGuardConfig::effective`]
/// 存執行期使用的縮放值。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ResourceGuardConfig {
    pub(crate) enabled: bool,
    pub(crate) sample_interval_seconds: f64,
    pub(crate) startup_rate_grace_seconds: f64,
    pub(crate) scale_with_streams: bool,
    pub(crate) baseline_streams: f64,
    pub(crate) base_memory_mib: f64,
    pub(crate) warning_memory_mib: f64,
    pub(crate) warning_reset_memory_mib: f64,
    pub(crate) browser_recycle_memory_mib: f64,
    pub(crate) browser_recycle_consecutive_samples: f64,
    pub(crate) emergency_memory_mib: f64,
    pub(crate) emergency_swap_mib: f64,
    pub(crate) fast_growth_mib: f64,
    pub(crate) fast_growth_window_seconds: f64,
    /// 瀏覽器重啟／回收後，抑制 fast_memory_growth 的秒數（session 回填不是洩漏）。
    pub(crate) post_browser_restart_rate_grace_seconds: f64,
    pub(crate) post_recycle_observation_seconds: f64,
    pub(crate) post_recycle_target_memory_mib: f64,
    pub(crate) post_recycle_minimum_drop_mib: f64,
    /// 依串流數縮放後的絕對門檻。
    pub(crate) effective: ResourceGuardEffectiveThresholds,
}

/// 對應舊版 `BrowserRecoveryConfig`。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct BrowserRecoveryConfig {
    pub(crate) page_crash_backoff_seconds: Vec<f64>,
    pub(crate) channel_crash_window_seconds: f64,
    pub(crate) channel_quarantine_threshold: f64,
    pub(crate) channel_quarantine_seconds: f64,
    pub(crate) stable_reset_seconds: f64,
    pub(crate) multi_channel_crash_window_seconds: f64,
    pub(crate) multi_channel_crash_threshold: f64,
    pub(crate) browser_failure_window_seconds: f64,
    pub(crate) browser_failure_container_threshold: f64,
}

/// 對應舊版 `BrowserSessionStartConfig`。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct BrowserSessionStartConfig {
    pub(crate) failure_backoff_seconds: Vec<f64>,
    pub(crate) maximum_cooldown_seconds: f64,
}

/// 對應舊版 `BrowserConfig`。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct BrowserConfig {
    pub(crate) engine: BrowserEngine,
    pub(crate) navigation_timeout_ms: f64,
    pub(crate) page_health_check_interval_seconds: f64,
    pub(crate) reward_check_interval_seconds: f64,
    pub(crate) page_refresh_interval_seconds: f64,
    pub(crate) restart_on_crash: bool,
    pub(crate) stream_quality: StreamQuality,
    pub(crate) enforce_stream_quality_seconds: f64,
    pub(crate) viewport_width: f64,
    pub(crate) viewport_height: f64,
    pub(crate) mute_audio: bool,
    pub(crate) block_images: bool,
    pub(crate) block_fonts: bool,
    pub(crate) block_known_tracking: bool,
    pub(crate) disable_chat: bool,
    pub(crate) resource_telemetry_interval_seconds: f64,
    pub(crate) recovery: BrowserRecoveryConfig,
    pub(crate) session_start: BrowserSessionStartConfig,
    pub(crate) resource_guard: ResourceGuardConfig,
}

/// 對應舊版 `TelegramConfig`。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TelegramConfig {
    pub(crate) enabled: bool,
    pub(crate) bot_token: String,
    pub(crate) allowed_chat_ids: Vec<String>,
    pub(crate) polling_timeout_seconds: f64,
}

/// 對應舊版 `DiscordConfig`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DiscordConfig {
    pub(crate) enabled: bool,
    pub(crate) bot_token: String,
    pub(crate) application_id: String,
    pub(crate) guild_id: String,
    pub(crate) allowed_channel_ids: Vec<String>,
    pub(crate) allow_direct_messages: bool,
    pub(crate) allowed_user_ids: Vec<String>,
}

/// 對應舊版 `AppConfig`：`YamlConfigLoader::load` 的回傳值。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AppConfig {
    pub(crate) channels: Vec<String>,
    pub(crate) check_interval_seconds: f64,
    pub(crate) max_concurrent_streams: f64,
    pub(crate) headless: bool,
    pub(crate) storage_state_path: String,
    pub(crate) log_level: LogLevel,
    pub(crate) twitch_api: TwitchApiConfig,
    pub(crate) browser: BrowserConfig,
    pub(crate) telegram: TelegramConfig,
    pub(crate) discord: DiscordConfig,
}

impl TwitchApiConfig {
    /// golden 比對與 S2.6 fingerprint 用的 JSON（欄位順序照舊版物件建立的順序）。
    pub(crate) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "clientId": self.client_id,
            "accessToken": self.access_token,
            "clientSecret": self.client_secret,
        })
    }
}

impl ResourceGuardEffectiveThresholds {
    /// golden 比對與 S2.6 fingerprint 用的 JSON。
    pub(crate) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "maxConcurrentStreams": self.max_concurrent_streams,
            "warningMemoryMib": self.warning_memory_mib,
            "warningResetMemoryMib": self.warning_reset_memory_mib,
            "browserRecycleMemoryMib": self.browser_recycle_memory_mib,
            "emergencyMemoryMib": self.emergency_memory_mib,
            "postRecycleTargetMemoryMib": self.post_recycle_target_memory_mib,
        })
    }
}

impl ResourceGuardConfig {
    /// golden 比對與 S2.6 fingerprint 用的 JSON。
    pub(crate) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "enabled": self.enabled,
            "sampleIntervalSeconds": self.sample_interval_seconds,
            "startupRateGraceSeconds": self.startup_rate_grace_seconds,
            "scaleWithStreams": self.scale_with_streams,
            "baselineStreams": self.baseline_streams,
            "baseMemoryMib": self.base_memory_mib,
            "warningMemoryMib": self.warning_memory_mib,
            "warningResetMemoryMib": self.warning_reset_memory_mib,
            "browserRecycleMemoryMib": self.browser_recycle_memory_mib,
            "browserRecycleConsecutiveSamples": self.browser_recycle_consecutive_samples,
            "emergencyMemoryMib": self.emergency_memory_mib,
            "emergencySwapMib": self.emergency_swap_mib,
            "fastGrowthMib": self.fast_growth_mib,
            "fastGrowthWindowSeconds": self.fast_growth_window_seconds,
            "postBrowserRestartRateGraceSeconds": self.post_browser_restart_rate_grace_seconds,
            "postRecycleObservationSeconds": self.post_recycle_observation_seconds,
            "postRecycleTargetMemoryMib": self.post_recycle_target_memory_mib,
            "postRecycleMinimumDropMib": self.post_recycle_minimum_drop_mib,
            "effective": self.effective.to_json(),
        })
    }
}

impl BrowserRecoveryConfig {
    /// golden 比對與 S2.6 fingerprint 用的 JSON。
    pub(crate) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "pageCrashBackoffSeconds": self.page_crash_backoff_seconds,
            "channelCrashWindowSeconds": self.channel_crash_window_seconds,
            "channelQuarantineThreshold": self.channel_quarantine_threshold,
            "channelQuarantineSeconds": self.channel_quarantine_seconds,
            "stableResetSeconds": self.stable_reset_seconds,
            "multiChannelCrashWindowSeconds": self.multi_channel_crash_window_seconds,
            "multiChannelCrashThreshold": self.multi_channel_crash_threshold,
            "browserFailureWindowSeconds": self.browser_failure_window_seconds,
            "browserFailureContainerThreshold": self.browser_failure_container_threshold,
        })
    }
}

impl BrowserSessionStartConfig {
    /// golden 比對與 S2.6 fingerprint 用的 JSON。
    pub(crate) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "failureBackoffSeconds": self.failure_backoff_seconds,
            "maximumCooldownSeconds": self.maximum_cooldown_seconds,
        })
    }
}

impl BrowserConfig {
    /// golden 比對與 S2.6 fingerprint 用的 JSON。
    pub(crate) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "engine": self.engine.as_str(),
            "navigationTimeoutMs": self.navigation_timeout_ms,
            "pageHealthCheckIntervalSeconds": self.page_health_check_interval_seconds,
            "rewardCheckIntervalSeconds": self.reward_check_interval_seconds,
            "pageRefreshIntervalSeconds": self.page_refresh_interval_seconds,
            "restartOnCrash": self.restart_on_crash,
            "streamQuality": self.stream_quality.as_str(),
            "enforceStreamQualitySeconds": self.enforce_stream_quality_seconds,
            "viewportWidth": self.viewport_width,
            "viewportHeight": self.viewport_height,
            "muteAudio": self.mute_audio,
            "blockImages": self.block_images,
            "blockFonts": self.block_fonts,
            "blockKnownTracking": self.block_known_tracking,
            "disableChat": self.disable_chat,
            "resourceTelemetryIntervalSeconds": self.resource_telemetry_interval_seconds,
            "recovery": self.recovery.to_json(),
            "sessionStart": self.session_start.to_json(),
            "resourceGuard": self.resource_guard.to_json(),
        })
    }
}

impl TelegramConfig {
    /// golden 比對與 S2.6 fingerprint 用的 JSON。
    pub(crate) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "enabled": self.enabled,
            "botToken": self.bot_token,
            "allowedChatIds": self.allowed_chat_ids,
            "pollingTimeoutSeconds": self.polling_timeout_seconds,
        })
    }
}

impl DiscordConfig {
    /// golden 比對與 S2.6 fingerprint 用的 JSON。
    pub(crate) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "enabled": self.enabled,
            "botToken": self.bot_token,
            "applicationId": self.application_id,
            "guildId": self.guild_id,
            "allowedChannelIds": self.allowed_channel_ids,
            "allowDirectMessages": self.allow_direct_messages,
            "allowedUserIds": self.allowed_user_ids,
        })
    }
}

impl AppConfig {
    /// golden 比對與 S2.6 fingerprint 用的 JSON（欄位順序照舊版物件建立的順序）。
    pub(crate) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "channels": self.channels,
            "checkIntervalSeconds": self.check_interval_seconds,
            "maxConcurrentStreams": self.max_concurrent_streams,
            "headless": self.headless,
            "storageStatePath": self.storage_state_path,
            "logLevel": self.log_level.as_str(),
            "twitchApi": self.twitch_api.to_json(),
            "browser": self.browser.to_json(),
            "telegram": self.telegram.to_json(),
            "discord": self.discord.to_json(),
        })
    }
}
