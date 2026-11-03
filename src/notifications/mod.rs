//! `notifications` 模組：對應舊版 `src/notifications`（§4.2，S4.1）。
//!
//! - [`bot_notifications`]：`BotNotifications.ts` 的通知文字組裝函式
//!   （容器重啟、瀏覽器重啟、頁面崩潰；清楚地雷——全形標點照舊）。
//! - [`bot_command_context`]：`BotCommandContext.ts` 的共用指令介面，
//!   Telegram（S4.2）與 Discord（S4.3）Bot 都以此存取 scheduler／sessions／config。
//!
//! 本模組只有通知介面，不含 Bot 本體；介面形狀照舊版保持，唯一取捨是 §4.2
//! 註明的「Bot 本體在 S4.2 之後才接」：[`BotCommandContext`] 的 scheduler／
//! sessions 型別依 DEC-013 在本模組先行定義同形狀型別，S3.4／S3.5 落地後
//! 改重用正式型別（見各型別的註解）。
pub(crate) mod bot_command_context;
pub(crate) mod bot_notifications;
