//! `telegram` 模組：對應舊版 `src/telegram`（§4.2，S4.2；§7.8）。
//!
//! - [`api`]：`TelegramApiClient.ts` 的移植（reqwest：HTTP/1.1、不讀系統
//!   proxy、任何 redirect 都失敗；`getUpdates` 的長輪詢逾時含 10 秒的
//!   `SEND_TIMEOUT_MS`，涵蓋 request 與 body 讀取；逾時／中止的分類照舊版
//!   的 abort reason：外部訊號 → `aborted`、deadline → `timeout`）。
//! - [`bot`]：`TelegramBot.ts` 的移植（長輪詢 poll loop、指令、授權、通知；
//!   含舊版 `306b4ba`「Telegram 查詢停頓時保持回應」的行為：名額
//!   `MAX_ACTIVE_QUERIES`、30 秒查詢逾時計時器、事件 `telegram_query_timeout`
//!   與 `telegram_query_failed` 的語意照舊版）。
//!
//! 舊版 `index.ts` 的重匯出（`TelegramApiClient`、`TelegramApiError`、
//! `TelegramApi`、`DefaultTelegramBot` 等）在 Rust 端以路徑引用
//! （`crate::telegram::api::…`／`crate::telegram::bot::…`）。
pub(crate) mod api;
pub(crate) mod bot;
