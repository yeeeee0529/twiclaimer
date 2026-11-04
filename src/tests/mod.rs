//! 跨模組整合測試與需要 Chromium 的 e2e（計畫 §4.1）。
//!
//! 這裡放會 spawn 真實程序（假瀏覽器腳本或 headless shell）的測試；
//! 純函式與本機可驗證的行為在各模組的 `tests.rs`。
mod browser_diagnostics;
mod browser_invalidation;
mod browser_launch;
mod channel_points_reader;
mod channel_session;
mod chat_blocking;
mod locator_core;
mod multi_page;
mod playback_optimizer;
mod pointer_action;
mod reward_claimer;
mod selectors_parse;
mod startup_prerequisites;
mod watchdog_flow;
