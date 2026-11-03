//! 通知文字組裝：`notifications/BotNotifications.ts` 逐函式移植（§4.2，S4.1）。
//!
//! 三個函式的輸出文字（含全形標點與 emoji）逐字照舊版，Discord／Telegram 的
//! 通知以這些字串送出（R6 驗證回覆與舊版相同）。
#![allow(dead_code)]

/// 對應舊版 `BrowserRestartNotification`（單一 `mode` 欄位；Rust 以 enum 取代
/// 字面值 union `'automatic' | 'manual'`）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BrowserRestartNotification {
    pub(crate) mode: BrowserRestartMode,
}

/// 對應舊版 `mode: 'automatic' | 'manual'`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BrowserRestartMode {
    Automatic,
    Manual,
}

/// 對應舊版 `ContainerRestartNotification`。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ContainerRestartNotification {
    pub(crate) reason: String,
    pub(crate) source: String,
    /// 舊版 `channel?: string`。
    pub(crate) channel: Option<String>,
    /// 舊版 `detailReason?: string`。
    pub(crate) detail_reason: Option<String>,
}

/// 對應舊版 `formatContainerRestartMessage`。
///
/// `detailReason` 存在且與 `reason` 不同時，reason 顯示為
/// `<reason>（<detailReason>）`；`channel` 存在且長度大於 0 時才加頻道行。
pub(crate) fn format_container_restart_message(event: &ContainerRestartNotification) -> String {
    let reason = match event.detail_reason.as_deref() {
        Some(detail) if detail != event.reason => format!("{}（{}）", event.reason, detail),
        _ => event.reason.clone(),
    };
    let mut lines = vec![
        "🚨 容器即將重啟".to_string(),
        format!("原因：{reason}"),
        format!("來源：{}", event.source),
    ];
    if let Some(channel) = event
        .channel
        .as_deref()
        .filter(|channel| !channel.is_empty())
    {
        lines.push(format!("頻道：{channel}"));
    }
    lines.join("\n")
}

/// 對應舊版 `formatBrowserRestartMessage`；modeLabel 為 `自動恢復`（automatic）
/// 或 `完整回收`（manual）。
pub(crate) fn format_browser_restart_message(event: &BrowserRestartNotification) -> String {
    let mode_label = if event.mode == BrowserRestartMode::Automatic {
        "自動恢復"
    } else {
        "完整回收"
    };
    format!("♻️ Firefox 瀏覽器已重啟（{mode_label}）")
}

/// 對應舊版 `formatPageCrashMessage`。
pub(crate) fn format_page_crash_message(channel: &str) -> String {
    format!("💥 {channel} 觀看頁面崩潰")
}
