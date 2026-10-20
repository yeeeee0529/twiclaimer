//! SchedulerStallWatchdog 的單元測試：移植舊版
//! `test/unit/scheduler-stall-watchdog.test.ts` 的 3 個測試。計時以 tokio
//! paused time 驅動（對應舊版 fake timers；`now` 照舊測試另外注入）。
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use super::{
    SchedulerSnapshot, SchedulerStallWatchdog, SchedulerStallWatchdogOptions,
    StallRestartRequester, StallSchedulerSource, StallWatchdogLogger,
};
use crate::app::restart::ContainerRestartRequest;
use crate::logging::redact::SanitizedValue;

/// 測試用的欄位值快照：`SanitizedValue` 的可 Send 子集（記錄時就地轉換，
/// 讓假 logger 可以是 `Send + Sync`）。
#[derive(Clone, Debug, PartialEq)]
enum TestField {
    Null,
    Undefined,
    Bool(bool),
    Number(f64),
    Text(String),
    Other,
}

/// 記錄一次請求的欄位（event, subject, fields）。
type RecordedRequest = (String, String, Vec<(String, TestField)>);

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

/// 讓測試能改 `checkInFlight` 的假 scheduler（對應舊測試的 closure 變數）。
struct FakeScheduler {
    check_in_flight: Arc<AtomicBool>,
    last_checked_at: Option<&'static str>,
    get_snapshot_calls: Arc<AtomicUsize>,
}

impl FakeScheduler {
    fn new(check_in_flight: Arc<AtomicBool>) -> (Self, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        (
            Self {
                check_in_flight,
                last_checked_at: Some("2026-06-14T00:00:00.000Z"),
                get_snapshot_calls: Arc::clone(&calls),
            },
            calls,
        )
    }
}

impl StallSchedulerSource for FakeScheduler {
    fn get_snapshot(&self) -> SchedulerSnapshot {
        self.get_snapshot_calls.fetch_add(1, Ordering::SeqCst);
        SchedulerSnapshot {
            running: true,
            check_in_flight: self.check_in_flight.load(Ordering::SeqCst),
            retry_at: None,
            last_checked_at: self.last_checked_at.map(str::to_string),
        }
    }
}

/// 記錄 error 與 flush 的假 logger（對應舊測試的 inline 物件）。
/// 記錄一次錯誤的欄位（message, fields）。
type RecordedError = (String, Vec<(String, TestField)>);

#[derive(Default, Clone)]
struct FakeWatchdogLogger {
    errors: Arc<std::sync::Mutex<Vec<RecordedError>>>,
    infos: Arc<std::sync::Mutex<Vec<String>>>,
    flushes: Arc<AtomicUsize>,
}
#[async_trait::async_trait]
impl StallWatchdogLogger for FakeWatchdogLogger {
    fn info(&self, event: &str, _fields: Option<&SanitizedValue>) {
        self.infos.lock().unwrap().push(event.to_string());
    }

    fn error(&self, event: &str, fields: Option<&SanitizedValue>) {
        let fields = match fields {
            Some(SanitizedValue::Object(entries)) => entries
                .iter()
                .map(|(key, value)| (key.clone(), capture(value)))
                .collect(),
            Some(other) => panic!("預期欄位是物件：{other:?}"),
            None => Vec::new(),
        };
        self.errors
            .lock()
            .unwrap()
            .push((event.to_string(), fields));
    }

    async fn flush(&self) -> std::io::Result<()> {
        self.flushes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

impl FakeWatchdogLogger {
    fn error(&self, event: &str) -> Option<Vec<(String, TestField)>> {
        self.errors
            .lock()
            .unwrap()
            .iter()
            .find(|(name, _)| name == event)
            .map(|(_, fields)| fields.clone())
    }
}

/// 舊測試注入 `exit: vi.fn()` 的記錄器。
struct ExitSpy {
    calls: Arc<std::sync::Mutex<Vec<i32>>>,
}

impl ExitSpy {
    fn new() -> (Arc<dyn Fn(i32) + Send + Sync>, Self) {
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        (
            {
                let calls = Arc::clone(&calls);
                Arc::new(move |code| calls.lock().unwrap().push(code))
            },
            Self { calls },
        )
    }

    fn count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
}

/// 舊測試 `containerRestartController: { request }` 的假物件（同步記錄；
/// 欄位值在記錄時轉成可 Send 的快照）。
struct RecordingRequester {
    calls: Arc<std::sync::Mutex<Vec<RecordedRequest>>>,
}

impl StallRestartRequester for RecordingRequester {
    fn request(&self, request: ContainerRestartRequest) {
        let fields = match request.fields {
            Some(fields) => fields
                .iter()
                .map(|(key, value)| (key.clone(), capture(value)))
                .collect(),
            None => Vec::new(),
        };
        self.calls
            .lock()
            .unwrap()
            .push((request.reason, request.source, fields));
    }
}

impl RecordingRequester {
    fn new() -> (Arc<Self>, Arc<std::sync::Mutex<Vec<RecordedRequest>>>) {
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        (
            Arc::new(Self {
                calls: Arc::clone(&calls),
            }),
            calls,
        )
    }
}

/// 推進 paused time 後讓工作迴圈被排程執行（對應舊版 fake timer 的觸發）。
async fn pump() {
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
}

fn shared_clock() -> (Arc<AtomicU64>, super::StallClock) {
    let now_ms = Arc::new(AtomicU64::new(0));
    let clock = Arc::clone(&now_ms);
    (now_ms, Arc::new(move || clock.load(Ordering::SeqCst)))
}

// 原測試：checkInFlight 持續超過門檻時透過 ContainerRestartController 要求重啟
#[tokio::test(start_paused = true)]
async fn sustained_in_flight_requests_restart_via_controller() {
    let check_in_flight = Arc::new(AtomicBool::new(true));
    let (scheduler, _snapshot_calls) = FakeScheduler::new(Arc::clone(&check_in_flight));
    let logger = FakeWatchdogLogger::default();
    let (requester, requester_calls) = RecordingRequester::new();
    let (now_ms, now) = shared_clock();

    let watchdog = SchedulerStallWatchdog::new(SchedulerStallWatchdogOptions {
        scheduler: Arc::new(scheduler),
        logger: Arc::new(logger.clone()),
        interval_seconds: 10.0,
        stall_threshold_ms: 30_000.0,
        now: Some(now),
        container_restart_controller: Some(requester),
        exit: None,
    })
    .unwrap_or_else(|error| panic!("建構 watchdog 應成功：{error}"));

    watchdog.start().await;
    now_ms.store(20_000, Ordering::SeqCst);
    tokio::time::advance(std::time::Duration::from_secs(10)).await;
    pump().await;
    assert!(requester_calls.lock().unwrap().is_empty());

    now_ms.store(30_000, Ordering::SeqCst);
    tokio::time::advance(std::time::Duration::from_secs(10)).await;
    pump().await;
    // 舊版 await vi.waitFor(...)：request 的呼叫在工作迴圈同步段落內完成。
    assert_eq!(
        *requester_calls.lock().unwrap(),
        vec![(
            "scheduler_stall".to_string(),
            "scheduler_stall_watchdog".to_string(),
            vec![
                (
                    "inFlightDurationMs".to_string(),
                    TestField::Number(30_000.0)
                ),
                ("stallThresholdMs".to_string(), TestField::Number(30_000.0)),
            ],
        )],
    );

    assert_eq!(
        logger.error("scheduler_stall_detected"),
        Some(vec![
            (
                "inFlightDurationMs".to_string(),
                TestField::Number(30_000.0)
            ),
            ("stallThresholdMs".to_string(), TestField::Number(30_000.0)),
            ("running".to_string(), TestField::Bool(true)),
            (
                "lastCheckedAt".to_string(),
                TestField::Text("2026-06-14T00:00:00.000Z".to_string()),
            ),
            // 舊版 retryAt: undefined（不存在）。
            ("retryAt".to_string(), TestField::Undefined),
        ]),
    );

    check_in_flight.store(false, Ordering::SeqCst);
    watchdog.stop().await;
}

// 原測試：checkInFlight 解除後會重置卡住計時
#[tokio::test(start_paused = true)]
async fn in_flight_reset_after_release() {
    let check_in_flight = Arc::new(AtomicBool::new(true));
    let (scheduler, _snapshot_calls) = FakeScheduler::new(Arc::clone(&check_in_flight));
    let logger = FakeWatchdogLogger::default();
    let (exit_spy_fn, exit_spy) = ExitSpy::new();
    let (now_ms, now) = shared_clock();

    let watchdog = SchedulerStallWatchdog::new(SchedulerStallWatchdogOptions {
        scheduler: Arc::new(scheduler),
        logger: Arc::new(logger),
        interval_seconds: 10.0,
        stall_threshold_ms: 30_000.0,
        now: Some(now),
        container_restart_controller: None,
        exit: Some(exit_spy_fn),
    })
    .unwrap_or_else(|error| panic!("建構 watchdog 應成功：{error}"));

    watchdog.start().await;
    now_ms.store(20_000, Ordering::SeqCst);
    tokio::time::advance(std::time::Duration::from_secs(10)).await;
    pump().await;
    check_in_flight.store(false, Ordering::SeqCst);
    now_ms.store(25_000, Ordering::SeqCst);
    tokio::time::advance(std::time::Duration::from_secs(10)).await;
    pump().await;
    check_in_flight.store(true, Ordering::SeqCst);
    now_ms.store(50_000, Ordering::SeqCst);
    tokio::time::advance(std::time::Duration::from_secs(10)).await;
    pump().await;

    assert_eq!(exit_spy.count(), 0);
    watchdog.stop().await;
}

// 原測試：stop 後不再檢查 scheduler 狀態
#[tokio::test(start_paused = true)]
async fn no_scheduler_checks_after_stop() {
    let check_in_flight = Arc::new(AtomicBool::new(true));
    let (scheduler, snapshot_calls) = FakeScheduler::new(Arc::clone(&check_in_flight));
    let logger = FakeWatchdogLogger::default();
    let (exit_spy_fn, exit_spy) = ExitSpy::new();
    let (_now_ms, now) = shared_clock();

    let watchdog = SchedulerStallWatchdog::new(SchedulerStallWatchdogOptions {
        scheduler: Arc::new(scheduler),
        logger: Arc::new(logger),
        interval_seconds: 10.0,
        stall_threshold_ms: 30_000.0,
        now: Some(now),
        container_restart_controller: None,
        exit: Some(exit_spy_fn),
    })
    .unwrap_or_else(|error| panic!("建構 watchdog 應成功：{error}"));

    watchdog.start().await;
    watchdog.stop().await;
    tokio::time::advance(std::time::Duration::from_secs(60)).await;
    pump().await;

    assert_eq!(snapshot_calls.load(Ordering::SeqCst), 1);
    assert_eq!(exit_spy.count(), 0);
}
