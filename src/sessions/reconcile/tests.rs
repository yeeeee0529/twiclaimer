//! Reconcile 協調器的單元測試：移植舊版 `test/unit/reconcile-coordinator.test.ts`
//! 的 2 個測試。shutdown 逾時以 tokio paused time 驅動（對應舊測試的
//! `vi.useFakeTimers`），不等真實時間；reconcile 卡點用 watch 閘門（對應
//! 舊測試的 `deferred<T>()`）。
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex as StdMutex, MutexGuard};

use tokio::sync::watch;

use super::{
    DefaultReconcileCoordinator, FlightSignal, ReconcileClock, ReconcileCoordinatorOptions,
    ReconcileError, ReconcileLogger, ReconcileSessionManager,
};
use crate::logging::redact::SanitizedValue;

// ---------------------------------------------------------------------------
// 測試替身（對應舊測試的 fake sessionManager／logger／deferred）
// ---------------------------------------------------------------------------

fn lock<T>(mutex: &StdMutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// 延遲完成閘門（對應舊測試的 `deferred<T>()`）：等待端 `wait()` 到完成通知，
/// 釋放端 `release()` 放行。
#[derive(Clone)]
struct DeferredGate {
    flight: Arc<FlightSignal>,
}

impl DeferredGate {
    fn new() -> Self {
        Self {
            flight: Arc::new(FlightSignal::new()),
        }
    }

    async fn wait(&self) {
        self.flight.wait().await;
    }

    fn release(&self) {
        self.flight.complete();
    }
}

/// 欄位值快照（`SanitizedValue` 的可 Send／可比對子集；同
/// `app/stall_watchdog/tests.rs` 前例）。
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

fn channels(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_string()).collect()
}

/// 欄位查找（名稱＋值）。
fn field<'a>(fields: &'a [(String, TestField)], key: &str) -> Option<&'a TestField> {
    fields
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value)
}

type TestRecord = (String, Vec<(String, TestField)>);

/// 記錄 debug 與 error 呼叫的假 logger（對應舊測試 `createLogger()`）。
#[derive(Clone, Default)]
struct FakeReconcileLogger {
    debug: Arc<StdMutex<Vec<TestRecord>>>,
    error: Arc<StdMutex<Vec<TestRecord>>>,
}

impl FakeReconcileLogger {
    fn debug_records(&self) -> Vec<TestRecord> {
        lock(&self.debug).clone()
    }
}

impl ReconcileLogger for FakeReconcileLogger {
    fn debug(&self, event: &str, fields: Option<&SanitizedValue>) {
        self.debug.lock().unwrap().push((
            event.to_string(),
            fields
                .map(|value| match capture(value) {
                    TestField::Object(entries) => entries,
                    other => vec![(String::new(), other)],
                })
                .unwrap_or_default(),
        ));
    }

    fn error(&self, event: &str, fields: Option<&SanitizedValue>) {
        self.error.lock().unwrap().push((
            event.to_string(),
            fields
                .map(|value| match capture(value) {
                    TestField::Object(entries) => entries,
                    other => vec![(String::new(), other)],
                })
                .unwrap_or_default(),
        ));
    }
}

/// 記錄 reconcile／cancelPendingStarts 呼叫的假 SessionManager；reconcile
/// 依規則卡在閘門，並以 watch 通知呼叫數（對應舊測試的 `vi.fn` + `vi.waitFor`）。
struct FakeSessionManager {
    calls: Arc<StdMutex<Vec<Vec<String>>>>,
    notify: Arc<watch::Sender<u64>>,
    count: Arc<std::sync::atomic::AtomicU64>,
    /// 對應舊測試的 reconcile 卡點。`gate_mode`：
    /// - `OnlyFirst`：僅 `channels[0] === 'first'` 的呼叫卡住（舊測試一）。
    /// - `Always`：每筆呼叫都卡住（舊測試二）。
    /// - `Never`：不卡（記點後立即完成）。
    gate: DeferredGate,
    gate_mode: GateMode,
    cancel_calls: Arc<StdMutex<Vec<String>>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum GateMode {
    OnlyFirst,
    Always,
}

impl Default for FakeSessionManager {
    fn default() -> Self {
        Self {
            calls: Arc::default(),
            notify: Arc::new(watch::channel(0u64).0),
            count: Arc::default(),
            gate: DeferredGate::new(),
            gate_mode: GateMode::OnlyFirst,
            cancel_calls: Arc::default(),
        }
    }
}

impl FakeSessionManager {
    /// 舊測試一的形：channels[0]=='first' 才卡閘門。
    fn gated_on_first() -> Self {
        Self {
            gate_mode: GateMode::OnlyFirst,
            ..Self::default()
        }
    }

    /// 舊測試二的形：每筆 reconcile 都卡閘門。
    fn gated_always() -> Self {
        Self {
            gate_mode: GateMode::Always,
            ..Self::default()
        }
    }

    fn watch_receiver(&self) -> watch::Receiver<u64> {
        self.notify.subscribe()
    }

    fn release_gate(&self) {
        self.gate.release();
    }
}

#[async_trait::async_trait]
impl ReconcileSessionManager for FakeSessionManager {
    async fn reconcile(&self, desired_channels: &[String]) -> Result<(), ReconcileError> {
        {
            let mut calls = lock(&self.calls);
            calls.push(desired_channels.to_vec());
        }
        // 通知呼叫數 +1（watch 更新；對應舊測試 await reconcile 被呼叫）。
        let count = self.count.fetch_add(1, Ordering::SeqCst) + 1;
        self.notify.send_replace(count);
        let blocked = match self.gate_mode {
            GateMode::Always => true,
            GateMode::OnlyFirst => desired_channels.first().map(String::as_str) == Some("first"),
        };
        if blocked {
            self.gate.wait().await;
        }
        Ok(())
    }

    async fn cancel_pending_starts(&self, reason: &str) {
        lock(&self.cancel_calls).push(reason.to_string());
    }
}

/// 等呼叫數到達（取代舊測試的 `vi.waitFor`；watch 通知驅動，無真實時間）。
async fn await_call_count(receiver: &mut watch::Receiver<u64>, target: u64) {
    while *receiver.borrow_and_update() < target {
        // `Err` 只表示 sender 消失；假物自持 Arc，不會發生（同正式碼語意）。
        let _ = receiver.changed().await;
    }
}

// ---------------------------------------------------------------------------
// 測試本體
// ---------------------------------------------------------------------------

// 原測試：進行中的 reconcile 保留，尚未處理的 desired state 只保留最新一筆
#[tokio::test(start_paused = true)]
async fn in_flight_reconcile_keeper_unprocessed_desired_state_only_newest_kept() {
    let manager = Arc::new(FakeSessionManager::gated_on_first());
    let logger = Arc::new(FakeReconcileLogger::default());
    let coordinator = DefaultReconcileCoordinator::new(ReconcileCoordinatorOptions {
        session_manager: Arc::clone(&manager) as Arc<dyn ReconcileSessionManager>,
        logger: Arc::clone(&logger) as Arc<dyn ReconcileLogger>,
        now: None,
        shutdown_timeout_ms: None,
    });

    let first = channels(&["first"]);
    coordinator.request(&first);
    {
        let mut rx = manager.watch_receiver();
        await_call_count(&mut rx, 1).await;
    }
    let stale = channels(&["stale"]);
    coordinator.request(&stale);
    let latest = channels(&["latest"]);
    coordinator.request(&latest);

    manager.release_gate();
    coordinator.wait_for_idle().await;

    // 舊測試：reconcile.mock.calls === [[['first']], [['latest']]]
    let calls = lock(&manager.calls).clone();
    assert_eq!(calls, vec![channels(&["first"]), channels(&["latest"])]);

    // 舊測試：logger.debug 以 'reconcile_coalesced' + desiredChannels ['latest']
    // 呼叫（本例只會有一筆 coalesced）。
    let coalesced: Vec<Vec<(String, TestField)>> = logger
        .debug_records()
        .into_iter()
        .filter(|(event, _)| event == "reconcile_coalesced")
        .map(|(_, fields)| fields)
        .collect();
    assert!(
        coalesced.len() == 1,
        "coalesced 記點應只有一筆：實際 {} 筆",
        coalesced.len(),
    );
    assert_eq!(
        field(&coalesced[0], "desiredChannels"),
        Some(&TestField::Array(vec![TestField::Text(
            "latest".to_string()
        )])),
    );
}

// 原測試：shutdown 停止接收新 request 並只等待有限 grace period
#[tokio::test(start_paused = true)]
async fn shutdown_stops_accepting_requests_and_waits_only_a_finite_grace_period() {
    let manager = Arc::new(FakeSessionManager::gated_always());
    let logger = Arc::new(FakeReconcileLogger::default());
    // 注入測試時鐘（epoch 毫秒可寫），對應舊測試的 `now` 缺省值無關緊要。
    let now_ms = Arc::new(AtomicU64::new(0));
    let clock: ReconcileClock = {
        let now_ms = Arc::clone(&now_ms);
        Arc::new(move || now_ms.load(Ordering::Relaxed))
    };
    let coordinator = DefaultReconcileCoordinator::new(ReconcileCoordinatorOptions {
        session_manager: Arc::clone(&manager) as Arc<dyn ReconcileSessionManager>,
        logger: Arc::clone(&logger) as Arc<dyn ReconcileLogger>,
        now: Some(clock),
        shutdown_timeout_ms: Some(500),
    });

    let active = channels(&["active"]);
    coordinator.request(&active);
    {
        let mut rx = manager.watch_receiver();
        await_call_count(&mut rx, 1).await;
    }

    let stop = tokio::spawn({
        let coordinator = coordinator.clone();
        async move {
            coordinator.stop("shutdown").await;
        }
    });
    let ignored = channels(&["ignored"]);
    coordinator.request(&ignored);

    // 舊測試：await vi.advanceTimersByTimeAsync(500); await expect(stop).resolves…
    // （逾時語意：grace period 到點就 resolve，不報錯。）
    tokio::time::advance(std::time::Duration::from_millis(500)).await;
    stop.await.expect("stop is not awaited with an error");
    // reconcile 只被呼叫過一次（'ignored' 被拒收或已被 stop 丟棄）。
    assert_eq!(lock(&manager.calls).len(), 1);
    // cancelPendingStarts 以 stop 的 reason（'shutdown'）呼叫。
    assert_eq!(
        lock(&manager.cancel_calls).clone(),
        vec!["shutdown".to_string()],
    );

    // 舊測試：reconcileGate.resolve(undefined); await coordinator.waitForIdle()
    // ——worker flight 照舊收尾，waitForIdle 可等到。
    manager.release_gate();
    coordinator.wait_for_idle().await;
}
