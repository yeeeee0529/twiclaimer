//! `twitch` 模組：對應舊版 `src/twitch`（§4.2，S3.1）。
//!
//! - [`api`]：`TwitchApiClient` 的移植（Helix 直播查詢、OAuth token 驗證與換發、
//!   HTTP transport）。
//! - [`live_status`]：`ChannelLiveStatus` 型別與 `LiveStatusProvider` 介面。
//! - [`errors`]：三種 `TwitchApiError` 類別與建構子驗證錯誤。
//!
//! HTTP 以 [`TwitchHttpTransport`](api::TwitchHttpTransport) trait 注入
//! （對應舊版 options 的 `fetch`）；選 trait 注入而非本機 stub server 的理由、
//! 以及預設 reqwest transport 的行為設定，見 [`api`] 的模組文件。
pub(crate) mod api;
pub(crate) mod errors;
pub(crate) mod live_status;
