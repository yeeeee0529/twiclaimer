//! 選台器：對應舊版 `src/scheduler/StreamSelector.ts` 逐函式移植（§4.2）。
//!
//! 行為重點（照舊版）：
//! - `maxConcurrentStreams` 必須是 JS 安全正整數（`Number.isSafeInteger` 且 > 0），
//!   否則回傳空清單；因此以 `f64` 模擬 JS number，讓 NaN、±Infinity 與非整數
//!   走同一條拒絕路徑（舊測試的 it.each 案例涵蓋這些值）。
//! - 頻道名稱以小寫比對（JS `String#toLowerCase`）；設定清單先以正規化後的
//!   名稱去重（保留第一次拼法），再檢查直播狀態。
//! - 重複的 live status 採保守合併：同一頻道只要有一筆 offline 就視為 offline。
//! - 回傳值保留設定清單中的原拼法與原順序。
//!
//! 舊版的 `StreamSelector` 介面（WatchdogScheduler 的 DI 用）與 `WatchdogScheduler`
//! 本體屬 S3.5 移植時再接；本模組目前只提供移植的自由函式。
//!
//! 舊版 `ChannelLiveStatus` 型別（原定義於 `src/twitch/LiveStatusProvider.ts`）
//! 先在此最小移植，僅含 S2.5 測試用到的欄位；S3.1 移植 twitch 模組時把完整型別
//! （含 `streamId`、`title`、`startedAt`、`viewerCount` 等選用欄位）移過去，
//! 此處改 import。
#![allow(dead_code)]

use std::collections::{HashMap, HashSet};

/// 舊版 `Number.MAX_SAFE_INTEGER`：安全整數的上界（2^53 − 1）。
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

/// 舊版 `ChannelLiveStatus` 的最小移植（S2.5：只含本模組測試用到的欄位）。
///
/// S3.1 移植 twitch 模組時把完整型別移過去、此處改 import。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChannelLiveStatus {
    /// 頻道名稱（原拼法）。
    pub(crate) channel: String,
    /// 是否直播中（對應舊版 `isLive`）。
    pub(crate) is_live: bool,
    /// 狀態查詢時間（ISO 8601 字串，對應舊版 `checkedAt`）。
    pub(crate) checked_at: String,
}

/// 選台輸入（對應舊版 `StreamSelectionInput`）。
///
/// `configured_channels` 與 `live_statuses` 以借用傳入（對應 readonly 陣列），
/// 本函式不會修改輸入，回傳值為新配置的清單。
pub(crate) struct StreamSelectionInput<'a> {
    /// 設定的頻道清單（原拼法、原順序）。
    pub(crate) configured_channels: &'a [String],
    /// 各頻道的直播狀態（可能缺漏、重複，順序不保證）。
    pub(crate) live_statuses: &'a [ChannelLiveStatus],
    /// 同時開台數上限：照舊版以 JS number 傳入，必須是安全正整數。
    pub(crate) max_concurrent_streams: f64,
}

/// 對應舊版 `selectActiveChannels`：從設定的頻道中選出直播中的頻道。
///
/// 上限不是安全正整數時回傳空清單；直播頻道數超過上限時依設定順序截斷。
pub(crate) fn select_active_channels(input: StreamSelectionInput<'_>) -> Vec<String> {
    if !is_safe_integer(input.max_concurrent_streams) || input.max_concurrent_streams <= 0.0 {
        return Vec::new();
    }

    let live_by_channel = collect_conservative_live_statuses(input.live_statuses);
    let mut selected_channels: Vec<String> = Vec::new();
    let mut configured_channels_seen: HashSet<String> = HashSet::new();

    for channel in input.configured_channels {
        let normalized_channel = normalize_channel(channel);

        if configured_channels_seen.contains(&normalized_channel) {
            continue;
        }
        configured_channels_seen.insert(normalized_channel.clone());

        // 對應舊版 `liveByChannel.get(normalizedChannel) !== true`：缺漏或非 true 都略過。
        if live_by_channel.get(normalized_channel.as_str()).copied() != Some(true) {
            continue;
        }

        selected_channels.push(channel.clone());
        if selected_channels.len() as f64 == input.max_concurrent_streams {
            break;
        }
    }

    selected_channels
}

/// 對應舊版 `collectConservativeLiveStatuses`：以正規化名稱彙整各頻道的直播狀態。
///
/// 同一頻道有多筆狀態時沿用舊版的合併規則：第一筆直接採用，之後每一筆都與
/// 先前結果取 AND（`previousStatus === undefined ? isLive : previous && isLive`）。
fn collect_conservative_live_statuses(
    live_statuses: &[ChannelLiveStatus],
) -> HashMap<String, bool> {
    let mut live_by_channel: HashMap<String, bool> = HashMap::new();

    for status in live_statuses {
        let normalized_channel = normalize_channel(&status.channel);
        live_by_channel
            .entry(normalized_channel)
            .and_modify(|previous_status| *previous_status &= status.is_live)
            .or_insert(status.is_live);
    }

    live_by_channel
}

/// 對應舊版 `normalizeChannel`：以 JS `String#toLowerCase` 轉小寫後比對。
fn normalize_channel(channel: &str) -> String {
    channel.to_lowercase()
}

/// 對應舊版 `Number.isSafeInteger`：值為有限、整數，且絕對值不超過安全整數上界。
fn is_safe_integer(value: f64) -> bool {
    value.is_finite() && value.fract() == 0.0 && value.abs() <= MAX_SAFE_INTEGER
}

#[cfg(test)]
mod tests;
