//! `twiclaimer`：Twitch Watchdog 的 Rust 版（S1.1 骨架，模組依 §4.2 逐步加入）。
pub mod app;
pub mod browser;
pub mod config;
pub mod credentials;
pub mod discord;
pub mod logging;
pub mod notifications;
pub mod scheduler;
pub mod sessions;
pub mod telegram;
pub(crate) mod twitch;

/// `probe` 子命令的進入點（bin 的公開面；實作在 `browser::cdp::probe`，§7.3.7。
/// DEC-014 的收尾：bin 不再以 `#[path]` 掛載模組樹，改呼叫 lib 的公開 API）。
pub async fn run_probe(args: &[String]) -> Result<(), String> {
    browser::cdp::probe::run(args).await
}

/// 跨模組整合測試與 e2e（計畫 §4.1）。
#[cfg(test)]
mod tests;
