//! RuntimeResourceMonitor 的單元測試：移植舊版
//! `test/unit/runtime-resource-monitor.test.ts` 的 7 個測試。計時以 tokio
//! paused time 驅動（對應舊版 fake timers；`advance` 後以 `pump` 讓工作迴圈
//! 被排程執行）。
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use super::{
    MonitorBrowser, MonitorClock, MonitorLogger, MonitorSessions, ProcessMetricsSnapshot,
    ProcessMetricsSource, RuntimeResourceMonitor, RuntimeResourceMonitorOptions,
};
use crate::app::cgroup::{
    CgroupCpuStat, CgroupMemoryEvents, CgroupReaderAvailability, CgroupSnapshot,
    CgroupSnapshotSource, CgroupV2Error,
};
use crate::app::diagnostics::{DiagnosticLimitValue, RuntimeResourceLimitsSnapshot};
use crate::app::restart::{
    ContainerRestartNotifyObserver, ContainerRestartRequest, NotifyOutcomeFuture,
};
use crate::config::model::ResourceGuardConfig;
use crate::config::thresholds::{
    ResourceGuardAnchorThresholds, compute_effective_resource_guard_thresholds, mib_to_bytes,
};
use crate::logging::logger::LogLevel;
use crate::logging::redact::SanitizedValue;

/// 測試用的欄位值快照：`SanitizedValue` 的可 Send 子集（`Shared`／錯誤物件
/// 不會出現在這些測試的欄位中；記錄時就地轉換，讓假 logger 可以是
/// `Send + Sync`）。
#[derive(Clone, Debug, PartialEq)]
enum TestField {
    Null,
    Undefined,
    Bool(bool),
    Number(f64),
    Text(String),
    Other,
}

fn capture(value: &SanitizedValue) -> TestField {
    match value {
        SanitizedValue::Null => TestField::Null,
        SanitizedValue::Undefined => TestField::Undefined,
        SanitizedValue::Bool(value) => TestField::Bool(*value),
        SanitizedValue::Number(value) => TestField::Number(*value),
        SanitizedValue::String(value) => TestField::Text(value.clone()),
        _ => TestField::Other,
    }
}

/// 對應舊測試 helper `createLogger()`：收集四個層級的呼叫。
/// 測試記錄的單一事件欄位（level, event, fields）用別名。
type RecordedEntry = (LogLevel, String, Vec<(String, TestField)>);

#[derive(Default)]
struct RecordingLogger {
    entries: std::sync::Mutex<Vec<RecordedEntry>>,
}

fn fields_of(value: Option<&SanitizedValue>) -> Vec<(String, TestField)> {
    match value {
        Some(SanitizedValue::Object(entries)) => entries
            .iter()
            .map(|(key, value)| (key.clone(), capture(value)))
            .collect(),
        Some(other) => panic!("預期欄位是物件：{other:?}"),
        None => Vec::new(),
    }
}

impl MonitorLogger for RecordingLogger {
    fn debug(&self, event: &str, fields: Option<&SanitizedValue>) {
        self.entries
            .lock()
            .unwrap()
            .push((LogLevel::Debug, event.to_string(), fields_of(fields)));
    }

    fn info(&self, event: &str, fields: Option<&SanitizedValue>) {
        self.entries
            .lock()
            .unwrap()
            .push((LogLevel::Info, event.to_string(), fields_of(fields)));
    }

    fn warn(&self, event: &str, fields: Option<&SanitizedValue>) {
        self.entries
            .lock()
            .unwrap()
            .push((LogLevel::Warn, event.to_string(), fields_of(fields)));
    }

    fn error(&self, event: &str, fields: Option<&SanitizedValue>) {
        self.entries
            .lock()
            .unwrap()
            .push((LogLevel::Error, event.to_string(), fields_of(fields)));
    }
}

/// 把具體的記錄 logger 轉成 trait 物件（欄位需要 `Arc<dyn MonitorLogger>`）。
fn recording_logger(logger: &Arc<RecordingLogger>) -> Arc<dyn MonitorLogger> {
    let concrete: Arc<RecordingLogger> = Arc::clone(logger);
    concrete
}

impl RecordingLogger {
    fn calls(&self, level: LogLevel, event: &str) -> Vec<Vec<(String, TestField)>> {
        self.entries
            .lock()
            .unwrap()
            .iter()
            .filter(|(entry_level, entry_event, _)| *entry_level == level && entry_event == event)
            .map(|(_, _, fields)| fields.clone())
            .collect()
    }

    fn contains_field(&self, level: LogLevel, event: &str, key: &str, value: &TestField) -> bool {
        self.calls(level, event).iter().any(|fields| {
            fields
                .iter()
                .any(|(name, entry)| name == key && entry == value)
        })
    }
}

fn shared_clock() -> (Arc<AtomicU64>, MonitorClock) {
    let now_ms = Arc::new(AtomicU64::new(0));
    let clock = Arc::clone(&now_ms);
    (now_ms, Arc::new(move || clock.load(Ordering::SeqCst)))
}

/// 對應舊測試 helper `deferred()`：可手動解除的閘門（restart 永不回傳直到
/// resolve）。
struct RestartGate {
    tx: tokio::sync::watch::Sender<bool>,
    rx: tokio::sync::watch::Receiver<bool>,
}

impl RestartGate {
    fn new() -> Self {
        let (tx, rx) = tokio::sync::watch::channel(false);
        Self { tx, rx }
    }
}

async fn wait_for_gate(rx: &tokio::sync::watch::Receiver<bool>) {
    let mut rx = rx.clone();
    loop {
        if *rx.borrow() {
            return;
        }
        if rx.changed().await.is_err() {
            return;
        }
    }
}

/// 對應舊測試的 `browserManager` 假物件：頁數固定；restart 記錄次數，可選
/// 閘門（阻塞直到 resolve，對應舊測試的 deferred）。
struct FakeBrowser {
    page_count: u64,
    restarts: Arc<AtomicUsize>,
    gate_rx: Option<tokio::sync::watch::Receiver<bool>>,
}

impl FakeBrowser {
    fn new(page_count: u64) -> (Self, Arc<AtomicUsize>) {
        Self::with_gate(page_count, None)
    }

    fn with_gate(
        page_count: u64,
        gate_rx: Option<tokio::sync::watch::Receiver<bool>>,
    ) -> (Self, Arc<AtomicUsize>) {
        let restarts = Arc::new(AtomicUsize::new(0));
        (
            Self {
                page_count,
                restarts: Arc::clone(&restarts),
                gate_rx,
            },
            restarts,
        )
    }
}

#[async_trait::async_trait]
impl MonitorBrowser for FakeBrowser {
    fn get_page_count(&self) -> u64 {
        self.page_count
    }

    async fn restart(&self) -> Result<(), String> {
        self.restarts.fetch_add(1, Ordering::SeqCst);
        if let Some(gate_rx) = self.gate_rx.as_ref() {
            wait_for_gate(gate_rx).await;
        }
        Ok(())
    }
}

/// 對應舊測試的 `sessionManager: { getActiveChannels }`。
struct FakeSessions {
    channels: Vec<&'static str>,
}

impl MonitorSessions for FakeSessions {
    fn get_active_channels(&self) -> Vec<String> {
        self.channels
            .iter()
            .map(|channel| channel.to_string())
            .collect()
    }
}

/// 讀取佇列：取出時照舊測試 `samples[Math.min(sampleIndex, length - 1)]` 的
/// 語意——佇列空了就重複最後一個。
struct ReadQueue {
    items: VecDeque<CgroupSnapshot>,
    last: Option<CgroupSnapshot>,
    calls: usize,
}

impl ReadQueue {
    fn new(items: Vec<CgroupSnapshot>) -> Self {
        Self {
            items: items.into(),
            last: None,
            calls: 0,
        }
    }

    fn next(&mut self) -> CgroupSnapshot {
        self.calls += 1;
        if let Some(value) = self.items.pop_front() {
            self.last = Some(value);
        }
        self.last.unwrap_or_else(|| panic!("該讀法不應被呼叫"))
    }
}

/// 對應舊測試的 cgroupReader 假物件：probe 固定。`policy_queue` 為 `None`
/// 時兩種讀法共用同一個遞增序列（對應舊測試把 readSnapshot 與
/// readPolicySnapshot 接到同一個 readNextSnapshot）；`Some` 時各自有佇列
/// （對應舊測試 5 的分開兩個假函式）。
struct FakeReader {
    probe: CgroupReaderAvailability,
    queue: std::sync::Mutex<ReadQueue>,
    policy_queue: Option<std::sync::Mutex<ReadQueue>>,
}

impl FakeReader {
    fn unavailable(reason: &str) -> Self {
        Self {
            probe: CgroupReaderAvailability::Unavailable {
                reason: reason.to_string(),
            },
            queue: std::sync::Mutex::new(ReadQueue::new(Vec::new())),
            policy_queue: None,
        }
    }

    /// 舊測試 2／3／4：兩種讀法共用同一個序列。
    fn available_shared(items: Vec<CgroupSnapshot>) -> Self {
        Self {
            probe: CgroupReaderAvailability::Available {
                root_path: "/sys/fs/cgroup".to_string(),
            },
            queue: std::sync::Mutex::new(ReadQueue::new(items)),
            policy_queue: None,
        }
    }

    /// 舊測試 5／6／7：full 與 policy 各自有佇列（policy 未指定時為空，
    /// 被呼叫即 panic，對應舊測試的「should not read」）。
    fn available(full: Vec<CgroupSnapshot>, policy: Vec<CgroupSnapshot>) -> Self {
        Self {
            probe: CgroupReaderAvailability::Available {
                root_path: "/sys/fs/cgroup".to_string(),
            },
            queue: std::sync::Mutex::new(ReadQueue::new(full)),
            policy_queue: Some(std::sync::Mutex::new(ReadQueue::new(policy))),
        }
    }

    fn full_calls(&self) -> usize {
        self.queue.lock().unwrap().calls
    }

    fn policy_calls(&self) -> usize {
        self.policy_queue
            .as_ref()
            .map_or(self.queue.lock().unwrap().calls, |queue| {
                queue.lock().unwrap().calls
            })
    }
}

#[async_trait::async_trait]
impl CgroupSnapshotSource for FakeReader {
    async fn probe(&self) -> CgroupReaderAvailability {
        self.probe.clone()
    }

    async fn read_snapshot(&self) -> Result<CgroupSnapshot, CgroupV2Error> {
        Ok(self.queue.lock().unwrap().next())
    }

    async fn read_policy_snapshot(&self) -> Result<CgroupSnapshot, CgroupV2Error> {
        let queue = self.policy_queue.as_ref().unwrap_or(&self.queue);
        Ok(queue.lock().unwrap().next())
    }
}

/// 推進 paused time 後讓監視器的取樣完成（對應舊版
/// `vi.advanceTimersByTimeAsync` 的微任務排空）。
async fn pump() {
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
}

fn bytes(mib: f64) -> u64 {
    mib_to_bytes(mib) as u64
}

/// 舊測試的取樣形狀：只有取樣時間、記憶體與事件計數。
fn snapshot_at_memory(at_ms: u64, memory_mib: f64) -> CgroupSnapshot {
    CgroupSnapshot {
        sampled_at_monotonic_ms: at_ms,
        memory_current_bytes: bytes(memory_mib),
        memory_max_bytes: None,
        memory_peak_bytes: None,
        swap_current_bytes: None,
        pids_current: None,
        pids_max: None,
        events: CgroupMemoryEvents::default(),
        cpu: None,
    }
}

/// 舊測試 4 的第三個取樣：oom 事件為 1。
fn snapshot_with_oom(at_ms: u64, memory_mib: f64) -> CgroupSnapshot {
    CgroupSnapshot {
        events: CgroupMemoryEvents {
            oom: 1,
            ..CgroupMemoryEvents::default()
        },
        ..snapshot_at_memory(at_ms, memory_mib)
    }
}

/// 舊測試 5 的 full snapshot（memory.max 8GiB、peak 1.5GiB、pids、CPU）。
fn full_snapshot_with_limits() -> CgroupSnapshot {
    CgroupSnapshot {
        memory_max_bytes: Some(bytes(8_000.0)),
        memory_peak_bytes: Some(bytes(1_500.0)),
        pids_current: Some(20),
        pids_max: Some(512),
        cpu: Some(CgroupCpuStat {
            usage_usec: 100,
            user_usec: None,
            system_usec: None,
        }),
        swap_current_bytes: Some(0),
        ..snapshot_at_memory(0, 1_000.0)
    }
}

/// 舊測試 5 的 policy snapshot（1000MiB、swap 0）。
fn policy_snapshot() -> CgroupSnapshot {
    CgroupSnapshot {
        swap_current_bytes: Some(0),
        ..snapshot_at_memory(0, 1_000.0)
    }
}

/// 對應舊測試 helper `createDefaultResourceGuard(streams, overrides)`；本檔
/// 只用到 `enabled` 與 `scaleWithStreams` 兩個覆寫，其餘照舊 helper 的預設值。
fn create_default_resource_guard(
    max_concurrent_streams: f64,
    enabled: bool,
    scale_with_streams: bool,
) -> ResourceGuardConfig {
    let anchors = ResourceGuardAnchorThresholds {
        scale_with_streams,
        baseline_streams: 3.0,
        base_memory_mib: 512.0,
        warning_memory_mib: 4_096.0,
        warning_reset_memory_mib: 3_840.0,
        browser_recycle_memory_mib: 4_608.0,
        emergency_memory_mib: 5_376.0,
        post_recycle_target_memory_mib: 4_096.0,
    };
    ResourceGuardConfig {
        enabled,
        sample_interval_seconds: 2.0,
        startup_rate_grace_seconds: 120.0,
        scale_with_streams,
        baseline_streams: 3.0,
        base_memory_mib: 512.0,
        warning_memory_mib: 4_096.0,
        warning_reset_memory_mib: 3_840.0,
        browser_recycle_memory_mib: 4_608.0,
        browser_recycle_consecutive_samples: 2.0,
        emergency_memory_mib: 5_376.0,
        emergency_swap_mib: 768.0,
        fast_growth_mib: 1_024.0,
        fast_growth_window_seconds: 30.0,
        post_browser_restart_rate_grace_seconds: 120.0,
        post_recycle_observation_seconds: 20.0,
        post_recycle_target_memory_mib: 4_096.0,
        post_recycle_minimum_drop_mib: 512.0,
        effective: compute_effective_resource_guard_thresholds(&anchors, max_concurrent_streams),
    }
}

/// 舊測試 2／4 的取樣計畫：`now` 照讀取進度推進
/// （`now: () => samples[min(sampleIndex, len-1)].sampledAt…`）。
struct SamplePlan {
    samples: Arc<Vec<CgroupSnapshot>>,
    index: Arc<AtomicUsize>,
}

impl SamplePlan {
    fn new(samples: Vec<CgroupSnapshot>) -> (Self, Arc<AtomicUsize>) {
        let index = Arc::new(AtomicUsize::new(0));
        (
            Self {
                samples: Arc::new(samples),
                index: Arc::clone(&index),
            },
            index,
        )
    }

    fn clock(&self) -> MonitorClock {
        let samples = Arc::clone(&self.samples);
        let index = Arc::clone(&self.index);
        Arc::new(move || {
            let sample_index = index.load(Ordering::SeqCst).min(samples.len() - 1);
            samples[sample_index].sampled_at_monotonic_ms
        })
    }

    fn reader(&self) -> FakeReader {
        FakeReader::available_shared(self.samples.to_vec())
    }
}

fn create_monitor(options: RuntimeResourceMonitorOptions) -> RuntimeResourceMonitor {
    RuntimeResourceMonitor::new(options)
        .unwrap_or_else(|error| panic!("建構 monitor 應成功：{error}"))
}

// 原測試：啟動時立即記錄 process 欄位，停止後不再排程
#[tokio::test(start_paused = true)]
async fn records_process_fields_on_start_and_stops_scheduling() {
    let logger = Arc::new(RecordingLogger::default());
    let (browser, _restarts) = FakeBrowser::new(2);
    let reader = FakeReader::unavailable("test");
    let monitor = create_monitor(RuntimeResourceMonitorOptions {
        browser: Arc::new(browser),
        sessions: Arc::new(FakeSessions {
            channels: vec!["one", "two"],
        }),
        logger: recording_logger(&logger),
        interval_seconds: 10.0,
        resource_guard: Some(create_default_resource_guard(2.0, false, true)),
        cgroup_reader: Some(Arc::new(reader)),
        process_metrics: Some(Arc::new(FixedProcessMetrics)),
        now: None,
        on_container_restart_requested: None,
    });

    monitor.start().await;
    let snapshots = logger.calls(LogLevel::Info, "runtime_resource_snapshot");
    assert_eq!(snapshots.len(), 1);
    let fields = &snapshots[0];
    let field = |name: &str| {
        fields
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
            .unwrap_or_else(|| panic!("欄位 {name} 不存在"))
    };
    assert_eq!(field("activeChannelCount"), TestField::Number(2.0));
    assert_eq!(field("browserPageCount"), TestField::Number(2.0));
    // 舊版 expect.any(Number)：process 指標由注入的來源提供。
    assert!(matches!(field("processRssBytes"), TestField::Number(_)));
    assert_eq!(
        field("resourceGuardState"),
        TestField::Text("disabled".to_string()),
    );
    assert!(logger.contains_field(
        LogLevel::Warn,
        "cgroup_metrics_unavailable",
        "reason",
        &TestField::Text("test".to_string()),
    ));

    monitor.stop().await;
    tokio::time::advance(std::time::Duration::from_secs(20)).await;
    pump().await;
    assert_eq!(
        logger
            .calls(LogLevel::Info, "runtime_resource_snapshot")
            .len(),
        1,
    );
}

/// 固定 process 指標：讓「processRssBytes 為數字」在 macOS 也可驗證。
struct FixedProcessMetrics;

impl ProcessMetricsSource for FixedProcessMetrics {
    fn snapshot(&self) -> ProcessMetricsSnapshot {
        ProcessMetricsSnapshot {
            cpu_user_us: Some(12_345.0),
            cpu_system_us: Some(2_345.0),
            rss_bytes: Some(12_345_678),
            heap_used_bytes: None,
            external_bytes: None,
            max_rss_kb: Some(12_058.0),
        }
    }
}

// 原測試：cgroup 可用時依政策觸發 browser recycle
#[tokio::test(start_paused = true)]
async fn recycles_browser_according_to_policy_when_cgroup_available() {
    let logger = Arc::new(RecordingLogger::default());
    let (plan, _index) = SamplePlan::new(vec![
        snapshot_at_memory(0, 4_700.0),
        snapshot_at_memory(2_000, 4_700.0),
        snapshot_at_memory(4_000, 2_000.0),
    ]);
    let (browser, restarts) = FakeBrowser::new(1);
    let monitor = create_monitor(RuntimeResourceMonitorOptions {
        browser: Arc::new(browser),
        sessions: Arc::new(FakeSessions {
            channels: vec!["one"],
        }),
        logger: recording_logger(&logger),
        interval_seconds: 60.0,
        resource_guard: Some(create_default_resource_guard(3.0, true, false)),
        cgroup_reader: Some(Arc::new(plan.reader())),
        process_metrics: None,
        now: Some(plan.clock()),
        on_container_restart_requested: None,
    });

    monitor.start().await;
    pump().await;
    // First sample is high but only one consecutive → no recycle yet.（舊版註解）
    assert_eq!(restarts.load(Ordering::SeqCst), 0);

    tokio::time::advance(std::time::Duration::from_millis(2_000)).await;
    pump().await;
    assert_eq!(restarts.load(Ordering::SeqCst), 1);
    assert!(logger.contains_field(
        LogLevel::Warn,
        "resource_guard_browser_recycle_requested",
        "reason",
        &TestField::Text("sustained_high_memory".to_string()),
    ));

    monitor.stop().await;
}

// 原測試：warning latch 未降到 reset 門檻前 snapshot 維持 warning
#[tokio::test(start_paused = true)]
async fn warning_state_is_latched_until_reset_threshold() {
    let logger = Arc::new(RecordingLogger::default());
    let (now_ms, now) = shared_clock();
    let samples = vec![
        snapshot_at_memory(0, 4_100.0),
        snapshot_at_memory(2_000, 3_900.0),
        snapshot_at_memory(4_000, 3_800.0),
    ];
    let reader = FakeReader::available_shared(samples);
    let (browser, _restarts) = FakeBrowser::new(1);
    let monitor = create_monitor(RuntimeResourceMonitorOptions {
        browser: Arc::new(browser),
        sessions: Arc::new(FakeSessions {
            channels: vec!["one"],
        }),
        logger: recording_logger(&logger),
        interval_seconds: 2.0,
        resource_guard: Some(create_default_resource_guard(3.0, true, false)),
        cgroup_reader: Some(Arc::new(reader)),
        process_metrics: None,
        now: Some(now),
        on_container_restart_requested: None,
    });

    monitor.start().await;
    pump().await;
    now_ms.store(2_000, Ordering::SeqCst);
    tokio::time::advance(std::time::Duration::from_millis(2_000)).await;
    pump().await;
    now_ms.store(4_000, Ordering::SeqCst);
    tokio::time::advance(std::time::Duration::from_millis(2_000)).await;
    pump().await;

    let states: Vec<String> = logger
        .calls(LogLevel::Info, "runtime_resource_snapshot")
        .iter()
        .map(|fields| {
            fields
                .iter()
                .find(|(key, _)| key == "resourceGuardState")
                .map(|(_, value)| match value {
                    TestField::Text(state) => state.clone(),
                    other => panic!("預期字串：{other:?}"),
                })
                .unwrap_or_else(|| panic!("欄位 resourceGuardState 不存在"))
        })
        .collect();
    assert_eq!(states, vec!["warning", "warning", "idle"]);

    monitor.stop().await;
}

// 原測試：browser recycle 進行中仍會偵測 cgroup OOM 並要求重啟容器
#[tokio::test(start_paused = true)]
async fn detects_cgroup_oom_during_browser_recycle() {
    let logger = Arc::new(RecordingLogger::default());
    let gate = Arc::new(RestartGate::new());
    let (plan, _index) = SamplePlan::new(vec![
        snapshot_at_memory(0, 4_700.0),
        snapshot_at_memory(2_000, 4_700.0),
        snapshot_with_oom(4_000, 4_700.0),
    ]);
    let gate_rx = gate.rx.clone();
    let (browser, restarts) = FakeBrowser::with_gate(1, Some(gate_rx));
    let handler_calls: Arc<std::sync::Mutex<Vec<(String, String)>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let calls_for_handler = Arc::clone(&handler_calls);
    let on_container_restart_requested: ContainerRestartNotifyObserver =
        Arc::new(move |request: ContainerRestartRequest| {
            calls_for_handler
                .lock()
                .unwrap()
                .push((request.reason.clone(), request.source.clone()));
            let ok: NotifyOutcomeFuture = Box::pin(async { Ok(()) });
            ok
        });
    let monitor = create_monitor(RuntimeResourceMonitorOptions {
        browser: Arc::new(browser),
        sessions: Arc::new(FakeSessions {
            channels: vec!["one"],
        }),
        logger: recording_logger(&logger),
        interval_seconds: 60.0,
        resource_guard: Some(create_default_resource_guard(3.0, true, false)),
        cgroup_reader: Some(Arc::new(plan.reader())),
        process_metrics: None,
        now: Some(plan.clock()),
        on_container_restart_requested: Some(on_container_restart_requested),
    });

    monitor.start().await;
    pump().await;
    tokio::time::advance(std::time::Duration::from_millis(2_000)).await;
    pump().await;
    assert_eq!(restarts.load(Ordering::SeqCst), 1);

    tokio::time::advance(std::time::Duration::from_millis(2_000)).await;
    pump().await;
    {
        let handler_requests = handler_calls.lock().unwrap();
        assert!(
            handler_requests
                .iter()
                .any(|(reason, source)| reason == "cgroup_oom" && source == "resource_guard")
        );
    }

    // 舊版 restartGate.resolve()：解除閘門讓 recycle 完成，stop 才能收斂。
    let _ = gate.tx.send(true);
    pump().await;
    monitor.stop().await;
}

// 原測試：guard 高頻採樣使用 policy snapshot，telemetry cadence 使用 full snapshot
#[tokio::test(start_paused = true)]
async fn guard_sampling_uses_policy_snapshot_and_telemetry_uses_full() {
    let logger = Arc::new(RecordingLogger::default());
    let (now_ms, now) = shared_clock();
    let reader = Arc::new(FakeReader::available(
        vec![full_snapshot_with_limits()],
        vec![policy_snapshot()],
    ));
    let counts_reader = Arc::clone(&reader);
    let (browser, _restarts) = FakeBrowser::new(1);
    let monitor = create_monitor(RuntimeResourceMonitorOptions {
        browser: Arc::new(browser),
        sessions: Arc::new(FakeSessions {
            channels: vec!["one"],
        }),
        logger: recording_logger(&logger),
        interval_seconds: 60.0,
        resource_guard: Some(create_default_resource_guard(3.0, true, false)),
        cgroup_reader: Some(reader),
        process_metrics: None,
        now: Some(now),
        on_container_restart_requested: None,
    });

    monitor.start().await;
    pump().await;
    assert_eq!(counts_reader.full_calls(), 1);
    assert_eq!(counts_reader.policy_calls(), 0);
    assert_eq!(
        monitor.get_latest_resource_limits().await,
        RuntimeResourceLimitsSnapshot {
            memory_max_bytes: DiagnosticLimitValue::Number(bytes(8_000.0) as f64),
            pids_max: DiagnosticLimitValue::Number(512.0),
        },
    );

    let mut now_value = 2_000u64;
    while now_value < 60_000 {
        now_ms.store(now_value, Ordering::SeqCst);
        tokio::time::advance(std::time::Duration::from_millis(2_000)).await;
        pump().await;
        now_value += 2_000;
    }
    assert_eq!(counts_reader.full_calls(), 1);
    assert_eq!(counts_reader.policy_calls(), 29);

    now_ms.store(60_000, Ordering::SeqCst);
    tokio::time::advance(std::time::Duration::from_millis(2_000)).await;
    pump().await;
    assert_eq!(counts_reader.full_calls(), 2);
    assert_eq!(counts_reader.policy_calls(), 29);
    assert_eq!(
        logger
            .calls(LogLevel::Info, "runtime_resource_snapshot")
            .len(),
        2,
    );

    monitor.stop().await;
}

// 原測試：runtime snapshot 包含 cgroup CPU 累計時間
#[tokio::test(start_paused = true)]
async fn runtime_snapshot_includes_cgroup_cpu_totals() {
    let logger = Arc::new(RecordingLogger::default());
    let (browser, _restarts) = FakeBrowser::new(1);
    let reader = FakeReader::available(
        vec![CgroupSnapshot {
            sampled_at_monotonic_ms: 0,
            memory_current_bytes: 1_000,
            memory_max_bytes: None,
            memory_peak_bytes: None,
            swap_current_bytes: None,
            pids_current: None,
            pids_max: None,
            events: CgroupMemoryEvents::default(),
            cpu: Some(CgroupCpuStat {
                usage_usec: 9_007_199_254_740_992,
                user_usec: Some(700),
                system_usec: Some(300),
            }),
        }],
        Vec::new(),
    );
    let monitor = create_monitor(RuntimeResourceMonitorOptions {
        browser: Arc::new(browser),
        sessions: Arc::new(FakeSessions {
            channels: vec!["one"],
        }),
        logger: recording_logger(&logger),
        interval_seconds: 60.0,
        resource_guard: None,
        cgroup_reader: Some(Arc::new(reader)),
        process_metrics: None,
        now: None,
        on_container_restart_requested: None,
    });

    monitor.start().await;

    let snapshots = logger.calls(LogLevel::Info, "runtime_resource_snapshot");
    assert_eq!(snapshots.len(), 1);
    let fields = &snapshots[0];
    let field = |name: &str| {
        fields
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
            .unwrap_or_else(|| panic!("欄位 {name} 不存在"))
    };
    assert_eq!(
        field("cgroupCpuUsageUsec"),
        TestField::Text("9007199254740992".to_string()),
    );
    assert_eq!(field("cgroupCpuUserUsec"), TestField::Number(700.0));
    assert_eq!(field("cgroupCpuSystemUsec"), TestField::Number(300.0));
    monitor.stop().await;
}

// 原測試：emergency 時呼叫 container restart handler
#[tokio::test(start_paused = true)]
async fn emergency_requests_container_restart_handler() {
    let logger = Arc::new(RecordingLogger::default());
    let (browser, _restarts) = FakeBrowser::new(1);
    let handler_calls: Arc<std::sync::Mutex<Vec<(String, String)>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let calls_for_handler = Arc::clone(&handler_calls);
    let on_container_restart_requested: ContainerRestartNotifyObserver =
        Arc::new(move |request: ContainerRestartRequest| {
            calls_for_handler
                .lock()
                .unwrap()
                .push((request.reason.clone(), request.source.clone()));
            let ok: NotifyOutcomeFuture = Box::pin(async { Ok(()) });
            ok
        });
    let reader = FakeReader::available(vec![snapshot_at_memory(0, 6_000.0)], Vec::new());
    let monitor = create_monitor(RuntimeResourceMonitorOptions {
        browser: Arc::new(browser),
        sessions: Arc::new(FakeSessions {
            channels: vec!["one"],
        }),
        logger: recording_logger(&logger),
        interval_seconds: 60.0,
        resource_guard: Some(create_default_resource_guard(3.0, true, false)),
        cgroup_reader: Some(Arc::new(reader)),
        process_metrics: None,
        now: None,
        on_container_restart_requested: Some(on_container_restart_requested),
    });

    monitor.start().await;

    {
        let handler_requests = handler_calls.lock().unwrap();
        assert!(
            handler_requests
                .iter()
                .any(|(reason, source)| reason == "emergency_memory" && source == "resource_guard")
        );
    }
    assert!(logger.contains_field(
        LogLevel::Error,
        "resource_guard_container_restart_requested",
        "reason",
        &TestField::Text("emergency_memory".to_string()),
    ));
    monitor.stop().await;
}
