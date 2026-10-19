//! Resource guard 政策：`ResourceGuardPolicy` 逐函式移植（§4.2，行為依據舊版
//! `src/app/ResourceGuardPolicy.ts`）——純決策狀態機，量測與副作用在
//! RuntimeResourceMonitor（S3.2 移植）。
//!
//! 取捨：
//! - `CgroupSnapshot`／`CgroupMemoryEvents` 屬於舊版 `app/CgroupV2Reader.ts`，
//!   reader 本體在 S3.2 移植；此處定義政策可見的欄位子集。
//! - `ResourceGuardPolicyConfig`／`ResourceGuardEffectiveThresholds` 與轉換函式
//!   `bytesToMibNumber`／`mibToBytes` 屬於舊版 `config/resourceGuardThresholds.ts`，
//!   config 層尚未移植（S2.3），先在此處定義同形狀的 `pub(crate)` 型別。
//! - 時鐘以 [`GuardClock`] 注入（對應舊版 options 的 `now`，預設
//!   `performance.now()`）；舊測試注入固定／可寫入時鐘，不等真實時間。
#![allow(dead_code)]

use std::sync::OnceLock;
use std::time::Instant;

/// 舊版 `CgroupMemoryEvents`（`app/CgroupV2Reader.ts`）：memory.events 計數器
/// （JS bigint 以 u64 呈現，cgroup 計數不會超出）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CgroupMemoryEvents {
    pub high: u64,
    pub max: u64,
    pub oom: u64,
    pub oom_kill: u64,
}

/// 舊版 `CgroupSnapshot`（`app/CgroupV2Reader.ts`）中政策消費的欄位子集；
/// memory.max／peak／pids／cpu 由 reader 端移植時補齊。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CgroupSnapshot {
    pub sampled_at_monotonic_ms: u64,
    pub memory_current_bytes: u64,
    pub swap_current_bytes: Option<u64>,
    pub events: CgroupMemoryEvents,
}

/// 舊版 `ResourceGuardEffectiveThresholds`（`config/AppConfig.ts`）：隨串流數
/// 縮放的絕對門檻（binary MiB；JS number 以 f64 呈現）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ResourceGuardEffectiveThresholds {
    pub max_concurrent_streams: u32,
    pub warning_memory_mib: f64,
    pub warning_reset_memory_mib: f64,
    pub browser_recycle_memory_mib: f64,
    pub emergency_memory_mib: f64,
    pub post_recycle_target_memory_mib: f64,
}

/// 舊版 `ResourceGuardPolicyConfig`（`config/resourceGuardThresholds.ts` 的
/// `Pick<…> & { effective }` 別名）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ResourceGuardPolicyConfig {
    pub sample_interval_seconds: u64,
    pub startup_rate_grace_seconds: u64,
    pub browser_recycle_consecutive_samples: u64,
    pub emergency_swap_mib: f64,
    pub fast_growth_mib: f64,
    pub fast_growth_window_seconds: u64,
    pub post_browser_restart_rate_grace_seconds: u64,
    pub post_recycle_observation_seconds: u64,
    pub post_recycle_minimum_drop_mib: f64,
    pub effective: ResourceGuardEffectiveThresholds,
}

/// 對應舊版 `ResourceGuardPolicyOptions`；`now: None` 用單調時鐘。
pub(crate) struct ResourceGuardPolicyOptions {
    pub config: ResourceGuardPolicyConfig,
    pub now: Option<GuardClock>,
}

/// 舊版 options 的 `now: () => number`：回傳單調毫秒（預設 [`monotonic_now_ms`]）。
pub(crate) type GuardClock = Box<dyn Fn() -> u64 + Send + Sync>;

/// 舊版 `ResourceGuardDecision`：`reason` 照舊版的固定字面值（編譯期常數）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResourceGuardDecision {
    None,
    Warn { reason: &'static str },
    RecycleBrowser { reason: &'static str },
    RestartContainer { reason: &'static str },
}

/// 政策內部的取樣點（對應舊版私有 `SamplePoint`）。
#[derive(Debug, Clone, Copy)]
struct SamplePoint {
    at_ms: u64,
    memory_current_bytes: u64,
}

/// 政策內部的 recycle 觀察記錄（對應舊版 recycleObservation 的形狀）。
#[derive(Debug, Clone, Copy)]
struct RecycleObservation {
    deadline_ms: u64,
    memory_before_bytes: u64,
}

/// 對應舊版 `ResourceGuardPolicy`：cgroup snapshot 之上的純決策狀態機。
pub(crate) struct ResourceGuardPolicy {
    now: GuardClock,
    config: ResourceGuardPolicyConfig,
    started_at_ms: Option<u64>,
    warning_latched: bool,
    consecutive_high_samples: u64,
    sample_history: Vec<SamplePoint>,
    last_events: Option<CgroupMemoryEvents>,
    pending_memory_high_event: bool,
    recycle_observation: Option<RecycleObservation>,
    /// 單調期限：在此之前抑制 fast_memory_growth。
    rate_growth_suppressed_until_ms: u64,
}

impl ResourceGuardPolicy {
    pub(crate) fn new(mut options: ResourceGuardPolicyOptions) -> Self {
        let now = options
            .now
            .take()
            .unwrap_or_else(|| Box::new(monotonic_now_ms));
        Self {
            now,
            config: options.config,
            started_at_ms: None,
            warning_latched: false,
            consecutive_high_samples: 0,
            sample_history: Vec::new(),
            last_events: None,
            pending_memory_high_event: false,
            recycle_observation: None,
            rate_growth_suppressed_until_ms: 0,
        }
    }

    /// 對應舊版 `updateConfig`。
    pub(crate) fn update_config(&mut self, config: ResourceGuardPolicyConfig) {
        self.config = config;
    }

    /// 對應舊版 `getConfig`。
    pub(crate) fn get_config(&self) -> &ResourceGuardPolicyConfig {
        &self.config
    }

    /// 對應舊版 `isRecycleObservationActive`。
    pub(crate) fn is_recycle_observation_active(&self) -> bool {
        self.recycle_observation.is_some()
    }

    /// 對應舊版 `isWarningLatched`。
    pub(crate) fn is_warning_latched(&self) -> bool {
        self.warning_latched
    }

    /// 對應舊版 `beginRecycleObservation`。
    pub(crate) fn begin_recycle_observation(&mut self, memory_before_bytes: u64) {
        self.recycle_observation = Some(RecycleObservation {
            memory_before_bytes,
            deadline_ms: (self.now)() + self.config.post_recycle_observation_seconds * 1_000,
        });
        self.consecutive_high_samples = 0;
        // 舊版註解：Browser refill after recycle must not be misclassified as fast growth.
        self.note_browser_restart();
    }

    /// 對應舊版 `clearRecycleObservation`。
    pub(crate) fn clear_recycle_observation(&mut self) {
        self.recycle_observation = None;
    }

    /// 對應舊版 `noteBrowserRestart`：在任何成功的瀏覽器重啟／recycle 之後呼叫。
    /// 清除成長取樣歷史，並套用 post-restart 的 rate 寬限，讓預期的 session
    /// refill 不被誤判為致命的記憶體成長。
    pub(crate) fn note_browser_restart(&mut self) {
        self.sample_history.clear();
        self.consecutive_high_samples = 0;
        let grace_ms = self.config.post_browser_restart_rate_grace_seconds * 1_000;
        if grace_ms == 0 {
            self.rate_growth_suppressed_until_ms = 0;
            return;
        }
        let until_ms = (self.now)() + grace_ms;
        if until_ms > self.rate_growth_suppressed_until_ms {
            self.rate_growth_suppressed_until_ms = until_ms;
        }
    }

    /// 對應舊版 `isRateGrowthSuppressed`。
    pub(crate) fn is_rate_growth_suppressed(&self) -> bool {
        (self.now)() < self.rate_growth_suppressed_until_ms
    }

    /// 對應舊版 `evaluate`。
    pub(crate) fn evaluate(&mut self, snapshot: &CgroupSnapshot) -> ResourceGuardDecision {
        let now_ms = snapshot.sampled_at_monotonic_ms;
        if self.started_at_ms.is_none() {
            self.started_at_ms = Some(now_ms);
        }
        self.push_sample(snapshot);

        if let Some(emergency) = self.evaluate_emergency(snapshot, true) {
            return emergency;
        }

        if self.recycle_observation.is_some() {
            if let Some(post) = self.evaluate_post_recycle(snapshot) {
                return post;
            }
            // 舊版註解：Suppress further recycle while observing; emergencies already returned.
            return ResourceGuardDecision::None;
        }

        if let Some(recycle) = self.evaluate_recycle(snapshot) {
            return recycle;
        }

        if let Some(warning) = self.evaluate_warning(snapshot) {
            return warning;
        }

        ResourceGuardDecision::None
    }

    /// 對應舊版 `evaluateEmergencyOnly`：browser recycle 期間僅評估不可延後的
    /// 致命訊號，避免 refill 觸發一般門檻。
    pub(crate) fn evaluate_emergency_only(
        &mut self,
        snapshot: &CgroupSnapshot,
    ) -> ResourceGuardDecision {
        let now_ms = snapshot.sampled_at_monotonic_ms;
        if self.started_at_ms.is_none() {
            self.started_at_ms = Some(now_ms);
        }
        self.push_sample(snapshot);
        self.evaluate_emergency(snapshot, false)
            .unwrap_or(ResourceGuardDecision::None)
    }

    /// 對應舊版私有 `evaluateEmergency`。
    fn evaluate_emergency(
        &mut self,
        snapshot: &CgroupSnapshot,
        include_fast_growth: bool,
    ) -> Option<ResourceGuardDecision> {
        // 舊版註解：Always advance event counters first so deltas stay accurate.
        let event_emergency = self.evaluate_event_deltas(snapshot);
        let effective = self.config.effective;
        let memory_mib = bytes_to_mib_number(snapshot.memory_current_bytes);

        if memory_mib >= effective.emergency_memory_mib {
            return Some(ResourceGuardDecision::RestartContainer {
                reason: "emergency_memory",
            });
        }

        if let Some(swap_current_bytes) = snapshot.swap_current_bytes
            && bytes_to_mib_number(swap_current_bytes) >= self.config.emergency_swap_mib
        {
            return Some(ResourceGuardDecision::RestartContainer {
                reason: "emergency_swap",
            });
        }

        if let Some(event_emergency) = event_emergency {
            return Some(event_emergency);
        }

        if !include_fast_growth || self.should_ignore_fast_growth(snapshot) {
            return None;
        }

        if self.has_fast_growth(snapshot) {
            return Some(ResourceGuardDecision::RestartContainer {
                reason: "fast_memory_growth",
            });
        }

        None
    }

    /// 對應舊版私有 `shouldIgnoreFastGrowth`：啟動寬限期內、或 browser restart
    /// 後的 rate 寬限期內，一律忽略成長速率。
    fn should_ignore_fast_growth(&self, snapshot: &CgroupSnapshot) -> bool {
        let startup_grace_ms = self.config.startup_rate_grace_seconds * 1_000;
        let past_startup_grace = self.started_at_ms.is_some_and(|started_at_ms| {
            snapshot
                .sampled_at_monotonic_ms
                .saturating_sub(started_at_ms)
                >= startup_grace_ms
        });
        if !past_startup_grace {
            return true;
        }
        // 舊版註解：noteBrowserRestart() records the deadline with the same now() clock.
        if (self.now)() < self.rate_growth_suppressed_until_ms {
            return true;
        }
        false
    }

    /// 對應舊版私有 `evaluateEventDeltas`：先推進事件計數快照，再依增量判定。
    fn evaluate_event_deltas(
        &mut self,
        snapshot: &CgroupSnapshot,
    ) -> Option<ResourceGuardDecision> {
        let current = snapshot.events;
        let previous = self.last_events.replace(current)?;

        if current.oom > previous.oom {
            return Some(ResourceGuardDecision::RestartContainer {
                reason: "cgroup_oom",
            });
        }
        if current.oom_kill > previous.oom_kill {
            return Some(ResourceGuardDecision::RestartContainer {
                reason: "cgroup_oom_kill",
            });
        }
        if current.max > previous.max {
            return Some(ResourceGuardDecision::RestartContainer {
                reason: "cgroup_memory_max",
            });
        }
        if current.high > previous.high {
            // 舊版註解：Logged by the monitor; not a fatal signal unless memory.high is configured.
            self.pending_memory_high_event = true;
        }
        None
    }

    /// 對應舊版 `takeMemoryHighEvent`：消費最新一次 evaluate() 設定的一次性
    /// high-event 旗標。
    pub(crate) fn take_memory_high_event(&mut self) -> bool {
        let value = self.pending_memory_high_event;
        self.pending_memory_high_event = false;
        value
    }

    /// 對應舊版私有 `evaluatePostRecycle`。
    fn evaluate_post_recycle(
        &mut self,
        snapshot: &CgroupSnapshot,
    ) -> Option<ResourceGuardDecision> {
        let observation = self.recycle_observation?;
        let effective = self.config.effective;
        let memory = snapshot.memory_current_bytes;
        let target_bytes = mib_to_bytes(effective.post_recycle_target_memory_mib);
        let recycle_bytes = mib_to_bytes(effective.browser_recycle_memory_mib);
        // drop 可為負（refill 讓 memory 超過 before），以 i64 呈現 JS bigint 的正負差。
        let drop_bytes = observation.memory_before_bytes as i64 - memory as i64;
        let min_drop = mib_to_bytes(self.config.post_recycle_minimum_drop_mib) as i64;

        let reached_target = memory < target_bytes;
        let sufficient_drop = drop_bytes >= min_drop && memory < recycle_bytes;

        if reached_target || sufficient_drop {
            self.recycle_observation = None;
            self.consecutive_high_samples = 0;
            // 舊版回傳 decision（action none）而非 undefined；以此結束觀察。
            return Some(ResourceGuardDecision::None);
        }

        if snapshot.sampled_at_monotonic_ms >= observation.deadline_ms {
            self.recycle_observation = None;
            return Some(ResourceGuardDecision::RestartContainer {
                reason: "post_recycle_reclamation_failed",
            });
        }

        None
    }

    /// 對應舊版私有 `evaluateRecycle`。
    fn evaluate_recycle(&mut self, snapshot: &CgroupSnapshot) -> Option<ResourceGuardDecision> {
        let recycle_bytes = mib_to_bytes(self.config.effective.browser_recycle_memory_mib);
        if snapshot.memory_current_bytes >= recycle_bytes {
            self.consecutive_high_samples += 1;
        } else {
            self.consecutive_high_samples = 0;
            return None;
        }

        if self.consecutive_high_samples >= self.config.browser_recycle_consecutive_samples {
            self.consecutive_high_samples = 0;
            return Some(ResourceGuardDecision::RecycleBrowser {
                reason: "sustained_high_memory",
            });
        }

        None
    }

    /// 對應舊版私有 `evaluateWarning`。
    fn evaluate_warning(&mut self, snapshot: &CgroupSnapshot) -> Option<ResourceGuardDecision> {
        let warning_bytes = mib_to_bytes(self.config.effective.warning_memory_mib);
        let reset_bytes = mib_to_bytes(self.config.effective.warning_reset_memory_mib);

        if snapshot.memory_current_bytes >= warning_bytes {
            if !self.warning_latched {
                self.warning_latched = true;
                return Some(ResourceGuardDecision::Warn {
                    reason: "memory_warning",
                });
            }
            return None;
        }

        if snapshot.memory_current_bytes < reset_bytes {
            self.warning_latched = false;
        }
        None
    }

    /// 對應舊版私有 `hasFastGrowth`：單看速率在低水位太吵（recycle 後的
    /// refill 常從近空的谷底成長約 1–2 GiB），故要求目前水位已在 warning
    /// 水線附近。
    fn has_fast_growth(&self, snapshot: &CgroupSnapshot) -> bool {
        let min_absolute_bytes = mib_to_bytes(self.config.effective.warning_memory_mib);
        if snapshot.memory_current_bytes < min_absolute_bytes {
            return false;
        }

        let window_ms = self.config.fast_growth_window_seconds * 1_000;
        let growth_bytes = mib_to_bytes(self.config.fast_growth_mib);
        let now_ms = snapshot.sampled_at_monotonic_ms;
        let oldest_allowed = now_ms.saturating_sub(window_ms);

        let baseline = self
            .sample_history
            .iter()
            .find(|sample| sample.at_ms >= oldest_allowed);
        let Some(baseline) = baseline else {
            return false;
        };

        snapshot.memory_current_bytes >= baseline.memory_current_bytes.saturating_add(growth_bytes)
    }

    /// 對應舊版私有 `pushSample`。
    fn push_sample(&mut self, snapshot: &CgroupSnapshot) {
        self.sample_history.push(SamplePoint {
            at_ms: snapshot.sampled_at_monotonic_ms,
            memory_current_bytes: snapshot.memory_current_bytes,
        });

        // 舊版註解：Keep a bounded window slightly larger than the growth window.
        let retain_ms = self
            .config
            .fast_growth_window_seconds
            .max(self.config.sample_interval_seconds)
            * 1_000
            + self.config.sample_interval_seconds * 1_000 * 2;
        let cutoff = snapshot.sampled_at_monotonic_ms.saturating_sub(retain_ms);
        self.sample_history.retain(|sample| sample.at_ms >= cutoff);
    }
}

/// 舊版 `MIB`（`config/resourceGuardThresholds.ts`）。
const MIB: u64 = 1_048_576;

/// 舊版 `Number.MAX_SAFE_INTEGER`（`bytesToMibNumber` 的分界）。
const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;

/// 舊版 `mibToBytes`：MiB → 位元組。JS 的 `BigInt(mib)` 對小數無條件捨去、
/// 負數丟例外；u64 轉換同樣捨去，負數輸入照 u64 的飽和語意歸零（實際設定
/// 不會出現）。
pub(crate) fn mib_to_bytes(mib: f64) -> u64 {
    (mib as u64).saturating_mul(MIB)
}

/// 舊版 `bytesToMibNumber`：位元組 → MiB（f64 對應 JS number；超過安全整數時
/// 照舊版先做整數除法再轉 number）。
pub(crate) fn bytes_to_mib_number(bytes: u64) -> f64 {
    if bytes > MAX_SAFE_INTEGER {
        return (bytes / MIB) as f64;
    }
    bytes as f64 / MIB as f64
}

/// 舊版預設 `performance.now()`：單調毫秒（以程序內第一次呼叫為原點，僅供相對
/// 比較；u128 截斷為 u64 在實際壽命內不會發生）。
fn monotonic_now_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

#[cfg(test)]
mod tests;
