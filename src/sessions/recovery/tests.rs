//! 頻道復原政策的單元測試：移植舊版 `test/unit/channel-recovery-policy.test.ts`
//! 的 7 個測試。時間以注入的 `Arc<AtomicU64>` 時鐘控制（對應舊測試的
//! `setNow`），不等真實時間。
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use super::{
    BeginRecoveryProbeResult, BrowserRecoveryConfig, BrowserSessionStartConfig,
    ChannelRecoveryAvailability, ChannelRecoveryPolicy, ChannelRecoveryPolicyOptions,
    ChannelRecoveryState, PageCrashRecoveryDecision,
};

/// 舊測試 helper `createDefaultBrowserRecovery`（`test/helpers/test-config.ts`，
/// 數值照抄）。
fn create_default_browser_recovery() -> BrowserRecoveryConfig {
    BrowserRecoveryConfig {
        page_crash_backoff_seconds: vec![30, 60, 120],
        channel_crash_window_seconds: 600,
        channel_quarantine_threshold: 4,
        channel_quarantine_seconds: 900,
        stable_reset_seconds: 1_800,
        multi_channel_crash_window_seconds: 15,
        multi_channel_crash_threshold: 2,
        browser_failure_window_seconds: 600,
        browser_failure_container_threshold: 3,
    }
}

/// 舊測試 helper `createDefaultBrowserSessionStart`（數值照抄）。
fn create_default_browser_session_start() -> BrowserSessionStartConfig {
    BrowserSessionStartConfig {
        failure_backoff_seconds: vec![30, 60, 120],
        maximum_cooldown_seconds: 900,
    }
}

/// 對應舊測試 helper `createHarness`：policy 與可寫入的 `now` 控制點
/// （`setNow` 以 `now_ms.store` 呈現）。
fn create_harness() -> (ChannelRecoveryPolicy, Arc<AtomicU64>) {
    let now_ms = Arc::new(AtomicU64::new(0));
    let clock = Arc::clone(&now_ms);
    let policy = ChannelRecoveryPolicy::new(ChannelRecoveryPolicyOptions {
        recovery: create_default_browser_recovery(),
        session_start: create_default_browser_session_start(),
        now: Some(Box::new(move || clock.load(Ordering::Relaxed))),
    });
    (policy, now_ms)
}

#[test]
// 原測試：前三次單頻道 crash 依序產生 30、60、120 秒 backoff
fn first_three_page_crashes_back_off_30_60_120_seconds() {
    let (mut policy, now_ms) = create_harness();

    assert_eq!(
        policy.record_page_crash("Channel"),
        PageCrashRecoveryDecision::Backoff {
            retry_at_ms: 30_000,
            consecutive_page_crashes: 1,
        }
    );
    now_ms.store(30_000, Ordering::Relaxed);
    assert_eq!(
        policy.record_page_crash("channel"),
        PageCrashRecoveryDecision::Backoff {
            retry_at_ms: 90_000,
            consecutive_page_crashes: 2,
        }
    );
    now_ms.store(90_000, Ordering::Relaxed);
    assert_eq!(
        policy.record_page_crash("CHANNEL"),
        PageCrashRecoveryDecision::Backoff {
            retry_at_ms: 210_000,
            consecutive_page_crashes: 3,
        }
    );
    assert_eq!(
        policy.get_state("channel").page_crash_timestamps_ms,
        vec![0, 30_000, 90_000]
    );
}

#[test]
// 原測試：十分鐘視窗內第四次 crash 進入十五分鐘 quarantine
fn fourth_crash_within_ten_minute_window_quarantines_for_fifteen_minutes() {
    let (mut policy, now_ms) = create_harness();

    for crash_at_ms in [0, 30_000, 90_000] {
        now_ms.store(crash_at_ms, Ordering::Relaxed);
        policy.record_page_crash("channel");
    }
    now_ms.store(210_000, Ordering::Relaxed);

    assert_eq!(
        policy.record_page_crash("channel"),
        PageCrashRecoveryDecision::Quarantined {
            retry_at_ms: 1_110_000,
            consecutive_page_crashes: 4,
        }
    );
    assert_eq!(
        policy.get_availability("channel"),
        ChannelRecoveryAvailability::Quarantined {
            retry_at_ms: 1_110_000,
            recovery_generation: 0,
        }
    );
}

#[test]
// 原測試：quarantine 到期後只允許一個 probe 並以 generation 排除過期結果
fn after_quarantine_only_one_probe_is_allowed_and_stale_generation_is_rejected() {
    let (mut policy, now_ms) = create_harness();
    for crash_at_ms in [0, 30_000, 90_000, 210_000] {
        now_ms.store(crash_at_ms, Ordering::Relaxed);
        policy.record_page_crash("channel");
    }

    now_ms.store(1_110_000, Ordering::Relaxed);
    assert_eq!(
        policy.begin_probe("channel"),
        BeginRecoveryProbeResult::Started {
            recovery_generation: 1,
        }
    );
    assert_eq!(
        policy.begin_probe("CHANNEL"),
        BeginRecoveryProbeResult::NotEligible(ChannelRecoveryAvailability::ProbeInFlight {
            recovery_generation: 1,
        })
    );
    assert!(!policy.mark_stable("channel", 0));
    assert!(policy.mark_stable("channel", 1));
    assert!(policy.is_current_generation("channel", 1));
}

#[test]
// 原測試：持續穩定三十分鐘後才重設 failure state
fn failure_state_resets_only_after_thirty_stable_minutes() {
    let (mut policy, now_ms) = create_harness();
    policy.record_page_crash("channel");
    now_ms.store(30_000, Ordering::Relaxed);
    policy.record_start_failure("channel");
    now_ms.store(900_000, Ordering::Relaxed);
    let probe = policy.begin_probe("channel");
    assert_eq!(
        probe,
        BeginRecoveryProbeResult::Started {
            recovery_generation: 1,
        },
        "Expected recovery probe to start"
    );
    let BeginRecoveryProbeResult::Started {
        recovery_generation,
    } = probe
    else {
        return;
    };
    assert!(policy.mark_stable("channel", recovery_generation));

    now_ms.store(900_000 + 1_800_000 - 1, Ordering::Relaxed);
    let state = policy.get_state("channel");
    assert_eq!(state.consecutive_page_crashes, 1);
    assert_eq!(state.consecutive_start_failures, 1);
    assert_eq!(state.stable_since_ms, Some(900_000));

    now_ms.store(900_000 + 1_800_000, Ordering::Relaxed);
    assert_eq!(
        policy.get_state("channel"),
        ChannelRecoveryState {
            consecutive_page_crashes: 0,
            consecutive_start_failures: 0,
            page_crash_timestamps_ms: vec![],
            next_retry_at_ms: None,
            quarantined_until_ms: None,
            stable_since_ms: None,
            recovery_generation: 1,
        }
    );
}

#[test]
// 原測試：穩定期屆滿後的下一次 crash 會視為第一次
fn crash_after_stable_period_expires_counts_as_the_first_one() {
    let (mut policy, now_ms) = create_harness();
    policy.record_page_crash("channel");
    now_ms.store(30_000, Ordering::Relaxed);
    let probe = policy.begin_probe("channel");
    assert_eq!(
        probe,
        BeginRecoveryProbeResult::Started {
            recovery_generation: 1,
        },
        "Expected recovery probe to start"
    );
    let BeginRecoveryProbeResult::Started {
        recovery_generation,
    } = probe
    else {
        return;
    };
    assert!(policy.mark_stable("channel", recovery_generation));

    now_ms.store(1_830_000, Ordering::Relaxed);
    assert_eq!(
        policy.record_page_crash("channel"),
        PageCrashRecoveryDecision::Backoff {
            retry_at_ms: 1_860_000,
            consecutive_page_crashes: 1,
        }
    );
}

#[test]
// 原測試：start failure 依序使用 30、60、120 秒後以十五分鐘為上限
fn start_failures_use_30_60_120_seconds_then_fifteen_minute_cap() {
    let (mut policy, _now_ms) = create_harness();

    let retry_at_ms: Vec<u64> = (0..5)
        .map(|_| policy.record_start_failure("channel").retry_at_ms)
        .collect();
    assert_eq!(retry_at_ms, vec![30_000, 60_000, 120_000, 900_000, 900_000]);
}

#[test]
// 原測試：狀態 key 不區分大小寫，只有移除設定頻道時完全清除
fn state_key_is_case_insensitive_and_only_remove_channel_clears_it() {
    let (mut policy, _now_ms) = create_harness();
    policy.record_page_crash("MixedCase");

    assert_eq!(policy.get_state("mixedcase").consecutive_page_crashes, 1);
    policy.remove_channel("MIXEDCASE");
    assert_eq!(
        policy.get_state("mixedcase"),
        ChannelRecoveryState {
            consecutive_page_crashes: 0,
            consecutive_start_failures: 0,
            page_crash_timestamps_ms: vec![],
            next_retry_at_ms: None,
            quarantined_until_ms: None,
            stable_since_ms: None,
            recovery_generation: 0,
        }
    );
}
