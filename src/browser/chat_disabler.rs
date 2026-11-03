//! `ChatDisabler.ts` 的薄轉接：常數與 init script 產生器。
//!
//! 腳本內容已在 cdp context 層以 `include_str!` 與 DEC-004 的註解剝除規則載入，
//! 這裡只保留舊版模組的對外形狀，避免重複維護同一份 JS。
#![allow(dead_code)]

use super::cdp::context::chat_block_script;

/// 舊版 `CHAT_WEBSOCKET_HOSTNAME`。
pub(crate) const CHAT_WEBSOCKET_HOSTNAME: &str = "irc-ws.chat.twitch.tv";

/// 舊版 `CHAT_WEBSOCKET_BLOCK_SCRIPT`。
pub(crate) fn chat_websocket_block_script() -> &'static str {
    chat_block_script()
}

/// 舊版 `buildChatBlockInitScript()`。
pub(crate) fn build_chat_block_init_script() -> String {
    chat_block_script().to_string()
}
