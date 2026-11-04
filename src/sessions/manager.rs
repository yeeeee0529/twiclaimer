//! `sessions/SessionManager.ts` 的共用查詢結果型別（計畫 §4.2）。
//!
//! S3.5 前置整併（DEC-013 債清）：原本依 DEC-013 在
//! `notifications/bot_command_context.rs` 先行定義的型別移回本檔；
//! `SessionManager`／`DefaultSessionManager` 本體由此步驟補上。
//!
//! 對映附註：`SessionChannelPointsResult` 的 `balance` 照舊版 JS number
//! 形狀留 f64（DEC-021）；正式端由 `ChannelSession::get_channel_points`
//! 的 [`crate::browser::session::ChannelSessionPointsResult`]
//! （`Available { balance: u64 }`）轉接。舊版 `displayValue` 欄位沿
//! DEC-020 的省略契約移除：唯一生產端（ChannelPointsReader）依
//! DEC-020 只回 balance，而舊版 bots 渲染一律用
//! `balance.toLocaleString('en-US')`（不讀 displayValue），全鏈路無消費端。
#![allow(dead_code)]

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
/// （`ChannelSessionPointsResult` 的 `'not_found'` | `'parse_failed'` 加
/// `'page_unavailable'`，另加 SessionManager 的 `'failed'`）。
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

/// 舊版 `SessionChannelPointsResult`（`displayValue` 欄位省略：DEC-020 契約，
/// 見本檔模組文件）。
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum SessionChannelPointsResult {
    Available {
        channel: String,
        /// 舊版 `balance: number`（JS number）。
        balance: f64,
    },
    Unavailable {
        channel: String,
        reason: SessionChannelPointsUnavailableReason,
    },
}
