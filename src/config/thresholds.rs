//! Resource guard 門檻計算：對應舊版 `config/resourceGuardThresholds.ts` 逐函式移植。
//!
//! - 縮放公式：`effective = base + (anchor - base) × (N / baseline)`
//!   （`scale_with_streams` 為 false 時直接四捨五入成絕對值）。
//! - `Math.round`／`Math.max`／`Math.min` 的行為以 [`js_round`]／[`js_max`]／[`js_min`]
//!   重現；輸入皆為已驗證的有限數（設定驗證保證），不處理 NaN 的特殊排序。
//! - 位元組計數（cgroup memory.max 等）以 `u128` 對應舊版 `bigint`，
//!   避免 JS number 的精度問題；[`mib_to_bytes`] 等轉換的輸入是已驗證的安全整數。
#![allow(dead_code)]

use super::model::ResourceGuardEffectiveThresholds;
use crate::logging::redact::SanitizedValue;

/// 舊版 `MIB`：1 MiB 的位元組數。
const MIB: u128 = 1_048_576;

/// 舊版 `Number.MAX_SAFE_INTEGER`（bytes 與 JS number 的界線）。
pub(crate) const MAX_SAFE_INTEGER: u128 = 9_007_199_254_740_991;

/// 對應舊版 `ResourceGuardAnchorThresholds`：`N = baseline_streams` 的錨點門檻。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ResourceGuardAnchorThresholds {
    pub(crate) scale_with_streams: bool,
    pub(crate) baseline_streams: f64,
    pub(crate) base_memory_mib: f64,
    pub(crate) warning_memory_mib: f64,
    pub(crate) warning_reset_memory_mib: f64,
    pub(crate) browser_recycle_memory_mib: f64,
    pub(crate) emergency_memory_mib: f64,
    pub(crate) post_recycle_target_memory_mib: f64,
}

/// 對應舊版 `clampEffectiveThresholdsToMemoryMax` 的回傳物件。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ThresholdClamp {
    /// 套用（可能被壓低的）門檻；不需要時與輸入相同。
    pub(crate) effective: ResourceGuardEffectiveThresholds,
    /// 是否有壓低。
    pub(crate) clamped: bool,
}

/// 對應舊版 `computeEffectiveResourceGuardThresholds`：
/// 把 `N = baseline_streams` 的錨點縮放到設定的同時串流數。
pub(crate) fn compute_effective_resource_guard_thresholds(
    anchors: &ResourceGuardAnchorThresholds,
    max_concurrent_streams: f64,
) -> ResourceGuardEffectiveThresholds {
    let stream_count = js_max(1.0, max_concurrent_streams);
    let scale = |anchor_mib: f64| -> f64 {
        if !anchors.scale_with_streams {
            return js_round(anchor_mib);
        }
        let baseline = js_max(1.0, anchors.baseline_streams);
        let base = js_max(0.0, anchors.base_memory_mib);
        let scaled = base + (anchor_mib - base) * (stream_count / baseline);
        js_max(1.0, js_round(scaled))
    };

    let warning_memory_mib = scale(anchors.warning_memory_mib);
    let warning_reset_memory_mib = js_min(
        scale(anchors.warning_reset_memory_mib),
        warning_memory_mib - 1.0,
    );
    let browser_recycle_memory_mib = scale(anchors.browser_recycle_memory_mib);
    let emergency_memory_mib = scale(anchors.emergency_memory_mib);
    let post_recycle_target_memory_mib = scale(anchors.post_recycle_target_memory_mib);

    ResourceGuardEffectiveThresholds {
        max_concurrent_streams: stream_count,
        warning_memory_mib,
        warning_reset_memory_mib: js_max(1.0, warning_reset_memory_mib),
        browser_recycle_memory_mib,
        emergency_memory_mib,
        post_recycle_target_memory_mib,
    }
}

/// 對應舊版 `clampEffectiveThresholdsToMemoryMax`：
/// 把門檻壓在選用的 cgroup memory.max 硬上限之下。不需要壓低（或 max 不可用）
/// 時照舊版回傳原門檻與 `clamped: false`。
pub(crate) fn clamp_effective_thresholds_to_memory_max(
    effective: &ResourceGuardEffectiveThresholds,
    memory_max_bytes: Option<u128>,
) -> ThresholdClamp {
    let Some(memory_max_bytes) = memory_max_bytes else {
        return ThresholdClamp {
            effective: effective.clone(),
            clamped: false,
        };
    };
    // 舊版 `memoryMaxBytes <= 0n`：u128 不為負，只可能是 0。
    if memory_max_bytes == 0 {
        return ThresholdClamp {
            effective: effective.clone(),
            clamped: false,
        };
    }
    if memory_max_bytes > MAX_SAFE_INTEGER {
        return ThresholdClamp {
            effective: effective.clone(),
            clamped: false,
        };
    }

    let memory_max_mib = memory_max_bytes as f64 / MIB as f64;
    // 留 headroom，讓 emergency 在 kernel OOM killer 之前先觸發。
    let emergency_cap = js_max(1.0, (memory_max_mib - 512.0).floor());
    if effective.emergency_memory_mib <= emergency_cap {
        return ThresholdClamp {
            effective: effective.clone(),
            clamped: false,
        };
    }

    let emergency_memory_mib = emergency_cap;
    let browser_recycle_memory_mib = js_min(
        effective.browser_recycle_memory_mib,
        js_max(1.0, emergency_memory_mib - 256.0),
    );
    let warning_memory_mib = js_min(
        effective.warning_memory_mib,
        js_max(1.0, browser_recycle_memory_mib - 256.0),
    );
    let warning_reset_memory_mib = js_min(
        effective.warning_reset_memory_mib,
        js_max(1.0, warning_memory_mib - 1.0),
    );
    let post_recycle_target_memory_mib =
        js_min(effective.post_recycle_target_memory_mib, warning_memory_mib);

    ThresholdClamp {
        clamped: true,
        effective: ResourceGuardEffectiveThresholds {
            max_concurrent_streams: effective.max_concurrent_streams,
            warning_memory_mib,
            warning_reset_memory_mib,
            browser_recycle_memory_mib,
            emergency_memory_mib,
            post_recycle_target_memory_mib,
        },
    }
}

/// 對應舊版 `mibToBytes`：`BigInt(mib) × MIB`（`mib` 是已驗證的正整數，
/// `f64 as u128` 的截斷與 `BigInt()` 相同）。
pub(crate) fn mib_to_bytes(mib: f64) -> u128 {
    (mib as u128) * MIB
}

/// 對應舊版 `bytesToMibNumber`：超過安全整數先做整數除法再轉 number，
/// 其內直接轉 number 再除（避免精度損失）。
pub(crate) fn bytes_to_mib_number(bytes: u128) -> f64 {
    if bytes > MAX_SAFE_INTEGER {
        (bytes / MIB) as f64
    } else {
        bytes as f64 / MIB as f64
    }
}

/// 對應舊版 `serializableByteCount`：安全整數內以數字輸出，過大值以字串輸出；
/// `undefined` 對應 [`None`]。輸出型別是日誌值模型，供 JSON 日誌直接使用
/// （不會像 bigint 一樣在 `JSON.stringify` 丟錯）。
pub(crate) fn serializable_byte_count(value: Option<u128>) -> Option<SanitizedValue> {
    match value {
        None => None,
        Some(value) if value <= MAX_SAFE_INTEGER => Some(SanitizedValue::Number(value as f64)),
        Some(value) => Some(SanitizedValue::String(value.to_string())),
    }
}

/// 舊版 `Math.round`：半數向上（往 +∞）進位；Rust 的 `f64::round` 是半數遠離零，
/// 負數行為不同，因此自寫（`x + 0.5` 再向下取整；門檻值的量級下精度足夠）。
fn js_round(value: f64) -> f64 {
    (value + 0.5).floor()
}

/// 舊版 `Math.max`（輸入皆為有限數）。
fn js_max(a: f64, b: f64) -> f64 {
    if a < b { b } else { a }
}

/// 舊版 `Math.min`（輸入皆為有限數）。
fn js_min(a: f64, b: f64) -> f64 {
    if b < a { b } else { a }
}

#[cfg(test)]
mod tests;
