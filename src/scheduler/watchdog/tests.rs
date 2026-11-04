//! Watchdog 排程器的單元測試：移植舊版 `test/unit/watchdog-scheduler.test.ts`
//! 的 13 個標題（其中兩個是 `it.each` 模板——一案例一個 Rust 測試，合計 16 個
//! 測試函式；另加 ISO 換算輔助的格式鎖定測試）。時間以注入的時鐘＋fake
//! timer 驅動；等待以 watch 通知與 yield 輪詢（取代 `vi.waitFor`），不等
//! 真實時間。
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Mutex as StdMutex, MutexGuard};

use tokio::sync::watch;

use super::{
    DefaultSchedulerClock, DefaultWatchdogScheduler, DefaultWatchdogSchedulerOptions,
    SchedulerLiveStatusProvider, SchedulerSessionReconcile, SchedulerStreamSelector,
    SchedulerTickCallback, SchedulerTimer, StreamStatusChange, StreamStatusObserver,
    WatchdogLiveStatusError, WatchdogScheduler, WatchdogSchedulerConfig, WatchdogSchedulerLogger,
    iso_utc_string_from_epoch_ms,
};
use crate::config::writeback::RuntimeWatchConfig;
use crate::logging::logger::LogEvents;
use crate::logging::redact::SanitizedValue;
use crate::scheduler::selector::{
    ChannelLiveStatus as SelectorChannelStatus, StreamSelectionInput, select_active_channels,
};
use crate::sessions::reconcile::ReconcileError;
use crate::twitch::errors::{
    TwitchApiAuthError, TwitchApiError, TwitchApiRateLimitError, TwitchApiTemporaryError,
    TwitchApiTemporaryErrorReason,
};
use crate::twitch::live_status::ChannelLiveStatus;

// ---------------------------------------------------------------------------
// 常數與測試輔助
// ---------------------------------------------------------------------------

/// 舊測試的 `CHECKED_AT` 常數（harness 預設的固定 `now`）。
const CHECKED_AT: &str = "2026-06-14T12:00:00.000Z";

/// `CHECKED_AT`（2026-06-14T12:00:00.000Z）的 epoch 毫秒。
const CHECKED_AT_MS: i64 = 1_781_438_400_000;

/// 舊測試的 `retryAt`（2026-06-14T12:01:00.000Z）＝`CHECKED_AT`＋60 秒。
const RETRY_AT_MS: i64 = CHECKED_AT_MS + 60_000;

/// 舊測試的中途 `now`（2026-06-14T12:00:30.000Z）＝`CHECKED_AT`＋30 秒。
const NOW_MID_MS: i64 = CHECKED_AT_MS + 30_000;

fn lock<T>(mutex: &StdMutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// 對應舊測試 helper `status()`：組一筆 `ChannelLiveStatus`（twitch 模組形，
/// 選用欄位照 helix 回應外的缺省為 None）。
fn status(channel: &str, is_live: bool) -> ChannelLiveStatus {
    ChannelLiveStatus {
        channel: channel.to_string(),
        is_live,
        stream_id: None,
        title: None,
        started_at: None,
        viewer_count: None,
        checked_at: CHECKED_AT.to_string(),
    }
}

/// 測試用欄位值快照（`app/stall_watchdog/tests.rs` 前例）。
#[derive(Clone, Debug, PartialEq)]
enum TestField {
    Undefined,
    Bool(bool),
    Number(f64),
    Text(String),
    Array(Vec<TestField>),
    Object(Vec<(String, TestField)>),
    Other,
}

fn capture(value: &SanitizedValue) -> TestField {
    match value {
        SanitizedValue::Undefined => TestField::Undefined,
        SanitizedValue::Bool(value) => TestField::Bool(*value),
        SanitizedValue::Number(value) => TestField::Number(*value),
        SanitizedValue::String(value) => TestField::Text(value.clone()),
        SanitizedValue::Array(items) => TestField::Array(items.iter().map(capture).collect()),
        SanitizedValue::Object(entries) => TestField::Object(
            entries
                .iter()
                .map(|(key, value)| (key.clone(), capture(value)))
                .collect(),
        ),
        _ => TestField::Other,
    }
}

type TestRecord = (String, Vec<(String, TestField)>);

fn capture_fields(value: Option<&SanitizedValue>) -> Vec<(String, TestField)> {
    match value {
        Some(SanitizedValue::Object(entries)) => entries
            .iter()
            .map(|(key, value)| (key.clone(), capture(value)))
            .collect(),
        Some(other) => vec![(String::new(), capture(other))],
        None => Vec::new(),
    }
}

/// 欄位查找（名稱 → 值）。
fn field<'a>(fields: &'a [(String, TestField)], key: &str) -> Option<&'a TestField> {
    fields
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value)
}

fn text(value: &TestField) -> &str {
    match value {
        TestField::Text(value) => value,
        _ => panic!("預期字串欄位"),
    }
}

/// 等呼叫數到達（取代 `vi.waitFor` 的數字斷言；watch 通知驅動）。
async fn await_call_count(receiver: &mut watch::Receiver<u64>, target: u64) {
    while *receiver.borrow_and_update() < target {
        let _ = receiver.changed().await;
    }
}

/// 微任務輪詢（yield 驅動；無真實時間），條件成立即返回。
async fn wait_until(mut predicate: impl FnMut() -> bool) {
    for _ in 0..10_000 {
        if predicate() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("wait_until 超過上限次數（條件遲遲未成立）");
}

// ---------------------------------------------------------------------------
// 測試替身（對應舊測試的 fake logger／provider／timer／selector／observer）
// ---------------------------------------------------------------------------

/// 記錄四個層級呼叫的假 logger（對應舊測試 `createLoggerMock()`）。
#[derive(Clone, Default)]
struct FakeWatchdogLogger {
    debug: Arc<StdMutex<Vec<TestRecord>>>,
    info: Arc<StdMutex<Vec<TestRecord>>>,
    warn: Arc<StdMutex<Vec<TestRecord>>>,
    error: Arc<StdMutex<Vec<TestRecord>>>,
}

impl FakeWatchdogLogger {
    fn records_of(&self, level: &str) -> Vec<TestRecord> {
        match level {
            "debug" => lock(&self.debug).clone(),
            "info" => lock(&self.info).clone(),
            "warn" => lock(&self.warn).clone(),
            "error" => lock(&self.error).clone(),
            _ => unreachable!("測試 helper 的固定 level key"),
        }
    }

    /// 事件的原始序列（順序斷言用）。
    fn event_names(&self, level: &str) -> Vec<String> {
        self.records_of(level)
            .iter()
            .map(|(event, _)| event.clone())
            .collect()
    }

    /// 第一筆指定事件的欄位（呼叫過則 Some）。
    fn find(&self, level: &str, event: &str) -> Option<Vec<(String, TestField)>> {
        self.records_of(level)
            .into_iter()
            .find(|(name, _)| name == event)
            .map(|(_, fields)| fields)
    }
}

impl WatchdogSchedulerLogger for FakeWatchdogLogger {
    fn debug(&self, event: &str, fields: Option<&SanitizedValue>) {
        lock(&self.debug).push((event.to_string(), capture_fields(fields)));
    }

    fn info(&self, event: &str, fields: Option<&SanitizedValue>) {
        lock(&self.info).push((event.to_string(), capture_fields(fields)));
    }

    fn warn(&self, event: &str, fields: Option<&SanitizedValue>) {
        lock(&self.warn).push((event.to_string(), capture_fields(fields)));
    }

    fn error(&self, event: &str, fields: Option<&SanitizedValue>) {
        lock(&self.error).push((event.to_string(), capture_fields(fields)));
    }
}

/// 延遲交付閘門（對應舊測試的 `deferred<T>()`：release 前等待端卡住）。
struct DeferredValue<T> {
    slot: Arc<watch::Sender<Option<T>>>,
}

impl<T> Default for DeferredValue<T> {
    fn default() -> Self {
        Self {
            slot: Arc::new(watch::channel(None).0),
        }
    }
}

impl<T: Clone> DeferredValue<T> {
    async fn wait(&self) -> T {
        let mut receiver = self.slot.subscribe();
        while receiver.borrow_and_update().is_none() {
            let _ = receiver.changed().await;
        }
        receiver
            .borrow()
            .clone()
            .expect("deferred 值已由測試端釋放")
    }

    fn release(&self, value: T) {
        self.slot.send_replace(Some(value));
    }
}

type FakeLiveOutcome = Result<Vec<ChannelLiveStatus>, WatchdogLiveStatusError>;

/// 對應舊測試 mock 的回覆安排：依舊測試用到的 mock API 形狀設計。
#[derive(Clone)]
enum FakeReply {
    /// `mockResolvedValueOnce`／`mockRejectedValueOnce`：一筆用完即丟。
    Once(FakeLiveOutcome),
    /// `mockRejectedValue`：永遠回該錯誤。
    StickyErr(WatchdogLiveStatusError),
    /// `mockResolvedValue`：永遠回該值。
    StickyValue(Vec<ChannelLiveStatus>),
    /// `mockReturnValue(deferred promise)`：卡在閘門（release 時交付值）。
    PendingGate,
}

/// 記錄呼叫並依安排回覆的假 `getLiveStatuses`（對應舊測試的 provider mock）。
struct FakeLiveStatusProvider {
    /// `mockXxxValueOnce` 的序列（用後即丟）。
    queued: Arc<StdMutex<VecDeque<FakeReply>>>,
    /// queue 耗盡後的預設回覆（對應 mockResolvedValue／mockRejectedValue）。
    sticky: Arc<StdMutex<Option<FakeReply>>>,
    /// PendingGate 的交付閘門（每筆呼叫都等它）。
    pending_gate: Arc<DeferredValue<FakeLiveOutcome>>,
    calls: Arc<StdMutex<Vec<Vec<String>>>>,
    notify: Arc<watch::Sender<u64>>,
    count: Arc<AtomicU64>,
}

impl FakeLiveStatusProvider {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            queued: Arc::default(),
            sticky: Arc::default(),
            pending_gate: Arc::new(DeferredValue::default()),
            calls: Arc::default(),
            notify: Arc::new(watch::channel(0u64).0),
            count: Arc::default(),
        })
    }

    fn queue_once(&self, outcome: FakeLiveOutcome) {
        lock(&self.queued).push_back(FakeReply::Once(outcome));
    }

    fn stick_resolve(&self, statuses: Vec<ChannelLiveStatus>) {
        *lock(&self.sticky) = Some(FakeReply::StickyValue(statuses));
    }

    fn stick_reject(&self, error: WatchdogLiveStatusError) {
        *lock(&self.sticky) = Some(FakeReply::StickyErr(error));
    }

    fn stick_pending(&self) {
        *lock(&self.sticky) = Some(FakeReply::PendingGate);
    }

    /// 放行 pending 閘門。
    fn release_pending(&self, value: FakeLiveOutcome) {
        self.pending_gate.release(value);
    }

    /// 舊測試 `harness.getLiveStatuses` 的呼叫數通知。
    fn watch_receiver(&self) -> watch::Receiver<u64> {
        self.notify.subscribe()
    }

    fn recorded_calls(&self) -> Vec<Vec<String>> {
        lock(&self.calls).clone()
    }
}

#[async_trait::async_trait]
impl SchedulerLiveStatusProvider for FakeLiveStatusProvider {
    async fn get_live_statuses(&self, channels: &[String]) -> FakeLiveOutcome {
        {
            let mut calls = lock(&self.calls);
            calls.push(channels.to_vec());
        }
        let count = self.count.fetch_add(1, Ordering::SeqCst) + 1;
        self.notify.send_replace(count);

        // Once 項用後即丟；不從 queue 取時走 sticky／pending 閘門。
        let popped = {
            let mut queued = lock(&self.queued);
            match queued.front() {
                Some(FakeReply::Once(_)) => queued.pop_front(),
                _ => None,
            }
        };
        if let Some(FakeReply::Once(outcome)) = popped {
            return outcome;
        }
        let sticky = lock(&self.sticky).clone();
        match sticky {
            Some(FakeReply::StickyErr(error)) => Err(error),
            Some(FakeReply::StickyValue(statuses)) => Ok(statuses),
            Some(FakeReply::PendingGate) => self.pending_gate.wait().await,
            Some(FakeReply::Once(_)) | None => Ok(Vec::new()),
        }
    }
}

/// 記錄 reconcile 呼叫的假 SessionManager（窄接縫）；可讓每筆都卡在閘門。
struct FakeSessionReconciler {
    calls: Arc<StdMutex<Vec<Vec<String>>>>,
    notify: Arc<watch::Sender<u64>>,
    count: Arc<AtomicU64>,
    /// `Some` 時每筆 reconcile 都等同一閘門（對應舊測試 8 的 gate promise）。
    stuck_gate: Option<Arc<DeferredValue<()>>>,
}

impl FakeSessionReconciler {
    /// 舊測試 8 的形：reconcile 永遠卡在閘門（release 才放行）。
    fn stuck() -> Arc<Self> {
        Arc::new(Self {
            calls: Arc::default(),
            notify: Arc::new(watch::channel(0u64).0),
            count: Arc::default(),
            stuck_gate: Some(Arc::new(DeferredValue::default())),
        })
    }

    fn watch_receiver(&self) -> watch::Receiver<u64> {
        self.notify.subscribe()
    }

    fn recorded_calls(&self) -> Vec<Vec<String>> {
        lock(&self.calls).clone()
    }

    fn release_gate(&self) {
        if let Some(gate) = &self.stuck_gate {
            gate.release(());
        }
    }
}

#[async_trait::async_trait]
impl SchedulerSessionReconcile for FakeSessionReconciler {
    async fn reconcile(&self, channels: &[String]) -> Result<(), ReconcileError> {
        {
            let mut calls = lock(&self.calls);
            calls.push(channels.to_vec());
        }
        let count = self.count.fetch_add(1, Ordering::SeqCst) + 1;
        self.notify.send_replace(count);
        if let Some(gate) = &self.stuck_gate {
            gate.wait().await;
        }
        Ok(())
    }
}

/// 記錄選台輸入再轉催自由函式的假 selector（對應舊測試的 `select` mock）。
#[derive(Default)]
struct RecordingSelector {
    inputs: Arc<StdMutex<Vec<SelectorInputRecord>>>,
}

#[derive(Clone, Debug, PartialEq)]
struct SelectorInputRecord {
    configured_channels: Vec<String>,
    live_statuses: Vec<SelectorChannelStatus>,
    max_concurrent_streams: f64,
}

impl RecordingSelector {
    fn last_input(&self) -> Option<SelectorInputRecord> {
        lock(&self.inputs).last().cloned()
    }
}

impl SchedulerStreamSelector for RecordingSelector {
    fn select(&self, input: StreamSelectionInput<'_>) -> Vec<String> {
        lock(&self.inputs).push(SelectorInputRecord {
            configured_channels: input.configured_channels.to_vec(),
            live_statuses: input.live_statuses.to_vec(),
            max_concurrent_streams: input.max_concurrent_streams,
        });
        select_active_channels(input)
    }
}

/// 記錄狀態轉換通知的假 observer。
#[derive(Clone, Default)]
struct RecordingObserver {
    changes: Arc<StdMutex<Vec<StreamStatusChange>>>,
}

impl RecordingObserver {
    fn recorded(&self) -> Vec<StreamStatusChange> {
        lock(&self.changes).clone()
    }
}

#[async_trait::async_trait]
impl StreamStatusObserver for RecordingObserver {
    async fn on_stream_status_changed(&self, change: &StreamStatusChange) {
        lock(&self.changes).push(change.clone());
    }
}

/// 手動 fire 的假 timer（對應舊測試 `createTimer()`）。
struct FakeSchedulerTimer {
    callback: Arc<StdMutex<Option<SchedulerTickCallback>>>,
    intervals: Arc<StdMutex<Vec<f64>>>,
    clears: Arc<StdMutex<Vec<u64>>>,
    handle: u64,
}

impl FakeSchedulerTimer {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            callback: Arc::default(),
            intervals: Arc::default(),
            clears: Arc::default(),
            // 舊測試的 handle 物件 `{ type: 'test-interval' }` 以 u64 ids 呈現。
            handle: 7,
        })
    }

    fn fire(&self) {
        let callback = lock(&self.callback).clone();
        if let Some(callback) = callback {
            callback();
        }
    }

    fn recorded_intervals(&self) -> Vec<f64> {
        lock(&self.intervals).clone()
    }

    fn recorded_clears(&self) -> Vec<u64> {
        lock(&self.clears).clone()
    }
}

impl SchedulerTimer for FakeSchedulerTimer {
    fn set_interval(&self, on_tick: SchedulerTickCallback, interval_ms: f64) -> u64 {
        lock(&self.intervals).push(interval_ms);
        *lock(&self.callback) = Some(on_tick);
        self.handle
    }

    fn clear_interval(&self, handle: u64) {
        lock(&self.clears).push(handle);
        if handle == self.handle {
            *lock(&self.callback) = None;
        }
    }
}

// ---------------------------------------------------------------------------
// harness（對應舊測試 createHarness；預設值逐項照舊）
// ---------------------------------------------------------------------------

/// harness 選項的差異欄位（未列者照舊測試預設值）。
struct HarnessOptions {
    channels: Vec<&'static str>,
    check_interval_seconds: f64,
    max_concurrent_streams: f64,
    /// `Some`：注入可寫入的 `now`（對應 options.now）；`None` 用固定 CHECKED_AT。
    now_override_ms: Option<i64>,
    /// `Some`：注入 fake timer（對應 options.timer）。
    timer: Option<Arc<FakeSchedulerTimer>>,
}

impl Default for HarnessOptions {
    fn default() -> Self {
        Self {
            channels: vec!["live"],
            check_interval_seconds: 60.0,
            max_concurrent_streams: 1.0,
            now_override_ms: None,
            timer: None,
        }
    }
}

struct Harness {
    provider: Arc<FakeLiveStatusProvider>,
    reconciler: Arc<FakeSessionReconciler>,
    selector: Arc<RecordingSelector>,
    observer: Arc<RecordingObserver>,
    logger: Arc<FakeWatchdogLogger>,
    /// 可寫入的時鐘（測試 6 以 store 切換；其余僅供端証）。
    clock: Option<Arc<AtomicI64>>,
    scheduler: DefaultWatchdogScheduler,
}

impl Harness {
    /// 等 reconcile 呼叫數到 N（submit 的 spawn 是 fire-and-forget；舊測試
    /// promise 同步前綴的記點在移植端以此等待點對應）。
    async fn await_reconcile_calls(&self, target: u64) {
        let mut rx = self.reconciler.watch_receiver();
        await_call_count(&mut rx, target).await;
    }
}

fn create_harness(options: HarnessOptions) -> Harness {
    let provider = FakeLiveStatusProvider::new();
    let reconciler = {
        // 舊 harness 的 reconcile mock 永遠立即完成（無閘門）。
        Arc::new(FakeSessionReconciler {
            calls: Arc::default(),
            notify: Arc::new(watch::channel(0u64).0),
            count: Arc::default(),
            stuck_gate: None,
        })
    };
    let selector = Arc::new(RecordingSelector::default());
    let observer = Arc::new(RecordingObserver::default());
    let logger = Arc::new(FakeWatchdogLogger::default());
    // 舊 harness now：options.now ?? (() => new Date(CHECKED_AT))。
    let fixed_clock: Arc<AtomicI64> = Arc::new(AtomicI64::new(
        options.now_override_ms.unwrap_or(CHECKED_AT_MS),
    ));
    let clock: DefaultSchedulerClock = {
        let value = Arc::clone(&fixed_clock);
        Arc::new(move || value.load(Ordering::SeqCst).max(0) as u64)
    };
    let scheduler = DefaultWatchdogScheduler::new(DefaultWatchdogSchedulerOptions {
        config: WatchdogSchedulerConfig {
            channels: options
                .channels
                .iter()
                .map(|channel| (*channel).to_string())
                .collect(),
            check_interval_seconds: options.check_interval_seconds,
            max_concurrent_streams: options.max_concurrent_streams,
        },
        live_status_provider: provider.clone(),
        stream_selector: selector.clone(),
        session_manager: Some(reconciler.clone()),
        reconcile_coordinator: None,
        logger: logger.clone(),
        timer: options
            .timer
            .clone()
            .map(|timer| timer as Arc<dyn SchedulerTimer>),
        now: Some(clock),
        on_stream_status_changed: Some(observer.clone()),
    });
    Harness {
        provider,
        reconciler,
        selector,
        observer,
        logger,
        clock: Some(fixed_clock),
        scheduler,
    }
}

// ---------------------------------------------------------------------------
// 測試本體（舊測試 13 個標題）
// ---------------------------------------------------------------------------

// 原測試：第一輪 live 輸出 online，後續不變不重複輸出，offline 時輸出轉換
#[tokio::test]
async fn first_live_round_logs_online_and_later_transition_logs_to_offline() {
    let harness = create_harness(HarnessOptions {
        channels: vec!["first", "second"],
        max_concurrent_streams: 2.0,
        ..HarnessOptions::default()
    });
    harness
        .provider
        .queue_once(Ok(vec![status("first", true), status("second", false)]));
    harness
        .provider
        .queue_once(Ok(vec![status("first", true), status("second", false)]));
    harness
        .provider
        .queue_once(Ok(vec![status("first", false), status("second", true)]));

    harness.scheduler.run_once().await;
    harness.scheduler.run_once().await;
    harness.scheduler.run_once().await;
    harness.await_reconcile_calls(3).await;
    // observer 通知由 spawn 排程（fire-and-forget）；等三筆轉換到齊後再斷言。
    wait_until(|| harness.observer.recorded().len() == 3).await;

    // reconcile.mock.calls === [[['first']], [['first']], [['second']]]
    let calls = harness.reconciler.recorded_calls();
    assert_eq!(
        calls,
        vec![
            vec!["first".to_string()],
            vec!["first".to_string()],
            vec!["second".to_string()],
        ]
    );
    // info.mock.calls 照序列：online first → offline first → online second。
    let info = harness.logger.records_of("info");
    let names: Vec<&str> = info.iter().map(|(event, _)| event.as_str()).collect();
    assert_eq!(
        names,
        vec![
            LogEvents::STREAM_ONLINE,
            LogEvents::STREAM_OFFLINE,
            LogEvents::STREAM_ONLINE,
        ]
    );
    assert_eq!(field(&info[0].1, "channel").map(text), Some("first"),);
    assert_eq!(field(&info[1].1, "channel").map(text), Some("first"));
    assert_eq!(field(&info[2].1, "channel").map(text), Some("second"));
    // observer 收到同一序列的轉換。
    assert_eq!(
        harness.observer.recorded(),
        vec![
            StreamStatusChange {
                channel: "first".to_string(),
                is_live: true
            },
            StreamStatusChange {
                channel: "first".to_string(),
                is_live: false
            },
            StreamStatusChange {
                channel: "second".to_string(),
                is_live: true
            },
        ]
    );
    // snapshot：running false、in-flight false、lastCheckedAt 常數時間、頻道狀態。
    assert_eq!(
        harness.scheduler.get_snapshot(),
        expected_first_round_snapshot()
    );
}

/// 測試一預期的 snapshot（獨立函式以避免長運算式的借用噪音）。
fn expected_first_round_snapshot() -> super::WatchdogSchedulerSnapshot {
    super::WatchdogSchedulerSnapshot {
        running: false,
        check_in_flight: false,
        retry_at: None,
        last_checked_at: Some(CHECKED_AT.to_string()),
        channels: vec![
            super::SchedulerChannelStatus {
                channel: "first".to_string(),
                is_live: Some(false),
            },
            super::SchedulerChannelStatus {
                channel: "second".to_string(),
                is_live: Some(true),
            },
        ],
    }
}

// 原測試：第一輪全部 offline 不輸出狀態噪音但仍 reconcile 空集合
#[tokio::test]
async fn first_all_offline_round_is_silent_but_still_reconciles_empty_set() {
    let harness = create_harness(HarnessOptions {
        channels: vec!["first", "second"],
        ..HarnessOptions::default()
    });
    harness
        .provider
        .stick_resolve(vec![status("first", false), status("second", false)]);

    harness.scheduler.run_once().await;

    harness.await_reconcile_calls(1).await;

    assert!(harness.logger.records_of("info").is_empty());
    let calls = harness.reconciler.recorded_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0], Vec::<String>::new());
}

// 原測試：高優先序頻道上線時由 selector 與 reconcile 替換低優先序頻道
#[tokio::test]
async fn noble_prio_channel_going_live_swaps_out_lower_priority_via_selector_and_reconcile() {
    let harness = create_harness(HarnessOptions {
        channels: vec!["high", "low"],
        max_concurrent_streams: 1.0,
        ..HarnessOptions::default()
    });
    harness
        .provider
        .queue_once(Ok(vec![status("low", true), status("high", false)]));
    harness
        .provider
        .queue_once(Ok(vec![status("low", true), status("high", true)]));

    harness.scheduler.run_once().await;
    harness.scheduler.run_once().await;
    harness.await_reconcile_calls(2).await;

    let calls = harness.reconciler.recorded_calls();
    assert_eq!(
        calls,
        vec![vec!["low".to_string()], vec!["high".to_string()]]
    );
    // selector 最後一次輸入：configured／live_statuses／上限原樣。
    let last = harness.selector.last_input().expect("selector was called");
    assert_eq!(
        last.configured_channels,
        vec!["high".to_string(), "low".to_string()]
    );
    assert_eq!(
        last.live_statuses,
        vec![
            SelectorChannelStatus {
                channel: "low".to_string(),
                is_live: true,
                checked_at: CHECKED_AT.to_string(),
            },
            SelectorChannelStatus {
                channel: "high".to_string(),
                is_live: true,
                checked_at: CHECKED_AT.to_string(),
            },
        ]
    );
    assert_eq!(last.max_concurrent_streams, 1.0);
}

// 原測試：%s API 失敗時保留既有 session，不 reconcile 空集合
// 案例註記：auth
#[tokio::test]
async fn auth_api_failure_keeps_sessions_and_does_not_reconcile_empty_set() {
    api_failure_keeps_sessions_and_does_not_reconcile_empty_set_common(
        WatchdogLiveStatusError::Api(TwitchApiError::Auth(TwitchApiAuthError::new(401))),
        |harness| {
            let error_fields = harness
                .logger
                .find("error", LogEvents::TWITCH_API_AUTH_FAILED)
                .expect("auth failure is logged via error");
            assert_eq!(
                field(&error_fields, "statusCode"),
                Some(&TestField::Number(401.0)),
            );
        },
    )
    .await;
}

// 原測試：%s API 失敗時保留既有 session，不 reconcile 空集合
// 案例註記：rate-limit
#[tokio::test]
async fn rate_limit_api_failure_keeps_sessions_and_does_not_reconcile_empty_set() {
    let retry_at_ms = RETRY_AT_MS;
    api_failure_keeps_sessions_and_does_not_reconcile_empty_set_common(
        WatchdogLiveStatusError::Api(TwitchApiError::RateLimit(TwitchApiRateLimitError::new(
            retry_at_ms,
            60_000,
        ))),
        |harness| {
            let fields = harness
                .logger
                .find("warn", "twitch_api_rate_limited")
                .expect("rate limit is logged via warn");
            assert_eq!(
                text(field(&fields, "retryAt").unwrap_or(&TestField::Other)),
                "2026-06-14T12:01:00.000Z",
            );
            assert_eq!(
                field(&fields, "retryAfterMs"),
                Some(&TestField::Number(60_000.0)),
            );
        },
    )
    .await;
}

// 原測試：%s API 失敗時保留既有 session，不 reconcile 空集合
// 案例註記：temporary
#[tokio::test]
async fn temporary_api_failure_keeps_sessions_and_does_not_reconcile_empty_set() {
    api_failure_keeps_sessions_and_does_not_reconcile_empty_set_common(
        WatchdogLiveStatusError::Api(TwitchApiError::Temporary(TwitchApiTemporaryError::new(
            TwitchApiTemporaryErrorReason::Network,
            None,
        ))),
        |harness| {
            let fields = harness
                .logger
                .find("warn", "twitch_api_temporary_error")
                .expect("temporary failure is logged via warn");
            assert_eq!(
                text(field(&fields, "reason").unwrap_or(&TestField::Other)),
                "network"
            );
            // 舊版：statusCode undefined 的欄位整個省略。
            assert_eq!(field(&fields, "statusCode"), None);
        },
    )
    .await;
}

/// 三個 auth／rate-limit／temporary 案例的共通流程。
async fn api_failure_keeps_sessions_and_does_not_reconcile_empty_set_common(
    error: WatchdogLiveStatusError,
    assert_log: impl Fn(&Harness),
) {
    let harness = create_harness(HarnessOptions::default());
    harness.provider.queue_once(Ok(vec![status("live", true)]));
    harness.provider.queue_once(Err(error));

    harness.scheduler.run_once().await;
    harness.scheduler.run_once().await;
    harness.await_reconcile_calls(1).await;

    // 既有 session 保留：reconcile 只有第一輪的成功呼叫（不收空集合）。
    let calls = harness.reconciler.recorded_calls();
    assert_eq!(calls, vec![vec!["live".to_string()]]);
    assert_log(&harness);
}

// 原測試：%s 失敗後於下一個正常 interval 重試
// 案例註記：auth
#[tokio::test]
async fn auth_failures_retry_on_next_normal_interval() {
    interval_retry_common(
        WatchdogLiveStatusError::Api(TwitchApiError::Auth(TwitchApiAuthError::new(403))),
        |harness| {
            let fields = harness
                .logger
                .find("error", LogEvents::TWITCH_API_AUTH_FAILED)
                .expect("auth failure logged");
            assert_eq!(
                field(&fields, "statusCode"),
                Some(&TestField::Number(403.0)),
            );
        },
    )
    .await;
}

// 原測試：%s 失敗後於下一個正常 interval 重試
// 案例註記：temporary
#[tokio::test]
async fn temporary_failures_retry_on_next_normal_interval() {
    interval_retry_common(
        WatchdogLiveStatusError::Api(TwitchApiError::Temporary(TwitchApiTemporaryError::new(
            TwitchApiTemporaryErrorReason::Server,
            Some(503),
        ))),
        |harness| {
            let fields = harness
                .logger
                .find("warn", "twitch_api_temporary_error")
                .expect("temporary failure logged");
            assert_eq!(
                text(field(&fields, "reason").unwrap_or(&TestField::Other)),
                "server"
            );
            assert_eq!(
                field(&fields, "statusCode"),
                Some(&TestField::Number(503.0))
            );
        },
    )
    .await;
}

/// 兩個 auth／temporary 案例的共通流程（start → 失敗 → fire → 成功）。
async fn interval_retry_common(error: WatchdogLiveStatusError, assert_log: impl Fn(&Harness)) {
    let timer = FakeSchedulerTimer::new();
    let harness = create_harness(HarnessOptions {
        timer: Some(Arc::clone(&timer)),
        ..HarnessOptions::default()
    });
    harness.provider.queue_once(Err(error));
    harness.provider.queue_once(Ok(vec![status("live", true)]));

    harness.scheduler.start();
    {
        let mut rx = harness.provider.watch_receiver();
        await_call_count(&mut rx, 1).await;
    }
    assert_log(&harness);

    timer.fire();
    {
        let mut rx = harness.provider.watch_receiver();
        await_call_count(&mut rx, 2).await;
        let mut reconcile_rx = harness.reconciler.watch_receiver();
        await_call_count(&mut reconcile_rx, 1).await;
    }
    assert_eq!(
        harness.reconciler.recorded_calls(),
        vec![vec!["live".to_string()]]
    );

    harness.scheduler.stop().await;
}

// 原測試：rate-limit 視窗結束前 interval tick 會略過，到 retryAt 才重新請求
#[tokio::test]
async fn rate_limited_ticks_are_skipped_until_retry_at() {
    let timer = FakeSchedulerTimer::new();
    let harness = create_harness(HarnessOptions {
        now_override_ms: Some(CHECKED_AT_MS),
        timer: Some(Arc::clone(&timer)),
        ..HarnessOptions::default()
    });
    harness
        .provider
        .queue_once(Err(WatchdogLiveStatusError::Api(
            TwitchApiError::RateLimit(TwitchApiRateLimitError::new(RETRY_AT_MS, 60_000)),
        )));
    harness.provider.queue_once(Ok(vec![status("live", true)]));

    harness.scheduler.start();
    {
        let mut rx = harness.provider.watch_receiver();
        await_call_count(&mut rx, 1).await;
    }
    {
        let fields = harness
            .logger
            .find("warn", "twitch_api_rate_limited")
            .expect("rate-limited first tick");
        assert_eq!(
            text(field(&fields, "retryAt").unwrap_or(&TestField::Other)),
            "2026-06-14T12:01:00.000Z",
        );
        assert_eq!(
            field(&fields, "retryAfterMs"),
            Some(&TestField::Number(60_000.0)),
        );
    }

    // now = 12:00:30（retryAt 之前）：interval tick 被略過。
    if let Some(clock) = harness.clock.as_ref() {
        clock.store(NOW_MID_MS, Ordering::SeqCst);
    }
    timer.fire();
    {
        let mut rx = harness.provider.watch_receiver();
        await_call_count(&mut rx, 1).await;
        // skip 的 warn 到齊（spawned tick 的同步前綴完成於 provider count 重查時）。
        wait_until(|| {
            harness
                .logger
                .find("warn", "scheduler_tick_skipped")
                .is_some()
        })
        .await;
        let fields = harness
            .logger
            .find("warn", "scheduler_tick_skipped")
            .expect("skipped");
        assert_eq!(
            text(field(&fields, "reason").unwrap_or(&TestField::Other)),
            "rate_limited"
        );
        assert_eq!(
            text(field(&fields, "retryAt").unwrap_or(&TestField::Other)),
            "2026-06-14T12:01:00.000Z",
        );
    }
    assert_eq!(harness.provider.recorded_calls().len(), 1);

    // now = retryAt：重新請求。
    if let Some(clock) = harness.clock.as_ref() {
        clock.store(RETRY_AT_MS, Ordering::SeqCst);
    }
    timer.fire();
    {
        let mut rx = harness.provider.watch_receiver();
        await_call_count(&mut rx, 2).await;
        let mut reconcile_rx = harness.reconciler.watch_receiver();
        await_call_count(&mut reconcile_rx, 1).await;
    }
    assert_eq!(
        harness.reconciler.recorded_calls(),
        vec![vec!["live".to_string()]]
    );

    harness.scheduler.stop().await;
}

// 原測試：runOnce 不重入，略過的呼叫立即 resolve
#[tokio::test]
async fn run_once_is_not_reentrant_and_skipped_call_resolves_immediately() {
    let harness = create_harness(HarnessOptions::default());
    harness.provider.stick_pending();

    let first = tokio::spawn({
        let scheduler = harness.scheduler.clone();
        async move {
            scheduler.run_once().await;
        }
    });
    {
        let mut rx = harness.provider.watch_receiver();
        await_call_count(&mut rx, 1).await;
    }

    harness.scheduler.run_once().await;

    // 舊測試：firstRunSettled 還是 false（第二次呼叫時第一輪尚未完成）。
    assert!(!first.is_finished());
    assert_eq!(harness.provider.recorded_calls().len(), 1);
    let fields = harness
        .logger
        .find("warn", "scheduler_tick_skipped")
        .expect("skip logged");
    assert_eq!(
        text(field(&fields, "reason").unwrap_or(&TestField::Other)),
        "in_flight"
    );

    harness
        .provider
        .release_pending(Ok(vec![status("live", true)]));
    first.await.expect("first run completes");
}

// 原測試：API polling 完成後不等待 session reconcile
#[tokio::test]
async fn scheduler_does_not_await_session_reconcile_after_polling() {
    let harness = create_harness(HarnessOptions::default());
    harness.provider.stick_resolve(vec![status("live", true)]);
    let stuck = FakeSessionReconciler::stuck();
    let stuck_rx = stuck.watch_receiver();

    // 以 stuck 的 reconciler 重建 scheduler（替換 session manager 接縫）。
    let scheduler = rebuild_with_reconciler(&harness, Arc::clone(&stuck));
    scheduler.run_once().await;

    {
        let mut rx = stuck_rx;
        await_call_count(&mut rx, 1).await;
    }
    assert_eq!(stuck.recorded_calls(), vec![vec!["live".to_string()]]);
    let snapshot = scheduler.get_snapshot();
    assert!(!snapshot.check_in_flight);
    assert_eq!(snapshot.last_checked_at, Some(CHECKED_AT.to_string()));
    // 舊測試尾：reconcileGate.resolve(undefined)（清殘餘任務）。
    stuck.release_gate();
}

/// 把 harness 的 scheduler 依指定 reconciler 重建（等同舊測試改 mock 行為）。
fn rebuild_with_reconciler(
    harness: &Harness,
    reconciler: Arc<FakeSessionReconciler>,
) -> DefaultWatchdogScheduler {
    // 其餘接縫沿用 harness 的既有 dependency（照舊測試只有 reconcile mock 變化）。
    DefaultWatchdogScheduler::new(DefaultWatchdogSchedulerOptions {
        config: WatchdogSchedulerConfig {
            channels: vec!["live".to_string()],
            check_interval_seconds: 60.0,
            max_concurrent_streams: 1.0,
        },
        live_status_provider: harness.provider.clone(),
        stream_selector: harness.selector.clone(),
        session_manager: Some(reconciler as Arc<dyn SchedulerSessionReconcile>),
        reconcile_coordinator: None,
        logger: harness.logger.clone(),
        timer: None,
        now: Some({
            let value = Arc::new(AtomicI64::new(CHECKED_AT_MS));
            Arc::new(move || value.load(Ordering::SeqCst) as u64)
        }),
        on_stream_status_changed: None,
    })
}

// 原測試：start 立即執行首輪並依設定建立 interval，重複 start 不新增 timer
#[tokio::test]
async fn start_runs_first_tick_and_creates_interval_once() {
    let timer = FakeSchedulerTimer::new();
    let harness = create_harness(HarnessOptions {
        check_interval_seconds: 45.0,
        timer: Some(Arc::clone(&timer)),
        ..HarnessOptions::default()
    });
    harness.provider.stick_resolve(vec![status("live", true)]);

    harness.scheduler.start();
    harness.scheduler.start();

    {
        let mut rx = harness.provider.watch_receiver();
        await_call_count(&mut rx, 1).await;
    }
    assert_eq!(timer.recorded_intervals(), vec![45_000.0]);
    assert_eq!(
        harness.reconciler.recorded_calls(),
        vec![vec!["live".to_string()]]
    );

    harness.scheduler.stop().await;
}

// 原測試：stop 清除 timer 並等待執行中的首輪完成，停止後 timer callback 無效
#[tokio::test]
async fn stop_clears_timer_and_awaits_in_flight_first_tick() {
    let timer = FakeSchedulerTimer::new();
    let harness = create_harness(HarnessOptions {
        timer: Some(Arc::clone(&timer)),
        ..HarnessOptions::default()
    });
    harness.provider.stick_pending();

    harness.scheduler.start();
    {
        let mut rx = harness.provider.watch_receiver();
        await_call_count(&mut rx, 1).await;
    }

    let stop = tokio::spawn({
        let scheduler = harness.scheduler.clone();
        async move {
            scheduler.stop().await;
        }
    });
    // stop 的同步前綴：clearInterval 已被呼叫（handle 相同），但 stop 尚未完成。
    wait_until(|| !timer.recorded_clears().is_empty()).await;
    assert_eq!(timer.recorded_clears(), vec![timer_handle(&timer)]);
    assert!(!stop.is_finished());

    harness
        .provider
        .release_pending(Ok(vec![status("live", true)]));
    stop.await.expect("stop completes");
    harness.scheduler.stop().await;
    timer.fire();

    // 停止後 timer callback 無效：provider 不再有第 2 筆呼叫。
    assert_eq!(harness.provider.recorded_calls().len(), 1);
    assert_eq!(timer.recorded_clears(), vec![timer_handle(&timer)]);
}

fn timer_handle(timer: &FakeSchedulerTimer) -> u64 {
    timer.handle
}

// 原測試：API 回傳缺少 configured channel 時保留 session 與上一輪狀態
#[tokio::test]
async fn missing_configured_channel_keeps_sessions_and_previous_state() {
    let harness = create_harness(HarnessOptions {
        channels: vec!["first", "second"],
        max_concurrent_streams: 2.0,
        ..HarnessOptions::default()
    });
    harness
        .provider
        .queue_once(Ok(vec![status("first", true), status("second", true)]));
    harness
        .provider
        .queue_once(Ok(vec![status("first", false)]));
    harness
        .provider
        .queue_once(Ok(vec![status("first", true), status("second", true)]));

    harness.scheduler.run_once().await;
    harness.scheduler.run_once().await;
    harness.scheduler.run_once().await;
    harness.await_reconcile_calls(2).await;

    // reconcile 只有第 1 與第 3 輪（缺值的第 2 輪整批跳過）。
    let calls = harness.reconciler.recorded_calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0], vec!["first".to_string(), "second".to_string()]);
    assert_eq!(calls[1], vec!["first".to_string(), "second".to_string()]);
    assert!(
        !harness
            .logger
            .event_names("info")
            .iter()
            .any(|event| event == LogEvents::STREAM_OFFLINE)
    );
}

// 原測試：更新 runtime config 後立即套用新頻道與同時觀看上限
#[tokio::test]
async fn updated_runtime_config_applies_new_channels_and_cap_immediately() {
    let harness = create_harness(HarnessOptions {
        channels: vec!["first", "second"],
        max_concurrent_streams: 2.0,
        ..HarnessOptions::default()
    });
    harness
        .provider
        .queue_once(Ok(vec![status("first", true), status("second", true)]));
    harness
        .provider
        .stick_resolve(vec![status("second", true), status("third", false)]);

    harness.scheduler.run_once().await;
    harness
        .scheduler
        .update_config(RuntimeWatchConfig {
            channels: vec!["second".to_string(), "third".to_string()],
            max_concurrent_streams: 1.0,
        })
        .await;
    harness.await_reconcile_calls(3).await;

    // getLiveStatuses 最後以新頻道查詢；reconcile：update 的提交＋runOnce 的提交。
    let provider_calls = harness.provider.recorded_calls();
    assert_eq!(
        provider_calls.last(),
        Some(&vec!["second".to_string(), "third".to_string()]),
    );
    let reconcile_calls = harness.reconciler.recorded_calls();
    assert_eq!(reconcile_calls[1], vec!["second".to_string()]);
    assert_eq!(reconcile_calls.last(), Some(&vec!["second".to_string()]),);
    // snapshot：新頻道＋上一輪狀態的 isLive。
    let snapshot = harness.scheduler.get_snapshot();
    assert_eq!(
        snapshot.channels,
        vec![
            super::SchedulerChannelStatus {
                channel: "second".to_string(),
                is_live: Some(true),
            },
            super::SchedulerChannelStatus {
                channel: "third".to_string(),
                is_live: Some(false),
            },
        ]
    );
}

// 原測試：錯誤日誌不轉送 API 原始錯誤中的 token、Authorization 或 Bearer 值
#[tokio::test]
async fn error_logs_do_not_forward_secrets_from_raw_api_errors() {
    let secret = "scheduler-secret-token";
    let harness = create_harness(HarnessOptions::default());
    harness
        .provider
        .stick_reject(WatchdogLiveStatusError::Unknown(format!(
            "Authorization: Bearer {secret}; access_token={secret}"
        )));

    harness.scheduler.run_once().await;

    // 讓 fire-and-forget 的 spawn 有機會執行（仍不應有 reconcile；對應舊測試
    // 在 await runOnce 後直接斷言 not.toHaveBeenCalled()）。
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }

    // 序列化所有層級的紀錄；不得出現 secret／Authorization／Bearer（與舊測試
    // JSON.stringify 檢查同語意）。
    let serialized = format!(
        "{:?}{:?}{:?}{:?}",
        harness.logger.records_of("debug"),
        harness.logger.records_of("info"),
        harness.logger.records_of("warn"),
        harness.logger.records_of("error"),
    );
    assert!(!serialized.contains(secret));
    assert!(!serialized.contains("Authorization"));
    assert!(!serialized.contains("Bearer"));
    let fields = harness
        .logger
        .find("warn", "twitch_api_request_failed")
        .expect("unknown errors log twitch_api_request_failed");
    assert_eq!(
        text(field(&fields, "kind").unwrap_or(&TestField::Other)),
        "unknown"
    );
    // catch-all 不替換 session：reconcile 沒有被呼叫。
    assert!(harness.reconciler.recorded_calls().is_empty());
}

// ---------------------------------------------------------------------------
// ISO 換算輔助的格式鎖定（計畫 §7.7 的 `YYYY-MM-DDTHH:MM:SS.sssZ`）
// ---------------------------------------------------------------------------

// freeze-checked fixed（非舊測試）：iso_utc_string_from_epoch_ms 的格式。
#[test]
fn iso_utc_string_from_epoch_ms_formats_to_documented_shape() {
    assert_eq!(iso_utc_string_from_epoch_ms(CHECKED_AT_MS), CHECKED_AT);
    assert_eq!(
        iso_utc_string_from_epoch_ms(RETRY_AT_MS),
        "2026-06-14T12:01:00.000Z",
    );
    assert_eq!(iso_utc_string_from_epoch_ms(0), "1970-01-01T00:00:00.000Z",);
}

// 直接用 epoch 常數的另一個檢核：`CHECKED_AT_MS` 與文字描述互相轉換一致。
#[test]
fn checked_at_constant_matches_documented_date() {
    // 以 time 組合 2026-06-14T12:00:00Z，核對常數值（防手算錯誤）。
    use time::{Date, Month, PrimitiveDateTime, Time};
    let date = Date::from_calendar_date(2026, Month::June, 14).expect("valid date");
    let time = Time::from_hms(12, 0, 0).expect("valid time");
    let datetime = PrimitiveDateTime::new(date, time).assume_utc();
    assert_eq!(datetime.unix_timestamp() * 1_000, CHECKED_AT_MS);
}
