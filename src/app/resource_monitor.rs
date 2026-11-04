//! Runtime 資源監視器：`app/RuntimeResourceMonitor.ts` 逐函式移植（§4.2，
//! S3.2）。發出程序＋cgroup 的資源遙測（runtime_resource_snapshot），並在
//! guard 啟用時執行 [`ResourceGuardPolicy`]。
//!
//! 取捨（macOS 可驗證性，照計畫 §122 與 DI 前例）：
//! - cgroup 讀取以 [`CgroupSnapshotSource`](super::cgroup::CgroupSnapshotSource)
//!   注入（預設 [`CgroupV2Reader`](super::cgroup::CgroupV2Reader)；
//!   `/sys/fs/cgroup` 等預設路徑只在 Linux 有內容，macOS 上回報不可用）。
//!   瀏覽器／session／日誌／process 指標以 trait 注入，測試在 macOS 全跑；
//!   預設組裝的 `#[cfg(target_os = "linux")]` 切分在呼叫端薄層（S3.6），由
//!   Docker test stage 驗證。
//! - 舊版 `setTimeout`／`setInterval` 的計時改用 tokio paused time（計畫 §9；
//!   舊測試注入 `setTimeoutFn`／`clearTimeoutFn` 的縫由 paused time 涵蓋）。
//!   取樣順序與舊版相同：初始取樣（force telemetry）→ 等間隔 → 取樣 → …
//!   （舊版的 `setTimeout` 也是在上一次取樣完成後才排下一次）。
//! - 舊版 `sampleFlight` 的單飛（dedup）：取樣只由 start 與工作迴圈順序驅動，
//!   不會並發；stop 會等工作迴圈收尾。Rust 版以同樣的順序結構等效涵蓋，
//!   不再另設 flight 物件。
//! - 舊版 `process.*`（Node）指標改由 [`ProcessMetricsSource`] 注入：CPU 與
//!   maxRSS 以 getrusage 取得；RSS 在 Linux 取自 `/proc/self/statm`
//!   （macOS 沒有對應來源，為 null）；`heapUsed`／`external` 是 V8 專屬，
//!   一律 null（X5，`logging/logger.rs` 的模組文件已註記）。
//! - `start`／`stop`／`notify_browser_restarted`／`get_latest_resource_limits`
//!   在 Rust 為 async（狀態在 tokio Mutex 內）；取樣時持鎖，語意對應舊版的
//!   sampleFlight 串行。
//! - 舊版 `applyDecision` 的 `recycle_browser` 是 fire-and-forget；Rust 版以
//!   `tokio::spawn` 外派（flight 槽位以 epoch 代號對照，對應舊版 finally 的
//!   身分比對）。

#![allow(dead_code)]

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use super::cgroup::{
    CgroupClock, CgroupReaderAvailability, CgroupSnapshot, CgroupSnapshotSource, CgroupV2Reader,
    CgroupV2ReaderOptions,
};
use super::diagnostics::{DiagnosticLimitValue, RuntimeResourceLimitsSnapshot};
use super::resource_guard::{
    GuardClock, ResourceGuardDecision, ResourceGuardPolicy, ResourceGuardPolicyConfig,
    ResourceGuardPolicyOptions,
};
use super::restart::{ContainerRestartNotifyObserver, ControllerFieldValue};
use crate::config::model::{ResourceGuardConfig, ResourceGuardEffectiveThresholds};
use crate::config::thresholds::{
    MAX_SAFE_INTEGER, clamp_effective_thresholds_to_memory_max, serializable_byte_count,
};
use crate::logging::logger::JsonLineLogger;
use crate::logging::redact::SanitizedValue;

/// 舊版 `RuntimeResourceMonitor` 重匯出的 `ContainerRestartRequest`（舊版
/// `export type { ContainerRestartRequest }`）。
pub(crate) use super::restart::ContainerRestartRequest;

/// 舊版 options 的 `now: () => number`（預設 [`monotonic_now_ms`]）；
/// 重用 reader 的共用時鐘型別。
pub(crate) type MonitorClock = CgroupClock;

/// 舊版 `Pick<BrowserManager, 'getPageCount' | 'restart'>`。
#[async_trait]
pub(crate) trait MonitorBrowser: Send + Sync {
    /// 對應 `getPageCount()`。
    fn get_page_count(&self) -> u64;

    /// 對應 `restart(): Promise<void>`；失敗以 `error.message` 呈現。
    async fn restart(&self) -> Result<(), String>;
}

/// 舊版 `Pick<SessionManager, 'getActiveChannels'>`。
pub(crate) trait MonitorSessions: Send + Sync {
    fn get_active_channels(&self) -> Vec<String>;
}

/// 舊版 `Pick<Logger, 'info' | 'warn' | 'error' | 'debug'>`。
pub(crate) trait MonitorLogger: Send + Sync {
    fn debug(&self, event: &str, fields: Option<&SanitizedValue>);
    fn info(&self, event: &str, fields: Option<&SanitizedValue>);
    fn warn(&self, event: &str, fields: Option<&SanitizedValue>);
    fn error(&self, event: &str, fields: Option<&SanitizedValue>);
}

impl MonitorLogger for JsonLineLogger {
    fn debug(&self, event: &str, fields: Option<&SanitizedValue>) {
        JsonLineLogger::debug(self, event, fields);
    }

    fn info(&self, event: &str, fields: Option<&SanitizedValue>) {
        JsonLineLogger::info(self, event, fields);
    }

    fn warn(&self, event: &str, fields: Option<&SanitizedValue>) {
        JsonLineLogger::warn(self, event, fields);
    }

    fn error(&self, event: &str, fields: Option<&SanitizedValue>) {
        JsonLineLogger::error(self, event, fields);
    }
}

/// 舊版 `process.memoryUsage()`／`cpuUsage()`／`resourceUsage()` 的取樣值；
/// `heap_used_bytes`／`external_bytes` 是 V8 專屬（X5），預設為 `None`。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ProcessMetricsSnapshot {
    pub cpu_user_us: Option<f64>,
    pub cpu_system_us: Option<f64>,
    pub rss_bytes: Option<u64>,
    pub heap_used_bytes: Option<u64>,
    pub external_bytes: Option<u64>,
    pub max_rss_kb: Option<f64>,
}

/// process 指標的注入介面（正式程式用 [`DefaultProcessMetrics`]，測試注入
/// 固定值）。
pub(crate) trait ProcessMetricsSource: Send + Sync {
    fn snapshot(&self) -> ProcessMetricsSnapshot;
}

/// 舊版 `process.*` 的預設來源：getrusage（CPU、maxRSS）與
/// `/proc/self/statm`（RSS）。
pub(crate) struct DefaultProcessMetrics;

impl ProcessMetricsSource for DefaultProcessMetrics {
    fn snapshot(&self) -> ProcessMetricsSnapshot {
        let (cpu_user_us, cpu_system_us, max_rss_kb) =
            match nix::sys::resource::getrusage(nix::sys::resource::UsageWho::RUSAGE_SELF) {
                Ok(usage) => {
                    // 舊版 process.cpuUsage()：微秒。
                    let user_time = usage.user_time();
                    let system_time = usage.system_time();
                    let user_us = user_time.tv_sec() * 1_000_000 + i64::from(user_time.tv_usec());
                    let system_us =
                        system_time.tv_sec() * 1_000_000 + i64::from(system_time.tv_usec());
                    // 舊版 resourceUsage().maxRSS 是 KB：Linux 的 ru_maxrss 已是
                    // KB；macOS 是 bytes（除以 1024 換成 KB）。
                    #[cfg(target_os = "macos")]
                    let max_rss_kb = usage.max_rss() as f64 / 1024.0;
                    #[cfg(not(target_os = "macos"))]
                    let max_rss_kb = usage.max_rss() as f64;
                    (
                        Some(user_us as f64),
                        Some(system_us as f64),
                        Some(max_rss_kb),
                    )
                }
                Err(_) => (None, None, None),
            };

        ProcessMetricsSnapshot {
            cpu_user_us,
            cpu_system_us,
            rss_bytes: read_rss_bytes(),
            heap_used_bytes: None,
            external_bytes: None,
            max_rss_kb,
        }
    }
}

/// 舊版 `process.memoryUsage().rss`：Linux 取 `/proc/self/status` 的
/// `VmRSS`（KB）；`/proc` 不存在（macOS 開發機）時為 `None`。A/B 比較的 RSS
/// 統計在 Linux 容器內進行（計畫 §10）。
fn read_rss_bytes() -> Option<u64> {
    let source = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = source.lines().find(|line| line.starts_with("VmRSS:"))?;
    let kb: u64 = line["VmRSS:".len()..]
        .trim()
        .trim_end_matches("kB")
        .trim()
        .parse()
        .ok()?;
    kb.checked_mul(1024)
}

/// 舊版 resourceGuardState 的字面值（照舊版的字串）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GuardState {
    Disabled,
    Unavailable,
    Idle,
    Warning,
    Recycling,
    Observing,
    Restarting,
}

impl GuardState {
    fn as_str(self) -> &'static str {
        match self {
            GuardState::Disabled => "disabled",
            GuardState::Unavailable => "unavailable",
            GuardState::Idle => "idle",
            GuardState::Warning => "warning",
            GuardState::Recycling => "recycling",
            GuardState::Observing => "observing",
            GuardState::Restarting => "restarting",
        }
    }
}

/// 對應舊版 `RuntimeResourceMonitorOptions`。
pub(crate) struct RuntimeResourceMonitorOptions {
    pub browser: Arc<dyn MonitorBrowser>,
    pub sessions: Arc<dyn MonitorSessions>,
    pub logger: Arc<dyn MonitorLogger>,
    /// 舊版 `intervalSeconds`：runtime_resource_snapshot 的正常節奏（正整數）。
    pub interval_seconds: f64,
    pub resource_guard: Option<ResourceGuardConfig>,
    pub cgroup_reader: Option<Arc<dyn CgroupSnapshotSource>>,
    pub process_metrics: Option<Arc<dyn ProcessMetricsSource>>,
    pub now: Option<MonitorClock>,
    pub on_container_restart_requested: Option<ContainerRestartNotifyObserver>,
}

/// monitor 的可變狀態（對應舊版的私有欄位群；取樣期間持鎖）。
struct MonitorState {
    stopped: bool,
    policy: Option<ResourceGuardPolicy>,
    cgroup_available: Option<bool>,
    cgroup_unavailable_logged: bool,
    thresholds_clamped_logged: bool,
    /// recycle flight 的代號（對應舊版 finally 的身分比對）。
    recycle_epoch: u64,
    recycle_flight: Option<JoinHandle<()>>,
    last_telemetry_at_ms: u64,
    latest_resource_limits: RuntimeResourceLimitsSnapshot,
    guard_state: GuardState,
}

struct Inner {
    browser: Arc<dyn MonitorBrowser>,
    sessions: Arc<dyn MonitorSessions>,
    logger: Arc<dyn MonitorLogger>,
    interval_seconds: f64,
    resource_guard: Option<ResourceGuardConfig>,
    reader: Arc<dyn CgroupSnapshotSource>,
    process_metrics: Arc<dyn ProcessMetricsSource>,
    now: MonitorClock,
    on_container_restart_requested: Option<ContainerRestartNotifyObserver>,
    state: Mutex<MonitorState>,
    /// 舊版 `this.timer`：工作迴圈的 handle（stop 時結束）。
    task: Mutex<Option<JoinHandle<()>>>,
    /// 停止訊號（對應舊版 clearTimeout 後不再排程）。
    stop_tx: tokio::sync::watch::Sender<bool>,
    stop_rx: tokio::sync::watch::Receiver<bool>,
}

/// 對應舊版 `RuntimeResourceMonitor`：發出程序＋cgroup 資源遙測並執行
/// ResourceGuardPolicy。
pub(crate) struct RuntimeResourceMonitor(Arc<Inner>);

impl RuntimeResourceMonitor {
    pub(crate) fn new(mut options: RuntimeResourceMonitorOptions) -> Result<Self, String> {
        // 舊版 TypeError：`intervalSeconds must be a positive integer`。
        if !is_positive_safe_integer(options.interval_seconds) {
            return Err("intervalSeconds must be a positive integer".to_string());
        }
        let interval_seconds = options.interval_seconds;

        let now: MonitorClock = options
            .now
            .take()
            .unwrap_or_else(|| Arc::new(monotonic_now_ms));
        let reader: Arc<dyn CgroupSnapshotSource> = match options.cgroup_reader.take() {
            Some(reader) => reader,
            None => {
                // 舊版：`new CgroupV2Reader({ now: this.now })`。
                Arc::new(CgroupV2Reader::new(CgroupV2ReaderOptions {
                    now: Some(Arc::clone(&now)),
                    ..CgroupV2ReaderOptions::default()
                }))
            }
        };
        let process_metrics: Arc<dyn ProcessMetricsSource> = options
            .process_metrics
            .take()
            .unwrap_or_else(|| Arc::new(DefaultProcessMetrics));

        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        Ok(Self(Arc::new(Inner {
            browser: options.browser,
            sessions: options.sessions,
            logger: options.logger,
            interval_seconds,
            resource_guard: options.resource_guard,
            reader,
            process_metrics,
            now,
            on_container_restart_requested: options.on_container_restart_requested,
            state: Mutex::new(MonitorState {
                stopped: true,
                policy: None,
                cgroup_available: None,
                cgroup_unavailable_logged: false,
                thresholds_clamped_logged: false,
                recycle_epoch: 0,
                recycle_flight: None,
                last_telemetry_at_ms: 0,
                latest_resource_limits: RuntimeResourceLimitsSnapshot {
                    memory_max_bytes: DiagnosticLimitValue::Null,
                    pids_max: DiagnosticLimitValue::Null,
                },
                guard_state: GuardState::Disabled,
            }),
            task: Mutex::new(None),
            stop_tx,
            stop_rx,
        })))
    }

    /// 對應舊版 `start()`。
    pub(crate) async fn start(&self) {
        // 舊版：`if (!this.stopped && this.timer !== undefined) return;`
        {
            let state = self.0.state.lock().await;
            if !state.stopped && self.0.task.lock().await.is_some() {
                return;
            }
        }
        let mut state = self.0.state.lock().await;
        state.stopped = false;

        let guard = self.0.resource_guard.as_ref().filter(|guard| guard.enabled);
        if let Some(guard) = guard {
            state.policy = Some(ResourceGuardPolicy::new(ResourceGuardPolicyOptions {
                config: to_policy_config(guard),
                now: Some(guard_clock_from_shared(&self.0.now)),
            }));
            state.guard_state = GuardState::Idle;
            // 舊版：logger.info('resource_guard_enabled', { …effective,
            // sampleIntervalSeconds, scaleWithStreams })。
            let mut fields = serialize_effective(&guard.effective);
            fields.push((
                "sampleIntervalSeconds".to_string(),
                SanitizedValue::Number(guard.sample_interval_seconds),
            ));
            fields.push((
                "scaleWithStreams".to_string(),
                SanitizedValue::Bool(guard.scale_with_streams),
            ));
            self.0.logger.info(
                "resource_guard_enabled",
                Some(&SanitizedValue::Object(fields)),
            );
        } else {
            state.policy = None;
            state.guard_state = GuardState::Disabled;
        }
        drop(state);

        run_sample(&self.0, true).await;
        // 舊版 scheduleNextSample 的 stopped 檢查：初始取樣期間被 stop 時不排程。
        if self.0.state.lock().await.stopped {
            return;
        }
        let inner = Arc::clone(&self.0);
        let handle = tokio::spawn(async move { run_loop(inner).await });
        *self.0.task.lock().await = Some(handle);
    }

    /// 對應舊版 `stop()`：停止排程並等工作迴圈（含進行中的取樣）與
    /// recycle flight 收尾（錯誤忽略）。
    pub(crate) async fn stop(&self) {
        {
            let mut state = self.0.state.lock().await;
            state.stopped = true;
        }
        let _ = self.0.stop_tx.send(true);
        let task = self.0.task.lock().await.take();
        if let Some(task) = task {
            let _ = task.await;
        }
        let recycle = self.0.state.lock().await.recycle_flight.take();
        if let Some(recycle) = recycle {
            // 舊版：await recycleFlight（錯誤忽略）。
            let _ = recycle.await;
        }
    }

    /// 對應舊版 `notifyBrowserRestarted`：通知 guard 瀏覽器在 guard 自己的
    /// recycle 路徑之外被重啟（crash-loop recycle、斷線恢復等；舊版註解）。
    pub(crate) async fn notify_browser_restarted(&self) {
        let mut state = self.0.state.lock().await;
        if let Some(policy) = state.policy.as_mut() {
            policy.note_browser_restart();
        }
    }

    /// 對應舊版 `getLatestResourceLimits`。
    pub(crate) async fn get_latest_resource_limits(&self) -> RuntimeResourceLimitsSnapshot {
        self.0.state.lock().await.latest_resource_limits.clone()
    }
}

/// 對應舊版私有 `runSample`：取樣一次（單飛語意見模組文件）。
async fn run_sample(inner: &Arc<Inner>, force_telemetry: bool) {
    sample_once(inner, force_telemetry).await;
}

/// 對應舊版私有 `sampleOnce`。
async fn sample_once(inner: &Arc<Inner>, force_telemetry: bool) {
    let mut state = inner.state.lock().await;
    if state.stopped {
        return;
    }

    // 舊版：`options.forceTelemetry ||
    //   this.now() - this.lastTelemetryAtMs >= this.options.intervalSeconds * 1_000`。
    let now_ms = (inner.now)();
    let should_log_telemetry = force_telemetry
        || (now_ms as f64) - (state.last_telemetry_at_ms as f64) >= inner.interval_seconds * 1000.0;

    let guard_enabled = inner
        .resource_guard
        .as_ref()
        .is_some_and(|guard| guard.enabled);
    // 舊版：`this.resourceGuard?.enabled === true || this.cgroupAvailable !== false`。
    let mut cgroup = None;
    if guard_enabled || state.cgroup_available != Some(false) {
        cgroup = inner
            .read_cgroup_safely(&mut state, should_log_telemetry)
            .await;
    }

    // 舊版：`cgroup !== undefined && this.policy !== undefined &&
    //   !this.thresholdsClampedLogged`。
    if let Some(snapshot) = cgroup.filter(|_| !state.thresholds_clamped_logged) {
        inner.maybe_clamp_thresholds(&mut state, snapshot);
    }

    // 舊版：evaluate（recycle 進行中則 evaluateEmergencyOnly）→ applyDecision。
    if let Some(snapshot) = cgroup {
        let recycle_in_flight = state.recycle_flight.is_some();
        let decision = match state.policy.as_mut() {
            Some(policy) if !recycle_in_flight => Some(policy.evaluate(&snapshot)),
            Some(policy) => Some(policy.evaluate_emergency_only(&snapshot)),
            None => None,
        };
        if let Some(decision) = decision {
            apply_decision(inner, &mut state, decision, snapshot).await;
        }
    }

    // 舊版：`cgroup !== undefined && this.policy?.takeMemoryHighEvent() === true`。
    if let Some(snapshot) = cgroup {
        let high_event = state
            .policy
            .as_mut()
            .is_some_and(|policy| policy.take_memory_high_event());
        if high_event {
            inner.logger.warn(
                "resource_guard_memory_high_event",
                Some(&SanitizedValue::Object(vec![
                    (
                        "memoryCurrentBytes".to_string(),
                        byte_value(snapshot.memory_current_bytes),
                    ),
                    (
                        "cgroupMemoryEventsHigh".to_string(),
                        byte_value(snapshot.events.high),
                    ),
                ])),
            );
        }
    }

    if should_log_telemetry {
        state.latest_resource_limits = RuntimeResourceLimitsSnapshot {
            memory_max_bytes: to_diagnostic_limit(
                cgroup.and_then(|snapshot| snapshot.memory_max_bytes),
            ),
            pids_max: to_diagnostic_limit(cgroup.and_then(|snapshot| snapshot.pids_max)),
        };
        inner.record_snapshot(&state, cgroup);
        state.last_telemetry_at_ms = (inner.now)();
    }
}

/// 對應舊版私有 `applyDecision`（recycle 以工作迴圈外派，對應舊版
/// startBrowserRecycle 的 fire-and-forget）。
async fn apply_decision(
    inner: &Arc<Inner>,
    state: &mut MonitorState,
    decision: ResourceGuardDecision,
    snapshot: CgroupSnapshot,
) {
    match decision {
        ResourceGuardDecision::None => {
            if matches!(
                state.guard_state,
                GuardState::Warning | GuardState::Observing
            ) {
                state.guard_state = match state.policy.as_ref() {
                    Some(policy) if policy.is_recycle_observation_active() => GuardState::Observing,
                    Some(policy) if policy.is_warning_latched() => GuardState::Warning,
                    _ => GuardState::Idle,
                };
            }
        }
        ResourceGuardDecision::Warn { reason } => {
            state.guard_state = GuardState::Warning;
            // 舊版：`this.policy?.getConfig().effective ?? this.resourceGuard?.effective`。
            let effective = state
                .policy
                .as_ref()
                .map(|policy| policy.get_config().effective.clone())
                .or_else(|| {
                    inner
                        .resource_guard
                        .as_ref()
                        .map(|guard| guard.effective.clone())
                });
            let mut fields = vec![
                (
                    "reason".to_string(),
                    SanitizedValue::String(reason.to_string()),
                ),
                (
                    "memoryCurrentBytes".to_string(),
                    byte_value(snapshot.memory_current_bytes),
                ),
                (
                    "swapCurrentBytes".to_string(),
                    byte_option(snapshot.swap_current_bytes),
                ),
            ];
            if let Some(effective) = effective {
                fields.extend(serialize_effective(&effective));
            }
            inner.logger.warn(
                "resource_guard_warning",
                Some(&SanitizedValue::Object(fields)),
            );
        }
        ResourceGuardDecision::RecycleBrowser { reason } => {
            spawn_browser_recycle(inner, state, reason, snapshot);
        }
        ResourceGuardDecision::RestartContainer { reason } => {
            let outcome = inner.request_container_restart(state, reason, snapshot, Vec::new());
            // 舊版：await handler；失敗記錄（此處的 await 只持有 Send 的
            // boxed future，不持有 SanitizedValue）。
            if let Some(outcome) = outcome
                && let Err(message) = outcome.await
            {
                inner.logger.error(
                    "resource_guard_container_restart_handler_failed",
                    Some(&SanitizedValue::Object(vec![
                        (
                            "reason".to_string(),
                            SanitizedValue::String(reason.to_string()),
                        ),
                        ("error".to_string(), SanitizedValue::String(message)),
                    ])),
                );
            }
        }
    }
}

/// 對應舊版私有 `startBrowserRecycle`：單飛的外派（flight 槽位與 epoch 代號）。
fn spawn_browser_recycle(
    inner: &Arc<Inner>,
    state: &mut MonitorState,
    reason: &'static str,
    snapshot: CgroupSnapshot,
) {
    if state.recycle_flight.is_some() {
        return;
    }
    state.recycle_epoch += 1;
    let epoch = state.recycle_epoch;
    let worker = Arc::clone(inner);
    let handle = tokio::spawn(async move {
        recycle_browser(Arc::clone(&worker), reason, snapshot).await;
        // 舊版 finally：清掉自己的 flight（身分以 epoch 對照）。
        let mut state = worker.state.lock().await;
        if state.recycle_epoch == epoch {
            state.recycle_flight = None;
        }
    });
    state.recycle_flight = Some(handle);
}

/// 對應舊版私有 `recycleBrowser`。
async fn recycle_browser(inner: Arc<Inner>, reason: &'static str, snapshot: CgroupSnapshot) {
    {
        let mut state = inner.state.lock().await;
        state.guard_state = GuardState::Recycling;
    }
    let memory_before_bytes = snapshot.memory_current_bytes;
    inner.logger.warn(
        "resource_guard_browser_recycle_requested",
        Some(&SanitizedValue::Object(vec![
            (
                "reason".to_string(),
                SanitizedValue::String(reason.to_string()),
            ),
            (
                "memoryCurrentBytes".to_string(),
                byte_value(memory_before_bytes),
            ),
            (
                "swapCurrentBytes".to_string(),
                byte_option(snapshot.swap_current_bytes),
            ),
        ])),
    );

    match inner.browser.restart().await {
        Ok(()) => {
            let mut state = inner.state.lock().await;
            if state.guard_state != GuardState::Restarting {
                if let Some(policy) = state.policy.as_mut() {
                    policy.begin_recycle_observation(memory_before_bytes);
                }
                state.guard_state = GuardState::Observing;
            }
            inner.logger.info(
                "resource_guard_browser_recycle_completed",
                Some(&SanitizedValue::Object(vec![
                    (
                        "reason".to_string(),
                        SanitizedValue::String(reason.to_string()),
                    ),
                    (
                        "memoryBeforeBytes".to_string(),
                        byte_value(memory_before_bytes),
                    ),
                ])),
            );
        }
        Err(message) => {
            inner.logger.error(
                "resource_guard_browser_recycle_failed",
                Some(&SanitizedValue::Object(vec![
                    (
                        "reason".to_string(),
                        SanitizedValue::String(reason.to_string()),
                    ),
                    ("error".to_string(), SanitizedValue::String(message)),
                ])),
            );
            let mut state = inner.state.lock().await;
            let outcome = inner.request_container_restart(
                &mut state,
                "browser_recycle_failed",
                snapshot,
                vec![(
                    "recycleReason".to_string(),
                    SanitizedValue::String(reason.to_string()),
                )],
            );
            if let Some(outcome) = outcome
                && let Err(handler_message) = outcome.await
            {
                inner.logger.error(
                    "resource_guard_container_restart_handler_failed",
                    Some(&SanitizedValue::Object(vec![
                        (
                            "reason".to_string(),
                            SanitizedValue::String("browser_recycle_failed".to_string()),
                        ),
                        ("error".to_string(), SanitizedValue::String(handler_message)),
                    ])),
                );
            }
        }
    }
}

impl Inner {
    /// 對應舊版私有 `readCgroupSafely`。
    async fn read_cgroup_safely(
        &self,
        state: &mut MonitorState,
        full_snapshot: bool,
    ) -> Option<CgroupSnapshot> {
        if state.cgroup_available.is_none() {
            match self.reader.probe().await {
                CgroupReaderAvailability::Available { .. } => {
                    state.cgroup_available = Some(true);
                }
                CgroupReaderAvailability::Unavailable { reason } => {
                    state.cgroup_available = Some(false);
                    self.log_cgroup_unavailable(state, &reason);
                    return None;
                }
            }
        }
        if state.cgroup_available == Some(false) {
            return None;
        }

        let outcome = if full_snapshot {
            self.reader.read_snapshot().await
        } else {
            self.reader.read_policy_snapshot().await
        };
        match outcome {
            Ok(snapshot) => Some(snapshot),
            Err(error) => {
                if state.cgroup_available == Some(true) {
                    // 舊版：debug 'cgroup_sample_failed'。
                    self.logger.debug(
                        "cgroup_sample_failed",
                        Some(&SanitizedValue::Object(vec![(
                            "error".to_string(),
                            SanitizedValue::String(error.to_string()),
                        )])),
                    );
                    None
                } else {
                    state.cgroup_available = Some(false);
                    self.log_cgroup_unavailable(state, &error.to_string());
                    None
                }
            }
        }
    }

    /// 對應舊版私有 `logCgroupUnavailable`（只記錄一次）。
    fn log_cgroup_unavailable(&self, state: &mut MonitorState, reason: &str) {
        if state.cgroup_unavailable_logged {
            return;
        }
        state.cgroup_unavailable_logged = true;
        if self
            .resource_guard
            .as_ref()
            .is_some_and(|guard| guard.enabled)
        {
            state.guard_state = GuardState::Unavailable;
        }
        self.logger.warn(
            "cgroup_metrics_unavailable",
            Some(&SanitizedValue::Object(vec![(
                "reason".to_string(),
                SanitizedValue::String(reason.to_string()),
            )])),
        );
    }

    /// 對應舊版私有 `maybeClampThresholds`：把門檻壓在 cgroup memory.max 之下
    /// （只在首次取樣時評估，與舊版相同）。
    fn maybe_clamp_thresholds(&self, state: &mut MonitorState, snapshot: CgroupSnapshot) {
        // 舊版：policy === undefined || resourceGuard === undefined → return。
        if state.policy.is_none() || self.resource_guard.is_none() {
            return;
        }
        let Some(effective) = state
            .policy
            .as_ref()
            .map(|policy| policy.get_config().effective.clone())
        else {
            return;
        };
        let clamp = clamp_effective_thresholds_to_memory_max(
            &effective,
            snapshot.memory_max_bytes.map(u128::from),
        );
        state.thresholds_clamped_logged = true;
        if !clamp.clamped {
            return;
        }
        if let Some(policy) = state.policy.as_mut() {
            let mut config = policy.get_config().clone();
            config.effective = clamp.effective.clone();
            policy.update_config(config);
        }
        // 舊版：logger.warn('resource_guard_limit_clamped', { reason,
        // memoryMaxBytes, …effective })。
        let mut fields = vec![
            (
                "reason".to_string(),
                SanitizedValue::String("memory_max_below_scaled_emergency".to_string()),
            ),
            (
                "memoryMaxBytes".to_string(),
                byte_option(snapshot.memory_max_bytes),
            ),
        ];
        fields.extend(serialize_effective(&clamp.effective));
        self.logger.warn(
            "resource_guard_limit_clamped",
            Some(&SanitizedValue::Object(fields)),
        );
    }

    /// 對應舊版私有 `requestContainerRestart` 的同步段落：狀態與日誌；回傳
    /// handler 的未來（`None` 表示已在重啟中或無 handler）。await 與
    /// `SanitizedValue` 分離，讓外派的 recycle worker 保持 Send。
    fn request_container_restart(
        &self,
        state: &mut MonitorState,
        reason: &'static str,
        snapshot: CgroupSnapshot,
        extra_fields: Vec<(String, SanitizedValue)>,
    ) -> Option<super::restart::NotifyOutcomeFuture> {
        if state.guard_state == GuardState::Restarting {
            return None;
        }
        state.guard_state = GuardState::Restarting;
        let mut fields = vec![
            (
                "reason".to_string(),
                SanitizedValue::String(reason.to_string()),
            ),
            (
                "source".to_string(),
                SanitizedValue::String("resource_guard".to_string()),
            ),
            (
                "memoryCurrentBytes".to_string(),
                byte_value(snapshot.memory_current_bytes),
            ),
            (
                "swapCurrentBytes".to_string(),
                byte_option(snapshot.swap_current_bytes),
            ),
        ];
        fields.extend(extra_fields);
        self.logger.error(
            "resource_guard_container_restart_requested",
            Some(&SanitizedValue::Object(fields.clone())),
        );

        let handler = self.on_container_restart_requested.clone()?;
        // 欄位與 request 都移進 handler 的未來；request 欄位已封閉為 Send 的
        // [`ControllerFieldValue`]（生產端只產字串／數字／null；非純量照
        // 舊版 `sanitizeFields` 落入捨棄形，輸出可觀察的行為相同）。
        Some((handler)(ContainerRestartRequest {
            reason: reason.to_string(),
            source: "resource_guard".to_string(),
            fields: Some(restart_fields_from_log(&fields)),
        }))
    }
    /// 對應舊版私有 `recordSnapshot`：Node 的 process 指標由
    /// [`ProcessMetricsSource`] 提供（heapUsed／external 為 null，X5）。
    fn record_snapshot(&self, state: &MonitorState, cgroup: Option<CgroupSnapshot>) {
        let metrics = self.process_metrics.snapshot();
        let cpu = cgroup.and_then(|snapshot| snapshot.cpu);
        let events = cgroup.map(|snapshot| snapshot.events);

        let fields = vec![
            (
                "processCpuUserUs".to_string(),
                optional_number(metrics.cpu_user_us),
            ),
            (
                "processCpuSystemUs".to_string(),
                optional_number(metrics.cpu_system_us),
            ),
            (
                "processRssBytes".to_string(),
                optional_number(metrics.rss_bytes.map(|bytes| bytes as f64)),
            ),
            (
                "processHeapUsedBytes".to_string(),
                optional_number(metrics.heap_used_bytes.map(|bytes| bytes as f64)),
            ),
            (
                "processExternalBytes".to_string(),
                optional_number(metrics.external_bytes.map(|bytes| bytes as f64)),
            ),
            (
                "processMaxRssKb".to_string(),
                optional_number(metrics.max_rss_kb),
            ),
            (
                "activeChannelCount".to_string(),
                SanitizedValue::Number(self.sessions.get_active_channels().len() as f64),
            ),
            (
                "browserPageCount".to_string(),
                SanitizedValue::Number(self.browser.get_page_count() as f64),
            ),
            (
                "cgroupMemoryCurrentBytes".to_string(),
                byte_option(cgroup.map(|snapshot| snapshot.memory_current_bytes)),
            ),
            (
                "cgroupMemoryMaxBytes".to_string(),
                byte_option(cgroup.and_then(|snapshot| snapshot.memory_max_bytes)),
            ),
            (
                "cgroupMemoryPeakBytes".to_string(),
                byte_option(cgroup.and_then(|snapshot| snapshot.memory_peak_bytes)),
            ),
            (
                "cgroupSwapCurrentBytes".to_string(),
                byte_option(cgroup.and_then(|snapshot| snapshot.swap_current_bytes)),
            ),
            (
                "cgroupPidsCurrent".to_string(),
                byte_option(cgroup.and_then(|snapshot| snapshot.pids_current)),
            ),
            (
                "cgroupPidsMax".to_string(),
                byte_option(cgroup.and_then(|snapshot| snapshot.pids_max)),
            ),
            (
                "cgroupCpuUsageUsec".to_string(),
                byte_option(cpu.map(|stat| stat.usage_usec)),
            ),
            (
                "cgroupCpuUserUsec".to_string(),
                byte_option(cpu.and_then(|stat| stat.user_usec)),
            ),
            (
                "cgroupCpuSystemUsec".to_string(),
                byte_option(cpu.and_then(|stat| stat.system_usec)),
            ),
            (
                "cgroupMemoryEventsHigh".to_string(),
                byte_option(events.map(|events| events.high)),
            ),
            (
                "cgroupMemoryEventsMax".to_string(),
                byte_option(events.map(|events| events.max)),
            ),
            (
                "cgroupMemoryEventsOom".to_string(),
                byte_option(events.map(|events| events.oom)),
            ),
            (
                "cgroupMemoryEventsOomKill".to_string(),
                byte_option(events.map(|events| events.oom_kill)),
            ),
            (
                "resourceGuardState".to_string(),
                SanitizedValue::String(state.guard_state.as_str().to_string()),
            ),
            (
                "browserRecycleInFlight".to_string(),
                SanitizedValue::Bool(state.recycle_flight.is_some()),
            ),
        ];
        self.logger.info(
            "runtime_resource_snapshot",
            Some(&SanitizedValue::Object(fields)),
        );
    }
}

/// 舊版工作迴圈（scheduleNextSample 的 setTimeout 鏈）：睡到間隔後取樣，
/// stop 訊號會先於 sleep 到期被檢查。
async fn run_loop(inner: Arc<Inner>) {
    // watch::Receiver::changed() 需要 &mut；本迴圈獨佔一份 clone。
    let mut stop_rx = inner.stop_rx.clone();
    loop {
        // 舊版：guard 啟用時用 sampleIntervalSeconds，否則 intervalSeconds。
        let interval_seconds = match &inner.resource_guard {
            Some(guard) if guard.enabled => guard.sample_interval_seconds,
            _ => inner.interval_seconds,
        };
        // 舊版：`Math.min(2_147_483_647, Math.max(1, intervalSeconds) * 1_000)`。
        let delay_ms = ((interval_seconds.max(1.0) * 1000.0).min(2_147_483_647.0)) as u64;
        tokio::select! {
            biased;
            _ = stop_rx.changed() => {}
            _ = tokio::time::sleep(Duration::from_millis(delay_ms)) => {}
        }
        if *stop_rx.borrow() {
            return;
        }
        run_sample(&inner, false).await;
    }
}

/// 舊版 `toPolicyConfig`：欄位同名取值（對應舊版的物件字面值）。
fn to_policy_config(guard: &ResourceGuardConfig) -> ResourceGuardPolicyConfig {
    ResourceGuardPolicyConfig::from(guard)
}

/// 舊版 `serializeEffective`：effective 門檻的日誌欄位（照舊版順序）。
fn serialize_effective(
    effective: &ResourceGuardEffectiveThresholds,
) -> Vec<(String, SanitizedValue)> {
    vec![
        (
            "maxConcurrentStreams".to_string(),
            SanitizedValue::Number(effective.max_concurrent_streams),
        ),
        (
            "warningMemoryMib".to_string(),
            SanitizedValue::Number(effective.warning_memory_mib),
        ),
        (
            "warningResetMemoryMib".to_string(),
            SanitizedValue::Number(effective.warning_reset_memory_mib),
        ),
        (
            "browserRecycleMemoryMib".to_string(),
            SanitizedValue::Number(effective.browser_recycle_memory_mib),
        ),
        (
            "emergencyMemoryMib".to_string(),
            SanitizedValue::Number(effective.emergency_memory_mib),
        ),
        (
            "postRecycleTargetMemoryMib".to_string(),
            SanitizedValue::Number(effective.post_recycle_target_memory_mib),
        ),
    ]
}

/// 舊版 `serializableByteCount(n)`：一定有值（u64 位元組數）。
fn byte_value(bytes: u64) -> SanitizedValue {
    serializable_byte_count(Some(u128::from(bytes))).unwrap_or(SanitizedValue::Null)
}

/// 舊版 `serializableByteCount(x) ?? null`。
fn byte_option(bytes: Option<u64>) -> SanitizedValue {
    serializable_byte_count(bytes.map(u128::from)).unwrap_or(SanitizedValue::Null)
}

/// 舊版 `值 ?? null` 的純 number 欄位。
fn optional_number(value: Option<f64>) -> SanitizedValue {
    value.map_or(SanitizedValue::Null, SanitizedValue::Number)
}

/// 把位元組數轉成 [`RuntimeResourceLimitsSnapshot`] 的欄位值
/// （`serializableByteCount(x) ?? null`）。
fn to_diagnostic_limit(bytes: Option<u64>) -> DiagnosticLimitValue {
    match serializable_byte_count(bytes.map(u128::from)) {
        Some(SanitizedValue::Number(value)) => DiagnosticLimitValue::Number(value),
        Some(SanitizedValue::String(value)) => DiagnosticLimitValue::Text(value),
        _ => DiagnosticLimitValue::Null,
    }
}

/// 把共用的 Arc 時鐘包成 GuardClock（policy options 需要一次性所有權的 Box）。
fn guard_clock_from_shared(clock: &MonitorClock) -> GuardClock {
    let clock = Arc::clone(clock);
    Box::new(move || clock())
}

/// 舊版 `Number.isSafeInteger(value) && value > 0`。
fn is_positive_safe_integer(value: f64) -> bool {
    value.is_finite() && value > 0.0 && value.fract() == 0.0 && value <= MAX_SAFE_INTEGER as f64
}

/// 舊版預設 `performance.now()`：單調毫秒（與 `resource_guard.rs`、
/// `cgroup.rs` 各有一份程序層級起點，互不影響）。
fn monotonic_now_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

#[cfg(test)]
mod tests;

/// request 欄位與記點欄位同值（舊版同一物件）：純量值直接轉
/// [`ControllerFieldValue`]（本檔的生產端只產字串／數字／null）。
fn restart_fields_from_log(
    fields: &[(String, SanitizedValue)],
) -> Vec<(String, ControllerFieldValue)> {
    fields
        .iter()
        .map(|(name, value)| (name.clone(), ControllerFieldValue::from_sanitized(value)))
        .collect()
}
