//! `scheduler/WatchdogScheduler.ts` 的共用快照型別（計畫 §4.2）。
//!
//! S3.5 前置整併（DEC-013 債清）：原本依 DEC-013 在
//! `notifications/bot_command_context.rs` 先行定義的快照型別移回本檔；
//! `DefaultWatchdogScheduler` 本體由此步驟補上。
#![allow(dead_code)]

/// 舊版 `WatchdogSchedulerSnapshot`。
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

/// 舊版 `StreamStatusChange`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StreamStatusChange {
    pub(crate) channel: String,
    pub(crate) is_live: bool,
}
