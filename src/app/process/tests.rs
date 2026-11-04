//! `process.rs` 的測試：移植舊版 `test/unit/app-runner.test.ts` 第 43 列的
//! `未處理 rejection 設定 exitCode、記錄安全錯誤並停止服務`（X6）。
//!
//! X6：Rust 沒有 `unhandledRejection`；§7.9 的對應機制是 panic hook——記
//! `uncaught_exception {error}`、把 exit code 設為 1、再以
//! `uncaught_exception` 為 reason 停止服務。本測試以 `catch_unwind` 觸發
//! panic（hook 同步執行、unwind 被吞掉讓測試存活），斷言同一組行為與消毒。
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use super::{
    InstallProcessHandlersOptions, ProcessHandlerTarget, ProcessSignalReceiver, SignalDelivery,
    install_process_handlers,
};
use crate::app::runner::{AppRunner, RunnerLogger};
use crate::logging::redact::SanitizedValue;

/// 舊測試常數。
const ACCESS_TOKEN: &str = "highly-sensitive-access-token";

/// `vi.waitFor` 的輪詢對應（單元測試不等真實時間）。
async fn eventually(condition: impl Fn() -> bool) -> bool {
    for _ in 0..1_000 {
        if condition() {
            return true;
        }
        tokio::task::yield_now().await;
    }
    false
}

/// 斷言側的值：[`SanitizedValue`] → JSON 值（同 runner 測試的寫法）。
fn to_json(value: &SanitizedValue) -> serde_json::Value {
    serde_json::from_str(&value.to_json_string()).unwrap_or(serde_json::Value::Null)
}

// ---------------------------------------------------------------------------
// fake（舊測試的 FakeProcessTarget／logger／app；放測試模組內）
// ---------------------------------------------------------------------------

/// 舊測試 `FakeProcessTarget`（本測試只需 exit code 與註冊／撤銷）。
struct FakeProcessTarget {
    exit_code: Mutex<Option<i32>>,
    subscriptions: Mutex<Vec<tokio::sync::mpsc::UnboundedSender<super::ProcessSignal>>>,
}

impl FakeProcessTarget {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            exit_code: Mutex::new(None),
            subscriptions: Mutex::new(Vec::new()),
        })
    }

    fn exit_code(&self) -> Option<i32> {
        *self
            .exit_code
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl ProcessHandlerTarget for FakeProcessTarget {
    fn subscribe(&self) -> ProcessSignalReceiver {
        // 訊號來源只在 fake 內保存 sender（本測試不 emit；receiver 停在等待）。
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        self.subscriptions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(sender);
        ProcessSignalReceiver::new(Arc::new(ChannelDelivery {
            receiver: tokio::sync::Mutex::new(receiver),
        }) as Arc<dyn SignalDelivery>)
    }

    fn remove(&self) {
        self.subscriptions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }

    fn set_exit_code(&self, code: i32) {
        *self
            .exit_code
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(code);
    }
}

/// fake 的訊號來源：channel 的接收端。
struct ChannelDelivery {
    receiver: tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<super::ProcessSignal>>,
}

#[async_trait]
impl SignalDelivery for ChannelDelivery {
    async fn next_signal(&self) -> Option<super::ProcessSignal> {
        self.receiver.lock().await.recv().await
    }
}

/// 舊 `createTestLogger([])`：記錄 error 呼叫（斷言用 serialized）。
struct FakeRunnerLogger {
    entries: Mutex<Vec<(String, serde_json::Value)>>,
}

impl FakeRunnerLogger {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            entries: Mutex::new(Vec::new()),
        })
    }

    /// 對應 `JSON.stringify(logger.error.mock.calls)`。
    fn serialized(&self) -> String {
        let entries = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        serde_json::to_string(&*entries).unwrap_or_default()
    }
}

#[async_trait]
impl RunnerLogger for FakeRunnerLogger {
    fn debug(&self, _event: &str, _fields: Option<&SanitizedValue>) {}
    fn info(&self, _event: &str, _fields: Option<&SanitizedValue>) {}
    fn warn(&self, _event: &str, _fields: Option<&SanitizedValue>) {}

    fn error(&self, event: &str, fields: Option<&SanitizedValue>) {
        let value = match fields {
            Some(fields) => to_json(fields),
            None => serde_json::Value::Object(serde_json::Map::new()),
        };
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push((event.to_string(), value));
    }

    async fn flush(&self) -> Result<(), String> {
        Ok(())
    }
}

/// 舊測試的 fake `app`（`start`／`stop` 的 vi.fn）。
struct FakeApp {
    stop_reasons: Mutex<Vec<String>>,
}

impl FakeApp {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            stop_reasons: Mutex::new(Vec::new()),
        })
    }

    fn stop_reasons(&self) -> Vec<String> {
        self.stop_reasons
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

#[async_trait]
impl AppRunner for FakeApp {
    async fn start(&self) -> Result<(), String> {
        Ok(())
    }

    async fn stop(&self, reason: &str) -> Result<(), String> {
        self.stop_reasons
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(reason.to_string());
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 測試本體
// ---------------------------------------------------------------------------

// 原測試：未處理 rejection 設定 exitCode、記錄安全錯誤並停止服務
// （X6：Rust 無 unhandledRejection；§7.9 以 panic hook 為對應機制，
// reason 為 `uncaught_exception`。）
#[tokio::test]
async fn fatal_error_sets_exit_code_logs_safe_error_and_stops_service() {
    let target = FakeProcessTarget::new();
    let logger = FakeRunnerLogger::new();
    let app = FakeApp::new();
    let remove_handlers = install_process_handlers(InstallProcessHandlersOptions {
        app: Arc::clone(&app) as Arc<dyn AppRunner>,
        logger: Arc::clone(&logger) as Arc<dyn RunnerLogger>,
        target: Some(Arc::clone(&target) as Arc<dyn ProcessHandlerTarget>),
    });

    // 舊 `target.emit('unhandledRejection', new Error(...))` 的對應：觸發
    // panic（hook 同步執行；catch_unwind 吞掉 unwind 讓測試存活）。
    let message = format!("access_token={ACCESS_TOKEN}");
    let _ = std::panic::catch_unwind(move || panic!("{message}"));

    assert!(
        eventually(|| app.stop_reasons() == vec!["uncaught_exception".to_string()]).await,
        "stop 未以 uncaught_exception 呼叫",
    );
    assert_eq!(target.exit_code(), Some(1));
    let logged = logger.serialized();
    assert!(!logged.contains(ACCESS_TOKEN), "記錄不得帶 token：{logged}");
    assert!(
        logged.contains("[REDACTED]"),
        "記錄需帶 [REDACTED]：{logged}"
    );

    remove_handlers();
}
