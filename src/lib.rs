//! twiclaimer：Twitch Watchdog 的 Rust 版（S1.1 骨架，模組依 §4.2 逐步加入）。
pub mod browser;
pub mod logging;

/// 跨模組整合測試與 e2e（計畫 §4.1）。
#[cfg(test)]
mod tests;
