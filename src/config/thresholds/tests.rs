//! 門檻計算的測試：移植舊版 `test/unit/resource-guard-thresholds.test.ts` 的 5 個案例。
use super::{
    MAX_SAFE_INTEGER, ResourceGuardAnchorThresholds, clamp_effective_thresholds_to_memory_max,
    compute_effective_resource_guard_thresholds, mib_to_bytes, serializable_byte_count,
};
use crate::config::golden::json_eq;
use crate::logging::redact::SanitizedValue;

/// 舊測試的 `anchors` 常數（N=3 的預設錨點）。
fn anchors() -> ResourceGuardAnchorThresholds {
    ResourceGuardAnchorThresholds {
        scale_with_streams: true,
        baseline_streams: 3.0,
        base_memory_mib: 512.0,
        warning_memory_mib: 4096.0,
        warning_reset_memory_mib: 3840.0,
        browser_recycle_memory_mib: 4608.0,
        emergency_memory_mib: 5376.0,
        post_recycle_target_memory_mib: 4096.0,
    }
}

// 原測試：N=3 時維持錨點
// 舊版以 toEqual 比對整個物件；這裡以 to_json 加 json_eq（數字轉 f64）比對。
#[test]
fn compute_effective_thresholds_keep_anchors_at_n3() {
    let effective = compute_effective_resource_guard_thresholds(&anchors(), 3.0);
    assert!(
        json_eq(
            &effective.to_json(),
            &serde_json::json!({
                "maxConcurrentStreams": 3,
                "warningMemoryMib": 4096,
                "warningResetMemoryMib": 3840,
                "browserRecycleMemoryMib": 4608,
                "emergencyMemoryMib": 5376,
                "postRecycleTargetMemoryMib": 4096,
            })
        ),
        "effective 不符：{:?}",
        effective
    );
}

// 原測試：N=5 時依公式放大
#[test]
fn compute_effective_thresholds_scale_at_n5() {
    let effective = compute_effective_resource_guard_thresholds(&anchors(), 5.0);
    assert!(
        json_eq(
            &effective.to_json(),
            &serde_json::json!({
                "maxConcurrentStreams": 5,
                "warningMemoryMib": 6485,
                "warningResetMemoryMib": 6059,
                "browserRecycleMemoryMib": 7339,
                "emergencyMemoryMib": 8619,
                "postRecycleTargetMemoryMib": 6485,
            })
        ),
        "effective 不符：{:?}",
        effective
    );
}

// 原測試：scale_with_streams 關閉時使用絕對值
#[test]
fn compute_effective_thresholds_use_absolute_values_when_scaling_disabled() {
    let mut disabled = anchors();
    disabled.scale_with_streams = false;
    let effective = compute_effective_resource_guard_thresholds(&disabled, 5.0);
    assert_eq!(effective.warning_memory_mib, 4096.0);
}

// 原測試：memory.max 低於 emergency 時往下壓並維持順序
// 舊版以 mibToBytes 建構 bigint；Rust 直接以 u128 對應。
#[test]
fn clamp_effective_thresholds_to_memory_max_compresses_below_emergency() {
    let effective = compute_effective_resource_guard_thresholds(&anchors(), 5.0);
    let clamp = clamp_effective_thresholds_to_memory_max(&effective, Some(mib_to_bytes(6144.0)));

    assert!(clamp.clamped);
    assert_eq!(clamp.effective.emergency_memory_mib, 6144.0 - 512.0);
    assert!(clamp.effective.warning_memory_mib < clamp.effective.browser_recycle_memory_mib);
    assert!(clamp.effective.browser_recycle_memory_mib < clamp.effective.emergency_memory_mib);
}

// 原測試：安全整數以 number 輸出，過大值以字串輸出
// 舊版以 10n 與 BigInt(Number.MAX_SAFE_INTEGER) + 10n 餵入；Rust 以 u128 對應。
#[test]
fn serializable_byte_count_outputs_number_within_safe_integer_and_string_beyond() {
    assert_eq!(
        serializable_byte_count(Some(10)),
        Some(SanitizedValue::Number(10.0))
    );
    let huge = MAX_SAFE_INTEGER + 10;
    assert_eq!(
        serializable_byte_count(Some(huge)),
        Some(SanitizedValue::String(huge.to_string()))
    );
    assert_eq!(serializable_byte_count(None), None);
}
