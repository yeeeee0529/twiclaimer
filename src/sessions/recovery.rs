//! 頻道復原政策：`ChannelRecoveryPolicy` 逐函式移植（§4.2，行為依據舊版
//! `src/sessions/ChannelRecoveryPolicy.ts`）。
//!
//! 保存獨立於 active session registry 的頻道復原狀態；量測與副作用在呼叫端
//! （S3.x 的 SessionManager）。時鐘以 [`RecoveryClock`] 注入（對應舊版 options
//! 的 `now`，預設 `Date.now`），單元測試注入固定時鐘、不等真實時間。
//!
//! 取捨：`BrowserRecoveryConfig`／`BrowserSessionStartConfig` 屬於舊版
//! `config/AppConfig.ts`，該層尚未移植（S2.3 的 ConfigLoader／model），此處先
//! 定義同形狀的 `pub(crate)` 型別，待 config 模組落地後改為重用。
#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

/// 舊版 `BrowserRecoveryConfig`（`config/AppConfig.ts`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BrowserRecoveryConfig {
    pub page_crash_backoff_seconds: Vec<u64>,
    pub channel_crash_window_seconds: u64,
    pub channel_quarantine_threshold: usize,
    pub channel_quarantine_seconds: u64,
    pub stable_reset_seconds: u64,
    pub multi_channel_crash_window_seconds: u64,
    pub multi_channel_crash_threshold: usize,
    pub browser_failure_window_seconds: u64,
    pub browser_failure_container_threshold: usize,
}

/// 舊版 `BrowserSessionStartConfig`（`config/AppConfig.ts`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BrowserSessionStartConfig {
    pub failure_backoff_seconds: Vec<u64>,
    pub maximum_cooldown_seconds: u64,
}

/// 舊版 options 的 `now: () => number`：回傳 epoch 毫秒（預設 [`default_now_ms`]）。
pub(crate) type RecoveryClock = Box<dyn Fn() -> u64 + Send + Sync>;

/// 對應舊版 `ChannelRecoveryPolicyOptions`；`now: None` 用系統時鐘。
pub(crate) struct ChannelRecoveryPolicyOptions {
    pub recovery: BrowserRecoveryConfig,
    pub session_start: BrowserSessionStartConfig,
    pub now: Option<RecoveryClock>,
}

/// 舊版 `ChannelRecoveryState`。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ChannelRecoveryState {
    pub consecutive_page_crashes: u32,
    pub consecutive_start_failures: u32,
    pub page_crash_timestamps_ms: Vec<u64>,
    pub next_retry_at_ms: Option<u64>,
    pub quarantined_until_ms: Option<u64>,
    pub stable_since_ms: Option<u64>,
    pub recovery_generation: u32,
}

/// 舊版 `ChannelRecoveryAvailability`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChannelRecoveryAvailability {
    Eligible {
        recovery_generation: u32,
    },
    Deferred {
        retry_at_ms: u64,
        recovery_generation: u32,
    },
    Quarantined {
        retry_at_ms: u64,
        recovery_generation: u32,
    },
    ProbeInFlight {
        recovery_generation: u32,
    },
}

/// 舊版 `PageCrashRecoveryDecision`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PageCrashRecoveryDecision {
    Backoff {
        retry_at_ms: u64,
        consecutive_page_crashes: u32,
    },
    Quarantined {
        retry_at_ms: u64,
        consecutive_page_crashes: u32,
    },
}

/// 舊版 `StartFailureRecoveryDecision`（`status` 恆為 `backoff`，故不另設欄位）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StartFailureRecoveryDecision {
    pub retry_at_ms: u64,
    pub consecutive_start_failures: u32,
}

/// 舊版 `BeginRecoveryProbeResult`：`started` 或 ineligible 的可用性狀態
/// （對應舊版 `Exclude<ChannelRecoveryAvailability, { status: 'eligible' }>`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BeginRecoveryProbeResult {
    Started { recovery_generation: u32 },
    NotEligible(ChannelRecoveryAvailability),
}

/// 對應舊版 `ChannelRecoveryPolicy`：保存獨立於 active session registry 的
/// 頻道復原狀態。
pub(crate) struct ChannelRecoveryPolicy {
    options: ChannelRecoveryPolicyOptions,
    states: HashMap<String, ChannelRecoveryState>,
    probes_in_flight: HashSet<String>,
    now: RecoveryClock,
}

impl ChannelRecoveryPolicy {
    pub(crate) fn new(mut options: ChannelRecoveryPolicyOptions) -> Self {
        let now = options
            .now
            .take()
            .unwrap_or_else(|| Box::new(default_now_ms));
        Self {
            options,
            states: HashMap::new(),
            probes_in_flight: HashSet::new(),
            now,
        }
    }

    /// 對應舊版 `getState`：回傳狀態的複本（deep clone）。
    pub(crate) fn get_state(&mut self, channel: &str) -> ChannelRecoveryState {
        let key = normalize_recovery_channel(channel);
        self.reset_after_stable_period_if_needed(&key);
        // 舊版順序為 getOrCreateState → reset；reset 只會修改、不刪鍵，對結果等價。
        self.states.entry(key).or_default().clone()
    }

    /// 對應舊版 `getAvailability`：quarantined → probe_in_flight → deferred →
    /// eligible 的判定順序照舊版。
    pub(crate) fn get_availability(&mut self, channel: &str) -> ChannelRecoveryAvailability {
        let key = normalize_recovery_channel(channel);
        self.reset_after_stable_period_if_needed(&key);
        let now_ms = (self.now)();
        let Some(state) = self.states.get(&key) else {
            // reset 不刪鍵、get_or_create 已建立；防衛性分支（實際不可達）。
            return ChannelRecoveryAvailability::Eligible {
                recovery_generation: 0,
            };
        };

        if let Some(quarantined_until_ms) = state.quarantined_until_ms
            && now_ms < quarantined_until_ms
        {
            return ChannelRecoveryAvailability::Quarantined {
                retry_at_ms: quarantined_until_ms,
                recovery_generation: state.recovery_generation,
            };
        }
        if self.probes_in_flight.contains(&key) {
            return ChannelRecoveryAvailability::ProbeInFlight {
                recovery_generation: state.recovery_generation,
            };
        }
        if let Some(next_retry_at_ms) = state.next_retry_at_ms
            && now_ms < next_retry_at_ms
        {
            return ChannelRecoveryAvailability::Deferred {
                retry_at_ms: next_retry_at_ms,
                recovery_generation: state.recovery_generation,
            };
        }
        ChannelRecoveryAvailability::Eligible {
            recovery_generation: state.recovery_generation,
        }
    }

    /// 對應舊版 `recordPageCrash`。
    pub(crate) fn record_page_crash(&mut self, channel: &str) -> PageCrashRecoveryDecision {
        let key = normalize_recovery_channel(channel);
        self.reset_after_stable_period_if_needed(&key);
        let now_ms = (self.now)();
        self.probes_in_flight.remove(&key);
        let state = self.states.entry(key.clone()).or_default();
        state.stable_since_ms = None;
        state.consecutive_page_crashes += 1;
        let crash_window_ms = self.options.recovery.channel_crash_window_seconds * 1_000;
        state
            .page_crash_timestamps_ms
            .retain(|&timestamp_ms| now_ms.saturating_sub(timestamp_ms) <= crash_window_ms);
        state.page_crash_timestamps_ms.push(now_ms);

        if state.page_crash_timestamps_ms.len()
            >= self.options.recovery.channel_quarantine_threshold
        {
            let retry_at_ms = now_ms + self.options.recovery.channel_quarantine_seconds * 1_000;
            state.quarantined_until_ms = Some(retry_at_ms);
            state.next_retry_at_ms = None;
            return PageCrashRecoveryDecision::Quarantined {
                retry_at_ms,
                consecutive_page_crashes: state.consecutive_page_crashes,
            };
        }

        let backoff_ms = page_crash_backoff_ms(
            &self.options.recovery.page_crash_backoff_seconds,
            state.consecutive_page_crashes,
        );
        let retry_at_ms = now_ms + backoff_ms;
        state.next_retry_at_ms = Some(retry_at_ms);
        PageCrashRecoveryDecision::Backoff {
            retry_at_ms,
            consecutive_page_crashes: state.consecutive_page_crashes,
        }
    }

    /// 對應舊版 `recordStartFailure`：backoff 依設定陣列取第 N 項，缺項時以
    /// `maximumCooldownSeconds` 為上限。
    pub(crate) fn record_start_failure(&mut self, channel: &str) -> StartFailureRecoveryDecision {
        let key = normalize_recovery_channel(channel);
        self.reset_after_stable_period_if_needed(&key);
        let now_ms = (self.now)();
        self.probes_in_flight.remove(&key);
        let state = self.states.entry(key.clone()).or_default();
        state.stable_since_ms = None;
        state.consecutive_start_failures += 1;
        let maximum_cooldown_seconds = self.options.session_start.maximum_cooldown_seconds;
        let index = (state.consecutive_start_failures as usize).saturating_sub(1);
        let backoff_seconds = self
            .options
            .session_start
            .failure_backoff_seconds
            .get(index)
            .copied()
            .unwrap_or(maximum_cooldown_seconds);
        let retry_at_ms = now_ms + backoff_seconds.min(maximum_cooldown_seconds) * 1_000;
        state.next_retry_at_ms = Some(retry_at_ms);
        StartFailureRecoveryDecision {
            retry_at_ms,
            consecutive_start_failures: state.consecutive_start_failures,
        }
    }

    /// 對應舊版 `beginProbe`：只有 eligible 狀態能啟動 probe，啟動時推進
    /// generation 並清掉暫存的 retry／quarantine 時點。
    pub(crate) fn begin_probe(&mut self, channel: &str) -> BeginRecoveryProbeResult {
        match self.get_availability(channel) {
            ChannelRecoveryAvailability::Eligible { .. } => {}
            not_eligible => return BeginRecoveryProbeResult::NotEligible(not_eligible),
        }

        let key = normalize_recovery_channel(channel);
        let state = self.states.entry(key.clone()).or_default();
        state.recovery_generation += 1;
        state.next_retry_at_ms = None;
        state.quarantined_until_ms = None;
        self.probes_in_flight.insert(key);
        BeginRecoveryProbeResult::Started {
            recovery_generation: state.recovery_generation,
        }
    }

    /// 對應舊版 `markStable`：generation 需與現狀一致；成功時記下穩定起點。
    pub(crate) fn mark_stable(&mut self, channel: &str, recovery_generation: u32) -> bool {
        let key = normalize_recovery_channel(channel);
        let generation_matches = self
            .states
            .get(&key)
            .is_some_and(|state| state.recovery_generation == recovery_generation);
        if !generation_matches {
            return false;
        }

        self.probes_in_flight.remove(&key);
        let now_ms = (self.now)();
        if let Some(state) = self.states.get_mut(&key) {
            state.stable_since_ms = Some(now_ms);
        }
        true
    }

    /// 對應舊版 `cancelProbe`：generation 一致且 probe 在飛時才取消。
    pub(crate) fn cancel_probe(&mut self, channel: &str, recovery_generation: u32) -> bool {
        let key = normalize_recovery_channel(channel);
        let generation_matches = self
            .states
            .get(&key)
            .is_some_and(|state| state.recovery_generation == recovery_generation)
            && self.probes_in_flight.contains(&key);
        if !generation_matches {
            return false;
        }
        self.probes_in_flight.remove(&key);
        true
    }

    /// 對應舊版 `isCurrentGeneration`；鍵不存在（或 generation 不符）回傳 false。
    pub(crate) fn is_current_generation(&self, channel: &str, recovery_generation: u32) -> bool {
        self.states
            .get(&normalize_recovery_channel(channel))
            .is_some_and(|state| state.recovery_generation == recovery_generation)
    }

    /// 對應舊版 `removeChannel`：完全清除 probe 與狀態。
    pub(crate) fn remove_channel(&mut self, channel: &str) {
        let key = normalize_recovery_channel(channel);
        self.probes_in_flight.remove(&key);
        self.states.remove(&key);
    }

    /// 對應舊版私有 `resetAfterStablePeriodIfNeeded`：穩定期達
    /// `stableResetSeconds` 後重設 failure 計數與暫存時點、取消 probe。
    fn reset_after_stable_period_if_needed(&mut self, key: &str) {
        let now_ms = (self.now)();
        let Some(stable_since_ms) = self.states.get(key).and_then(|state| state.stable_since_ms)
        else {
            return;
        };
        if now_ms.saturating_sub(stable_since_ms)
            < self.options.recovery.stable_reset_seconds * 1_000
        {
            return;
        }

        if let Some(state) = self.states.get_mut(key) {
            state.consecutive_page_crashes = 0;
            state.consecutive_start_failures = 0;
            state.page_crash_timestamps_ms.clear();
            state.next_retry_at_ms = None;
            state.quarantined_until_ms = None;
            state.stable_since_ms = None;
        }
        self.probes_in_flight.remove(key);
    }
}

/// 對應舊版私有 `pageCrashBackoffMs`：以連續 crash 數取 backoff 陣列第 N 項
/// （超過長度取最後一項）。舊版空陣列時 index 為 -1、乘 1_000 得 NaN（無效
/// 設定）；Rust 的時間以 u64 呈現，空陣列以 0 秒對應，避免時間失去界域。
fn page_crash_backoff_ms(backoffs: &[u64], consecutive_page_crashes: u32) -> u64 {
    if backoffs.is_empty() {
        return 0;
    }
    let index = (consecutive_page_crashes as usize)
        .saturating_sub(1)
        .min(backoffs.len() - 1);
    backoffs[index] * 1_000
}

/// 對應舊版 `normalizeRecoveryChannel`：狀態 key 不區分大小寫。
pub(crate) fn normalize_recovery_channel(channel: &str) -> String {
    channel.to_lowercase()
}

/// 舊版預設 `Date.now`：epoch 毫秒。時鐘倒退（理論上不會）以 0 對應。
fn default_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests;
