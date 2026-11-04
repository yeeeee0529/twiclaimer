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
//! （S3.5 前置整併：共用型別已移至 `crate::scheduler::watchdog`、
//! `crate::sessions::manager` 與 `crate::browser::session`，本檔以
//! `pub(crate) use` 保持既有 import 路徑不變。）
#![allow(dead_code)]

use async_trait::async_trait;

use crate::config::writeback::{ConfigWriteError, RuntimeWatchConfig};

/// 對應舊版 Promise reject 的 `Error`：舊版呼叫端不使用 name／message（catch
/// 只記 warn），移植以訊息字串保存最少診斷資訊。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BotCommandError {
    pub(crate) message: String,
}

/// 共用型別的正式家位置（S3.5 前置整併，原 DEC-013 佔位定義已移出）：
/// - `WatchdogSchedulerSnapshot`／`SchedulerChannelStatus`／`StreamStatusChange`
///   → [`crate::scheduler::watchdog`]。
/// - `SessionRefreshStatus`／`SessionRefreshResult`／`SessionScreenshot`／
///   `SessionChannelPointsResult`（含 reason 聯集）→ [`crate::sessions::manager`]。
/// - `ChannelSessionRefreshStatus` → [`crate::browser::session`]（S3.4 落地的
///   正式型別；`seconds_until_refresh` 為 `Option<u64>`）。
// 型別以 `pub(crate) use` 重導出，供 notifications 既有與未來的消費端沿用
// 舊 import 路徑；部分型別目前僅由正式家位置直接使用（clippy 會標未用 import）。
#[allow(unused_imports)]
pub(crate) use crate::browser::session::ChannelSessionRefreshStatus;
#[allow(unused_imports)]
pub(crate) use crate::scheduler::watchdog::{
    SchedulerChannelStatus, StreamStatusChange, WatchdogSchedulerSnapshot,
};
#[allow(unused_imports)]
pub(crate) use crate::sessions::manager::{
    SessionChannelPointsResult, SessionChannelPointsUnavailableReason, SessionRefreshResult,
    SessionRefreshStatus, SessionScreenshot,
};

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
