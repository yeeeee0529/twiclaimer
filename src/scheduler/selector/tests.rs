//! 選台器的單元測試：移植舊版 `test/unit/stream-selector.test.ts`（§2.1）。
use super::{ChannelLiveStatus, StreamSelectionInput, select_active_channels};

/// 對應舊測試的 `CHECKED_AT` 常數：所有狀態共用的查詢時間。
const CHECKED_AT: &str = "2026-06-14T12:00:00.000Z";

/// 測試輔助（對應舊測試的 `status()`）：組一筆 `ChannelLiveStatus`。
fn status(channel: &str, is_live: bool) -> ChannelLiveStatus {
    ChannelLiveStatus {
        channel: channel.to_string(),
        is_live,
        checked_at: CHECKED_AT.to_string(),
    }
}

/// 測試輔助：把字面值清單轉成設定的頻道清單（保留原拼法與順序）。
fn channels(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_string()).collect()
}

/// 測試輔助：組選台輸入（對應舊測試直接以物件字面值傳入）。
fn input<'a>(
    configured_channels: &'a [String],
    live_statuses: &'a [ChannelLiveStatus],
    max_concurrent_streams: f64,
) -> StreamSelectionInput<'a> {
    StreamSelectionInput {
        configured_channels,
        live_statuses,
        max_concurrent_streams,
    }
}

// 原測試：無人開台時回傳空陣列
#[test]
fn returns_empty_array_when_no_channel_is_live() {
    let configured = channels(&["first", "second"]);
    let live_statuses = vec![status("first", false), status("second", false)];

    let selected = select_active_channels(input(&configured, &live_statuses, 2.0));

    assert!(selected.is_empty());
}

// 原測試：live 數量少於上限時回傳所有 live 頻道
#[test]
fn returns_all_live_channels_when_live_count_is_below_limit() {
    let configured = channels(&["first", "second", "third"]);
    let live_statuses = vec![
        status("first", true),
        status("second", false),
        status("third", true),
    ];

    let selected = select_active_channels(input(&configured, &live_statuses, 3.0));

    assert_eq!(selected, vec!["first", "third"]);
}

// 原測試：live 數量等於上限時回傳所有 live 頻道
#[test]
fn returns_all_live_channels_when_live_count_equals_limit() {
    let configured = channels(&["first", "second", "third"]);
    let live_statuses = vec![
        status("first", true),
        status("second", false),
        status("third", true),
    ];

    let selected = select_active_channels(input(&configured, &live_statuses, 2.0));

    assert_eq!(selected, vec!["first", "third"]);
}

// 原測試：live 數量超過上限時依設定順序截斷
#[test]
fn truncates_in_configured_order_when_live_count_exceeds_limit() {
    let configured = channels(&["first", "second", "third"]);
    let live_statuses = vec![
        status("first", true),
        status("second", true),
        status("third", true),
    ];

    let selected = select_active_channels(input(&configured, &live_statuses, 2.0));

    assert_eq!(selected, vec!["first", "second"]);
}

// 原測試：API 回傳順序不同時仍依設定順序選台
#[test]
fn selects_in_configured_order_regardless_of_api_order() {
    let configured = channels(&["first", "second", "third"]);
    let live_statuses = vec![
        status("third", true),
        status("first", true),
        status("second", true),
    ];

    let selected = select_active_channels(input(&configured, &live_statuses, 2.0));

    assert_eq!(selected, vec!["first", "second"]);
}

// 原測試：忽略未設定頻道的 live status
#[test]
fn ignores_live_status_of_unconfigured_channels() {
    let configured = channels(&["first", "second"]);
    let live_statuses = vec![status("unknown", true), status("second", true)];

    let selected = select_active_channels(input(&configured, &live_statuses, 2.0));

    assert_eq!(selected, vec!["second"]);
}

// 原測試：缺少 status 的設定頻道視為 offline
#[test]
fn treats_configured_channel_without_status_as_offline() {
    let configured = channels(&["missing", "live"]);
    let live_statuses = vec![status("live", true)];

    let selected = select_active_channels(input(&configured, &live_statuses, 2.0));

    assert_eq!(selected, vec!["live"]);
}

// 原測試：重複設定頻道以大小寫不敏感方式去重並保留第一次拼法
#[test]
fn dedupes_configured_channels_case_insensitively_keeping_first_spelling() {
    let configured = channels(&["First", "first", "second", "second"]);
    let live_statuses = vec![status("FIRST", true), status("second", true)];

    let selected = select_active_channels(input(&configured, &live_statuses, 3.0));

    assert_eq!(selected, vec!["First", "second"]);
}

// 原測試：重複且大小寫不同的 status 一致為 live 時可選取一次
#[test]
fn selects_once_when_case_variants_of_status_agree_on_live() {
    let configured = channels(&["First"]);
    let live_statuses = vec![status("first", true), status("FIRST", true)];

    let selected = select_active_channels(input(&configured, &live_statuses, 1.0));

    assert_eq!(selected, vec!["First"]);
}

// 原測試：重複 status 衝突時採 offline 優先的保守行為
#[test]
fn prefers_offline_when_duplicate_statuses_conflict() {
    let configured = channels(&["first", "second"]);
    let live_statuses = vec![
        status("FIRST", true),
        status("first", false),
        status("second", true),
    ];

    let selected = select_active_channels(input(&configured, &live_statuses, 2.0));

    assert_eq!(selected, vec!["second"]);
}

// 舊測試的 it.each 案例表照抄：[0, -1, 1.5, Number.NaN, Number.POSITIVE_INFINITY]。

// 原測試：maxConcurrentStreams=%s 不是正整數時回傳空陣列
#[test]
fn max_concurrent_streams_0_returns_empty_array() {
    let configured = channels(&["first"]);
    let live_statuses = vec![status("first", true)];

    let selected = select_active_channels(input(&configured, &live_statuses, 0.0));

    assert!(selected.is_empty());
}

// 原測試：maxConcurrentStreams=%s 不是正整數時回傳空陣列
#[test]
fn max_concurrent_streams_negative_1_returns_empty_array() {
    let configured = channels(&["first"]);
    let live_statuses = vec![status("first", true)];

    let selected = select_active_channels(input(&configured, &live_statuses, -1.0));

    assert!(selected.is_empty());
}

// 原測試：maxConcurrentStreams=%s 不是正整數時回傳空陣列
#[test]
fn max_concurrent_streams_1_5_returns_empty_array() {
    let configured = channels(&["first"]);
    let live_statuses = vec![status("first", true)];

    let selected = select_active_channels(input(&configured, &live_statuses, 1.5));

    assert!(selected.is_empty());
}

// 原測試：maxConcurrentStreams=%s 不是正整數時回傳空陣列
#[test]
fn max_concurrent_streams_nan_returns_empty_array() {
    let configured = channels(&["first"]);
    let live_statuses = vec![status("first", true)];

    let selected = select_active_channels(input(&configured, &live_statuses, f64::NAN));

    assert!(selected.is_empty());
}

// 原測試：maxConcurrentStreams=%s 不是正整數時回傳空陣列
#[test]
fn max_concurrent_streams_positive_infinity_returns_empty_array() {
    let configured = channels(&["first"]);
    let live_statuses = vec![status("first", true)];

    let selected = select_active_channels(input(&configured, &live_statuses, f64::INFINITY));

    assert!(selected.is_empty());
}

// 原測試：不改變 configuredChannels 或 liveStatuses 輸入
#[test]
fn does_not_mutate_configured_channels_or_live_statuses() {
    let configured = channels(&["second", "first"]);
    let live_statuses = vec![status("first", true), status("second", true)];
    let configured_snapshot = configured.clone();
    let statuses_snapshot = live_statuses.clone();

    let mut selected = select_active_channels(input(&configured, &live_statuses, 1.0));
    // 舊版對凍結輸入取快照後比對；Rust 端輸入為不可變借用、回傳值是獨立擁有的
    // 清單，此處對結果 push 以驗證同樣性質：修改結果不會影響輸入。
    selected.push("caller-owned-change".to_string());

    assert_eq!(configured, configured_snapshot);
    assert_eq!(live_statuses, statuses_snapshot);
}
