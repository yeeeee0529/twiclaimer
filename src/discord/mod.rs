//! `discord` 模組：對應舊版 `src/discord`（§4.2，S4.3；§7.8）。
//!
//! - [`api`]：`DiscordApiClient` 的移植（REST 用 reqwest；路徑、headers、
//!   屬於 ping 的逾時照舊版，`redirect: 'error'`、HTTP/1.1、不讀 proxy）。
//! - [`bot`]：`DiscordBot.ts` 的移植（指令、授權、通知；gateway 以 trait 包住
//!   twilight Shard，X4 斷線恢復由 twilight 處理）。
//!
//! 路徑與舊版 `index.ts` 對應的匯出（`DiscordApiClient`、`DiscordApiError`、
//! `DiscordApi`、`DiscordBot` 等）皆在 `index.ts` 的重匯出位置；Rust 版以
//! 路徑引用（`crate::discord::api::…`／`crate::discord::bot::…`）。
pub(crate) mod api;
pub(crate) mod bot;
