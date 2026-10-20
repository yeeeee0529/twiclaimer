//! ContainerRestartController 的單元測試：移植舊版
//! `test/unit/container-restart-controller.test.ts` 的 7 個測試。逾時以
//! tokio paused time 驅動（`start_paused = true` 時，等待中的逾時會由
//! runtime 自動推進，對應舊版 `vi.advanceTimersByTimeAsync`）。
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::{
    ContainerRestartController, ContainerRestartControllerOptions, ContainerRestartNotifyObserver,
    ContainerRestartRequest, NotifyOutcomeFuture, RestartLogger,
};
use crate::logging::logger::LogEvents;
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

/// 對應舊測試的 `error: vi.fn()`：記錄 (event, 欄位)。
type ErrorCall = (String, Option<Vec<(String, TestField)>>);

/// 欄位物件的複製（測試比對用；非物件欄位視為空）。
fn object_fields(value: &SanitizedValue) -> Vec<(String, TestField)> {
    match value {
        SanitizedValue::Object(entries) => entries
            .iter()
            .map(|(key, value)| (key.clone(), capture(value)))
            .collect(),
        other => panic!("預期欄位是物件：{other:?}"),
    }
}

/// 可設定的假 logger：error 記錄、flush 行為可選（立即成功／失敗／永不完成）。
#[derive(Clone)]
struct FakeRestartLogger {
    errors: Arc<std::sync::Mutex<Vec<ErrorCall>>>,
    flushes: Arc<AtomicUsize>,
    flush_result: FlushResult,
}

#[derive(Clone, Copy, PartialEq)]
enum FlushResult {
    Immediate,
    Failure,
    Pending,
}

impl FakeRestartLogger {
    fn new(flush_result: FlushResult) -> Self {
        Self {
            errors: Arc::new(std::sync::Mutex::new(Vec::new())),
            flushes: Arc::new(AtomicUsize::new(0)),
            flush_result,
        }
    }

    fn errors(&self) -> Vec<ErrorCall> {
        self.errors.lock().unwrap().clone()
    }

    fn flush_count(&self) -> usize {
        self.flushes.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl RestartLogger for FakeRestartLogger {
    fn error(&self, event: &str, fields: Option<&SanitizedValue>) {
        self.errors
            .lock()
            .unwrap()
            .push((event.to_string(), fields.map(object_fields)));
    }

    async fn flush(&self) -> io::Result<()> {
        self.flushes.fetch_add(1, Ordering::SeqCst);
        match self.flush_result {
            FlushResult::Immediate => Ok(()),
            FlushResult::Failure => Err(io::Error::other("flush failed")),
            FlushResult::Pending => std::future::pending().await,
        }
    }
}

/// 舊測試的 `exit: vi.fn()`：回傳 (注入用，記錄用) 的成對物件。
type ExitRecorder = (
    Arc<dyn Fn(i32) + Send + Sync>,
    Arc<std::sync::Mutex<Vec<i32>>>,
);

fn make_exit_recorder() -> ExitRecorder {
    let slots = Arc::new(std::sync::Mutex::new(Vec::new()));
    let slots_for_spy = Arc::clone(&slots);
    (
        Arc::new(move |code| slots_for_spy.lock().unwrap().push(code)),
        slots,
    )
}

fn create_controller(
    logger: FakeRestartLogger,
    exit: Arc<dyn Fn(i32) + Send + Sync>,
) -> ContainerRestartController {
    ContainerRestartController::new(ContainerRestartControllerOptions {
        logger: Arc::new(logger),
        exit: Some(exit),
        flush_timeout_ms: None,
        on_notify: None,
        notify_timeout_ms: None,
    })
}

fn request(reason: &str, source: &str) -> ContainerRestartRequest {
    ContainerRestartRequest::new(reason, source)
}

// 原測試：第一次 request 會 emit 一次事件、flush 後 exit(1)
#[tokio::test(start_paused = true)]
async fn first_request_emits_event_flushes_then_exits() {
    let logger = FakeRestartLogger::new(FlushResult::Immediate);
    let (exit, exit_slots) = make_exit_recorder();
    let controller = create_controller(logger.clone(), exit);

    controller
        .request(ContainerRestartRequest {
            reason: "emergency_memory".to_string(),
            source: "resource_guard".to_string(),
            fields: Some(vec![(
                "memoryCurrentBytes".to_string(),
                SanitizedValue::Number(1_000.0),
            )]),
        })
        .await;

    let errors = logger.errors();
    assert_eq!(errors.len(), 1);
    assert_eq!(
        errors[0],
        (
            LogEvents::CONTAINER_RESTART_REQUESTED.to_string(),
            Some(vec![
                (
                    "reason".to_string(),
                    TestField::Text("emergency_memory".to_string()),
                ),
                (
                    "source".to_string(),
                    TestField::Text("resource_guard".to_string()),
                ),
                ("memoryCurrentBytes".to_string(), TestField::Number(1_000.0)),
            ]),
        ),
    );
    assert_eq!(logger.flush_count(), 1);
    assert_eq!(*exit_slots.lock().unwrap(), vec![1]);
}

// 原測試：多來源同時 request 仍只 exit 一次
#[tokio::test(start_paused = true)]
async fn concurrent_requests_from_multiple_sources_exit_once() {
    let logger = FakeRestartLogger::new(FlushResult::Immediate);
    let (exit, exit_slots) = make_exit_recorder();
    let controller = Arc::new(create_controller(logger.clone(), exit));

    let ((), (), ()) = tokio::join!(
        controller.request(request("a", "one")),
        controller.request(request("b", "two")),
        controller.request(request("c", "three")),
    );

    assert_eq!(logger.errors().len(), 1);
    assert_eq!(*exit_slots.lock().unwrap(), vec![1]);
}

// 原測試：flush 逾時仍會 exit
#[tokio::test(start_paused = true)]
async fn flush_timeout_still_exits() {
    let logger = FakeRestartLogger::new(FlushResult::Pending);
    let (exit, exit_slots) = make_exit_recorder();
    let controller = Arc::new(ContainerRestartController::new(
        ContainerRestartControllerOptions {
            logger: Arc::new(logger),
            exit: Some(exit),
            flush_timeout_ms: Some(100.0),
            on_notify: None,
            notify_timeout_ms: None,
        },
    ));

    // 舊版 await vi.advanceTimersByTimeAsync(100)：paused time 下，等待中的
    // flush 逾時由 runtime 自動推進到期限。
    controller.request(request("stall", "scheduler")).await;
    assert_eq!(*exit_slots.lock().unwrap(), vec![1]);
}

// 原測試：flush 失敗仍會 exit
#[tokio::test(start_paused = true)]
async fn flush_failure_still_exits() {
    let logger = FakeRestartLogger::new(FlushResult::Failure);
    let (exit, exit_slots) = make_exit_recorder();
    let controller = create_controller(logger, exit);

    controller.request(request("x", "y")).await;
    assert_eq!(*exit_slots.lock().unwrap(), vec![1]);
}

// 原測試：忽略巢狀 fields 以避免意外帶入敏感物件
#[tokio::test(start_paused = true)]
async fn nested_fields_are_dropped() {
    let logger = FakeRestartLogger::new(FlushResult::Immediate);
    let (exit, _exit_slots) = make_exit_recorder();
    let controller = create_controller(logger.clone(), exit);

    controller
        .request(ContainerRestartRequest {
            reason: "x".to_string(),
            source: "y".to_string(),
            fields: Some(vec![
                (
                    "channel".to_string(),
                    SanitizedValue::String("ok".to_string()),
                ),
                (
                    "nested".to_string(),
                    SanitizedValue::Object(vec![(
                        "cookie".to_string(),
                        SanitizedValue::String("secret".to_string()),
                    )]),
                ),
            ]),
        })
        .await;

    let errors = logger.errors();
    assert_eq!(errors.len(), 1);
    assert_eq!(
        errors[0],
        (
            LogEvents::CONTAINER_RESTART_REQUESTED.to_string(),
            Some(vec![
                ("reason".to_string(), TestField::Text("x".to_string())),
                ("source".to_string(), TestField::Text("y".to_string())),
                ("channel".to_string(), TestField::Text("ok".to_string())),
            ]),
        ),
    );
}

// 原測試：exit 前會 best-effort 呼叫 onNotify，且 notify 逾時仍會 exit
#[tokio::test(start_paused = true)]
async fn notify_best_effort_and_notify_timeout_still_exits() {
    let logger = FakeRestartLogger::new(FlushResult::Immediate);
    let (exit, exit_slots) = make_exit_recorder();
    let notifies: Arc<std::sync::Mutex<Vec<RecordedRequest>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let notifies_for_observer = Arc::clone(&notifies);
    let on_notify: ContainerRestartNotifyObserver =
        Arc::new(move |observed: ContainerRestartRequest| {
            let fields = observed
                .fields
                .unwrap_or_default()
                .iter()
                .map(|(key, value)| (key.clone(), capture(value)))
                .collect();
            notifies_for_observer
                .lock()
                .unwrap()
                .push((observed.reason, observed.source, fields));
            // 舊版 `() => new Promise(() => undefined)`：永不完成。
            let pending: NotifyOutcomeFuture = Box::pin(std::future::pending());
            pending
        });
    let controller = Arc::new(ContainerRestartController::new(
        ContainerRestartControllerOptions {
            logger: Arc::new(logger.clone()),
            exit: Some(exit),
            flush_timeout_ms: Some(50.0),
            on_notify: Some(on_notify),
            notify_timeout_ms: Some(50.0),
        },
    ));

    controller
        .request(ContainerRestartRequest {
            reason: "browser_crash_loop".to_string(),
            source: "browser_manager".to_string(),
            fields: Some(vec![(
                "channel".to_string(),
                SanitizedValue::String("first".to_string()),
            )]),
        })
        .await;

    assert_eq!(
        *notifies.lock().unwrap(),
        vec![(
            "browser_crash_loop".to_string(),
            "browser_manager".to_string(),
            vec![("channel".to_string(), TestField::Text("first".to_string()))],
        )],
    );
    assert_eq!(logger.flush_count(), 1);
    assert_eq!(*exit_slots.lock().unwrap(), vec![1]);
}

// 原測試：onNotify 失敗仍會 flush 並 exit
#[tokio::test(start_paused = true)]
async fn notify_failure_still_flushes_and_exits() {
    let logger = FakeRestartLogger::new(FlushResult::Immediate);
    let (exit, exit_slots) = make_exit_recorder();
    let on_notify: ContainerRestartNotifyObserver =
        Arc::new(|_observed: ContainerRestartRequest| {
            let failed: NotifyOutcomeFuture = Box::pin(async { Err("notify failed".to_string()) });
            failed
        });
    let controller = ContainerRestartController::new(ContainerRestartControllerOptions {
        logger: Arc::new(logger.clone()),
        exit: Some(exit),
        flush_timeout_ms: None,
        on_notify: Some(on_notify),
        notify_timeout_ms: None,
    });

    controller
        .request(request("scheduler_stall", "scheduler_stall_watchdog"))
        .await;

    assert_eq!(logger.flush_count(), 1);
    assert_eq!(*exit_slots.lock().unwrap(), vec![1]);
}

// 額外驗證（對應舊版 `isRestartInFlight`）：首個 request 之後一直為 true。
#[tokio::test(start_paused = true)]
async fn restart_in_flight_stays_true_after_first_request() {
    let logger = FakeRestartLogger::new(FlushResult::Immediate);
    let (exit, _exit_slots) = make_exit_recorder();
    let controller = Arc::new(create_controller(logger, exit));

    assert!(!controller.is_restart_in_flight().await);
    controller.request(request("x", "y")).await;
    assert!(controller.is_restart_in_flight().await);
}
