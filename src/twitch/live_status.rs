//! 直播狀態的型別：對應舊版 `twitch/LiveStatusProvider.ts`。
//!
//! - [`ChannelLiveStatus`]：選擇性欄位（`streamId`、`title`、`startedAt`、
//!   `viewerCount`）以 [`Option`] 對應舊版「屬性不存在」的省略。
//! - [`LiveStatusProvider`]：D12 的介面 → trait；舊版方法用拋出例外回報錯誤，
//!   Rust 版以 [`Result`] 的 [`TwitchApiError`] 對應。
#![allow(dead_code)]

use async_trait::async_trait;

use super::errors::TwitchApiError;

/// 對應舊版 `ChannelLiveStatus`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ChannelLiveStatus {
    /// 對應 `channel`（照設定值原樣，不做大小寫正規化）。
    pub(crate) channel: String,
    /// 對應 `isLive`。
    pub(crate) is_live: bool,
    /// 對應 `streamId`（live 才有）。
    pub(crate) stream_id: Option<String>,
    /// 對應 `title`（live 才有）。
    pub(crate) title: Option<String>,
    /// 對應 `startedAt`（live 才有；照 Helix 回傳的字串）。
    pub(crate) started_at: Option<String>,
    /// 對應 `viewerCount`（live 才有）。
    pub(crate) viewer_count: Option<u64>,
    /// 對應 `checkedAt`（ISO 8601 的 UTC 字串）。
    pub(crate) checked_at: String,
}

/// 對應舊版 `LiveStatusProvider` 介面（S3.x 的注入點）。
#[async_trait]
pub(crate) trait LiveStatusProvider: Send + Sync {
    /// 對應 `getLiveStatuses(channels)`。
    async fn get_live_statuses(
        &self,
        channels: &[String],
    ) -> Result<Vec<ChannelLiveStatus>, TwitchApiError>;
}
