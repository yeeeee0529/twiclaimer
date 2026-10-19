//! Resource guard 政策的單元測試：移植舊版 `test/unit/resource-guard-policy.test.ts`
//! 的 12 個測試。時間以注入的 `Arc<AtomicU64>` 時鐘（或固定 `() => 0`）控制，
//! 不等真實時間。
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use super::{
    CgroupMemoryEvents, CgroupSnapshot, GuardClock, ResourceGuardDecision,
    ResourceGuardEffectiveThresholds, ResourceGuardPolicy, ResourceGuardPolicyConfig,
    ResourceGuardPolicyOptions, mib_to_bytes,
};

/// 對應舊測試 helper `snapshot(input)`：`memoryMib` 轉位元組；事件計數未指定的
/// 位元以 0 起始（照舊測試的 `?? 0n`）。
fn snapshot(at_ms: u64, memory_mib: u64) -> CgroupSnapshot {
    snapshot_with(at_ms, memory_mib, None, CgroupMemoryEvents::default())
}

/// 對應舊測試 helper `snapshot(input)` 的完整版（swap 與事件計數照參數）。
fn snapshot_with(
    at_ms: u64,
    memory_mib: u64,
    swap_mib: Option<u64>,
    events: CgroupMemoryEvents,
) -> CgroupSnapshot {
    CgroupSnapshot {
        sampled_at_monotonic_ms: at_ms,
        memory_current_bytes: mib_to_bytes(memory_mib as f64),
        swap_current_bytes: swap_mib.map(|swap_mib| mib_to_bytes(swap_mib as f64)),
        events,
    }
}

/// 對應舊測試 helper `createPolicy` 的 config（數值照抄）。
fn create_test_config() -> ResourceGuardPolicyConfig {
    ResourceGuardPolicyConfig {
        sample_interval_seconds: 2,
        startup_rate_grace_seconds: 120,
        browser_recycle_consecutive_samples: 2,
        emergency_swap_mib: 768.0,
        fast_growth_mib: 1_024.0,
        fast_growth_window_seconds: 30,
        post_browser_restart_rate_grace_seconds: 120,
        post_recycle_observation_seconds: 20,
        post_recycle_minimum_drop_mib: 512.0,
        effective: ResourceGuardEffectiveThresholds {
            max_concurrent_streams: 3,
            warning_memory_mib: 4_096.0,
            warning_reset_memory_mib: 3_840.0,
            browser_recycle_memory_mib: 4_608.0,
            emergency_memory_mib: 5_376.0,
            post_recycle_target_memory_mib: 4_096.0,
        },
    }
}

/// 對應舊測試 helper `createPolicy(now = () => 0)`。
fn create_policy() -> ResourceGuardPolicy {
    create_policy_with_now(Box::new(|| 0))
}

/// 對應舊測試 helper `createPolicy(now)`：注入可寫入的時鐘控制點。
fn create_policy_with_now(now: GuardClock) -> ResourceGuardPolicy {
    ResourceGuardPolicy::new(ResourceGuardPolicyOptions {
        now: Some(now),
        config: create_test_config(),
    })
}

/// 對應舊測試 `createPolicy(() => now)` 的可寫入時鐘：回傳 (policy, 控制點)。
fn create_policy_with_shared_clock() -> (ResourceGuardPolicy, Arc<AtomicU64>) {
    let now_ms = Arc::new(AtomicU64::new(0));
    let clock = Arc::clone(&now_ms);
    (
        create_policy_with_now(Box::new(move || clock.load(Ordering::Relaxed))),
        now_ms,
    )
}

#[test]
// 原測試：低於門檻時不動作
fn does_nothing_below_thresholds() {
    let mut policy = create_policy();
    assert_eq!(
        policy.evaluate(&snapshot(0, 1_000)),
        ResourceGuardDecision::None
    );
}

#[test]
// 原測試：warning 只觸發一次，降到 reset 以下才解除
fn warning_fires_once_and_clears_only_below_reset() {
    let mut policy = create_policy();
    assert!(!policy.is_warning_latched());
    assert_eq!(
        policy.evaluate(&snapshot(0, 4_100)),
        ResourceGuardDecision::Warn {
            reason: "memory_warning",
        }
    );
    assert!(policy.is_warning_latched());
    assert_eq!(
        policy.evaluate(&snapshot(2_000, 4_100)),
        ResourceGuardDecision::None
    );
    assert_eq!(
        policy.evaluate(&snapshot(4_000, 3_900)),
        ResourceGuardDecision::None
    );
    assert!(policy.is_warning_latched());
    // Still above reset (3840).（照舊版註解）
    assert_eq!(
        policy.evaluate(&snapshot(6_000, 4_100)),
        ResourceGuardDecision::None
    );
    assert_eq!(
        policy.evaluate(&snapshot(8_000, 3_000)),
        ResourceGuardDecision::None
    );
    assert!(!policy.is_warning_latched());
    assert_eq!(
        policy.evaluate(&snapshot(10_000, 4_100)),
        ResourceGuardDecision::Warn {
            reason: "memory_warning",
        }
    );
}

#[test]
// 原測試：需連續兩個高水位 sample 才 recycle
fn recycle_requires_two_consecutive_high_watermark_samples() {
    let mut policy = create_policy();
    // First high sample also crosses warning; recycle still needs two samples.（照舊版註解）
    assert_eq!(
        policy.evaluate(&snapshot(0, 4_700)),
        ResourceGuardDecision::Warn {
            reason: "memory_warning",
        }
    );
    assert_eq!(
        policy.evaluate(&snapshot(2_000, 4_700)),
        ResourceGuardDecision::RecycleBrowser {
            reason: "sustained_high_memory",
        }
    );
}

#[test]
// 原測試：emergency memory 優先於 recycle
fn emergency_memory_takes_priority_over_recycle() {
    let mut policy = create_policy();
    assert_eq!(
        policy.evaluate(&snapshot(0, 5_500)),
        ResourceGuardDecision::RestartContainer {
            reason: "emergency_memory",
        }
    );
}

#[test]
// 原測試：swap emergency 立即重啟容器
fn swap_emergency_restarts_container_immediately() {
    let mut policy = create_policy();
    assert_eq!(
        policy.evaluate(&snapshot_with(
            0,
            1_000,
            Some(800),
            CgroupMemoryEvents::default()
        )),
        ResourceGuardDecision::RestartContainer {
            reason: "emergency_swap",
        }
    );
}

#[test]
// 原測試：oom / max event 上升觸發 container restart
fn rising_oom_or_max_events_trigger_container_restart() {
    let mut policy = create_policy();
    assert_eq!(
        policy.evaluate(&snapshot_with(
            0,
            1_000,
            None,
            CgroupMemoryEvents {
                oom: 0,
                max: 0,
                ..Default::default()
            }
        )),
        ResourceGuardDecision::None
    );
    assert_eq!(
        policy.evaluate(&snapshot_with(
            2_000,
            1_000,
            None,
            CgroupMemoryEvents {
                oom: 1,
                ..Default::default()
            }
        )),
        ResourceGuardDecision::RestartContainer {
            reason: "cgroup_oom",
        }
    );

    let mut policy_max = create_policy();
    policy_max.evaluate(&snapshot_with(
        0,
        1_000,
        None,
        CgroupMemoryEvents {
            max: 0,
            ..Default::default()
        },
    ));
    assert_eq!(
        policy_max.evaluate(&snapshot_with(
            2_000,
            1_000,
            None,
            CgroupMemoryEvents {
                max: 1,
                ..Default::default()
            }
        )),
        ResourceGuardDecision::RestartContainer {
            reason: "cgroup_memory_max",
        }
    );
}

#[test]
// 原測試：emergency-only 模式略過 recycle/warning，但仍偵測 OOM
fn emergency_only_mode_skips_recycle_and_warning_but_still_detects_oom() {
    let mut policy = create_policy();
    policy.evaluate(&snapshot_with(
        0,
        4_700,
        None,
        CgroupMemoryEvents {
            oom: 0,
            ..Default::default()
        },
    ));

    assert_eq!(
        policy.evaluate_emergency_only(&snapshot_with(
            2_000,
            4_700,
            None,
            CgroupMemoryEvents {
                oom: 0,
                ..Default::default()
            }
        )),
        ResourceGuardDecision::None
    );
    assert_eq!(
        policy.evaluate_emergency_only(&snapshot_with(
            4_000,
            4_700,
            None,
            CgroupMemoryEvents {
                oom: 1,
                ..Default::default()
            }
        )),
        ResourceGuardDecision::RestartContainer {
            reason: "cgroup_oom",
        }
    );
}

#[test]
// 原測試：啟動 grace 期間忽略 fast growth，之後在 warning 以上才觸發
fn fast_growth_is_ignored_during_startup_grace_then_fires_above_warning() {
    let (mut policy, now_ms) = create_policy_with_shared_clock();

    policy.evaluate(&snapshot(0, 1_000));
    now_ms.store(8_000, Ordering::Relaxed);
    assert_eq!(
        policy.evaluate(&snapshot(8_000, 2_000)),
        ResourceGuardDecision::None
    );

    // After startup grace: growth is large but absolute level stays below warning.
    now_ms.store(120_000, Ordering::Relaxed);
    policy.evaluate(&snapshot(120_000, 1_000));
    now_ms.store(150_000, Ordering::Relaxed);
    assert_eq!(
        policy.evaluate(&snapshot(150_000, 2_100)),
        ResourceGuardDecision::None
    );

    // Same rate of growth, but only fatal when already at/above warning.（照舊版註解）
    now_ms.store(160_000, Ordering::Relaxed);
    policy.evaluate(&snapshot(160_000, 4_100));
    now_ms.store(190_000, Ordering::Relaxed);
    assert_eq!(
        policy.evaluate(&snapshot(190_000, 5_200)),
        ResourceGuardDecision::RestartContainer {
            reason: "fast_memory_growth",
        }
    );
}

#[test]
// 原測試：browser restart 後清除 sample history 並套用 rate grace
fn browser_restart_clears_sample_history_and_applies_rate_grace() {
    let (mut policy, now_ms) = create_policy_with_shared_clock();

    now_ms.store(120_000, Ordering::Relaxed);
    policy.evaluate(&snapshot(120_000, 4_100));
    policy.note_browser_restart();

    // During post-restart grace, large refill toward warning is ignored.
    now_ms.store(150_000, Ordering::Relaxed);
    assert_eq!(
        policy.evaluate(&snapshot(150_000, 5_200)),
        ResourceGuardDecision::None
    );
    assert!(policy.is_rate_growth_suppressed());

    // After grace expires, growth above warning still restarts.
    now_ms.store(240_000, Ordering::Relaxed);
    assert!(!policy.is_rate_growth_suppressed());
    policy.evaluate(&snapshot(240_000, 4_100));
    now_ms.store(270_000, Ordering::Relaxed);
    assert_eq!(
        policy.evaluate(&snapshot(270_000, 5_200)),
        ResourceGuardDecision::RestartContainer {
            reason: "fast_memory_growth",
        }
    );
}

#[test]
// 原測試：post-recycle observation 期間也不會因 refill 觸發 fast_memory_growth
fn fast_memory_growth_is_not_triggered_by_refill_during_post_recycle_observation() {
    let (mut policy, now_ms) = create_policy_with_shared_clock();

    now_ms.store(120_000, Ordering::Relaxed);
    policy.begin_recycle_observation(mib_to_bytes(5_000.0));
    assert!(policy.is_rate_growth_suppressed());

    now_ms.store(140_000, Ordering::Relaxed);
    // Memory drops after recycle then climbs during session refill.
    assert_eq!(
        policy.evaluate(&snapshot(140_000, 400)),
        ResourceGuardDecision::None
    );
    now_ms.store(160_000, Ordering::Relaxed);
    assert_eq!(
        policy.evaluate(&snapshot(160_000, 1_700)),
        ResourceGuardDecision::None
    );
}

#[test]
// 原測試：post-recycle 成功後清除觀察；逾時則 container restart
fn post_recycle_observation_clears_on_success_and_restarts_container_on_timeout() {
    let mut policy = create_policy();
    policy.begin_recycle_observation(mib_to_bytes(5_000.0));

    assert_eq!(
        policy.evaluate(&snapshot(1_000, 3_500)),
        ResourceGuardDecision::None
    );
    assert!(!policy.is_recycle_observation_active());

    let mut policy_timeout = create_policy();
    policy_timeout.begin_recycle_observation(mib_to_bytes(5_000.0));
    assert_eq!(
        policy_timeout.evaluate(&snapshot(0, 4_800)),
        ResourceGuardDecision::None
    );
    assert_eq!(
        policy_timeout.evaluate(&snapshot(21_000, 4_800)),
        ResourceGuardDecision::RestartContainer {
            reason: "post_recycle_reclamation_failed",
        }
    );
}

#[test]
// 原測試：memory.events.high 上升可被 takeMemoryHighEvent 讀取且非 fatal
fn rising_memory_events_high_is_readable_via_take_memory_high_event_and_not_fatal() {
    let mut policy = create_policy();
    policy.evaluate(&snapshot_with(
        0,
        1_000,
        None,
        CgroupMemoryEvents {
            high: 0,
            ..Default::default()
        },
    ));
    let decision = policy.evaluate(&snapshot_with(
        2_000,
        1_000,
        None,
        CgroupMemoryEvents {
            high: 1,
            ..Default::default()
        },
    ));
    assert_eq!(decision, ResourceGuardDecision::None);
    assert!(policy.take_memory_high_event());
    assert!(!policy.take_memory_high_event());
}
