//! 共用指令介面：`notifications/BotCommandContext.ts` 逐方法移植（§4.2，S4.1）。
//!
//! 介面形狀照舊版：Telegram／Discord Bot 用它跑檢查、查清單、改設定。
//! 錯誤的分類照舊版 JavaScript 的鬆散 reject 語意建模：
//!
//! - `run_check`／`pause_checks`／`refresh_pages`／`capture_screenshot`／
//!   `get_channel_points` 在舊版回傳 Promise、reject 時只由呼叫端記 log（不取
//!   name／message），移植以 [`BotCommandError`]（只保留訊息）呈現。
//! - `set_channels`／`set_max_concurrent_streams` 的 reject 是
//!   `ConfigValidationError` 或其他 Error（`runConfigCommand` 依
//!   `instanceof ConfigValidationError` 分回覆），移植以
//!   [`ConfigWriteError`](crate::config::writeback::ConfigWriteError) 呈現，
//!   `name()` 即舊版 `Error.name`。
//!
//! S3.5／S3.4 未移植前，scheduler／sessions 的型別依 DEC-013 在本檔
//! 先行定義同形狀型別（欄位名稱與型別和舊版一致），落地後改 import 正式型別。
#![allow(dead_code)]

use async_trait::async_trait;

use crate::config::writeback::{ConfigWriteError, RuntimeWatchConfig};

/// 對應舊版 Promise reject 的 `Error`：舊版呼叫端不使用 name／message（catch
/// 只記 warn），移植以訊息字串保存最少診斷資訊。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BotCommandError {
    pub(crate) message: String,
}

/// 舊版 `WatchdogSchedulerSnapshot`（DEC-013：S3.5 未落地前先在此定義，
/// 欄位照舊版 `scheduler/WatchdogScheduler.ts`）。
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct WatchdogSchedulerSnapshot {
    pub(crate) running: bool,
    pub(crate) check_in_flight: bool,
    /// 舊版 `retryAt?: string`（ISO 時間字串）。
    pub(crate) retry_at: Option<String>,
    /// 舊版 `lastCheckedAt?: string`。
    pub(crate) last_checked_at: Option<String>,
    pub(crate) channels: Vec<SchedulerChannelStatus>,
}

/// 舊版 `SchedulerChannelStatus`；`is_live` 反映舊版 `isLive?: boolean`
/// （`None` 表示狀態未知，顯示 `❔`）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SchedulerChannelStatus {
    pub(crate) channel: String,
    pub(crate) is_live: Option<bool>,
}

/// 舊版 `StreamStatusChange`（同屬 S3.5 未落地，DEC-013）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StreamStatusChange {
    pub(crate) channel: String,
    pub(crate) is_live: bool,
}

/// 舊版 `ChannelSessionRefreshStatus`（`browser/ChannelSession.ts` 定義、經
/// `sessions` 重匯出；DEC-013：S3.4 未落地前先在此定義）。
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct ChannelSessionRefreshStatus {
    pub(crate) channel: String,
    pub(crate) enabled: bool,
    /// 舊版 `nextRefreshAt?: string`。
    pub(crate) next_refresh_at: Option<String>,
    /// 舊版 `secondsUntilRefresh?: number`。
    pub(crate) seconds_until_refresh: Option<f64>,
}

/// 舊版 `SessionRefreshResult.status`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionRefreshStatus {
    Refreshed,
    Unavailable,
    Failed,
}

/// 舊版 `SessionRefreshResult`。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SessionRefreshResult {
    pub(crate) channel: String,
    pub(crate) status: SessionRefreshStatus,
    /// 舊版 `error?: string`。
    pub(crate) error: Option<String>,
}

/// 舊版 `SessionScreenshot`；`image` 以位元組向量對應 Node 的 `Buffer`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SessionScreenshot {
    pub(crate) channel: String,
    pub(crate) image: Vec<u8>,
}

/// 舊版 `SessionChannelPointsResult` 的 `reason` 聯集合
/// （`ChannelPointsReadResult` 的 `'not_found' | 'parse_failed'` 加
/// `ChannelSessionPointsResult` 的 `'page_unavailable'`，另加 SessionManager
/// 的 `'failed'`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionChannelPointsUnavailableReason {
    NotFound,
    ParseFailed,
    PageUnavailable,
    Failed,
}

impl SessionChannelPointsUnavailableReason {
    /// 舊版聯集的字面值（log 用不到，供測試與除錯）。
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::ParseFailed => "parse_failed",
            Self::PageUnavailable => "page_unavailable",
            Self::Failed => "failed",
        }
    }
}

/// 舊版 `SessionChannelPointsResult`。
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum SessionChannelPointsResult {
    Available {
        channel: String,
        /// 舊版 `balance: number`（JS number）。
        balance: f64,
        display_value: String,
    },
    Unavailable {
        channel: String,
        reason: SessionChannelPointsUnavailableReason,
    },
}

/// 對應舊版 `BotCommandContext`。
///
/// `resumeChecks()` 在舊版是同步 `void`；其餘方法回傳 Promise。
/// `channel` 選用參數以 [`Option`] 呈現（舊版 `channel?: string`）。
#[async_trait]
pub(crate) trait BotCommandContext: Send + Sync {
    /// 舊版 `runCheck()`。
    async fn run_check(&self) -> Result<(), BotCommandError>;

    /// 舊版 `pauseChecks()`。
    async fn pause_checks(&self) -> Result<(), BotCommandError>;

    /// 舊版 `resumeChecks()`（非 Promise）。
    fn resume_checks(&self);

    /// 舊版 `getSchedulerSnapshot()`。
    fn get_scheduler_snapshot(&self) -> WatchdogSchedulerSnapshot;

    /// 舊版 `getActiveChannels()`。
    fn get_active_channels(&self) -> Vec<String>;

    /// 舊版 `getRefreshStatuses()`。
    fn get_refresh_statuses(&self) -> Vec<ChannelSessionRefreshStatus>;

    /// 舊版 `refreshPages(channel?)`。
    async fn refresh_pages(
        &self,
        channel: Option<&str>,
    ) -> Result<Vec<SessionRefreshResult>, BotCommandError>;

    /// 舊版 `captureScreenshot(channel?)`。
    async fn capture_screenshot(
        &self,
        channel: Option<&str>,
    ) -> Result<Option<SessionScreenshot>, BotCommandError>;

    /// 舊版 `getChannelPoints(channel?)`。
    async fn get_channel_points(
        &self,
        channel: Option<&str>,
    ) -> Result<Vec<SessionChannelPointsResult>, BotCommandError>;

    /// 舊版 `getConfig()`。
    fn get_config(&self) -> RuntimeWatchConfig;

    /// 舊版 `setChannels(channels)`。
    async fn set_channels(
        &self,
        channels: &[String],
    ) -> Result<RuntimeWatchConfig, ConfigWriteError>;

    /// 舊版 `setMaxConcurrentStreams(value)`；`value` 照舊版是 JS number。
    async fn set_max_concurrent_streams(
        &self,
        value: f64,
    ) -> Result<RuntimeWatchConfig, ConfigWriteError>;
}
