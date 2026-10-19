//! 設定載入器：對應舊版 `config/ConfigLoader.ts` 逐函式移植（§7.1 下半）。
//!
//! 行為重點（照舊版）：
//! - 設定檔路徑為 `(env.CONFIG_PATH ?? path) || DEFAULT_CONFIG_PATH`；不存在 →
//!   [`ConfigFileNotFoundError`]（`找不到設定檔：<path>`），其他讀取錯誤 →
//!   [`ConfigParseError`](crate::config::errors::ConfigParseError)
//!   （`設定檔格式錯誤：無法讀取設定檔 <path>`）。
//! - `${VAR}` 只替換字串值、不替換鍵；未定義的變數換成空字串。
//! - 直接環境變數覆寫：`HEADLESS`、`LOG_LEVEL`、`TWITCH_CLIENT_ID`、
//!   `TWITCH_ACCESS_TOKEN`、`TWITCH_CLIENT_SECRET`、`TELEGRAM_ENABLED`、
//!   `TELEGRAM_BOT_TOKEN`、`TELEGRAM_ALLOWED_CHAT_IDS`、`DISCORD_ENABLED`、
//!   `DISCORD_BOT_TOKEN`、`DISCORD_APPLICATION_ID`、`DISCORD_GUILD_ID`、
//!   `DISCORD_ALLOWED_CHANNEL_IDS`、`DISCORD_ALLOW_DIRECT_MESSAGES`、
//!   `DISCORD_ALLOWED_USER_IDS`；環境值存在時優先於 YAML 值。
//! - 欄位驗證的順序與訊息不變（照舊版各 `build*` 函式的求值順序）。
//! - 允許差異 X1（§6.2）：`browser.engine` 必須是 `chromium`；X2：`headless`
//!   覆寫後的值必須是 `true`（檢查放在 `buildConfig` 最後，原有欄位驗證的
//!   順序與訊息完全不變）。
//! - 舊版 `deepFreeze` 不移植：Rust 的所有權模型使回傳值天然不可變。
#![allow(dead_code)]

use std::collections::HashMap;
use std::collections::HashSet;
use std::io;
use std::sync::{Arc, OnceLock};

use regex::{Captures, Regex};

use super::errors::{
    ConfigFileNotFoundError, ConfigLoadError, ConfigParseError, ConfigValidationError,
};
use super::model::{
    AppConfig, BROWSER_ENGINES, BrowserConfig, BrowserEngine, BrowserRecoveryConfig,
    BrowserSessionStartConfig, DiscordConfig, ResourceGuardConfig, STREAM_QUALITIES, StreamQuality,
    TelegramConfig, TwitchApiConfig,
};
use super::thresholds::{
    ResourceGuardAnchorThresholds, compute_effective_resource_guard_thresholds,
};
use super::yaml::{JsValue, parse_yaml};
use crate::logging::logger::{JsonLineLogger, LOG_LEVELS, LogLevel};
use crate::logging::redact::SanitizedValue;

/// 舊版 `DEFAULT_CONFIG_PATH`。
pub(crate) const DEFAULT_CONFIG_PATH: &str = "/app/config.yml";
/// 舊版 `DEFAULT_CHECK_INTERVAL_SECONDS`。
const DEFAULT_CHECK_INTERVAL_SECONDS: f64 = 60.0;
/// 舊版 `DEFAULT_MAX_CONCURRENT_STREAMS`。
const DEFAULT_MAX_CONCURRENT_STREAMS: f64 = 3.0;
/// 舊版 `DEFAULT_STORAGE_STATE_PATH`。
const DEFAULT_STORAGE_STATE_PATH: &str = "/data/browser-state/storage-state.json";
/// 舊版 `DEFAULT_NAVIGATION_TIMEOUT_MS`。
const DEFAULT_NAVIGATION_TIMEOUT_MS: f64 = 30_000.0;
/// 舊版 `DEFAULT_BROWSER_ENGINE`。
const DEFAULT_BROWSER_ENGINE: BrowserEngine = BrowserEngine::Chromium;
/// 舊版 `DEFAULT_PAGE_HEALTH_CHECK_INTERVAL_SECONDS`。
const DEFAULT_PAGE_HEALTH_CHECK_INTERVAL_SECONDS: f64 = 60.0;
/// 舊版 `DEFAULT_REWARD_CHECK_INTERVAL_SECONDS`。
const DEFAULT_REWARD_CHECK_INTERVAL_SECONDS: f64 = 30.0;
/// 舊版 `DEFAULT_PAGE_REFRESH_INTERVAL_SECONDS`（0 = 不定期換頁）。
const DEFAULT_PAGE_REFRESH_INTERVAL_SECONDS: f64 = 0.0;
/// 舊版 `DEFAULT_STREAM_QUALITY`。
const DEFAULT_STREAM_QUALITY: StreamQuality = StreamQuality::P160;
/// 舊版 `DEFAULT_ENFORCE_STREAM_QUALITY_SECONDS`。
const DEFAULT_ENFORCE_STREAM_QUALITY_SECONDS: f64 = 120.0;
/// 舊版 `DEFAULT_VIEWPORT_WIDTH`。
const DEFAULT_VIEWPORT_WIDTH: f64 = 1280.0;
/// 舊版 `DEFAULT_VIEWPORT_HEIGHT`。
const DEFAULT_VIEWPORT_HEIGHT: f64 = 720.0;
/// 舊版 `DEFAULT_RESOURCE_TELEMETRY_INTERVAL_SECONDS`。
const DEFAULT_RESOURCE_TELEMETRY_INTERVAL_SECONDS: f64 = 60.0;
/// 舊版 `DEFAULT_PAGE_CRASH_BACKOFF_SECONDS`。
const DEFAULT_PAGE_CRASH_BACKOFF_SECONDS: [f64; 3] = [30.0, 60.0, 120.0];
/// 舊版 `DEFAULT_CHANNEL_CRASH_WINDOW_SECONDS`。
const DEFAULT_CHANNEL_CRASH_WINDOW_SECONDS: f64 = 600.0;
/// 舊版 `DEFAULT_CHANNEL_QUARANTINE_THRESHOLD`。
const DEFAULT_CHANNEL_QUARANTINE_THRESHOLD: f64 = 4.0;
/// 舊版 `DEFAULT_CHANNEL_QUARANTINE_SECONDS`。
const DEFAULT_CHANNEL_QUARANTINE_SECONDS: f64 = 900.0;
/// 舊版 `DEFAULT_STABLE_RESET_SECONDS`。
const DEFAULT_STABLE_RESET_SECONDS: f64 = 1_800.0;
/// 舊版 `DEFAULT_MULTI_CHANNEL_CRASH_WINDOW_SECONDS`。
const DEFAULT_MULTI_CHANNEL_CRASH_WINDOW_SECONDS: f64 = 15.0;
/// 舊版 `DEFAULT_MULTI_CHANNEL_CRASH_THRESHOLD`。
const DEFAULT_MULTI_CHANNEL_CRASH_THRESHOLD: f64 = 2.0;
/// 舊版 `DEFAULT_BROWSER_FAILURE_WINDOW_SECONDS`。
const DEFAULT_BROWSER_FAILURE_WINDOW_SECONDS: f64 = 600.0;
/// 舊版 `DEFAULT_BROWSER_FAILURE_CONTAINER_THRESHOLD`。
const DEFAULT_BROWSER_FAILURE_CONTAINER_THRESHOLD: f64 = 3.0;
/// 舊版 `DEFAULT_SESSION_START_FAILURE_BACKOFF_SECONDS`。
const DEFAULT_SESSION_START_FAILURE_BACKOFF_SECONDS: [f64; 3] = [30.0, 60.0, 120.0];
/// 舊版 `DEFAULT_SESSION_START_MAXIMUM_COOLDOWN_SECONDS`。
const DEFAULT_SESSION_START_MAXIMUM_COOLDOWN_SECONDS: f64 = 900.0;
/// 舊版 `DEFAULT_RESOURCE_GUARD_SAMPLE_INTERVAL_SECONDS`。
const DEFAULT_RESOURCE_GUARD_SAMPLE_INTERVAL_SECONDS: f64 = 2.0;
/// 舊版 `DEFAULT_RESOURCE_GUARD_STARTUP_RATE_GRACE_SECONDS`。
const DEFAULT_RESOURCE_GUARD_STARTUP_RATE_GRACE_SECONDS: f64 = 120.0;
/// 舊版 `DEFAULT_RESOURCE_GUARD_BASELINE_STREAMS`。
const DEFAULT_RESOURCE_GUARD_BASELINE_STREAMS: f64 = 3.0;
/// 舊版 `DEFAULT_RESOURCE_GUARD_BASE_MEMORY_MIB`。
const DEFAULT_RESOURCE_GUARD_BASE_MEMORY_MIB: f64 = 512.0;
/// 舊版 `DEFAULT_RESOURCE_GUARD_WARNING_MEMORY_MIB`。
const DEFAULT_RESOURCE_GUARD_WARNING_MEMORY_MIB: f64 = 4_096.0;
/// 舊版 `DEFAULT_RESOURCE_GUARD_WARNING_RESET_MEMORY_MIB`。
const DEFAULT_RESOURCE_GUARD_WARNING_RESET_MEMORY_MIB: f64 = 3_840.0;
/// 舊版 `DEFAULT_RESOURCE_GUARD_BROWSER_RECYCLE_MEMORY_MIB`。
const DEFAULT_RESOURCE_GUARD_BROWSER_RECYCLE_MEMORY_MIB: f64 = 4_608.0;
/// 舊版 `DEFAULT_RESOURCE_GUARD_BROWSER_RECYCLE_CONSECUTIVE_SAMPLES`。
const DEFAULT_RESOURCE_GUARD_BROWSER_RECYCLE_CONSECUTIVE_SAMPLES: f64 = 2.0;
/// 舊版 `DEFAULT_RESOURCE_GUARD_EMERGENCY_MEMORY_MIB`。
const DEFAULT_RESOURCE_GUARD_EMERGENCY_MEMORY_MIB: f64 = 5_376.0;
/// 舊版 `DEFAULT_RESOURCE_GUARD_EMERGENCY_SWAP_MIB`。
const DEFAULT_RESOURCE_GUARD_EMERGENCY_SWAP_MIB: f64 = 768.0;
/// 舊版 `DEFAULT_RESOURCE_GUARD_FAST_GROWTH_MIB`。
const DEFAULT_RESOURCE_GUARD_FAST_GROWTH_MIB: f64 = 1_024.0;
/// 舊版 `DEFAULT_RESOURCE_GUARD_FAST_GROWTH_WINDOW_SECONDS`。
const DEFAULT_RESOURCE_GUARD_FAST_GROWTH_WINDOW_SECONDS: f64 = 30.0;
/// 舊版 `DEFAULT_RESOURCE_GUARD_POST_BROWSER_RESTART_RATE_GRACE_SECONDS`。
const DEFAULT_RESOURCE_GUARD_POST_BROWSER_RESTART_RATE_GRACE_SECONDS: f64 = 120.0;
/// 舊版 `DEFAULT_RESOURCE_GUARD_POST_RECYCLE_OBSERVATION_SECONDS`。
const DEFAULT_RESOURCE_GUARD_POST_RECYCLE_OBSERVATION_SECONDS: f64 = 20.0;
/// 舊版 `DEFAULT_RESOURCE_GUARD_POST_RECYCLE_TARGET_MEMORY_MIB`。
const DEFAULT_RESOURCE_GUARD_POST_RECYCLE_TARGET_MEMORY_MIB: f64 = 4_096.0;
/// 舊版 `DEFAULT_RESOURCE_GUARD_POST_RECYCLE_MINIMUM_DROP_MIB`。
const DEFAULT_RESOURCE_GUARD_POST_RECYCLE_MINIMUM_DROP_MIB: f64 = 512.0;
/// 舊版 `MAX_RESOURCE_GUARD_MEMORY_MIB`。
const MAX_RESOURCE_GUARD_MEMORY_MIB: f64 = 1_048_576.0;
/// 舊版 `MINIMUM_VIEWPORT_WIDTH`。
const MINIMUM_VIEWPORT_WIDTH: f64 = 320.0;
/// 舊版 `MINIMUM_VIEWPORT_HEIGHT`。
const MINIMUM_VIEWPORT_HEIGHT: f64 = 180.0;
/// 舊版 `DEFAULT_TELEGRAM_POLLING_TIMEOUT_SECONDS`。
const DEFAULT_TELEGRAM_POLLING_TIMEOUT_SECONDS: f64 = 25.0;
/// 舊版 `MAX_TIMER_DELAY_MS`（Node timer 的上限）。
const MAX_TIMER_DELAY_MS: f64 = 2_147_483_647.0;
/// 舊版 `MAX_TIMER_DELAY_SECONDS`（`Math.floor(MAX_TIMER_DELAY_MS / 1000)`）。
const MAX_TIMER_DELAY_SECONDS: f64 = 2_147_483.0;
/// 舊版 `Number.MAX_SAFE_INTEGER`（`optionalPositiveInteger` 未給上限時的最大值）。
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

/// 設定值記錄的共用形狀（[`JsValue::Object`] 的內容）。
type Record = [(String, JsValue)];

/// 對應舊版 `NodeJS.ProcessEnv`：鍵存在即為已定義（值一律是字串）。
pub(crate) type ProcessEnv = HashMap<String, String>;

/// 對應舊版 `ConfigLogger`：載入器的 debug 日誌介面（正式程式用
/// [`JsonLineLogger`]，測試注入收集用的實作）。
pub(crate) trait ConfigLogger: Send + Sync {
    /// 對應 `debug(event, fields?)`；欄位值為消毒前的 [`SanitizedValue`]。
    fn debug(&self, event: &str, fields: Option<&SanitizedValue>);
}

/// 舊版預設參數的 `NOOP_LOGGER`。
struct NoopConfigLogger;

impl ConfigLogger for NoopConfigLogger {
    fn debug(&self, _event: &str, _fields: Option<&SanitizedValue>) {}
}

/// 對應舊版 `YamlConfigLoader`。
pub(crate) struct YamlConfigLoader {
    /// 舊版 `logger`（建構參數）。
    logger: Arc<dyn ConfigLogger>,
}

impl Default for YamlConfigLoader {
    /// 舊版建構子的預設參數（no-op logger）。
    fn default() -> Self {
        Self {
            logger: Arc::new(NoopConfigLogger),
        }
    }
}

impl YamlConfigLoader {
    /// 對應舊版建構子；沒有預設值的使用者明確傳入。
    pub(crate) fn new(logger: Arc<dyn ConfigLogger>) -> Self {
        Self { logger }
    }

    /// 對應舊版 `load`：讀檔 → 解析 → `${VAR}` 代換 → 驗證 → 建構 [`AppConfig`]。
    pub(crate) async fn load(
        &self,
        path: &str,
        env: &ProcessEnv,
    ) -> Result<AppConfig, ConfigLoadError> {
        // 舊版 `(env.CONFIG_PATH ?? path) || DEFAULT_CONFIG_PATH`：
        // 環境值（含空字串）優先，空值或未定義時用參數，參數為空時用預設。
        let selected = env.get("CONFIG_PATH").map(String::as_str).unwrap_or(path);
        let config_path = if selected.is_empty() {
            DEFAULT_CONFIG_PATH
        } else {
            selected
        };
        let source = read_config_file(config_path).await?;
        let parsed = parse_yaml(&source)?;
        let interpolated = interpolate_environment(&parsed, env);
        let root = require_record(&interpolated, "root")?;
        let config = build_config(root, env, self.logger.as_ref())?;
        // 舊版 deepFreeze：Rust 回傳擁有的值，透過參照無法修改，不需要對應實作。
        Ok(config)
    }
}

/// 對應舊版 `JsonLineLogger` 滿足 `ConfigLogger` 介面的轉接（S2.3 接線用）。
impl ConfigLogger for JsonLineLogger {
    fn debug(&self, event: &str, fields: Option<&SanitizedValue>) {
        // inherent 的 debug 同簽名；以完整路徑呼叫避免誤呼本 impl 造成遞迴。
        JsonLineLogger::debug(self, event, fields);
    }
}

/// 對應舊版 `readConfigFile`：讀取 UTF-8 文字；不存在與其他錯誤分開回報。
async fn read_config_file(path: &str) -> Result<String, ConfigLoadError> {
    match tokio::fs::read_to_string(path).await {
        Ok(source) => Ok(source),
        // 舊版 `error.code === 'ENOENT'`。
        Err(error) if error.kind() == io::ErrorKind::NotFound => Err(
            ConfigLoadError::FileNotFound(ConfigFileNotFoundError::new(path)),
        ),
        // 其他讀取錯誤（權限、目錄、非 UTF-8 等）。
        Err(_) => Err(ConfigLoadError::Parse(ConfigParseError::new(format!(
            "無法讀取設定檔 {path}"
        )))),
    }
}

/// 對應舊版 `buildConfig`：依物件屬性的求值順序驗證並建構設定。
fn build_config(
    root: &Record,
    env: &ProcessEnv,
    logger: &dyn ConfigLogger,
) -> Result<AppConfig, ConfigValidationError> {
    let channels = require_channels(record_get(root, "channels"))?;
    let requested_max_concurrent_streams = optional_positive_integer(
        record_get(root, "max_concurrent_streams"),
        "max_concurrent_streams",
        DEFAULT_MAX_CONCURRENT_STREAMS,
        None,
    )?;
    let max_concurrent_streams = requested_max_concurrent_streams.min(channels.len() as f64);

    if requested_max_concurrent_streams > channels.len() as f64 {
        logger.debug(
            "config_concurrency_clamped",
            Some(&SanitizedValue::Object(vec![
                (
                    "requestedMaxConcurrentStreams".to_string(),
                    SanitizedValue::Number(requested_max_concurrent_streams),
                ),
                (
                    "effectiveMaxConcurrentStreams".to_string(),
                    SanitizedValue::Number(max_concurrent_streams),
                ),
                (
                    "channelCount".to_string(),
                    SanitizedValue::Number(channels.len() as f64),
                ),
            ])),
        );
    }

    let check_interval_seconds = optional_integer_at_least(
        record_get(root, "check_interval_seconds"),
        "check_interval_seconds",
        DEFAULT_CHECK_INTERVAL_SECONDS,
        30.0,
        MAX_TIMER_DELAY_SECONDS,
    )?;
    // X2 的檢查放在 buildConfig 最後（見模組文件），原有順序不變。
    let headless = resolve_boolean_override(
        env.get("HEADLESS").map(String::as_str),
        record_get(root, "headless"),
        "headless",
        true,
    )?;
    let storage_state_path = optional_non_empty_string(
        record_get(root, "storage_state_path"),
        "storage_state_path",
        DEFAULT_STORAGE_STATE_PATH,
    )?;
    let log_level = resolve_log_level(
        env.get("LOG_LEVEL").map(String::as_str),
        record_get(root, "log_level"),
    )?;
    let twitch_api = build_twitch_api(record_get(root, "twitch_api"), env)?;
    let browser = build_browser_config(record_get(root, "browser"), max_concurrent_streams)?;
    let telegram = build_telegram_config(record_get(root, "telegram"), env)?;
    let discord = build_discord_config(record_get(root, "discord"), env)?;

    // X2（§6.2）：檢查覆寫後的值；Rust 版只支援 headless 模式。
    if !headless {
        return Err(ConfigValidationError::new("headless", "必須是 true"));
    }

    Ok(AppConfig {
        channels,
        check_interval_seconds,
        max_concurrent_streams,
        headless,
        storage_state_path,
        log_level,
        twitch_api,
        browser,
        telegram,
        discord,
    })
}

/// 對應舊版 `buildDiscordConfig`。
fn build_discord_config(
    value: Option<&JsValue>,
    env: &ProcessEnv,
) -> Result<DiscordConfig, ConfigValidationError> {
    let discord = optional_record(value, "discord")?;
    let enabled = resolve_boolean_override(
        env.get("DISCORD_ENABLED").map(String::as_str),
        record_get(discord, "enabled"),
        "discord.enabled",
        false,
    )?;
    let bot_token = resolve_optional_string_override(
        env.get("DISCORD_BOT_TOKEN").map(String::as_str),
        record_get(discord, "bot_token"),
        "discord.bot_token",
    )?;
    let application_id = resolve_optional_string_override(
        env.get("DISCORD_APPLICATION_ID").map(String::as_str),
        record_get(discord, "application_id"),
        "discord.application_id",
    )?;
    let guild_id = resolve_optional_string_override(
        env.get("DISCORD_GUILD_ID").map(String::as_str),
        record_get(discord, "guild_id"),
        "discord.guild_id",
    )?;
    let allowed_channel_ids = resolve_discord_channel_ids(
        env.get("DISCORD_ALLOWED_CHANNEL_IDS").map(String::as_str),
        record_get(discord, "allowed_channel_ids"),
    )?;
    let allow_direct_messages = resolve_boolean_override(
        env.get("DISCORD_ALLOW_DIRECT_MESSAGES").map(String::as_str),
        record_get(discord, "allow_direct_messages"),
        "discord.allow_direct_messages",
        false,
    )?;
    let allowed_user_ids = resolve_discord_user_ids(
        env.get("DISCORD_ALLOWED_USER_IDS").map(String::as_str),
        record_get(discord, "allowed_user_ids"),
    )?;

    if enabled && bot_token.is_empty() {
        return Err(ConfigValidationError::new(
            "discord.bot_token",
            "啟用 Discord 時必須是非空字串",
        ));
    }
    if enabled && !discord_snowflake_pattern().is_match(&application_id) {
        return Err(ConfigValidationError::new(
            "discord.application_id",
            "啟用 Discord 時必須是有效的 application ID",
        ));
    }
    if enabled && !guild_id.is_empty() && !discord_snowflake_pattern().is_match(&guild_id) {
        return Err(ConfigValidationError::new(
            "discord.guild_id",
            "必須是有效的 guild ID",
        ));
    }
    if enabled
        && allowed_channel_ids.is_empty()
        && (!allow_direct_messages || allowed_user_ids.is_empty())
    {
        return Err(ConfigValidationError::new(
            "discord.allowed_channel_ids",
            "啟用 Discord 時至少需要一個 channel ID，或啟用私訊並設定 user ID",
        ));
    }
    if enabled && allow_direct_messages && allowed_user_ids.is_empty() {
        return Err(ConfigValidationError::new(
            "discord.allowed_user_ids",
            "啟用 Discord 私訊時至少需要一個 user ID",
        ));
    }

    Ok(DiscordConfig {
        enabled,
        bot_token,
        application_id,
        guild_id,
        allowed_channel_ids,
        allow_direct_messages,
        allowed_user_ids,
    })
}

/// 對應舊版 `buildTelegramConfig`。
fn build_telegram_config(
    value: Option<&JsValue>,
    env: &ProcessEnv,
) -> Result<TelegramConfig, ConfigValidationError> {
    let telegram = optional_record(value, "telegram")?;
    let enabled = resolve_boolean_override(
        env.get("TELEGRAM_ENABLED").map(String::as_str),
        record_get(telegram, "enabled"),
        "telegram.enabled",
        false,
    )?;
    let bot_token = resolve_optional_string_override(
        env.get("TELEGRAM_BOT_TOKEN").map(String::as_str),
        record_get(telegram, "bot_token"),
        "telegram.bot_token",
    )?;
    let allowed_chat_ids = resolve_telegram_chat_ids(
        env.get("TELEGRAM_ALLOWED_CHAT_IDS").map(String::as_str),
        record_get(telegram, "allowed_chat_ids"),
    )?;

    if enabled && bot_token.is_empty() {
        return Err(ConfigValidationError::new(
            "telegram.bot_token",
            "啟用 Telegram 時必須是非空字串",
        ));
    }
    if enabled && allowed_chat_ids.is_empty() {
        return Err(ConfigValidationError::new(
            "telegram.allowed_chat_ids",
            "啟用 Telegram 時至少需要一個 chat ID",
        ));
    }

    // polling_timeout_seconds 照舊版在回傳物件內才驗證（兩個檢查之後）。
    let polling_timeout_seconds = optional_integer_at_least(
        record_get(telegram, "polling_timeout_seconds"),
        "telegram.polling_timeout_seconds",
        DEFAULT_TELEGRAM_POLLING_TIMEOUT_SECONDS,
        1.0,
        50.0,
    )?;

    Ok(TelegramConfig {
        enabled,
        bot_token,
        allowed_chat_ids,
        polling_timeout_seconds,
    })
}

/// 對應舊版 `resolveOptionalStringOverride`：環境值優先；空白字串視同未設定（`''`）。
fn resolve_optional_string_override(
    environment_value: Option<&str>,
    config_value: Option<&JsValue>,
    field: &str,
) -> Result<String, ConfigValidationError> {
    let value = match environment_value {
        Some(value) => Some(JsValue::String(value.to_string())),
        None => config_value.cloned(),
    };
    let Some(value) = value else {
        return Ok(String::new());
    };
    // 舊版 `typeof value === 'string' && value.trim() === ''`。
    if let JsValue::String(text) = &value
        && text.trim().is_empty()
    {
        return Ok(String::new());
    }
    require_non_empty_string(Some(&value), field)
}

/// 對應舊版 `resolveTelegramChatIds`。
fn resolve_telegram_chat_ids(
    environment_value: Option<&str>,
    config_value: Option<&JsValue>,
) -> Result<Vec<String>, ConfigValidationError> {
    // 舊版三元式：環境值存在時以逗號切開（空字串 → 空清單），否則用設定值。
    let values = match environment_value {
        Some(value) => Some(if value.trim().is_empty() {
            Vec::new()
        } else {
            value
                .split(',')
                .map(|item| JsValue::String(item.trim().to_string()))
                .collect::<Vec<_>>()
        }),
        None => match config_value {
            None => None,
            Some(JsValue::Array(items)) => Some(items.clone()),
            Some(_) => {
                return Err(ConfigValidationError::new(
                    "telegram.allowed_chat_ids",
                    "必須是 chat ID 字串陣列",
                ));
            }
        },
    };
    let Some(values) = values else {
        return Ok(Vec::new());
    };

    // 舊版 `[...new Set(values.map(...))]`：逐元素驗證後以第一次出現的順序去重。
    let mut result: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for (index, value) in values.into_iter().enumerate() {
        let text = match &value {
            JsValue::String(text) if telegram_chat_id_pattern().is_match(text) => text,
            _ => {
                return Err(ConfigValidationError::new(
                    format!("telegram.allowed_chat_ids[{index}]"),
                    "必須是整數格式的 chat ID 字串",
                ));
            }
        };
        if seen.insert(text.clone()) {
            result.push(text.clone());
        }
    }
    Ok(result)
}

/// 對應舊版 `resolveDiscordChannelIds`。
fn resolve_discord_channel_ids(
    environment_value: Option<&str>,
    config_value: Option<&JsValue>,
) -> Result<Vec<String>, ConfigValidationError> {
    resolve_discord_snowflake_list(
        environment_value,
        config_value,
        "discord.allowed_channel_ids",
        "channel ID",
    )
}

/// 對應舊版 `resolveDiscordUserIds`。
fn resolve_discord_user_ids(
    environment_value: Option<&str>,
    config_value: Option<&JsValue>,
) -> Result<Vec<String>, ConfigValidationError> {
    resolve_discord_snowflake_list(
        environment_value,
        config_value,
        "discord.allowed_user_ids",
        "user ID",
    )
}

/// 對應舊版 `resolveDiscordSnowflakeList`。
fn resolve_discord_snowflake_list(
    environment_value: Option<&str>,
    config_value: Option<&JsValue>,
    field: &str,
    label: &str,
) -> Result<Vec<String>, ConfigValidationError> {
    let values = match environment_value {
        Some(value) => Some(if value.trim().is_empty() {
            Vec::new()
        } else {
            value
                .split(',')
                .map(|item| JsValue::String(item.trim().to_string()))
                .collect::<Vec<_>>()
        }),
        None => match config_value {
            None => None,
            Some(JsValue::Array(items)) => Some(items.clone()),
            Some(_) => {
                return Err(ConfigValidationError::new(
                    field,
                    format!("必須是 {label} 字串陣列"),
                ));
            }
        },
    };
    let Some(values) = values else {
        return Ok(Vec::new());
    };

    // 舊版 `[...new Set(values.map(...))]`：逐元素驗證後以第一次出現的順序去重。
    let mut result: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for (index, value) in values.into_iter().enumerate() {
        let text = match &value {
            JsValue::String(text) if discord_snowflake_pattern().is_match(text) => text,
            _ => {
                return Err(ConfigValidationError::new(
                    format!("{field}[{index}]"),
                    format!("必須是 Discord snowflake {label} 字串"),
                ));
            }
        };
        if seen.insert(text.clone()) {
            result.push(text.clone());
        }
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
// 值級輔助
// ---------------------------------------------------------------------------

/// 對應舊版 `buildTwitchApi`。
fn build_twitch_api(
    value: Option<&JsValue>,
    env: &ProcessEnv,
) -> Result<TwitchApiConfig, ConfigValidationError> {
    let twitch_api = optional_record(value, "twitch_api")?;
    let access_token = resolve_optional_string_override(
        env.get("TWITCH_ACCESS_TOKEN").map(String::as_str),
        record_get(twitch_api, "access_token"),
        "twitch_api.access_token",
    )?;
    let client_secret = resolve_optional_string_override(
        env.get("TWITCH_CLIENT_SECRET").map(String::as_str),
        record_get(twitch_api, "client_secret"),
        "twitch_api.client_secret",
    )?;

    if access_token.is_empty() && client_secret.is_empty() {
        return Err(ConfigValidationError::new(
            "twitch_api",
            "access_token 與 client_secret 至少需要一個",
        ));
    }

    Ok(TwitchApiConfig {
        // 舊版 `env.TWITCH_CLIENT_ID ?? twitchApi.client_id`（未定義時直接要求非空字串）。
        client_id: {
            let client_id_value = match env.get("TWITCH_CLIENT_ID") {
                Some(value) => Some(JsValue::String(value.clone())),
                None => record_get(twitch_api, "client_id").cloned(),
            };
            require_non_empty_string(client_id_value.as_ref(), "twitch_api.client_id")?
        },
        access_token,
        client_secret,
    })
}

/// 對應舊版 `buildBrowserConfig`（求值順序照回傳物件的屬性順序）。
fn build_browser_config(
    value: Option<&JsValue>,
    max_concurrent_streams: f64,
) -> Result<BrowserConfig, ConfigValidationError> {
    let browser = optional_record(value, "browser")?;

    let engine = optional_browser_engine(record_get(browser, "engine"))?;
    let navigation_timeout_ms = optional_positive_integer(
        record_get(browser, "navigation_timeout_ms"),
        "browser.navigation_timeout_ms",
        DEFAULT_NAVIGATION_TIMEOUT_MS,
        Some(MAX_TIMER_DELAY_MS),
    )?;
    let page_health_check_interval_seconds = optional_positive_integer(
        record_get(browser, "page_health_check_interval_seconds"),
        "browser.page_health_check_interval_seconds",
        DEFAULT_PAGE_HEALTH_CHECK_INTERVAL_SECONDS,
        Some(MAX_TIMER_DELAY_SECONDS),
    )?;
    let reward_check_interval_seconds = optional_positive_integer(
        record_get(browser, "reward_check_interval_seconds"),
        "browser.reward_check_interval_seconds",
        DEFAULT_REWARD_CHECK_INTERVAL_SECONDS,
        Some(MAX_TIMER_DELAY_SECONDS),
    )?;
    let page_refresh_interval_seconds = optional_integer_at_least(
        record_get(browser, "page_refresh_interval_seconds"),
        "browser.page_refresh_interval_seconds",
        DEFAULT_PAGE_REFRESH_INTERVAL_SECONDS,
        0.0,
        MAX_TIMER_DELAY_SECONDS,
    )?;
    let restart_on_crash = optional_boolean(
        record_get(browser, "restart_on_crash"),
        "browser.restart_on_crash",
        true,
    )?;
    let stream_quality = optional_stream_quality(record_get(browser, "stream_quality"))?;
    let enforce_stream_quality_seconds = optional_positive_integer(
        record_get(browser, "enforce_stream_quality_seconds"),
        "browser.enforce_stream_quality_seconds",
        DEFAULT_ENFORCE_STREAM_QUALITY_SECONDS,
        Some(MAX_TIMER_DELAY_SECONDS),
    )?;
    let viewport_width = optional_integer_at_least(
        record_get(browser, "viewport_width"),
        "browser.viewport_width",
        DEFAULT_VIEWPORT_WIDTH,
        MINIMUM_VIEWPORT_WIDTH,
        3_840.0,
    )?;
    let viewport_height = optional_integer_at_least(
        record_get(browser, "viewport_height"),
        "browser.viewport_height",
        DEFAULT_VIEWPORT_HEIGHT,
        MINIMUM_VIEWPORT_HEIGHT,
        2_160.0,
    )?;
    let mute_audio = optional_boolean(
        record_get(browser, "mute_audio"),
        "browser.mute_audio",
        true,
    )?;
    let block_images = optional_boolean(
        record_get(browser, "block_images"),
        "browser.block_images",
        false,
    )?;
    let block_fonts = optional_boolean(
        record_get(browser, "block_fonts"),
        "browser.block_fonts",
        false,
    )?;
    let block_known_tracking = optional_boolean(
        record_get(browser, "block_known_tracking"),
        "browser.block_known_tracking",
        false,
    )?;
    let disable_chat = optional_boolean(
        record_get(browser, "disable_chat"),
        "browser.disable_chat",
        true,
    )?;
    let resource_telemetry_interval_seconds = optional_positive_integer(
        record_get(browser, "resource_telemetry_interval_seconds"),
        "browser.resource_telemetry_interval_seconds",
        DEFAULT_RESOURCE_TELEMETRY_INTERVAL_SECONDS,
        Some(MAX_TIMER_DELAY_SECONDS),
    )?;
    let recovery = build_browser_recovery_config(record_get(browser, "recovery"))?;
    let session_start = build_browser_session_start_config(record_get(browser, "session_start"))?;
    let resource_guard = build_resource_guard_config(
        record_get(browser, "resource_guard"),
        max_concurrent_streams,
    )?;

    Ok(BrowserConfig {
        engine,
        navigation_timeout_ms,
        page_health_check_interval_seconds,
        reward_check_interval_seconds,
        page_refresh_interval_seconds,
        restart_on_crash,
        stream_quality,
        enforce_stream_quality_seconds,
        viewport_width,
        viewport_height,
        mute_audio,
        block_images,
        block_fonts,
        block_known_tracking,
        disable_chat,
        resource_telemetry_interval_seconds,
        recovery,
        session_start,
        resource_guard,
    })
}

/// 對應舊版 `buildBrowserRecoveryConfig`。
fn build_browser_recovery_config(
    value: Option<&JsValue>,
) -> Result<BrowserRecoveryConfig, ConfigValidationError> {
    let recovery = optional_record(value, "browser.recovery")?;

    let page_crash_backoff_seconds = optional_positive_integer_array(
        record_get(recovery, "page_crash_backoff_seconds"),
        "browser.recovery.page_crash_backoff_seconds",
        &DEFAULT_PAGE_CRASH_BACKOFF_SECONDS,
        MAX_TIMER_DELAY_SECONDS,
    )?;
    let channel_crash_window_seconds = optional_positive_integer(
        record_get(recovery, "channel_crash_window_seconds"),
        "browser.recovery.channel_crash_window_seconds",
        DEFAULT_CHANNEL_CRASH_WINDOW_SECONDS,
        Some(MAX_TIMER_DELAY_SECONDS),
    )?;
    let channel_quarantine_threshold = optional_integer_at_least(
        record_get(recovery, "channel_quarantine_threshold"),
        "browser.recovery.channel_quarantine_threshold",
        DEFAULT_CHANNEL_QUARANTINE_THRESHOLD,
        2.0,
        MAX_SAFE_INTEGER,
    )?;
    let channel_quarantine_seconds = optional_integer_at_least(
        record_get(recovery, "channel_quarantine_seconds"),
        "browser.recovery.channel_quarantine_seconds",
        DEFAULT_CHANNEL_QUARANTINE_SECONDS,
        600.0,
        1_800.0,
    )?;
    let stable_reset_seconds = optional_positive_integer(
        record_get(recovery, "stable_reset_seconds"),
        "browser.recovery.stable_reset_seconds",
        DEFAULT_STABLE_RESET_SECONDS,
        Some(MAX_TIMER_DELAY_SECONDS),
    )?;
    let multi_channel_crash_window_seconds = optional_positive_integer(
        record_get(recovery, "multi_channel_crash_window_seconds"),
        "browser.recovery.multi_channel_crash_window_seconds",
        DEFAULT_MULTI_CHANNEL_CRASH_WINDOW_SECONDS,
        Some(MAX_TIMER_DELAY_SECONDS),
    )?;
    let multi_channel_crash_threshold = optional_integer_at_least(
        record_get(recovery, "multi_channel_crash_threshold"),
        "browser.recovery.multi_channel_crash_threshold",
        DEFAULT_MULTI_CHANNEL_CRASH_THRESHOLD,
        2.0,
        MAX_SAFE_INTEGER,
    )?;
    let browser_failure_window_seconds = optional_positive_integer(
        record_get(recovery, "browser_failure_window_seconds"),
        "browser.recovery.browser_failure_window_seconds",
        DEFAULT_BROWSER_FAILURE_WINDOW_SECONDS,
        Some(MAX_TIMER_DELAY_SECONDS),
    )?;
    let browser_failure_container_threshold = optional_positive_integer(
        record_get(recovery, "browser_failure_container_threshold"),
        "browser.recovery.browser_failure_container_threshold",
        DEFAULT_BROWSER_FAILURE_CONTAINER_THRESHOLD,
        None,
    )?;

    Ok(BrowserRecoveryConfig {
        page_crash_backoff_seconds,
        channel_crash_window_seconds,
        channel_quarantine_threshold,
        channel_quarantine_seconds,
        stable_reset_seconds,
        multi_channel_crash_window_seconds,
        multi_channel_crash_threshold,
        browser_failure_window_seconds,
        browser_failure_container_threshold,
    })
}

/// 對應舊版 `buildBrowserSessionStartConfig`。
fn build_browser_session_start_config(
    value: Option<&JsValue>,
) -> Result<BrowserSessionStartConfig, ConfigValidationError> {
    let session_start = optional_record(value, "browser.session_start")?;

    let failure_backoff_seconds = optional_positive_integer_array(
        record_get(session_start, "failure_backoff_seconds"),
        "browser.session_start.failure_backoff_seconds",
        &DEFAULT_SESSION_START_FAILURE_BACKOFF_SECONDS,
        MAX_TIMER_DELAY_SECONDS,
    )?;
    let maximum_cooldown_seconds = optional_positive_integer(
        record_get(session_start, "maximum_cooldown_seconds"),
        "browser.session_start.maximum_cooldown_seconds",
        DEFAULT_SESSION_START_MAXIMUM_COOLDOWN_SECONDS,
        Some(MAX_TIMER_DELAY_SECONDS),
    )?;

    Ok(BrowserSessionStartConfig {
        failure_backoff_seconds,
        maximum_cooldown_seconds,
    })
}

/// 對應舊版 `buildResourceGuardConfig`（含門檻關係與縮放後的有效門檻檢查）。
fn build_resource_guard_config(
    value: Option<&JsValue>,
    max_concurrent_streams: f64,
) -> Result<ResourceGuardConfig, ConfigValidationError> {
    let guard = optional_record(value, "browser.resource_guard")?;

    let sample_interval_seconds = optional_positive_integer(
        record_get(guard, "sample_interval_seconds"),
        "browser.resource_guard.sample_interval_seconds",
        DEFAULT_RESOURCE_GUARD_SAMPLE_INTERVAL_SECONDS,
        Some(MAX_TIMER_DELAY_SECONDS),
    )?;
    let startup_rate_grace_seconds = optional_integer_at_least(
        record_get(guard, "startup_rate_grace_seconds"),
        "browser.resource_guard.startup_rate_grace_seconds",
        DEFAULT_RESOURCE_GUARD_STARTUP_RATE_GRACE_SECONDS,
        0.0,
        MAX_TIMER_DELAY_SECONDS,
    )?;
    let scale_with_streams = optional_boolean(
        record_get(guard, "scale_with_streams"),
        "browser.resource_guard.scale_with_streams",
        true,
    )?;
    let baseline_streams = optional_positive_integer(
        record_get(guard, "baseline_streams"),
        "browser.resource_guard.baseline_streams",
        DEFAULT_RESOURCE_GUARD_BASELINE_STREAMS,
        Some(1_024.0),
    )?;
    let base_memory_mib = optional_integer_at_least(
        record_get(guard, "base_memory_mib"),
        "browser.resource_guard.base_memory_mib",
        DEFAULT_RESOURCE_GUARD_BASE_MEMORY_MIB,
        0.0,
        MAX_RESOURCE_GUARD_MEMORY_MIB,
    )?;
    let warning_memory_mib = optional_positive_integer(
        record_get(guard, "warning_memory_mib"),
        "browser.resource_guard.warning_memory_mib",
        DEFAULT_RESOURCE_GUARD_WARNING_MEMORY_MIB,
        Some(MAX_RESOURCE_GUARD_MEMORY_MIB),
    )?;
    let warning_reset_memory_mib = optional_positive_integer(
        record_get(guard, "warning_reset_memory_mib"),
        "browser.resource_guard.warning_reset_memory_mib",
        DEFAULT_RESOURCE_GUARD_WARNING_RESET_MEMORY_MIB,
        Some(MAX_RESOURCE_GUARD_MEMORY_MIB),
    )?;
    let browser_recycle_memory_mib = optional_positive_integer(
        record_get(guard, "browser_recycle_memory_mib"),
        "browser.resource_guard.browser_recycle_memory_mib",
        DEFAULT_RESOURCE_GUARD_BROWSER_RECYCLE_MEMORY_MIB,
        Some(MAX_RESOURCE_GUARD_MEMORY_MIB),
    )?;
    let browser_recycle_consecutive_samples = optional_positive_integer(
        record_get(guard, "browser_recycle_consecutive_samples"),
        "browser.resource_guard.browser_recycle_consecutive_samples",
        DEFAULT_RESOURCE_GUARD_BROWSER_RECYCLE_CONSECUTIVE_SAMPLES,
        Some(1_000.0),
    )?;
    let emergency_memory_mib = optional_positive_integer(
        record_get(guard, "emergency_memory_mib"),
        "browser.resource_guard.emergency_memory_mib",
        DEFAULT_RESOURCE_GUARD_EMERGENCY_MEMORY_MIB,
        Some(MAX_RESOURCE_GUARD_MEMORY_MIB),
    )?;
    let emergency_swap_mib = optional_positive_integer(
        record_get(guard, "emergency_swap_mib"),
        "browser.resource_guard.emergency_swap_mib",
        DEFAULT_RESOURCE_GUARD_EMERGENCY_SWAP_MIB,
        Some(MAX_RESOURCE_GUARD_MEMORY_MIB),
    )?;
    let fast_growth_mib = optional_positive_integer(
        record_get(guard, "fast_growth_mib"),
        "browser.resource_guard.fast_growth_mib",
        DEFAULT_RESOURCE_GUARD_FAST_GROWTH_MIB,
        Some(MAX_RESOURCE_GUARD_MEMORY_MIB),
    )?;
    let fast_growth_window_seconds = optional_positive_integer(
        record_get(guard, "fast_growth_window_seconds"),
        "browser.resource_guard.fast_growth_window_seconds",
        DEFAULT_RESOURCE_GUARD_FAST_GROWTH_WINDOW_SECONDS,
        Some(MAX_TIMER_DELAY_SECONDS),
    )?;
    let post_browser_restart_rate_grace_seconds = optional_integer_at_least(
        record_get(guard, "post_browser_restart_rate_grace_seconds"),
        "browser.resource_guard.post_browser_restart_rate_grace_seconds",
        DEFAULT_RESOURCE_GUARD_POST_BROWSER_RESTART_RATE_GRACE_SECONDS,
        0.0,
        MAX_TIMER_DELAY_SECONDS,
    )?;
    let post_recycle_observation_seconds = optional_positive_integer(
        record_get(guard, "post_recycle_observation_seconds"),
        "browser.resource_guard.post_recycle_observation_seconds",
        DEFAULT_RESOURCE_GUARD_POST_RECYCLE_OBSERVATION_SECONDS,
        Some(MAX_TIMER_DELAY_SECONDS),
    )?;
    let post_recycle_target_memory_mib = optional_positive_integer(
        record_get(guard, "post_recycle_target_memory_mib"),
        "browser.resource_guard.post_recycle_target_memory_mib",
        DEFAULT_RESOURCE_GUARD_POST_RECYCLE_TARGET_MEMORY_MIB,
        Some(MAX_RESOURCE_GUARD_MEMORY_MIB),
    )?;
    let post_recycle_minimum_drop_mib = optional_positive_integer(
        record_get(guard, "post_recycle_minimum_drop_mib"),
        "browser.resource_guard.post_recycle_minimum_drop_mib",
        DEFAULT_RESOURCE_GUARD_POST_RECYCLE_MINIMUM_DROP_MIB,
        Some(MAX_RESOURCE_GUARD_MEMORY_MIB),
    )?;

    // 錨點門檻的關係檢查（順序照舊版）。
    if warning_reset_memory_mib >= warning_memory_mib {
        return Err(ConfigValidationError::new(
            "browser.resource_guard.warning_reset_memory_mib",
            "必須小於 browser.resource_guard.warning_memory_mib",
        ));
    }
    if warning_memory_mib >= browser_recycle_memory_mib {
        return Err(ConfigValidationError::new(
            "browser.resource_guard.warning_memory_mib",
            "必須小於 browser.resource_guard.browser_recycle_memory_mib",
        ));
    }
    if browser_recycle_memory_mib >= emergency_memory_mib {
        return Err(ConfigValidationError::new(
            "browser.resource_guard.browser_recycle_memory_mib",
            "必須小於 browser.resource_guard.emergency_memory_mib",
        ));
    }
    if post_recycle_target_memory_mib >= browser_recycle_memory_mib {
        return Err(ConfigValidationError::new(
            "browser.resource_guard.post_recycle_target_memory_mib",
            "必須小於 browser.resource_guard.browser_recycle_memory_mib",
        ));
    }
    if fast_growth_window_seconds < sample_interval_seconds {
        return Err(ConfigValidationError::new(
            "browser.resource_guard.fast_growth_window_seconds",
            "必須大於或等於 browser.resource_guard.sample_interval_seconds",
        ));
    }

    let effective = compute_effective_resource_guard_thresholds(
        &ResourceGuardAnchorThresholds {
            scale_with_streams,
            baseline_streams,
            base_memory_mib,
            warning_memory_mib,
            warning_reset_memory_mib,
            browser_recycle_memory_mib,
            emergency_memory_mib,
            post_recycle_target_memory_mib,
        },
        max_concurrent_streams,
    );

    if !(effective.warning_reset_memory_mib < effective.warning_memory_mib
        && effective.warning_memory_mib < effective.browser_recycle_memory_mib
        && effective.browser_recycle_memory_mib < effective.emergency_memory_mib)
    {
        return Err(ConfigValidationError::new(
            "browser.resource_guard",
            "縮放後的記憶體門檻必須維持 warning_reset < warning < recycle < emergency",
        ));
    }

    Ok(ResourceGuardConfig {
        // `enabled` 照舊版在回傳物件內才驗證（所有檢查與縮放之後）。
        enabled: optional_boolean(
            record_get(guard, "enabled"),
            "browser.resource_guard.enabled",
            true,
        )?,
        sample_interval_seconds,
        startup_rate_grace_seconds,
        scale_with_streams,
        baseline_streams,
        base_memory_mib,
        warning_memory_mib,
        warning_reset_memory_mib,
        browser_recycle_memory_mib,
        browser_recycle_consecutive_samples,
        emergency_memory_mib,
        emergency_swap_mib,
        fast_growth_mib,
        fast_growth_window_seconds,
        post_browser_restart_rate_grace_seconds,
        post_recycle_observation_seconds,
        post_recycle_target_memory_mib,
        post_recycle_minimum_drop_mib,
        effective,
    })
}

/// 對應舊版 `optionalStreamQuality`。
fn optional_stream_quality(
    value: Option<&JsValue>,
) -> Result<StreamQuality, ConfigValidationError> {
    let Some(value) = value else {
        return Ok(DEFAULT_STREAM_QUALITY);
    };
    let quality = match value {
        JsValue::String(text) => StreamQuality::parse(text),
        _ => None,
    };
    let Some(quality) = quality else {
        return Err(ConfigValidationError::new(
            "browser.stream_quality",
            format!("必須是 {} 其中之一", STREAM_QUALITIES.join("、")),
        ));
    };
    Ok(quality)
}

/// 對應舊版 `optionalBrowserEngine`，外加 X1（§6.2）：firefox 在驗證通過後
/// 另以 X1 的訊息拒絕（仍先照舊版檢查允許清單，未知引擎的訊息不變）。
fn optional_browser_engine(
    value: Option<&JsValue>,
) -> Result<BrowserEngine, ConfigValidationError> {
    let Some(value) = value else {
        return Ok(DEFAULT_BROWSER_ENGINE);
    };
    let engine = match value {
        JsValue::String(text) => BrowserEngine::parse(text),
        _ => None,
    };
    let Some(engine) = engine else {
        return Err(ConfigValidationError::new(
            "browser.engine",
            format!("必須是 {} 其中之一", BROWSER_ENGINES.join("、")),
        ));
    };
    if engine == BrowserEngine::Firefox {
        return Err(ConfigValidationError::new(
            "browser.engine",
            "必須是 chromium",
        ));
    }
    Ok(engine)
}

/// 對應舊版 `requireChannels`。
fn require_channels(value: Option<&JsValue>) -> Result<Vec<String>, ConfigValidationError> {
    let Some(JsValue::Array(items)) = value else {
        return Err(ConfigValidationError::new("channels", "必須是非空陣列"));
    };
    if items.is_empty() {
        return Err(ConfigValidationError::new("channels", "必須是非空陣列"));
    }

    let mut channels = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let text = match item {
            JsValue::String(text) if channel_pattern().is_match(text) => text,
            _ => {
                return Err(ConfigValidationError::new(
                    format!("channels[{index}]"),
                    "必須是 1 到 25 字元的英數字或底線",
                ));
            }
        };
        channels.push(text.clone());
    }
    Ok(channels)
}

/// 對應舊版 `optionalIntegerAtLeast`。
fn optional_integer_at_least(
    value: Option<&JsValue>,
    field: &str,
    fallback: f64,
    minimum: f64,
    maximum: f64,
) -> Result<f64, ConfigValidationError> {
    let Some(value) = value else {
        return Ok(fallback);
    };
    let number_value = require_integer(Some(value), field)?;
    if number_value < minimum {
        return Err(ConfigValidationError::new(
            field,
            format!("必須大於或等於 {minimum}"),
        ));
    }
    if number_value > maximum {
        return Err(ConfigValidationError::new(
            field,
            format!("必須小於或等於 {maximum}"),
        ));
    }
    Ok(number_value)
}

/// 對應舊版 `optionalPositiveInteger`（`maximum` 未給時為 [`MAX_SAFE_INTEGER`]）。
fn optional_positive_integer(
    value: Option<&JsValue>,
    field: &str,
    fallback: f64,
    maximum: Option<f64>,
) -> Result<f64, ConfigValidationError> {
    optional_integer_at_least(
        value,
        field,
        fallback,
        1.0,
        maximum.unwrap_or(MAX_SAFE_INTEGER),
    )
}

/// 對應舊版 `optionalPositiveIntegerArray`。
fn optional_positive_integer_array(
    value: Option<&JsValue>,
    field: &str,
    fallback: &[f64],
    maximum: f64,
) -> Result<Vec<f64>, ConfigValidationError> {
    let Some(value) = value else {
        return Ok(fallback.to_vec());
    };
    let JsValue::Array(items) = value else {
        return Err(ConfigValidationError::new(field, "必須是非空的正整數陣列"));
    };
    if items.is_empty() {
        return Err(ConfigValidationError::new(field, "必須是非空的正整數陣列"));
    }
    items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            optional_positive_integer(Some(item), &format!("{field}[{index}]"), 1.0, Some(maximum))
        })
        .collect()
}

/// 對應舊版 `requireInteger`（`Number.isSafeInteger`）。
fn require_integer(value: Option<&JsValue>, field: &str) -> Result<f64, ConfigValidationError> {
    match value {
        Some(JsValue::Number(number)) if is_safe_integer(*number) => Ok(*number),
        _ => Err(ConfigValidationError::new(field, "必須是整數")),
    }
}

/// 舊版 `Number.isSafeInteger`：有限、整數值且絕對值 ≤ 2^53 − 1。
fn is_safe_integer(number: f64) -> bool {
    number.is_finite() && number.trunc() == number && number.abs() <= MAX_SAFE_INTEGER
}

/// 對應舊版 `resolveBooleanOverride`：環境值存在時以環境為準。
fn resolve_boolean_override(
    environment_value: Option<&str>,
    config_value: Option<&JsValue>,
    field: &str,
    fallback: bool,
) -> Result<bool, ConfigValidationError> {
    if let Some(environment_value) = environment_value {
        return parse_environment_boolean(environment_value, field);
    }
    optional_boolean(config_value, field, fallback)
}

/// 對應舊版 `parseEnvironmentBoolean`。
fn parse_environment_boolean(value: &str, field: &str) -> Result<bool, ConfigValidationError> {
    match value.trim().to_lowercase().as_str() {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(ConfigValidationError::new(
            field,
            "環境變數必須是 true 或 false",
        )),
    }
}

/// 對應舊版 `optionalBoolean`。
fn optional_boolean(
    value: Option<&JsValue>,
    field: &str,
    fallback: bool,
) -> Result<bool, ConfigValidationError> {
    match value {
        None => Ok(fallback),
        Some(JsValue::Bool(value)) => Ok(*value),
        Some(_) => Err(ConfigValidationError::new(field, "必須是布林值")),
    }
}

/// 對應舊版 `resolveLogLevel`：環境值 ?? 設定值 ?? `'info'`。
fn resolve_log_level(
    environment_value: Option<&str>,
    config_value: Option<&JsValue>,
) -> Result<LogLevel, ConfigValidationError> {
    let value = match environment_value {
        Some(value) => Some(JsValue::String(value.to_string())),
        None => config_value.cloned(),
    };
    let value = value.unwrap_or(JsValue::String("info".to_string()));
    let level = match &value {
        JsValue::String(text) if LOG_LEVELS.contains(&text.as_str()) => Some(match text.as_str() {
            "debug" => LogLevel::Debug,
            "info" => LogLevel::Info,
            "warn" => LogLevel::Warn,
            _ => LogLevel::Error,
        }),
        _ => None,
    };
    let Some(level) = level else {
        return Err(ConfigValidationError::new(
            "log_level",
            format!("必須是 {} 其中之一", LOG_LEVELS.join("、")),
        ));
    };
    Ok(level)
}

/// 對應舊版 `optionalNonEmptyString`。
fn optional_non_empty_string(
    value: Option<&JsValue>,
    field: &str,
    fallback: &str,
) -> Result<String, ConfigValidationError> {
    match value {
        None => Ok(fallback.to_string()),
        Some(value) => require_non_empty_string(Some(value), field),
    }
}

/// 對應舊版 `requireNonEmptyString`（`None` 對應 JS 的 `undefined`）。
fn require_non_empty_string(
    value: Option<&JsValue>,
    field: &str,
) -> Result<String, ConfigValidationError> {
    match value {
        Some(JsValue::String(text)) if !text.trim().is_empty() => Ok(text.clone()),
        _ => Err(ConfigValidationError::new(field, "必須是非空字串")),
    }
}

/// 對應舊版 `requireRecord`（null、陣列與純值都視為不是物件）。
fn require_record<'a>(
    value: &'a JsValue,
    field: &str,
) -> Result<&'a Record, ConfigValidationError> {
    match value {
        JsValue::Object(entries) => Ok(entries),
        _ => Err(ConfigValidationError::new(field, "必須是物件")),
    }
}

/// 對應舊版 `value === undefined ? {} : requireRecord(value, field)`：
/// 未定義時以空記錄（所有欄位走預設值）繼續。
fn optional_record<'a>(
    value: Option<&'a JsValue>,
    field: &str,
) -> Result<&'a Record, ConfigValidationError> {
    match value {
        Some(value) => require_record(value, field),
        None => Ok(&[]),
    }
}

/// 記錄的欄位查找（鍵在組合階段已去重，取第一個相符即可）。
fn record_get<'a>(record: &'a Record, key: &str) -> Option<&'a JsValue> {
    record
        .iter()
        .find(|(existing_key, _)| existing_key == key)
        .map(|(_, value)| value)
}

/// 對應舊版 `interpolateEnvironment`：只替換字串值（遞迴進入陣列與物件），
/// 鍵名不替換；未定義的變數換成空字串。
fn interpolate_environment(value: &JsValue, env: &ProcessEnv) -> JsValue {
    match value {
        JsValue::String(text) => JsValue::String(
            env_reference_pattern()
                .replace_all(text, |captures: &Captures| {
                    // 舊版 `env[variableName] ?? ''`。
                    env.get(&captures[1]).map(String::as_str).unwrap_or("")
                })
                .into_owned(),
        ),
        JsValue::Array(items) => JsValue::Array(
            items
                .iter()
                .map(|item| interpolate_environment(item, env))
                .collect(),
        ),
        JsValue::Object(entries) => JsValue::Object(
            entries
                .iter()
                .map(|(key, item)| (key.clone(), interpolate_environment(item, env)))
                .collect(),
        ),
        other => other.clone(),
    }
}

// ---------------------------------------------------------------------------
// 固定樣式（編譯失敗代表實作錯誤，依 redact.rs 的前例以明確訊息終止）
// ---------------------------------------------------------------------------

/// 編譯固定樣式。
fn compile_pattern(source: &str) -> Regex {
    match Regex::new(source) {
        Ok(regex) => regex,
        Err(error) => panic!("invalid built-in config pattern {source:?}: {error}"),
    }
}

/// 舊版 `CHANNEL_PATTERN`：`/^[A-Za-z0-9_]{1,25}$/u`。
fn channel_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| compile_pattern("^[A-Za-z0-9_]{1,25}$"))
}

/// 舊版 `ENV_REFERENCE_PATTERN`：`/\$\{([A-Za-z_][A-Za-z0-9_]*)\}/gu`。
fn env_reference_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| compile_pattern(r"\$\{([A-Za-z_][A-Za-z0-9_]*)\}"))
}

/// 舊版 `TELEGRAM_CHAT_ID_PATTERN`：`/^-?\d+$/u`（`\d` 展開成 `[0-9]`，與 JS 一致）。
fn telegram_chat_id_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| compile_pattern("^-?[0-9]+$"))
}

/// 舊版 `DISCORD_SNOWFLAKE_PATTERN`：`/^\d{1,20}$/u`（`\d` 展開成 `[0-9]`）。
fn discord_snowflake_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| compile_pattern("^[0-9]{1,20}$"))
}

#[cfg(test)]
mod tests;
