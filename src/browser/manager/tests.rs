//! `DefaultBrowserManager` 的單元測試：移植舊版
//! `test/unit/browser-manager.test.ts` 的 49 個案例。
//!
//! fake 行為照舊測試的 mock：`MockPageAdapter`／`MockContextAdapter`／
//! `MockBrowserAdapter`／`MockLauncher`（listener 集合、close 失敗佇列、
//! 「永遠不完成」的 closeImplementation、deferred 的 newPage）。時間的對應：
//! 舊測試的 `vi.useFakeTimers` → tokio 的 paused 計時（睡眠與逾時即時推進、
//! 單元測試不需等真實時間）；`now` 注入 → 共用時鐘的 closure；`vi.waitFor` →
//! [`eventually`] 輪詢。
//!
//! 日誌欄位的斷言對應：`toEqual` → [`object_equals`]（鍵序不敏感的完整相等）、
//! `expect.objectContaining` → [`object_contains`]、`not.toContain` → 對收集
//! 內容的 Debug 序列做子字串檢查。
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::oneshot;

use super::{BrowserManager, DefaultBrowserManager};
use crate::browser::cdp::context::DriverError;
use crate::browser::cdp::context::ResourceBlockingOptions;
use crate::browser::cdp::launcher::CancelListener;
use crate::browser::cdp::page::PageShared;
use crate::browser::page_adapter::{
    BrowserConsoleDiagnostic, BrowserHttpResponseDiagnostic, BrowserRequestFailureDiagnostic,
};
use crate::browser::types::{
    BrowserAdapter, BrowserContextAdapter, BrowserContextOptions, BrowserFatalRecoveryObserver,
    BrowserInvalidation, BrowserInvalidationObserver, BrowserInvalidationReason,
    BrowserLaunchOptions, BrowserLauncher, BrowserManagerBrowserConfig, BrowserManagerConfig,
    BrowserManagerDependencies, BrowserManagerLogger, BrowserManagerRecoveryConfig,
    BrowserNavigationOutcome, BrowserNavigationStatus, BrowserPage, BrowserPageAdapter,
    BrowserRestartedEvent, BrowserRestartedObserver, BrowserViewport, FatalRecoveryRequest,
    ManagerClock, SleepFn,
};
use crate::logging::logger::LogLevel;
use crate::logging::redact::SanitizedValue;

/// 舊測試的 `STORAGE_STATE_PATH`。
const STORAGE_STATE_PATH: &str = "/private/credentials/storage-state.json";

// ---------------------------------------------------------------------------
// 斷言輔助
// ---------------------------------------------------------------------------

fn v_str(value: impl Into<String>) -> SanitizedValue {
    SanitizedValue::String(value.into())
}

fn v_num(value: f64) -> SanitizedValue {
    SanitizedValue::Number(value)
}

fn v_bool(value: bool) -> SanitizedValue {
    SanitizedValue::Bool(value)
}

fn v_strs(values: &[&str]) -> SanitizedValue {
    SanitizedValue::Array(
        values
            .iter()
            .map(|value| SanitizedValue::String((*value).to_string()))
            .collect(),
    )
}

/// 對應 `expect(value).toEqual({...})`：物件鍵序不敏感的完整相等。
fn object_equals(expected: &[(&str, SanitizedValue)], actual: &serde_json::Value) -> bool {
    let Some(entries) = actual.as_object() else {
        return false;
    };
    entries.len() == expected.len()
        && expected
            .iter()
            .all(|(key, value)| entries.get(*key) == Some(&to_json(value)))
}

/// 對應 `expect.objectContaining({...})`：expected 的每個鍵值都要出現。
fn object_contains(expected: &[(&str, SanitizedValue)], actual: &serde_json::Value) -> bool {
    let Some(entries) = actual.as_object() else {
        return false;
    };
    expected
        .iter()
        .all(|(key, value)| entries.get(*key) == Some(&to_json(value)))
}

/// 對應舊測試的 `vi.waitFor`：輪詢條件；paused 計時下 sleep 立即推進，
/// 背景任務在每輪之間執行。
async fn eventually(condition: impl Fn() -> bool) -> bool {
    for _ in 0..1_000 {
        if condition() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
        tokio::task::yield_now().await;
    }
    false
}

async fn assert_eventually(message: &str, condition: impl Fn() -> bool) {
    assert!(eventually(condition).await, "{message}");
}

/// 對應 `rejects.toThrow(訊息)`：錯誤訊息包含指定文字。
fn error_message_contains<T>(result: &Result<T, DriverError>, needle: &str) -> bool {
    match result {
        Err(error) => error
            .to_string()
            .to_lowercase()
            .contains(&needle.to_lowercase()),
        Ok(_) => false,
    }
}

// ---------------------------------------------------------------------------
// 日誌收集（對應舊測試的 createLogger）
// ---------------------------------------------------------------------------

/// 對應舊測試 `createLogger` 的收集 logger：記錄（層級、事件、欄位）。
///
/// 欄位以 `serde_json::Value` 儲存（`SanitizedValue` 內含 `Rc`，不可 Send；
/// logger trait 要求 Send + Sync）。記錄時經 [`SanitizedValue::to_json_string`]
/// 轉一層，數字的字串化形狀兩側一致（ryu-js）。
struct FakeLogger {
    entries: Mutex<Vec<(LogLevel, String, serde_json::Value)>>,
}

/// 斷言側的值：SanitizedValue → JSON 值（與 logger 儲存的形狀相同）。
fn to_json(value: &SanitizedValue) -> serde_json::Value {
    serde_json::from_str(&value.to_json_string()).unwrap_or(serde_json::Value::Null)
}

impl FakeLogger {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            entries: Mutex::new(Vec::new()),
        })
    }

    fn record(&self, level: LogLevel, event: &str, fields: Option<&SanitizedValue>) {
        let json = match fields {
            Some(fields) => to_json(fields),
            None => serde_json::Value::Object(serde_json::Map::new()),
        };
        self.entries
            .lock()
            .unwrap()
            .push((level, event.to_string(), json));
    }

    /// 特定層級＋事件的欄位清單（對應 `vi.mocked(logger.warn).mock.calls` 過濾）。
    fn fields_of(&self, level: LogLevel, event: &str) -> Vec<serde_json::Value> {
        self.entries
            .lock()
            .unwrap()
            .iter()
            .filter(|(entry_level, entry_event, _)| *entry_level == level && entry_event == event)
            .map(|(_, _, fields)| fields.clone())
            .collect()
    }

    /// 特定層級的呼叫次數（對應 `mock.calls.length`）。
    fn call_count(&self, level: LogLevel) -> usize {
        self.entries
            .lock()
            .unwrap()
            .iter()
            .filter(|(entry_level, _, _)| *entry_level == level)
            .count()
    }

    /// 特定層級＋事件是否被呼叫過（對應 `toHaveBeenCalledWith`）。
    fn called_with(
        &self,
        level: LogLevel,
        event: &str,
        expected: &[(&str, SanitizedValue)],
    ) -> bool {
        self.fields_of(level, event)
            .iter()
            .any(|fields| object_contains(expected, fields))
    }

    /// 特定層級＋事件是否以完整欄位被呼叫（對應 `toHaveBeenCalledWith(event, {...})`
    /// 的嚴格相等）。
    fn called_exactly_with(
        &self,
        level: LogLevel,
        event: &str,
        expected: &[(&str, SanitizedValue)],
    ) -> bool {
        self.fields_of(level, event)
            .iter()
            .any(|fields| object_equals(expected, fields))
    }

    /// 特定事件是否「未被」以任何欄位呼叫（對應 `not.toHaveBeenCalledWith(event, …)`）。
    fn never_called_with(&self, level: LogLevel, event: &str) -> bool {
        self.fields_of(level, event).is_empty()
    }

    /// 全部內容的序列化（對應 `JSON.stringify(mock.calls)` 的洩漏檢查）。
    fn serialized(&self) -> String {
        format!("{:?}", self.entries.lock().unwrap())
    }
}

impl BrowserManagerLogger for FakeLogger {
    fn debug(&self, event: &str, fields: Option<&SanitizedValue>) {
        self.record(LogLevel::Debug, event, fields);
    }

    fn info(&self, event: &str, fields: Option<&SanitizedValue>) {
        self.record(LogLevel::Info, event, fields);
    }

    fn warn(&self, event: &str, fields: Option<&SanitizedValue>) {
        self.record(LogLevel::Warn, event, fields);
    }

    fn error(&self, event: &str, fields: Option<&SanitizedValue>) {
        self.record(LogLevel::Error, event, fields);
    }
}

// ---------------------------------------------------------------------------
// fake 的 listener 清單
// ---------------------------------------------------------------------------

type SharedListenerList<E> = Arc<Mutex<Vec<Arc<dyn Fn(&E) + Send + Sync>>>>;

/// 假 adapter 的 listener 清單：註冊回傳取消函式（以 Arc 識別移除）。
struct FakeListeners<E>(SharedListenerList<E>);

impl<E: 'static> FakeListeners<E> {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(Vec::new())))
    }

    fn subscribe(&self, listener: impl Fn(&E) + Send + Sync + 'static) -> CancelListener {
        let entry: Arc<dyn Fn(&E) + Send + Sync> = Arc::new(listener);
        self.0.lock().unwrap().push(Arc::clone(&entry));
        let listeners = Arc::clone(&self.0);
        Box::new(move || {
            listeners
                .lock()
                .unwrap()
                .retain(|registered| !Arc::ptr_eq(registered, &entry));
        })
    }

    fn fire(&self, event: &E) {
        let listeners = self.0.lock().unwrap().clone();
        for listener in listeners {
            listener(event);
        }
    }
}

/// 舊測試 deferred 的接收端型別（`createDeferred<BrowserPageAdapter>`）。
type DeferredPageReceiver = oneshot::Receiver<Result<Arc<MockPageAdapter>, DriverError>>;

/// 無參數 listener 的清單（對應舊測試的 `Set<() => void>`）。
type FakeUnitListeners = FakeListeners<()>;

impl FakeListeners<()> {
    fn subscribe_unit(&self, listener: Box<dyn Fn() + Send + Sync>) -> CancelListener {
        self.subscribe(move |_event: &()| listener())
    }

    fn fire_unit(&self) {
        self.fire(&());
    }
}

// ---------------------------------------------------------------------------
// fake page（對應舊測試的 MockPageAdapter）
// ---------------------------------------------------------------------------

/// 頁面把手的 fake（對應舊測試 `page.page` 的 `{mockPageName}` 識別物件）。
struct FakePageHandle {
    name: String,
    closed: AtomicBool,
}

#[async_trait]
impl BrowserPage for FakePageHandle {
    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    async fn close(&self) -> Result<(), DriverError> {
        self.closed.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn as_page_shared(&self) -> Option<Arc<PageShared>> {
        None
    }
}

/// popup 的 fake（對應 `{isClosed: () => false, close: popupClose}`）。
struct FakePopupPage {
    closed: AtomicBool,
    close_calls: AtomicUsize,
}

impl FakePopupPage {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            closed: AtomicBool::new(false),
            close_calls: AtomicUsize::new(0),
        })
    }

    fn close_calls(&self) -> usize {
        self.close_calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl BrowserPage for FakePopupPage {
    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    async fn close(&self) -> Result<(), DriverError> {
        self.close_calls.fetch_add(1, Ordering::SeqCst);
        self.closed.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn as_page_shared(&self) -> Option<Arc<PageShared>> {
        None
    }
}

/// 舊測試的 `MockPageAdapter`。
struct MockPageAdapter {
    /// 舊版 `page` 屬性（穩定把手，識別相等）。
    page: Arc<dyn BrowserPage>,
    close_calls: AtomicUsize,
    /// 舊版 `closeImplementation` 的「永遠不完成」情況。
    close_hangs: AtomicBool,
    /// 舊版 `closeFailures` 佇列。
    close_failures: Mutex<VecDeque<DriverError>>,
    closed: AtomicBool,
    crash_listeners: FakeUnitListeners,
    close_listeners: FakeUnitListeners,
    popup_listeners: FakeListeners<Arc<dyn BrowserPage>>,
    request_failed_listeners: FakeListeners<BrowserRequestFailureDiagnostic>,
    response_listeners: FakeListeners<BrowserHttpResponseDiagnostic>,
    console_listeners: FakeListeners<BrowserConsoleDiagnostic>,
}

impl MockPageAdapter {
    fn new(name: &str) -> Arc<Self> {
        Arc::new(Self {
            page: Arc::new(FakePageHandle {
                name: name.to_string(),
                closed: AtomicBool::new(false),
            }) as Arc<dyn BrowserPage>,
            close_calls: AtomicUsize::new(0),
            close_hangs: AtomicBool::new(false),
            close_failures: Mutex::new(VecDeque::new()),
            closed: AtomicBool::new(false),
            crash_listeners: FakeUnitListeners::new(),
            close_listeners: FakeUnitListeners::new(),
            popup_listeners: FakeListeners::new(),
            request_failed_listeners: FakeListeners::new(),
            response_listeners: FakeListeners::new(),
            console_listeners: FakeListeners::new(),
        })
    }

    /// 舊測試 `closeImplementation = () => new Promise(() => undefined)`。
    fn hang_on_close(&self) {
        self.close_hangs.store(true, Ordering::SeqCst);
    }

    fn fail_next_close(&self, error: DriverError) {
        self.close_failures.lock().unwrap().push_back(error);
    }

    fn close_calls(&self) -> usize {
        self.close_calls.load(Ordering::SeqCst)
    }

    fn emit_crash(&self) {
        self.crash_listeners.fire_unit();
    }

    fn emit_unexpected_close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.close_listeners.fire_unit();
    }

    fn emit_popup(&self, popup: Arc<dyn BrowserPage>) {
        self.popup_listeners.fire(&popup);
    }

    fn emit_request_failed(&self, diagnostic: BrowserRequestFailureDiagnostic) {
        self.request_failed_listeners.fire(&diagnostic);
    }

    fn emit_response(&self, diagnostic: BrowserHttpResponseDiagnostic) {
        self.response_listeners.fire(&diagnostic);
    }

    fn emit_console(&self, diagnostic: BrowserConsoleDiagnostic) {
        self.console_listeners.fire(&diagnostic);
    }
}

#[async_trait]
impl BrowserPageAdapter for MockPageAdapter {
    fn page(&self) -> Arc<dyn BrowserPage> {
        Arc::clone(&self.page)
    }

    async fn close(&self) -> Result<(), DriverError> {
        self.close_calls.fetch_add(1, Ordering::SeqCst);
        if self.close_hangs.load(Ordering::SeqCst) {
            // 舊測試的 closeImplementation：永遠不完成（等呼叫端逾時）。
            return futures_util::future::pending::<Result<(), DriverError>>().await;
        }
        if let Some(failure) = self.close_failures.lock().unwrap().pop_front() {
            return Err(failure);
        }
        self.closed.store(true, Ordering::SeqCst);
        self.close_listeners.fire_unit();
        Ok(())
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    fn on_crash(&self, listener: Box<dyn Fn() + Send + Sync>) -> CancelListener {
        self.crash_listeners.subscribe_unit(listener)
    }

    fn on_close(&self, listener: Box<dyn Fn() + Send + Sync>) -> CancelListener {
        self.close_listeners.subscribe_unit(listener)
    }

    fn on_popup(
        &self,
        listener: Box<dyn Fn(Arc<dyn BrowserPage>) + Send + Sync>,
    ) -> CancelListener {
        self.popup_listeners
            .subscribe(move |popup: &Arc<dyn BrowserPage>| listener(Arc::clone(popup)))
    }

    fn on_request_failed(
        &self,
        listener: Box<dyn Fn(&BrowserRequestFailureDiagnostic) + Send + Sync>,
    ) -> CancelListener {
        self.request_failed_listeners.subscribe(listener)
    }

    fn on_response(
        &self,
        listener: Box<dyn Fn(&BrowserHttpResponseDiagnostic) + Send + Sync>,
    ) -> CancelListener {
        self.response_listeners.subscribe(listener)
    }

    fn on_console(
        &self,
        listener: Box<dyn Fn(&BrowserConsoleDiagnostic) + Send + Sync>,
    ) -> CancelListener {
        self.console_listeners.subscribe(listener)
    }
}

// ---------------------------------------------------------------------------
// fake context（對應舊測試的 MockContextAdapter）
// ---------------------------------------------------------------------------

/// 舊測試的 `MockContextAdapter`。
struct MockContextAdapter {
    new_page_calls: AtomicUsize,
    /// 舊版 `newPageImplementation`（deferred 的接收端；take 一次）。
    new_page_deferred: Mutex<Option<DeferredPageReceiver>>,
    /// 舊版 `pageResults` 佇列。
    page_results: Mutex<VecDeque<Result<Arc<MockPageAdapter>, DriverError>>>,
    resource_blocking_calls: Mutex<Vec<ResourceBlockingOptions>>,
    chat_blocking_calls: Mutex<Vec<bool>>,
    close_calls: AtomicUsize,
    close_failures: Mutex<VecDeque<DriverError>>,
}

impl MockContextAdapter {
    fn new(page_results: Vec<Result<Arc<MockPageAdapter>, DriverError>>) -> Arc<Self> {
        Arc::new(Self {
            new_page_calls: AtomicUsize::new(0),
            new_page_deferred: Mutex::new(None),
            page_results: Mutex::new(page_results.into()),
            resource_blocking_calls: Mutex::new(Vec::new()),
            chat_blocking_calls: Mutex::new(Vec::new()),
            close_calls: AtomicUsize::new(0),
            close_failures: Mutex::new(VecDeque::new()),
        })
    }

    fn new_deferred() -> (
        Arc<Self>,
        oneshot::Sender<Result<Arc<MockPageAdapter>, DriverError>>,
    ) {
        let (sender, receiver) = oneshot::channel();
        let context = Self::new(Vec::new());
        *context.new_page_deferred.lock().unwrap() = Some(receiver);
        (context, sender)
    }

    fn fail_next_close(&self, error: DriverError) {
        self.close_failures.lock().unwrap().push_back(error);
    }

    fn new_page_calls(&self) -> usize {
        self.new_page_calls.load(Ordering::SeqCst)
    }

    fn close_calls(&self) -> usize {
        self.close_calls.load(Ordering::SeqCst)
    }

    fn chat_blocking_calls(&self) -> Vec<bool> {
        self.chat_blocking_calls.lock().unwrap().clone()
    }

    fn resource_blocking_calls(&self) -> Vec<ResourceBlockingOptions> {
        self.resource_blocking_calls.lock().unwrap().clone()
    }

    fn context_options(browser: &Arc<MockBrowserAdapter>) -> Vec<BrowserContextOptions> {
        browser.context_options.lock().unwrap().clone()
    }
}

#[async_trait]
impl BrowserContextAdapter for MockContextAdapter {
    async fn configure_resource_blocking(
        &self,
        options: ResourceBlockingOptions,
    ) -> Result<(), DriverError> {
        self.resource_blocking_calls.lock().unwrap().push(options);
        Ok(())
    }

    async fn configure_chat_blocking(&self, enabled: bool) -> Result<(), DriverError> {
        self.chat_blocking_calls.lock().unwrap().push(enabled);
        Ok(())
    }

    async fn new_page(&self) -> Result<Arc<dyn BrowserPageAdapter>, DriverError> {
        self.new_page_calls.fetch_add(1, Ordering::SeqCst);
        // take 在鎖內完成（鎖不跨 await）。
        let deferred = self.new_page_deferred.lock().unwrap().take();
        if let Some(receiver) = deferred {
            // 舊測試的 deferred：等測試端 resolve。
            return receiver
                .await
                .unwrap_or_else(|_| Err(DriverError::new("Error", "deferred dropped")))
                .map(|page| page as Arc<dyn BrowserPageAdapter>);
        }
        if let Some(result) = self.page_results.lock().unwrap().pop_front() {
            return result.map(|page| page as Arc<dyn BrowserPageAdapter>);
        }
        // 舊測試預設：`generated-${calls}`。
        let name = format!("generated-{}", self.new_page_calls.load(Ordering::SeqCst));
        Ok(MockPageAdapter::new(&name) as Arc<dyn BrowserPageAdapter>)
    }

    async fn close(&self) -> Result<(), DriverError> {
        self.close_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(failure) = self.close_failures.lock().unwrap().pop_front() {
            return Err(failure);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// fake browser（對應舊測試的 MockBrowserAdapter）
// ---------------------------------------------------------------------------

/// 舊測試的 `MockBrowserAdapter`。
struct MockBrowserAdapter {
    /// 舊版建構參數 `contextResult`（context 或建立時的錯誤）。
    context_result: Result<Arc<MockContextAdapter>, DriverError>,
    context_options: Mutex<Vec<BrowserContextOptions>>,
    connected: AtomicBool,
    disconnected_listeners: FakeUnitListeners,
    close_calls: AtomicUsize,
    close_hangs: AtomicBool,
    close_failures: Mutex<VecDeque<DriverError>>,
}

impl MockBrowserAdapter {
    fn new(context_result: Result<Arc<MockContextAdapter>, DriverError>) -> Arc<Self> {
        Arc::new(Self {
            context_result,
            context_options: Mutex::new(Vec::new()),
            connected: AtomicBool::new(true),
            disconnected_listeners: FakeUnitListeners::new(),
            close_calls: AtomicUsize::new(0),
            close_hangs: AtomicBool::new(false),
            close_failures: Mutex::new(VecDeque::new()),
        })
    }

    fn new_with(context: Arc<MockContextAdapter>) -> Arc<Self> {
        Self::new(Ok(context))
    }

    fn new_failing(error: DriverError) -> Arc<Self> {
        Self::new(Err(error))
    }

    /// 舊測試 `closeImplementation = () => new Promise(() => undefined)`。
    fn hang_on_close(&self) {
        self.close_hangs.store(true, Ordering::SeqCst);
    }

    fn fail_next_close(&self, error: DriverError) {
        self.close_failures.lock().unwrap().push_back(error);
    }

    fn close_calls(&self) -> usize {
        self.close_calls.load(Ordering::SeqCst)
    }

    fn emit_disconnected(&self) {
        self.connected.store(false, Ordering::SeqCst);
        self.disconnected_listeners.fire_unit();
    }
}

#[async_trait]
impl BrowserAdapter for MockBrowserAdapter {
    async fn new_context(
        &self,
        options: BrowserContextOptions,
    ) -> Result<Arc<dyn BrowserContextAdapter>, DriverError> {
        self.context_options.lock().unwrap().push(options);
        match self.context_result.as_ref() {
            Ok(context) => Ok(Arc::clone(context) as Arc<dyn BrowserContextAdapter>),
            Err(error) => Err(error.clone()),
        }
    }

    async fn close(&self) -> Result<(), DriverError> {
        self.close_calls.fetch_add(1, Ordering::SeqCst);
        if self.close_hangs.load(Ordering::SeqCst) {
            return futures_util::future::pending::<Result<(), DriverError>>().await;
        }
        if let Some(failure) = self.close_failures.lock().unwrap().pop_front() {
            return Err(failure);
        }
        self.emit_disconnected();
        Ok(())
    }

    fn on_disconnected(&self, listener: Box<dyn Fn() + Send + Sync>) -> CancelListener {
        self.disconnected_listeners.subscribe_unit(listener)
    }

    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    fn get_version(&self) -> String {
        "mock-browser-1.0".to_string()
    }
}

// ---------------------------------------------------------------------------
// fake launcher（對應舊測試的 MockLauncher）
// ---------------------------------------------------------------------------

/// 舊測試的 `MockLauncher`。
struct MockLauncher {
    launch_calls: AtomicUsize,
    options: Mutex<Vec<BrowserLaunchOptions>>,
    results: Mutex<VecDeque<Result<Arc<MockBrowserAdapter>, DriverError>>>,
}

impl MockLauncher {
    fn new(results: Vec<Result<Arc<MockBrowserAdapter>, DriverError>>) -> Arc<Self> {
        Arc::new(Self {
            launch_calls: AtomicUsize::new(0),
            options: Mutex::new(Vec::new()),
            results: Mutex::new(results.into()),
        })
    }

    fn launch_calls(&self) -> usize {
        self.launch_calls.load(Ordering::SeqCst)
    }

    fn options(&self) -> Vec<BrowserLaunchOptions> {
        self.options.lock().unwrap().clone()
    }
}

#[async_trait]
impl BrowserLauncher for MockLauncher {
    async fn launch(
        &self,
        options: BrowserLaunchOptions,
    ) -> Result<Arc<dyn BrowserAdapter>, DriverError> {
        self.launch_calls.fetch_add(1, Ordering::SeqCst);
        self.options.lock().unwrap().push(options);
        match self.results.lock().unwrap().pop_front() {
            Some(Ok(browser)) => Ok(browser as Arc<dyn BrowserAdapter>),
            Some(Err(error)) => Err(error),
            None => Err(DriverError::new("Error", "測試未提供下一個 browser")),
        }
    }
}

// ---------------------------------------------------------------------------
// fake 的 sleep 與觀察者
// ---------------------------------------------------------------------------

/// 舊測試的 sleep 注入：記錄毫秒數；佇列中的 gate 逐一擋住對應呼叫
///（對應 `sleep = vi.fn(() => sleepGate.promise)`）。
struct FakeSleep {
    calls: Mutex<Vec<u64>>,
    gates: Mutex<VecDeque<oneshot::Receiver<()>>>,
}

impl FakeSleep {
    fn immediate() -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            gates: Mutex::new(VecDeque::new()),
        })
    }

    /// 第一個呼叫由 gate 擋住（之後的呼叫立即完成）。
    fn first_gated() -> (Arc<Self>, oneshot::Sender<()>) {
        let (sender, receiver) = oneshot::channel();
        (
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
                gates: Mutex::new(VecDeque::from([receiver])),
            }),
            sender,
        )
    }

    fn calls(&self) -> Vec<u64> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl SleepFn for FakeSleep {
    async fn sleep(&self, milliseconds: u64) {
        self.calls.lock().unwrap().push(milliseconds);
        let gate = self.gates.lock().unwrap().pop_front();
        if let Some(gate) = gate {
            let _ = gate.await;
        }
    }
}

/// 舊測試的 `onInvalidated` 收集器。
#[derive(Clone, Default)]
struct InvalidationRecorder(Arc<Mutex<Vec<BrowserInvalidation>>>);

impl InvalidationRecorder {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(Vec::new())))
    }

    fn snapshot(&self) -> Vec<BrowserInvalidation> {
        self.0.lock().unwrap().clone()
    }

    fn len(&self) -> usize {
        self.0.lock().unwrap().len()
    }
}

#[async_trait]
impl BrowserInvalidationObserver for InvalidationRecorder {
    async fn on_invalidated(&self, invalidation: BrowserInvalidation) -> Result<(), DriverError> {
        self.0.lock().unwrap().push(invalidation);
        Ok(())
    }
}

/// 舊測試「拋錯的 invalidation observer」。
struct FailingInvalidationObserver;

#[async_trait]
impl BrowserInvalidationObserver for FailingInvalidationObserver {
    async fn on_invalidated(&self, _invalidation: BrowserInvalidation) -> Result<(), DriverError> {
        Err(DriverError::new("Error", "observer failed"))
    }
}

/// 舊測試的 `onFatalRecovery` 收集器。
#[derive(Clone, Default)]
struct FatalRecoveryRecorder(Arc<Mutex<Vec<FatalRecoveryRequest>>>);

impl FatalRecoveryRecorder {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(Vec::new())))
    }

    fn snapshot(&self) -> Vec<FatalRecoveryRequest> {
        self.0.lock().unwrap().clone()
    }
}

#[async_trait]
impl BrowserFatalRecoveryObserver for FatalRecoveryRecorder {
    async fn on_fatal_recovery(&self, request: FatalRecoveryRequest) -> Result<(), DriverError> {
        self.0.lock().unwrap().push(request);
        Ok(())
    }
}

/// 舊測試的 `onBrowserRestarted` 收集器。
#[derive(Clone, Default)]
struct RestartedRecorder(Arc<Mutex<Vec<BrowserRestartedEvent>>>);

impl RestartedRecorder {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(Vec::new())))
    }
}

impl BrowserRestartedObserver for RestartedRecorder {
    fn on_browser_restarted(&self, event: BrowserRestartedEvent) -> Result<(), DriverError> {
        self.0.lock().unwrap().push(event);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 設定與建構輔助（對應舊測試的 createConfig）
// ---------------------------------------------------------------------------

/// 舊測試 `createConfig` 的 recovery 預設值；兩個可覆寫的門檻帶參數。
fn recovery_config(
    multi_channel_crash_threshold: f64,
    browser_failure_container_threshold: f64,
) -> BrowserManagerRecoveryConfig {
    BrowserManagerRecoveryConfig {
        page_crash_backoff_seconds: vec![30.0, 60.0, 120.0],
        channel_crash_window_seconds: 600.0,
        channel_quarantine_threshold: 4.0,
        channel_quarantine_seconds: 900.0,
        stable_reset_seconds: 1_800.0,
        multi_channel_crash_window_seconds: 15.0,
        multi_channel_crash_threshold,
        browser_failure_window_seconds: 600.0,
        browser_failure_container_threshold,
    }
}

/// 對應舊測試 `createConfig()`（headless、restartOnCrash、disableChat 全開）。
fn create_config() -> BrowserManagerConfig {
    create_config_full(true, true, true)
}

fn create_config_full(
    headless: bool,
    restart_on_crash: bool,
    disable_chat: bool,
) -> BrowserManagerConfig {
    create_config_thresholds(headless, restart_on_crash, disable_chat, 2.0, 3.0)
}

fn create_config_thresholds(
    headless: bool,
    restart_on_crash: bool,
    disable_chat: bool,
    multi_channel_crash_threshold: f64,
    browser_failure_container_threshold: f64,
) -> BrowserManagerConfig {
    BrowserManagerConfig {
        headless,
        storage_state_path: STORAGE_STATE_PATH.to_string(),
        browser: BrowserManagerBrowserConfig {
            restart_on_crash,
            viewport_width: 1280.0,
            viewport_height: 720.0,
            block_images: false,
            block_fonts: false,
            block_known_tracking: false,
            disable_chat,
            recovery: recovery_config(
                multi_channel_crash_threshold,
                browser_failure_container_threshold,
            ),
        },
    }
}

fn error_of(message: &str) -> DriverError {
    DriverError::new("Error", message)
}

fn make_manager(
    config: BrowserManagerConfig,
    dependencies: BrowserManagerDependencies,
) -> DefaultBrowserManager {
    DefaultBrowserManager::new(config, dependencies)
}

fn launch_results(
    browsers: Vec<Result<Arc<MockBrowserAdapter>, DriverError>>,
) -> Vec<Result<Arc<MockBrowserAdapter>, DriverError>> {
    browsers
}

// ---------------------------------------------------------------------------
// 測試本體（49 個，順序照舊測試檔）
// ---------------------------------------------------------------------------

// 原測試：以 headless、storageState 與設定 viewport 啟動，重複 start 不重建
#[tokio::test(start_paused = true)]
async fn starts_with_headless_storage_state_and_viewport_without_rebuilding() {
    let context = MockContextAdapter::new(Vec::new());
    let browser = MockBrowserAdapter::new_with(context.clone());
    let launcher = MockLauncher::new(launch_results(vec![Ok(browser.clone())]));
    let manager = make_manager(
        create_config_full(false, true, true),
        BrowserManagerDependencies {
            launcher: Some(launcher.clone() as Arc<dyn BrowserLauncher>),
            ..BrowserManagerDependencies::default()
        },
    );

    let (first, second) = tokio::join!(manager.start(), manager.start());

    assert!(first.is_ok() && second.is_ok());
    assert_eq!(launcher.launch_calls(), 1);
    assert_eq!(
        launcher.options(),
        vec![BrowserLaunchOptions { headless: false }]
    );
    assert_eq!(
        MockContextAdapter::context_options(&browser),
        vec![BrowserContextOptions {
            storage_state: STORAGE_STATE_PATH.to_string(),
            viewport: BrowserViewport {
                width: 1280.0,
                height: 720.0
            },
        }]
    );
    assert_eq!(
        context.resource_blocking_calls(),
        vec![ResourceBlockingOptions::default()]
    );
    assert_eq!(context.chat_blocking_calls(), vec![true]);
    assert_eq!(
        manager.get_browser_version().as_deref(),
        Some("mock-browser-1.0")
    );
    assert_eq!(manager.get_browser_generation(), 1);
}

// 原測試：disableChat 設定為 false 時停用聊天封鎖
#[tokio::test(start_paused = true)]
async fn disable_chat_false_disables_chat_blocking() {
    let context = MockContextAdapter::new(Vec::new());
    let browser = MockBrowserAdapter::new_with(context.clone());
    let launcher = MockLauncher::new(launch_results(vec![Ok(browser)]));
    let manager = make_manager(
        create_config_full(true, true, false),
        BrowserManagerDependencies {
            launcher: Some(launcher as Arc<dyn BrowserLauncher>),
            ..BrowserManagerDependencies::default()
        },
    );

    manager.start().await.unwrap();

    assert_eq!(context.chat_blocking_calls(), vec![false]);
}

// 原測試：context 建立失敗時關閉 browser，之後可重新 start
#[tokio::test(start_paused = true)]
async fn context_creation_failure_closes_browser_and_allows_restart() {
    let failed_browser = MockBrowserAdapter::new_failing(error_of("context creation failed"));
    let recovered_context = MockContextAdapter::new(Vec::new());
    let recovered_browser = MockBrowserAdapter::new_with(recovered_context.clone());
    let launcher = MockLauncher::new(launch_results(vec![
        Ok(failed_browser.clone()),
        Ok(recovered_browser.clone()),
    ]));
    let manager = make_manager(
        create_config(),
        BrowserManagerDependencies {
            launcher: Some(launcher.clone() as Arc<dyn BrowserLauncher>),
            ..BrowserManagerDependencies::default()
        },
    );

    let first = manager.start().await;
    assert!(error_message_contains(&first, "context creation failed"));
    assert_eq!(failed_browser.close_calls(), 1);

    manager.start().await.unwrap();

    assert_eq!(launcher.launch_calls(), 2);
    assert_eq!(recovered_context.new_page_calls(), 0);
    assert_eq!(
        MockContextAdapter::context_options(&recovered_browser).len(),
        1
    );
}

// 原測試：未 start 時拒絕 createPage
#[tokio::test(start_paused = true)]
async fn create_page_rejected_before_start() {
    let launcher = MockLauncher::new(launch_results(vec![]));
    let manager = make_manager(
        create_config(),
        BrowserManagerDependencies {
            launcher: Some(launcher as Arc<dyn BrowserLauncher>),
            ..BrowserManagerDependencies::default()
        },
    );

    let result = manager.create_page("channel").await;
    assert!(error_message_contains(&result, "Browser Manager 尚未啟動"));
}

// 原測試：同一 channel 重複 createPage 回傳既有 page
#[tokio::test(start_paused = true)]
async fn repeated_create_page_for_same_channel_returns_existing_page() {
    let page = MockPageAdapter::new("channel");
    let context = MockContextAdapter::new(vec![Ok(page.clone())]);
    let launcher = MockLauncher::new(launch_results(vec![Ok(MockBrowserAdapter::new_with(
        context.clone(),
    ))]));
    let manager = make_manager(
        create_config(),
        BrowserManagerDependencies {
            launcher: Some(launcher as Arc<dyn BrowserLauncher>),
            ..BrowserManagerDependencies::default()
        },
    );
    manager.start().await.unwrap();

    let (first, second) = tokio::join!(
        manager.create_page("channel"),
        manager.create_page("channel")
    );

    let first = first.unwrap();
    let second = second.unwrap();
    assert!(Arc::ptr_eq(&first, &page.page));
    assert!(Arc::ptr_eq(&second, &page.page));
    assert_eq!(context.new_page_calls(), 1);
}

// 原測試：createPage 失敗不殘留 registry，下一次可重試
#[tokio::test(start_paused = true)]
async fn create_page_failure_leaves_no_registry_entry_and_can_retry() {
    let recovered_page = MockPageAdapter::new("recovered");
    let context = MockContextAdapter::new(vec![
        Err(error_of("new page failed")),
        Ok(recovered_page.clone()),
    ]);
    let launcher = MockLauncher::new(launch_results(vec![Ok(MockBrowserAdapter::new_with(
        context.clone(),
    ))]));
    let manager = make_manager(
        create_config(),
        BrowserManagerDependencies {
            launcher: Some(launcher as Arc<dyn BrowserLauncher>),
            ..BrowserManagerDependencies::default()
        },
    );
    manager.start().await.unwrap();

    let first = manager.create_page("channel").await;
    assert!(error_message_contains(&first, "new page failed"));
    let second = manager.create_page("channel").await.unwrap();
    assert!(Arc::ptr_eq(&second, &recovered_page.page));
    assert_eq!(context.new_page_calls(), 2);
}

// 原測試：closePage 可重複呼叫且正常關閉不通知 invalidation
#[tokio::test(start_paused = true)]
async fn repeated_close_page_closes_once_without_invalidation() {
    let invalidations = InvalidationRecorder::new();
    let page = MockPageAdapter::new("channel");
    let context = MockContextAdapter::new(vec![Ok(page.clone())]);
    let launcher = MockLauncher::new(launch_results(vec![Ok(MockBrowserAdapter::new_with(
        context,
    ))]));
    let manager = make_manager(
        create_config(),
        BrowserManagerDependencies {
            launcher: Some(launcher as Arc<dyn BrowserLauncher>),
            on_invalidated: Some(Arc::new(invalidations.clone())),
            ..BrowserManagerDependencies::default()
        },
    );
    manager.start().await.unwrap();
    manager.create_page("channel").await.unwrap();

    let (first, second) =
        tokio::join!(manager.close_page("channel"), manager.close_page("channel"));
    assert!(first.is_ok() && second.is_ok());

    assert_eq!(page.close_calls(), 1);
    assert_eq!(invalidations.snapshot(), Vec::new());
}

// 原測試：closePage 失敗時改為 full browser recycle，不在同 context 偷偷換 page
#[tokio::test(start_paused = true)]
async fn close_page_failure_falls_back_to_full_browser_recycle() {
    let page = MockPageAdapter::new("channel");
    page.fail_next_close(error_of("page close failed"));
    let first_context = MockContextAdapter::new(vec![Ok(page.clone())]);
    let first_browser = MockBrowserAdapter::new_with(first_context);
    let second_context = MockContextAdapter::new(Vec::new());
    let second_browser = MockBrowserAdapter::new_with(second_context.clone());
    let logger = FakeLogger::new();
    let launcher = MockLauncher::new(launch_results(vec![
        Ok(first_browser.clone()),
        Ok(second_browser.clone()),
    ]));
    let manager = make_manager(
        create_config(),
        BrowserManagerDependencies {
            launcher: Some(launcher.clone() as Arc<dyn BrowserLauncher>),
            logger: Some(logger.clone() as Arc<dyn BrowserManagerLogger>),
            ..BrowserManagerDependencies::default()
        },
    );
    manager.start().await.unwrap();
    manager.create_page("channel").await.unwrap();

    let closed = manager.close_page("channel").await;
    assert!(closed.is_ok());
    assert!(
        logger.called_with(
            LogLevel::Warn,
            "browser_page_close_requires_recycle",
            &[
                ("channel", v_str("channel")),
                ("reason", v_str("page_close_failed"))
            ],
        ),
        "warn 應含 channel 與 page_close_failed"
    );

    assert_eventually("第一個 browser 應被關閉", || {
        first_browser.close_calls() > 0
    })
    .await;
    assert_eventually("第二個 browser 應建立 context", || {
        !MockContextAdapter::context_options(&second_browser).is_empty()
    })
    .await;
    assert_eq!(manager.get_page_count(), 0);
}

// 原測試：stop 關閉 pages、context、browser，併發與重複 stop 皆安全
#[tokio::test(start_paused = true)]
async fn stop_closes_everything_and_is_safe_when_concurrent_or_repeated() {
    let first_page = MockPageAdapter::new("first");
    let second_page = MockPageAdapter::new("second");
    let context = MockContextAdapter::new(vec![Ok(first_page.clone()), Ok(second_page.clone())]);
    let browser = MockBrowserAdapter::new_with(context.clone());
    let launcher = MockLauncher::new(launch_results(vec![Ok(browser.clone())]));
    let manager = make_manager(
        create_config(),
        BrowserManagerDependencies {
            launcher: Some(launcher as Arc<dyn BrowserLauncher>),
            ..BrowserManagerDependencies::default()
        },
    );
    manager.start().await.unwrap();
    manager.create_page("first").await.unwrap();
    manager.create_page("second").await.unwrap();

    let (a, b) = tokio::join!(manager.stop(), manager.stop());
    assert!(a.is_ok() && b.is_ok());
    manager.stop().await.unwrap();

    assert_eq!(first_page.close_calls(), 1);
    assert_eq!(second_page.close_calls(), 1);
    assert_eq!(context.close_calls(), 1);
    assert_eq!(browser.close_calls(), 1);
}

// 原測試：stop 即使部分 close 失敗仍繼續清理其餘資源
#[tokio::test(start_paused = true)]
async fn stop_continues_cleanup_even_when_some_close_fail() {
    let page = MockPageAdapter::new("channel");
    page.fail_next_close(error_of("page close failed"));
    let context = MockContextAdapter::new(vec![Ok(page.clone())]);
    context.fail_next_close(error_of("context close failed"));
    let browser = MockBrowserAdapter::new_with(context.clone());
    browser.fail_next_close(error_of("browser close failed"));
    let logger = FakeLogger::new();
    let launcher = MockLauncher::new(launch_results(vec![Ok(browser.clone())]));
    let manager = make_manager(
        create_config(),
        BrowserManagerDependencies {
            launcher: Some(launcher as Arc<dyn BrowserLauncher>),
            logger: Some(logger.clone() as Arc<dyn BrowserManagerLogger>),
            ..BrowserManagerDependencies::default()
        },
    );
    manager.start().await.unwrap();
    manager.create_page("channel").await.unwrap();

    manager.stop().await.unwrap();

    assert_eq!(page.close_calls(), 1);
    assert_eq!(context.close_calls(), 1);
    assert_eq!(browser.close_calls(), 1);
    assert!(logger.call_count(LogLevel::Warn) > 0);
}

// 原測試：併發 manual restart 合併為一次並通知既有 channel 失效
#[tokio::test(start_paused = true)]
async fn concurrent_manual_restarts_are_merged_into_one_with_invalidation() {
    let page = MockPageAdapter::new("channel");
    let first_context = MockContextAdapter::new(vec![Ok(page.clone())]);
    let first_browser = MockBrowserAdapter::new_with(first_context.clone());
    let second_context = MockContextAdapter::new(Vec::new());
    let second_browser = MockBrowserAdapter::new_with(second_context);
    let launcher = MockLauncher::new(launch_results(vec![
        Ok(first_browser.clone()),
        Ok(second_browser),
    ]));
    let logger = FakeLogger::new();
    let invalidations = InvalidationRecorder::new();
    let manager = make_manager(
        create_config(),
        BrowserManagerDependencies {
            launcher: Some(launcher.clone() as Arc<dyn BrowserLauncher>),
            logger: Some(logger.clone() as Arc<dyn BrowserManagerLogger>),
            on_invalidated: Some(Arc::new(invalidations.clone())),
            ..BrowserManagerDependencies::default()
        },
    );
    manager.start().await.unwrap();
    manager.create_page("channel").await.unwrap();

    let (first, second) = tokio::join!(manager.restart(), manager.restart());
    assert!(first.is_ok() && second.is_ok());

    assert_eq!(launcher.launch_calls(), 2);
    assert_eq!(page.close_calls(), 1);
    assert_eq!(first_context.close_calls(), 1);
    assert_eq!(first_browser.close_calls(), 1);
    assert_eq!(
        invalidations.snapshot(),
        vec![BrowserInvalidation {
            channel: "channel".to_string(),
            reason: BrowserInvalidationReason::BrowserRestarted,
        }]
    );
    assert!(
        logger.called_exactly_with(
            LogLevel::Warn,
            "browser_restarted",
            &[
                ("mode", v_str("manual")),
                ("affectedChannelCount", v_num(1.0)),
                ("browserGeneration", v_num(2.0)),
            ],
        ),
        "warn 應為 manual 重啟的完整欄位"
    );
}

// 原測試：manual restart 啟動失敗時仍完成舊資源清理與失效通知
#[tokio::test(start_paused = true)]
async fn manual_restart_failure_still_cleans_up_and_notifies() {
    let page = MockPageAdapter::new("channel");
    let first_context = MockContextAdapter::new(vec![Ok(page.clone())]);
    let first_browser = MockBrowserAdapter::new_with(first_context.clone());
    let launcher = MockLauncher::new(launch_results(vec![
        Ok(first_browser.clone()),
        Err(error_of("restart launch failed")),
    ]));
    let invalidations = InvalidationRecorder::new();
    let manager = make_manager(
        create_config(),
        BrowserManagerDependencies {
            launcher: Some(launcher as Arc<dyn BrowserLauncher>),
            on_invalidated: Some(Arc::new(invalidations.clone())),
            ..BrowserManagerDependencies::default()
        },
    );
    manager.start().await.unwrap();
    manager.create_page("channel").await.unwrap();

    let restarted = manager.restart().await;
    assert!(error_message_contains(&restarted, "restart launch failed"));

    assert_eq!(page.close_calls(), 1);
    assert_eq!(first_context.close_calls(), 1);
    assert_eq!(first_browser.close_calls(), 1);
    assert_eq!(
        invalidations.snapshot(),
        vec![BrowserInvalidation {
            channel: "channel".to_string(),
            reason: BrowserInvalidationReason::BrowserRestarted,
        }]
    );
}

// 原測試：page crash 移除 registry、清理 page 並只通知一次
#[tokio::test(start_paused = true)]
async fn page_crash_removes_registry_cleans_page_and_notifies_once() {
    let crashed_page = MockPageAdapter::new("crashed");
    let replacement_page = MockPageAdapter::new("replacement");
    let context =
        MockContextAdapter::new(vec![Ok(crashed_page.clone()), Ok(replacement_page.clone())]);
    let invalidations = InvalidationRecorder::new();
    let launcher = MockLauncher::new(launch_results(vec![Ok(MockBrowserAdapter::new_with(
        context,
    ))]));
    let manager = make_manager(
        create_config(),
        BrowserManagerDependencies {
            launcher: Some(launcher as Arc<dyn BrowserLauncher>),
            on_invalidated: Some(Arc::new(invalidations.clone())),
            ..BrowserManagerDependencies::default()
        },
    );
    manager.start().await.unwrap();
    manager.create_page("channel").await.unwrap();

    crashed_page.emit_crash();

    assert_eventually("crash 應通知一次 page_crashed", || {
        invalidations.snapshot()
            == vec![BrowserInvalidation {
                channel: "channel".to_string(),
                reason: BrowserInvalidationReason::PageCrashed,
            }]
    })
    .await;
    let recreated = manager.create_page("channel").await.unwrap();
    assert!(Arc::ptr_eq(&recreated, &replacement_page.page));
    assert_eq!(crashed_page.close_calls(), 1);
}

// 原測試：同一 normalized channel 重複 crash 不會 recycle browser
#[tokio::test(start_paused = true)]
async fn repeated_crash_on_same_normalized_channel_does_not_recycle_browser() {
    let first_page = MockPageAdapter::new("first");
    let second_page = MockPageAdapter::new("second");
    let context = MockContextAdapter::new(vec![Ok(first_page.clone()), Ok(second_page.clone())]);
    let launcher = MockLauncher::new(launch_results(vec![Ok(MockBrowserAdapter::new_with(
        context,
    ))]));
    let invalidations = InvalidationRecorder::new();
    let logger = FakeLogger::new();
    let manager = make_manager(
        create_config(),
        BrowserManagerDependencies {
            launcher: Some(launcher.clone() as Arc<dyn BrowserLauncher>),
            logger: Some(logger.clone() as Arc<dyn BrowserManagerLogger>),
            on_invalidated: Some(Arc::new(invalidations.clone())),
            ..BrowserManagerDependencies::default()
        },
    );
    manager.start().await.unwrap();
    manager.create_page("Same_Channel").await.unwrap();
    manager.create_page("same_channel").await.unwrap();

    first_page.emit_crash();
    second_page.emit_crash();

    assert_eventually("兩個 crash 都應通知 invalidation", || {
        invalidations.len() == 2
    })
    .await;
    assert_eq!(launcher.launch_calls(), 1);
    assert!(logger.never_called_with(LogLevel::Warn, "browser_crash_loop_recycle_requested"));
}

// 原測試：15 秒內兩個不同 channel crash 只觸發同 generation 一次 recycle
#[tokio::test(start_paused = true)]
async fn two_distinct_channel_crashes_within_window_trigger_one_recycle_per_generation() {
    let first_page = MockPageAdapter::new("first");
    let second_page = MockPageAdapter::new("second");
    let third_page = MockPageAdapter::new("third");
    let first_browser = MockBrowserAdapter::new_with(MockContextAdapter::new(vec![
        Ok(first_page.clone()),
        Ok(second_page.clone()),
        Ok(third_page.clone()),
    ]));
    let recovered_browser = MockBrowserAdapter::new_with(MockContextAdapter::new(Vec::new()));
    let launcher = MockLauncher::new(launch_results(vec![
        Ok(first_browser),
        Ok(recovered_browser),
    ]));
    let logger = FakeLogger::new();
    let fatal_recovery = FatalRecoveryRecorder::new();
    let manager = make_manager(
        create_config(),
        BrowserManagerDependencies {
            launcher: Some(launcher.clone() as Arc<dyn BrowserLauncher>),
            logger: Some(logger.clone() as Arc<dyn BrowserManagerLogger>),
            on_fatal_recovery: Some(Arc::new(fatal_recovery.clone())),
            ..BrowserManagerDependencies::default()
        },
    );
    manager.start().await.unwrap();
    manager.create_page("First").await.unwrap();
    manager.create_page("SECOND").await.unwrap();
    manager.create_page("third").await.unwrap();

    first_page.emit_crash();
    second_page.emit_crash();
    third_page.emit_crash();

    assert_eventually("應重啟一次（launch 2 次）", || {
        launcher.launch_calls() == 2
    })
    .await;
    let recycle_requests = logger.fields_of(LogLevel::Warn, "browser_crash_loop_recycle_requested");
    assert_eq!(recycle_requests.len(), 1);
    assert!(
        object_equals(
            &[
                ("reason", v_str("multi_channel_page_crash")),
                ("channel", v_str("SECOND")),
                ("affectedChannels", v_strs(&["first", "second"])),
                ("distinctChannelCount", v_num(2.0)),
                ("browserGeneration", v_num(1.0)),
                ("pageGeneration", v_num(2.0)),
            ],
            &recycle_requests[0],
        ),
        "recycle 請求的欄位應完整：{:?}",
        recycle_requests[0]
    );
    assert!(fatal_recovery.snapshot().is_empty());
}

// 原測試：超出 multi-channel crash window 的不同 channel 不會 recycle
#[tokio::test(start_paused = true)]
async fn distinct_channel_crashes_beyond_window_do_not_recycle() {
    let clock = Arc::new(AtomicU64::new(0));
    let first_page = MockPageAdapter::new("first");
    let second_page = MockPageAdapter::new("second");
    let launcher = MockLauncher::new(launch_results(vec![Ok(MockBrowserAdapter::new_with(
        MockContextAdapter::new(vec![Ok(first_page.clone()), Ok(second_page.clone())]),
    ))]));
    let invalidations = InvalidationRecorder::new();
    let clock_for_manager = Arc::clone(&clock);
    let now: ManagerClock = Arc::new(move || clock_for_manager.load(Ordering::SeqCst));
    let manager = make_manager(
        create_config(),
        BrowserManagerDependencies {
            launcher: Some(launcher.clone() as Arc<dyn BrowserLauncher>),
            now: Some(now),
            on_invalidated: Some(Arc::new(invalidations.clone())),
            ..BrowserManagerDependencies::default()
        },
    );
    manager.start().await.unwrap();
    manager.create_page("first").await.unwrap();
    manager.create_page("second").await.unwrap();

    first_page.emit_crash();
    assert_eventually("第一個 crash 應通知", || invalidations.len() == 1).await;
    clock.store(15_001, Ordering::SeqCst);
    second_page.emit_crash();

    assert_eventually("第二個 crash 應通知", || invalidations.len() == 2).await;
    assert_eq!(launcher.launch_calls(), 1);
}

// 原測試：非預期 page close 通知失效且可重建相同 channel
#[tokio::test(start_paused = true)]
async fn unexpected_page_close_notifies_invalidation_and_allows_rebuild() {
    let closed_page = MockPageAdapter::new("closed");
    let replacement_page = MockPageAdapter::new("replacement");
    let context =
        MockContextAdapter::new(vec![Ok(closed_page.clone()), Ok(replacement_page.clone())]);
    let invalidations = InvalidationRecorder::new();
    let launcher = MockLauncher::new(launch_results(vec![Ok(MockBrowserAdapter::new_with(
        context,
    ))]));
    let manager = make_manager(
        create_config(),
        BrowserManagerDependencies {
            launcher: Some(launcher as Arc<dyn BrowserLauncher>),
            on_invalidated: Some(Arc::new(invalidations.clone())),
            ..BrowserManagerDependencies::default()
        },
    );
    manager.start().await.unwrap();
    manager.create_page("channel").await.unwrap();

    closed_page.emit_unexpected_close();

    assert_eventually("應通知 page_closed", || {
        invalidations.snapshot()
            == vec![BrowserInvalidation {
                channel: "channel".to_string(),
                reason: BrowserInvalidationReason::PageClosed,
            }]
    })
    .await;
    let recreated = manager.create_page("channel").await.unwrap();
    assert!(Arc::ptr_eq(&recreated, &replacement_page.page));
    assert_eq!(closed_page.close_calls(), 0);
}

// 原測試：頁面 popup 立即關閉且不加入 channel registry
#[tokio::test(start_paused = true)]
async fn page_popup_closed_immediately_without_entering_registry() {
    let page = MockPageAdapter::new("channel");
    let context = MockContextAdapter::new(vec![Ok(page.clone())]);
    let logger = FakeLogger::new();
    let launcher = MockLauncher::new(launch_results(vec![Ok(MockBrowserAdapter::new_with(
        context.clone(),
    ))]));
    let manager = make_manager(
        create_config(),
        BrowserManagerDependencies {
            launcher: Some(launcher as Arc<dyn BrowserLauncher>),
            logger: Some(logger.clone() as Arc<dyn BrowserManagerLogger>),
            ..BrowserManagerDependencies::default()
        },
    );
    let popup = FakePopupPage::new();
    manager.start().await.unwrap();
    manager.create_page("channel").await.unwrap();

    page.emit_popup(popup.clone() as Arc<dyn BrowserPage>);

    assert_eventually("popup 應被立即關閉", || popup.close_calls() == 1).await;
    assert!(
        logger.called_exactly_with(
            LogLevel::Warn,
            "browser_popup_blocked",
            &[("channel", v_str("channel"))],
        ),
        "應記錄 browser_popup_blocked"
    );
    assert_eq!(context.new_page_calls(), 1);
}

// 原測試：browser disconnect 通知全部 channel，並以 single-flight 退避重啟
#[tokio::test(start_paused = true)]
async fn browser_disconnect_notifies_all_channels_and_restarts_with_backoff() {
    let first_page = MockPageAdapter::new("first");
    let second_page = MockPageAdapter::new("second");
    let first_context =
        MockContextAdapter::new(vec![Ok(first_page.clone()), Ok(second_page.clone())]);
    let first_browser = MockBrowserAdapter::new_with(first_context.clone());
    let recovered_context = MockContextAdapter::new(Vec::new());
    let recovered_browser = MockBrowserAdapter::new_with(recovered_context);
    let launcher = MockLauncher::new(launch_results(vec![
        Ok(first_browser.clone()),
        Ok(recovered_browser),
    ]));
    let sleep = FakeSleep::immediate();
    let invalidations = InvalidationRecorder::new();
    let logger = FakeLogger::new();
    let manager = make_manager(
        create_config(),
        BrowserManagerDependencies {
            launcher: Some(launcher.clone() as Arc<dyn BrowserLauncher>),
            logger: Some(logger.clone() as Arc<dyn BrowserManagerLogger>),
            sleep: Some(sleep.clone() as Arc<dyn SleepFn>),
            restart_backoff_ms: Some(25),
            on_invalidated: Some(Arc::new(invalidations.clone())),
            ..BrowserManagerDependencies::default()
        },
    );
    manager.start().await.unwrap();
    manager.create_page("first").await.unwrap();
    manager.create_page("second").await.unwrap();

    first_browser.emit_disconnected();
    first_browser.emit_disconnected();

    assert_eventually("應重啟（launch 2 次）", || {
        launcher.launch_calls() == 2
    })
    .await;
    assert_eq!(sleep.calls(), vec![25]);
    assert_eq!(
        invalidations.snapshot(),
        vec![
            BrowserInvalidation {
                channel: "first".to_string(),
                reason: BrowserInvalidationReason::BrowserDisconnected,
            },
            BrowserInvalidation {
                channel: "second".to_string(),
                reason: BrowserInvalidationReason::BrowserDisconnected,
            },
        ]
    );
    assert_eq!(first_page.close_calls(), 1);
    assert_eq!(second_page.close_calls(), 1);
    assert_eq!(first_context.close_calls(), 1);
    assert!(
        logger.called_exactly_with(
            LogLevel::Warn,
            "browser_restarted",
            &[
                ("mode", v_str("automatic")),
                ("attempt", v_num(1.0)),
                ("browserGeneration", v_num(2.0)),
            ],
        ),
        "warn 應為 automatic 重啟的完整欄位"
    );
}

// 原測試：page close 先清除 registry 時 disconnect 仍回報 generation snapshot
#[tokio::test(start_paused = true)]
async fn disconnect_reports_generation_snapshot_after_registry_cleared() {
    let page = MockPageAdapter::new("channel");
    let first_browser =
        MockBrowserAdapter::new_with(MockContextAdapter::new(vec![Ok(page.clone())]));
    let recovered_browser = MockBrowserAdapter::new_with(MockContextAdapter::new(Vec::new()));
    let logger = FakeLogger::new();
    let launcher = MockLauncher::new(launch_results(vec![
        Ok(first_browser.clone()),
        Ok(recovered_browser),
    ]));
    let sleep = FakeSleep::immediate();
    let manager = make_manager(
        create_config(),
        BrowserManagerDependencies {
            launcher: Some(launcher as Arc<dyn BrowserLauncher>),
            logger: Some(logger.clone() as Arc<dyn BrowserManagerLogger>),
            sleep: Some(sleep as Arc<dyn SleepFn>),
            ..BrowserManagerDependencies::default()
        },
    );
    manager.start().await.unwrap();
    manager.create_page("channel").await.unwrap();

    page.emit_unexpected_close();
    assert_eventually("registry 應先清空", || manager.get_page_count() == 0).await;
    first_browser.emit_disconnected();

    assert_eventually(
        "disconnect 應回報 trackedChannels 快照，實際：{:?}",
        || {
            logger.called_exactly_with(
                LogLevel::Warn,
                "browser_disconnected",
                &[
                    ("affectedChannels", v_strs(&["channel"])),
                    ("affectedChannelCount", v_num(1.0)),
                    ("browserGeneration", v_num(1.0)),
                ],
            )
        },
    )
    .await;
    // 失敗時印出實際記錄（assert_eventually 的訊息不會展開 logger 內容）。
    assert!(
        logger.called_exactly_with(
            LogLevel::Warn,
            "browser_disconnected",
            &[
                ("affectedChannels", v_strs(&["channel"])),
                ("affectedChannelCount", v_num(1.0)),
                ("browserGeneration", v_num(1.0)),
            ],
        ),
        "disconnect 應回報 trackedChannels 快照，實際：{:?}",
        logger.entries.lock().unwrap()
    );
}

// 原測試：restartOnCrash=false 時只清理與通知，不自動重啟
#[tokio::test(start_paused = true)]
async fn restart_on_crash_false_only_cleans_up_and_notifies() {
    let page = MockPageAdapter::new("channel");
    let context = MockContextAdapter::new(vec![Ok(page)]);
    let browser = MockBrowserAdapter::new_with(context);
    let launcher = MockLauncher::new(launch_results(vec![Ok(browser.clone())]));
    let invalidations = InvalidationRecorder::new();
    let manager = make_manager(
        create_config_full(true, false, true),
        BrowserManagerDependencies {
            launcher: Some(launcher.clone() as Arc<dyn BrowserLauncher>),
            on_invalidated: Some(Arc::new(invalidations.clone())),
            ..BrowserManagerDependencies::default()
        },
    );
    manager.start().await.unwrap();
    manager.create_page("channel").await.unwrap();

    browser.emit_disconnected();

    assert_eventually("應通知 browser_disconnected", || {
        invalidations.snapshot()
            == vec![BrowserInvalidation {
                channel: "channel".to_string(),
                reason: BrowserInvalidationReason::BrowserDisconnected,
            }]
    })
    .await;
    assert_eq!(launcher.launch_calls(), 1);
}

// 原測試：自動重啟退避期間 stop 會取消後續復原
#[tokio::test(start_paused = true)]
async fn stop_during_backoff_cancels_recovery() {
    let browser = MockBrowserAdapter::new_with(MockContextAdapter::new(Vec::new()));
    let recovered_browser = MockBrowserAdapter::new_with(MockContextAdapter::new(Vec::new()));
    let launcher = MockLauncher::new(launch_results(vec![
        Ok(browser.clone()),
        Ok(recovered_browser),
    ]));
    let (sleep, sleep_gate) = FakeSleep::first_gated();
    let manager = make_manager(
        create_config(),
        BrowserManagerDependencies {
            launcher: Some(launcher.clone() as Arc<dyn BrowserLauncher>),
            sleep: Some(sleep.clone() as Arc<dyn SleepFn>),
            ..BrowserManagerDependencies::default()
        },
    );
    manager.start().await.unwrap();

    browser.emit_disconnected();
    assert_eventually("sleep 應被呼叫一次", || sleep.calls().len() == 1).await;
    manager.stop().await.unwrap();
    let _ = sleep_gate.send(());
    // 讓 flight 的後續（取消的復原）跑完。
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }

    assert_eq!(launcher.launch_calls(), 1);
}

// 原測試：新的 browser crash 會取代仍在等待的過期復原排程
#[tokio::test(start_paused = true)]
async fn new_browser_crash_supersedes_stale_pending_recovery_schedule() {
    let first_browser = MockBrowserAdapter::new_with(MockContextAdapter::new(Vec::new()));
    let second_browser = MockBrowserAdapter::new_with(MockContextAdapter::new(Vec::new()));
    let recovered_browser = MockBrowserAdapter::new_with(MockContextAdapter::new(Vec::new()));
    let launcher = MockLauncher::new(launch_results(vec![
        Ok(first_browser.clone()),
        Ok(second_browser.clone()),
        Ok(recovered_browser),
    ]));
    let (sleep, first_sleep_gate) = FakeSleep::first_gated();
    let manager = make_manager(
        create_config(),
        BrowserManagerDependencies {
            launcher: Some(launcher.clone() as Arc<dyn BrowserLauncher>),
            sleep: Some(sleep.clone() as Arc<dyn SleepFn>),
            ..BrowserManagerDependencies::default()
        },
    );
    manager.start().await.unwrap();

    first_browser.emit_disconnected();
    assert_eventually("第一次 sleep 應被呼叫", || sleep.calls().len() == 1).await;
    manager.start().await.unwrap();
    second_browser.emit_disconnected();

    assert_eventually("應第三次 launch", || launcher.launch_calls() == 3).await;
    let _ = first_sleep_gate.send(());
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }

    assert_eq!(sleep.calls().len(), 2);
    assert_eq!(launcher.launch_calls(), 3);
}

// 原測試：自動重啟失敗只嘗試一次，且不洩漏 storageState path
#[tokio::test(start_paused = true)]
async fn automatic_restart_failure_tries_once_and_does_not_leak_storage_state_path() {
    let context = MockContextAdapter::new(Vec::new());
    let browser = MockBrowserAdapter::new_with(context);
    let restart_error = DriverError::new(
        "Error",
        "storageState=/private/credentials/storage-state.json storageState={\"cookies\":[{\"value\":\"secret-cookie\"}]}",
    );
    let launcher = MockLauncher::new(launch_results(vec![
        Ok(browser.clone()),
        Err(restart_error),
    ]));
    let logger = FakeLogger::new();
    let sleep = FakeSleep::immediate();
    let manager = make_manager(
        create_config(),
        BrowserManagerDependencies {
            launcher: Some(launcher.clone() as Arc<dyn BrowserLauncher>),
            logger: Some(logger.clone() as Arc<dyn BrowserManagerLogger>),
            sleep: Some(sleep as Arc<dyn SleepFn>),
            max_automatic_restart_attempts: Some(1),
            ..BrowserManagerDependencies::default()
        },
    );
    manager.start().await.unwrap();

    browser.emit_disconnected();

    assert_eventually("應記錄 automatic 重啟失敗", || {
        logger.called_with(
            LogLevel::Error,
            "browser_restart_failed",
            &[("mode", v_str("automatic")), ("attempt", v_num(1.0))],
        )
    })
    .await;
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert_eq!(launcher.launch_calls(), 2);
    let serialized = logger.serialized();
    assert!(!serialized.contains(STORAGE_STATE_PATH), "{serialized}");
    assert!(!serialized.contains("secret-cookie"), "{serialized}");
}

// 原測試：自動重啟第一次失敗後以遞增退避再次嘗試
#[tokio::test(start_paused = true)]
async fn automatic_restart_retries_with_increasing_backoff() {
    let first_browser = MockBrowserAdapter::new_with(MockContextAdapter::new(Vec::new()));
    let recovered_browser = MockBrowserAdapter::new_with(MockContextAdapter::new(Vec::new()));
    let launcher = MockLauncher::new(launch_results(vec![
        Ok(first_browser.clone()),
        Err(error_of("first restart failed")),
        Ok(recovered_browser),
    ]));
    let logger = FakeLogger::new();
    let sleep = FakeSleep::immediate();
    let manager = make_manager(
        create_config(),
        BrowserManagerDependencies {
            launcher: Some(launcher.clone() as Arc<dyn BrowserLauncher>),
            logger: Some(logger.clone() as Arc<dyn BrowserManagerLogger>),
            sleep: Some(sleep.clone() as Arc<dyn SleepFn>),
            restart_backoff_ms: Some(25),
            max_automatic_restart_attempts: Some(2),
            ..BrowserManagerDependencies::default()
        },
    );
    manager.start().await.unwrap();

    first_browser.emit_disconnected();

    assert_eventually("應第三次 launch", || launcher.launch_calls() == 3).await;
    assert_eq!(sleep.calls(), vec![25, 50]);
    assert!(
        logger.called_with(
            LogLevel::Error,
            "browser_restart_failed",
            &[("mode", v_str("automatic")), ("attempt", v_num(1.0))],
        ),
        "應記錄第一次重啟失敗"
    );
    assert!(
        logger.called_exactly_with(
            LogLevel::Warn,
            "browser_restarted",
            &[
                ("mode", v_str("automatic")),
                ("attempt", v_num(2.0)),
                ("browserGeneration", v_num(2.0)),
            ],
        ),
        "第二次重啟應成功"
    );
}

// 原測試：快速連續 browser crash 達上限後停止自動重啟
#[tokio::test(start_paused = true)]
async fn rapid_crashes_stop_after_attempt_limit() {
    let first_browser = MockBrowserAdapter::new_with(MockContextAdapter::new(Vec::new()));
    let second_browser = MockBrowserAdapter::new_with(MockContextAdapter::new(Vec::new()));
    let third_browser = MockBrowserAdapter::new_with(MockContextAdapter::new(Vec::new()));
    let launcher = MockLauncher::new(launch_results(vec![
        Ok(first_browser.clone()),
        Ok(second_browser.clone()),
        Ok(third_browser.clone()),
    ]));
    let logger = FakeLogger::new();
    let fatal_recovery = FatalRecoveryRecorder::new();
    let sleep = FakeSleep::immediate();
    let now: ManagerClock = Arc::new(|| 1_000);
    let manager = make_manager(
        // crash-loop breaker 調高，隔離 launch-attempt 上限的測試。
        create_config_thresholds(true, true, true, 2.0, 10.0),
        BrowserManagerDependencies {
            launcher: Some(launcher.clone() as Arc<dyn BrowserLauncher>),
            logger: Some(logger.clone() as Arc<dyn BrowserManagerLogger>),
            sleep: Some(sleep as Arc<dyn SleepFn>),
            now: Some(now),
            max_automatic_restart_attempts: Some(2),
            on_fatal_recovery: Some(Arc::new(fatal_recovery.clone())),
            ..BrowserManagerDependencies::default()
        },
    );
    manager.start().await.unwrap();

    first_browser.emit_disconnected();
    assert_eventually("第一次 crash 應重啟", || launcher.launch_calls() == 2).await;
    second_browser.emit_disconnected();
    assert_eventually("第二次 crash 應重啟", || launcher.launch_calls() == 3).await;
    third_browser.emit_disconnected();

    assert_eventually("應記錄次數上限", || {
        logger.called_exactly_with(
            LogLevel::Error,
            "browser_restart_limit_reached",
            &[("maxAttempts", v_num(2.0))],
        )
    })
    .await;
    assert!(
        fatal_recovery
            .snapshot()
            .iter()
            .any(|request| request.reason == "automatic_restart_limit_reached"),
        "應要求 fatal recovery"
    );
    assert_eq!(launcher.launch_calls(), 3);
}

// 原測試：invalidation observer 拋錯不阻塞 page 清理與重建
#[tokio::test(start_paused = true)]
async fn throwing_invalidation_observer_does_not_block_cleanup_and_rebuild() {
    let failed_page = MockPageAdapter::new("failed");
    let replacement_page = MockPageAdapter::new("replacement");
    let context =
        MockContextAdapter::new(vec![Ok(failed_page.clone()), Ok(replacement_page.clone())]);
    let logger = FakeLogger::new();
    let launcher = MockLauncher::new(launch_results(vec![Ok(MockBrowserAdapter::new_with(
        context,
    ))]));
    let manager = make_manager(
        create_config(),
        BrowserManagerDependencies {
            launcher: Some(launcher as Arc<dyn BrowserLauncher>),
            logger: Some(logger.clone() as Arc<dyn BrowserManagerLogger>),
            on_invalidated: Some(Arc::new(FailingInvalidationObserver)),
            ..BrowserManagerDependencies::default()
        },
    );
    manager.start().await.unwrap();
    manager.create_page("channel").await.unwrap();

    failed_page.emit_crash();

    assert_eventually("observer 錯誤應被記錄", || {
        logger.called_with(
            LogLevel::Error,
            "browser_invalidation_observer_failed",
            &[
                ("channel", v_str("channel")),
                ("reason", v_str("page_crashed")),
            ],
        )
    })
    .await;
    let recreated = manager.create_page("channel").await.unwrap();
    assert!(Arc::ptr_eq(&recreated, &replacement_page.page));
}

// 原測試：page close 卡住時會 timeout 並排程 full browser recycle，不可同 context 重建
#[tokio::test(start_paused = true)]
async fn hung_page_close_times_out_and_schedules_full_recycle() {
    let failed_page = MockPageAdapter::new("failed");
    failed_page.hang_on_close();
    let first_context = MockContextAdapter::new(vec![Ok(failed_page)]);
    let first_browser = MockBrowserAdapter::new_with(first_context);
    let replacement_page = MockPageAdapter::new("replacement");
    let second_context = MockContextAdapter::new(vec![Ok(replacement_page.clone())]);
    let second_browser = MockBrowserAdapter::new_with(second_context.clone());
    let logger = FakeLogger::new();
    let invalidations = InvalidationRecorder::new();
    let launcher = MockLauncher::new(launch_results(vec![
        Ok(first_browser.clone()),
        Ok(second_browser.clone()),
    ]));
    let manager = make_manager(
        create_config(),
        BrowserManagerDependencies {
            launcher: Some(launcher as Arc<dyn BrowserLauncher>),
            logger: Some(logger.clone() as Arc<dyn BrowserManagerLogger>),
            on_invalidated: Some(Arc::new(invalidations.clone())),
            resource_close_timeout_ms: Some(100),
            ..BrowserManagerDependencies::default()
        },
    );
    manager.start().await.unwrap();
    manager.create_page("channel").await.unwrap();

    manager.close_page("channel").await.unwrap();

    assert!(
        logger.called_exactly_with(
            LogLevel::Warn,
            "browser_page_close_timeout",
            &[
                ("channel", v_str("channel")),
                ("phase", v_str("close")),
                ("timeoutMs", v_num(100.0)),
            ],
        ),
        "應記錄逾時警告"
    );
    assert!(
        logger.called_with(
            LogLevel::Warn,
            "browser_page_close_requires_recycle",
            &[
                ("channel", v_str("channel")),
                ("reason", v_str("page_close_timeout"))
            ],
        ),
        "應要求 recycle"
    );

    // full recycle 關閉舊 browser 並啟動替換。
    assert_eventually("舊 browser 應被關閉", || {
        first_browser.close_calls() > 0
    })
    .await;
    assert_eventually("registry 應清空", || manager.get_page_count() == 0).await;

    // 回收後在新 browser/context 建頁（逾時的頁面已在 recycle 前移除）。
    let recreated = manager.create_page("channel").await.unwrap();
    assert!(Arc::ptr_eq(&recreated, &replacement_page.page));
    assert!(second_context.new_page_calls() >= 1);
    assert!(first_browser.close_calls() >= 1);
}

// 原測試：browser close 無法確認終止時不 launch 替換 browser 並要求 fatal recovery
#[tokio::test(start_paused = true)]
async fn unconfirmed_browser_close_skips_replacement_and_requests_fatal_recovery() {
    let context = MockContextAdapter::new(vec![Ok(MockPageAdapter::new("page"))]);
    let hung_browser = MockBrowserAdapter::new_with(context);
    hung_browser.hang_on_close();
    let fatal_recovery = FatalRecoveryRecorder::new();
    let logger = FakeLogger::new();
    let launcher = MockLauncher::new(launch_results(vec![Ok(hung_browser.clone())]));
    let manager = make_manager(
        create_config(),
        BrowserManagerDependencies {
            launcher: Some(launcher as Arc<dyn BrowserLauncher>),
            logger: Some(logger.clone() as Arc<dyn BrowserManagerLogger>),
            on_fatal_recovery: Some(Arc::new(fatal_recovery.clone())),
            resource_close_timeout_ms: Some(50),
            ..BrowserManagerDependencies::default()
        },
    );
    manager.start().await.unwrap();
    manager.create_page("channel").await.unwrap();

    let restarted = manager.restart().await;
    assert!(
        error_message_contains(&restarted, "termination was not confirmed"),
        "{restarted:?}"
    );

    assert!(
        fatal_recovery
            .snapshot()
            .iter()
            .any(|request| request.reason == "browser_termination_unconfirmed"),
        "應要求 fatal recovery"
    );
    assert!(
        logger.called_with(
            LogLevel::Error,
            "browser_termination_unconfirmed",
            &[("mode", v_str("manual"))],
        ),
        "應記錄 browser_termination_unconfirmed"
    );
}

// 原測試：自動重啟次數耗盡時要求 container restart
#[tokio::test(start_paused = true)]
async fn automatic_restart_attempt_exhaustion_requests_container_restart() {
    let first = MockBrowserAdapter::new_with(MockContextAdapter::new(Vec::new()));
    let second = MockBrowserAdapter::new_with(MockContextAdapter::new(Vec::new()));
    let third = MockBrowserAdapter::new_with(MockContextAdapter::new(Vec::new()));
    let fatal_recovery = FatalRecoveryRecorder::new();
    let logger = FakeLogger::new();
    let launcher = MockLauncher::new(launch_results(vec![
        Ok(first.clone()),
        Ok(second.clone()),
        Ok(third.clone()),
    ]));
    let manager = make_manager(
        // crash-loop breaker 調高，隔離重啟次數上限。
        create_config_thresholds(true, true, true, 2.0, 10.0),
        BrowserManagerDependencies {
            launcher: Some(launcher.clone() as Arc<dyn BrowserLauncher>),
            logger: Some(logger.clone() as Arc<dyn BrowserManagerLogger>),
            on_fatal_recovery: Some(Arc::new(fatal_recovery.clone())),
            max_automatic_restart_attempts: Some(2),
            restart_backoff_ms: Some(1),
            restart_backoff_max_ms: Some(1),
            restart_attempt_reset_ms: Some(60_000),
            ..BrowserManagerDependencies::default()
        },
    );
    manager.start().await.unwrap();

    first.emit_disconnected();
    assert_eventually("第一次 crash 應重啟", || launcher.launch_calls() == 2).await;
    second.emit_disconnected();
    assert_eventually("第二次 crash 應重啟", || launcher.launch_calls() == 3).await;
    third.emit_disconnected();

    assert_eventually("應要求 container restart", || {
        fatal_recovery
            .snapshot()
            .iter()
            .any(|request| request.reason == "automatic_restart_limit_reached")
    })
    .await;
    assert!(
        logger.called_exactly_with(
            LogLevel::Error,
            "browser_restart_limit_reached",
            &[("maxAttempts", v_num(2.0))],
        ),
        "應記錄次數上限"
    );
}

// 原測試：短時間多次 unexpected browser disconnect 會以 crash-loop 要求 container restart
#[tokio::test(start_paused = true)]
async fn rapid_disconnects_escalate_crash_loop_to_container_restart() {
    let clock = Arc::new(AtomicU64::new(0));
    let first_browser = MockBrowserAdapter::new_with(MockContextAdapter::new(Vec::new()));
    let second_browser = MockBrowserAdapter::new_with(MockContextAdapter::new(Vec::new()));
    let third_browser = MockBrowserAdapter::new_with(MockContextAdapter::new(Vec::new()));
    let fatal_recovery = FatalRecoveryRecorder::new();
    let launcher = MockLauncher::new(launch_results(vec![
        Ok(first_browser.clone()),
        Ok(second_browser.clone()),
        Ok(third_browser.clone()),
    ]));
    let clock_for_manager = Arc::clone(&clock);
    let now: ManagerClock = Arc::new(move || clock_for_manager.load(Ordering::SeqCst));
    let mut config = create_config_thresholds(true, true, true, 2.0, 3.0);
    config.browser.recovery.browser_failure_window_seconds = 60.0;
    let manager = make_manager(
        config,
        BrowserManagerDependencies {
            launcher: Some(launcher.clone() as Arc<dyn BrowserLauncher>),
            logger: Some(FakeLogger::new() as Arc<dyn BrowserManagerLogger>),
            sleep: Some(FakeSleep::immediate() as Arc<dyn SleepFn>),
            now: Some(now),
            max_automatic_restart_attempts: Some(10),
            on_fatal_recovery: Some(Arc::new(fatal_recovery.clone())),
            ..BrowserManagerDependencies::default()
        },
    );
    manager.start().await.unwrap();
    assert_eq!(launcher.launch_calls(), 1);

    clock.store(1_000, Ordering::SeqCst);
    first_browser.emit_disconnected();
    assert_eventually("第一次 disconnect 應重啟", || {
        launcher.launch_calls() == 2
    })
    .await;

    clock.store(2_000, Ordering::SeqCst);
    second_browser.emit_disconnected();
    assert_eventually("第二次 disconnect 應重啟", || {
        launcher.launch_calls() == 3
    })
    .await;

    clock.store(3_000, Ordering::SeqCst);
    third_browser.emit_disconnected();
    assert_eventually("應以 crash-loop 要求 container restart", || {
        fatal_recovery
            .snapshot()
            .iter()
            .any(|request| request.reason == "browser_crash_loop")
    })
    .await;
    // 第三次 disconnect 升級 fatal，不再 launch。
    assert_eq!(launcher.launch_calls(), 3);
}

// 原測試：同一 channel 重複導覽逾時只記錄 per-channel diagnostic
#[tokio::test(start_paused = true)]
async fn repeated_navigation_timeouts_record_per_channel_diagnostic_only() {
    let first_browser = MockBrowserAdapter::new_with(MockContextAdapter::new(Vec::new()));
    let second_browser = MockBrowserAdapter::new_with(MockContextAdapter::new(Vec::new()));
    let launcher = MockLauncher::new(launch_results(vec![
        Ok(first_browser.clone()),
        Ok(second_browser),
    ]));
    let logger = FakeLogger::new();
    let manager = make_manager(
        create_config(),
        BrowserManagerDependencies {
            launcher: Some(launcher.clone() as Arc<dyn BrowserLauncher>),
            logger: Some(logger.clone() as Arc<dyn BrowserManagerLogger>),
            ..BrowserManagerDependencies::default()
        },
    );
    manager.start().await.unwrap();

    manager
        .report_navigation_outcomes(vec![BrowserNavigationOutcome {
            channel: "channel".to_string(),
            status: BrowserNavigationStatus::TimedOut,
        }])
        .await;
    assert_eq!(launcher.launch_calls(), 1);

    manager
        .report_navigation_outcomes(vec![BrowserNavigationOutcome {
            channel: "channel".to_string(),
            status: BrowserNavigationStatus::TimedOut,
        }])
        .await;

    assert_eq!(launcher.launch_calls(), 1);
    assert_eq!(first_browser.close_calls(), 0);
    let debug_fields = logger.fields_of(LogLevel::Debug, "browser_navigation_failure_recorded");
    let last = debug_fields.last().expect("應有導覽失敗的 debug 記錄");
    assert!(
        object_equals(
            &[
                ("channel", v_str("channel")),
                ("status", v_str("timed_out")),
                ("recovery", v_str("per_channel")),
                ("browserGeneration", v_num(1.0)),
            ],
            last,
        ),
        "最後一筆 debug 應為 per-channel 診斷：{last:?}"
    );
    assert!(logger.never_called_with(
        LogLevel::Warn,
        "browser_navigation_failure_recycle_requested"
    ));
}

// 原測試：成功導覽與後續 timeout 都不會建立 browser-level breaker
#[tokio::test(start_paused = true)]
async fn navigation_outcomes_do_not_build_browser_level_breaker() {
    let first_browser = MockBrowserAdapter::new_with(MockContextAdapter::new(Vec::new()));
    let second_browser = MockBrowserAdapter::new_with(MockContextAdapter::new(Vec::new()));
    let launcher = MockLauncher::new(launch_results(vec![Ok(first_browser), Ok(second_browser)]));
    let manager = make_manager(
        create_config(),
        BrowserManagerDependencies {
            launcher: Some(launcher.clone() as Arc<dyn BrowserLauncher>),
            ..BrowserManagerDependencies::default()
        },
    );
    manager.start().await.unwrap();

    let outcome = |channel: &str, status: BrowserNavigationStatus| BrowserNavigationOutcome {
        channel: channel.to_string(),
        status,
    };
    manager
        .report_navigation_outcomes(vec![outcome("channel", BrowserNavigationStatus::TimedOut)])
        .await;
    manager
        .report_navigation_outcomes(vec![outcome("channel", BrowserNavigationStatus::Succeeded)])
        .await;
    manager
        .report_navigation_outcomes(vec![outcome("channel", BrowserNavigationStatus::TimedOut)])
        .await;
    assert_eq!(launcher.launch_calls(), 1);

    manager
        .report_navigation_outcomes(vec![outcome("channel", BrowserNavigationStatus::TimedOut)])
        .await;
    assert_eq!(launcher.launch_calls(), 1);
}

// 原測試：不同 channel 的導覽逾時也不會回收 browser
#[tokio::test(start_paused = true)]
async fn distinct_channels_navigation_timeouts_do_not_recycle_browser() {
    let first_browser = MockBrowserAdapter::new_with(MockContextAdapter::new(Vec::new()));
    let second_browser = MockBrowserAdapter::new_with(MockContextAdapter::new(Vec::new()));
    let launcher = MockLauncher::new(launch_results(vec![Ok(first_browser), Ok(second_browser)]));
    let logger = FakeLogger::new();
    let manager = make_manager(
        create_config(),
        BrowserManagerDependencies {
            launcher: Some(launcher.clone() as Arc<dyn BrowserLauncher>),
            logger: Some(logger.clone() as Arc<dyn BrowserManagerLogger>),
            ..BrowserManagerDependencies::default()
        },
    );
    manager.start().await.unwrap();

    manager
        .report_navigation_outcomes(vec![
            BrowserNavigationOutcome {
                channel: "first".to_string(),
                status: BrowserNavigationStatus::TimedOut,
            },
            BrowserNavigationOutcome {
                channel: "second".to_string(),
                status: BrowserNavigationStatus::TimedOut,
            },
            BrowserNavigationOutcome {
                channel: "third".to_string(),
                status: BrowserNavigationStatus::TimedOut,
            },
        ])
        .await;

    assert_eq!(launcher.launch_calls(), 1);
    assert_eq!(logger.call_count(LogLevel::Debug), 3);
    assert!(logger.never_called_with(
        LogLevel::Warn,
        "browser_navigation_failure_recycle_requested"
    ));
}

// 原測試：不同時間的導覽逾時不會累積 browser-level recovery
#[tokio::test(start_paused = true)]
async fn navigation_timeouts_at_different_times_do_not_accumulate_breaker() {
    let clock = Arc::new(AtomicU64::new(0));
    let browser = MockBrowserAdapter::new_with(MockContextAdapter::new(Vec::new()));
    let launcher = MockLauncher::new(launch_results(vec![Ok(browser)]));
    let clock_for_manager = Arc::clone(&clock);
    let now: ManagerClock = Arc::new(move || clock_for_manager.load(Ordering::SeqCst));
    let manager = make_manager(
        create_config(),
        BrowserManagerDependencies {
            launcher: Some(launcher.clone() as Arc<dyn BrowserLauncher>),
            now: Some(now),
            ..BrowserManagerDependencies::default()
        },
    );
    manager.start().await.unwrap();

    manager
        .report_navigation_outcomes(vec![BrowserNavigationOutcome {
            channel: "channel".to_string(),
            status: BrowserNavigationStatus::TimedOut,
        }])
        .await;
    clock.store(5 * 60_000 + 1, Ordering::SeqCst);
    manager
        .report_navigation_outcomes(vec![BrowserNavigationOutcome {
            channel: "channel".to_string(),
            status: BrowserNavigationStatus::TimedOut,
        }])
        .await;

    assert_eq!(launcher.launch_calls(), 1);
}

// 原測試：回收後忽略舊 recovery epoch 排隊中的 navigation outcome
#[tokio::test(start_paused = true)]
async fn stale_epoch_navigation_outcomes_are_ignored_after_recycle() {
    let first_browser = MockBrowserAdapter::new_with(MockContextAdapter::new(Vec::new()));
    let second_browser = MockBrowserAdapter::new_with(MockContextAdapter::new(Vec::new()));
    let launcher = MockLauncher::new(launch_results(vec![Ok(first_browser), Ok(second_browser)]));
    let logger = FakeLogger::new();
    let manager = make_manager(
        create_config(),
        BrowserManagerDependencies {
            launcher: Some(launcher.clone() as Arc<dyn BrowserLauncher>),
            logger: Some(logger.clone() as Arc<dyn BrowserManagerLogger>),
            ..BrowserManagerDependencies::default()
        },
    );
    manager.start().await.unwrap();
    manager
        .report_navigation_outcomes(vec![BrowserNavigationOutcome {
            channel: "channel".to_string(),
            status: BrowserNavigationStatus::TimedOut,
        }])
        .await;

    let recycle = manager.restart();
    let stale_batch = manager.report_navigation_outcomes(vec![
        BrowserNavigationOutcome {
            channel: "first".to_string(),
            status: BrowserNavigationStatus::TimedOut,
        },
        BrowserNavigationOutcome {
            channel: "second".to_string(),
            status: BrowserNavigationStatus::TimedOut,
        },
        BrowserNavigationOutcome {
            channel: "third".to_string(),
            status: BrowserNavigationStatus::TimedOut,
        },
    ]);
    let (recycle_result, _) = tokio::join!(recycle, stale_batch);
    assert!(recycle_result.is_ok());

    assert_eq!(launcher.launch_calls(), 2);
    let stale_records = logger
        .fields_of(LogLevel::Debug, "browser_navigation_failure_recorded")
        .into_iter()
        .filter(|fields| object_contains(&[("channel", v_str("first"))], fields))
        .count();
    assert_eq!(stale_records, 0, "舊 epoch 的批次不應記錄");
}

// 原測試：大量 navigation timeout 不會要求 container restart
#[tokio::test(start_paused = true)]
async fn massive_navigation_timeouts_do_not_request_container_restart() {
    let clock = Arc::new(AtomicU64::new(0));
    let first_browser = MockBrowserAdapter::new_with(MockContextAdapter::new(Vec::new()));
    let second_browser = MockBrowserAdapter::new_with(MockContextAdapter::new(Vec::new()));
    let third_browser = MockBrowserAdapter::new_with(MockContextAdapter::new(Vec::new()));
    let launcher = MockLauncher::new(launch_results(vec![
        Ok(first_browser),
        Ok(second_browser),
        Ok(third_browser),
    ]));
    let fatal_recovery = FatalRecoveryRecorder::new();
    let clock_for_manager = Arc::clone(&clock);
    let now: ManagerClock = Arc::new(move || clock_for_manager.load(Ordering::SeqCst));
    let manager = make_manager(
        create_config(),
        BrowserManagerDependencies {
            launcher: Some(launcher.clone() as Arc<dyn BrowserLauncher>),
            now: Some(now),
            on_fatal_recovery: Some(Arc::new(fatal_recovery.clone())),
            ..BrowserManagerDependencies::default()
        },
    );
    manager.start().await.unwrap();

    for recovery in 1..=3u64 {
        clock.store(recovery * 1_000, Ordering::SeqCst);
        manager
            .report_navigation_outcomes(vec![BrowserNavigationOutcome {
                channel: "channel".to_string(),
                status: BrowserNavigationStatus::TimedOut,
            }])
            .await;
        manager
            .report_navigation_outcomes(vec![BrowserNavigationOutcome {
                channel: "channel".to_string(),
                status: BrowserNavigationStatus::TimedOut,
            }])
            .await;
    }

    assert!(fatal_recovery.snapshot().is_empty());
    assert_eq!(launcher.launch_calls(), 1);
}

// 原測試：createPage 進行中呼叫 stop 時會等待建立完成後再清理
#[tokio::test(start_paused = true)]
async fn stop_during_in_flight_create_page_waits_then_cleans_up() {
    let page = MockPageAdapter::new("delayed");
    let (context, deferred_sender) = MockContextAdapter::new_deferred();
    let browser = MockBrowserAdapter::new_with(context.clone());
    let launcher = MockLauncher::new(launch_results(vec![Ok(browser.clone())]));
    let manager = make_manager(
        create_config(),
        BrowserManagerDependencies {
            launcher: Some(launcher as Arc<dyn BrowserLauncher>),
            ..BrowserManagerDependencies::default()
        },
    );
    manager.start().await.unwrap();

    let create_future = manager.create_page("channel");
    let stop_future = manager.stop();
    deferred_sender
        .send(Ok(page.clone()))
        .map_err(|_| "deferred 已被取走")
        .unwrap();

    let (created, _) = tokio::join!(create_future, stop_future);
    assert!(Arc::ptr_eq(&created.unwrap(), &page.page));

    assert_eq!(page.close_calls(), 1);
    assert_eq!(context.close_calls(), 1);
    assert_eq!(browser.close_calls(), 1);
}

// 原測試：request failed 記錄含 channel 與 generation 的消毒後端點資訊
#[tokio::test(start_paused = true)]
async fn request_failed_logs_sanitized_endpoint_with_channel_and_generations() {
    let page = MockPageAdapter::new("channel");
    let context = MockContextAdapter::new(vec![Ok(page.clone())]);
    let logger = FakeLogger::new();
    let launcher = MockLauncher::new(launch_results(vec![Ok(MockBrowserAdapter::new_with(
        context,
    ))]));
    let manager = make_manager(
        create_config(),
        BrowserManagerDependencies {
            launcher: Some(launcher as Arc<dyn BrowserLauncher>),
            logger: Some(logger.clone() as Arc<dyn BrowserManagerLogger>),
            ..BrowserManagerDependencies::default()
        },
    );
    manager.start().await.unwrap();
    manager.create_page("channel").await.unwrap();

    page.emit_request_failed(BrowserRequestFailureDiagnostic {
        url: "https://static.twitchcdn.net/assets/main.js?token=super-secret&challenge=abc"
            .to_string(),
        method: "GET".to_string(),
        resource_type: "script".to_string(),
        failure_text: Some("net::ERR_ABORTED".to_string()),
        graphql_operation_names: None,
    });

    let records = logger.fields_of(LogLevel::Debug, "browser_request_failed");
    assert_eq!(records.len(), 1, "應記錄一筆 browser_request_failed");
    assert!(
        object_equals(
            &[
                ("channel", v_str("channel")),
                ("browserGeneration", v_num(1.0)),
                ("pageGeneration", v_num(1.0)),
                ("endpointCategory", v_str("twitch_javascript")),
                ("host", v_str("static.twitchcdn.net")),
                ("path", v_str("/assets/main.js")),
                ("method", v_str("GET")),
                ("resourceType", v_str("script")),
                ("failureText", v_str("net::ERR_ABORTED")),
            ],
            &records[0],
        ),
        "欄位應完整：{:?}",
        records[0]
    );
    let serialized = logger.serialized();
    assert!(!serialized.contains("super-secret"), "{serialized}");
    assert!(!serialized.contains("challenge=abc"), "{serialized}");
}

// 原測試：GraphQL request failed 記錄 operationName 但不記錄 body
#[tokio::test(start_paused = true)]
async fn graphql_request_failed_logs_operation_name_but_not_body() {
    let page = MockPageAdapter::new("channel");
    let context = MockContextAdapter::new(vec![Ok(page.clone())]);
    let logger = FakeLogger::new();
    let launcher = MockLauncher::new(launch_results(vec![Ok(MockBrowserAdapter::new_with(
        context,
    ))]));
    let manager = make_manager(
        create_config(),
        BrowserManagerDependencies {
            launcher: Some(launcher as Arc<dyn BrowserLauncher>),
            logger: Some(logger.clone() as Arc<dyn BrowserManagerLogger>),
            ..BrowserManagerDependencies::default()
        },
    );
    manager.start().await.unwrap();
    manager.create_page("channel").await.unwrap();

    page.emit_request_failed(BrowserRequestFailureDiagnostic {
        url: "https://gql.twitch.tv/gql?client_id=secret".to_string(),
        method: "POST".to_string(),
        resource_type: "fetch".to_string(),
        failure_text: Some("net::ERR_FAILED".to_string()),
        graphql_operation_names: Some(vec!["PlaybackAccessToken".to_string()]),
    });

    assert!(
        logger.called_with(
            LogLevel::Warn,
            "browser_request_failed",
            &[
                ("channel", v_str("channel")),
                ("endpointCategory", v_str("twitch_graphql")),
                ("host", v_str("gql.twitch.tv")),
                ("path", v_str("/gql")),
                ("method", v_str("POST")),
                ("resourceType", v_str("fetch")),
                (
                    "graphQlOperationNames",
                    SanitizedValue::Array(vec![v_str("PlaybackAccessToken")])
                ),
            ],
        ),
        "GraphQL 失敗應以 warn 記錄 operationName"
    );
}

// 原測試：third_party request failed 以 debug 記錄
#[tokio::test(start_paused = true)]
async fn third_party_request_failed_logged_as_debug() {
    let page = MockPageAdapter::new("channel");
    let context = MockContextAdapter::new(vec![Ok(page.clone())]);
    let logger = FakeLogger::new();
    let launcher = MockLauncher::new(launch_results(vec![Ok(MockBrowserAdapter::new_with(
        context,
    ))]));
    let manager = make_manager(
        create_config(),
        BrowserManagerDependencies {
            launcher: Some(launcher as Arc<dyn BrowserLauncher>),
            logger: Some(logger.clone() as Arc<dyn BrowserManagerLogger>),
            ..BrowserManagerDependencies::default()
        },
    );
    manager.start().await.unwrap();
    manager.create_page("channel").await.unwrap();

    page.emit_request_failed(BrowserRequestFailureDiagnostic {
        url: "https://spade.twitch.tv/track".to_string(),
        method: "POST".to_string(),
        resource_type: "xhr".to_string(),
        failure_text: Some("net::ERR_CONNECTION_REFUSED".to_string()),
        graphql_operation_names: None,
    });

    assert!(logger.call_count(LogLevel::Warn) == 0);
    assert!(
        logger.called_with(
            LogLevel::Debug,
            "browser_request_failed",
            &[
                ("channel", v_str("channel")),
                ("endpointCategory", v_str("twitch_other")),
                ("host", v_str("spade.twitch.tv")),
                ("path", v_str("/track")),
                ("method", v_str("POST")),
                ("resourceType", v_str("xhr")),
                ("failureText", v_str("net::ERR_CONNECTION_REFUSED")),
            ],
        ),
        "third_party 失敗應以 debug 記錄"
    );
}

// 原測試：twitch_media ERR_ABORTED request failed 以 debug 記錄
#[tokio::test(start_paused = true)]
async fn twitch_media_err_aborted_request_failed_logged_as_debug() {
    let page = MockPageAdapter::new("channel");
    let context = MockContextAdapter::new(vec![Ok(page.clone())]);
    let logger = FakeLogger::new();
    let launcher = MockLauncher::new(launch_results(vec![Ok(MockBrowserAdapter::new_with(
        context,
    ))]));
    let manager = make_manager(
        create_config(),
        BrowserManagerDependencies {
            launcher: Some(launcher as Arc<dyn BrowserLauncher>),
            logger: Some(logger.clone() as Arc<dyn BrowserManagerLogger>),
            ..BrowserManagerDependencies::default()
        },
    );
    manager.start().await.unwrap();
    manager.create_page("channel").await.unwrap();

    page.emit_request_failed(BrowserRequestFailureDiagnostic {
        url: "https://video-weaver.atl01.hls.ttvnw.net/v1/segment/part.ts".to_string(),
        method: "GET".to_string(),
        resource_type: "fetch".to_string(),
        failure_text: Some("net::ERR_ABORTED".to_string()),
        graphql_operation_names: None,
    });

    assert!(logger.call_count(LogLevel::Warn) == 0);
    assert!(
        logger.called_with(
            LogLevel::Debug,
            "browser_request_failed",
            &[
                ("channel", v_str("channel")),
                ("endpointCategory", v_str("twitch_media")),
                ("host", v_str("video-weaver.atl01.hls.ttvnw.net")),
                ("failureText", v_str("net::ERR_ABORTED")),
            ],
        ),
        "媒體端點的 ERR_ABORTED 應以 debug 記錄"
    );
}

// 原測試：5xx 回應以 warn 記錄
#[tokio::test(start_paused = true)]
async fn five_hundred_response_logged_as_warn() {
    let page = MockPageAdapter::new("channel");
    let context = MockContextAdapter::new(vec![Ok(page.clone())]);
    let logger = FakeLogger::new();
    let launcher = MockLauncher::new(launch_results(vec![Ok(MockBrowserAdapter::new_with(
        context,
    ))]));
    let manager = make_manager(
        create_config(),
        BrowserManagerDependencies {
            launcher: Some(launcher as Arc<dyn BrowserLauncher>),
            logger: Some(logger.clone() as Arc<dyn BrowserManagerLogger>),
            ..BrowserManagerDependencies::default()
        },
    );
    manager.start().await.unwrap();
    manager.create_page("channel").await.unwrap();

    page.emit_response(BrowserHttpResponseDiagnostic {
        url: "https://static.twitchcdn.net/assets/loader.js".to_string(),
        method: "GET".to_string(),
        resource_type: "script".to_string(),
        status: 503,
        status_text: "Service Unavailable".to_string(),
        graphql_operation_names: None,
    });

    assert!(
        logger.called_with(
            LogLevel::Warn,
            "browser_http_response",
            &[
                ("channel", v_str("channel")),
                ("endpointCategory", v_str("twitch_javascript")),
                ("httpStatus", v_num(503.0)),
                ("statusText", v_str("Service Unavailable")),
            ],
        ),
        "5xx 應以 warn 記錄"
    );
}

// 原測試：4xx document 回應以 warn 記錄
#[tokio::test(start_paused = true)]
async fn four_hundred_document_response_logged_as_warn() {
    let page = MockPageAdapter::new("channel");
    let context = MockContextAdapter::new(vec![Ok(page.clone())]);
    let logger = FakeLogger::new();
    let launcher = MockLauncher::new(launch_results(vec![Ok(MockBrowserAdapter::new_with(
        context,
    ))]));
    let manager = make_manager(
        create_config(),
        BrowserManagerDependencies {
            launcher: Some(launcher as Arc<dyn BrowserLauncher>),
            logger: Some(logger.clone() as Arc<dyn BrowserManagerLogger>),
            ..BrowserManagerDependencies::default()
        },
    );
    manager.start().await.unwrap();
    manager.create_page("channel").await.unwrap();

    page.emit_response(BrowserHttpResponseDiagnostic {
        url: "https://www.twitch.tv/some_channel".to_string(),
        method: "GET".to_string(),
        resource_type: "document".to_string(),
        status: 404,
        status_text: "Not Found".to_string(),
        graphql_operation_names: None,
    });

    assert!(
        logger.called_with(
            LogLevel::Warn,
            "browser_http_response",
            &[
                ("channel", v_str("channel")),
                ("endpointCategory", v_str("twitch_document")),
                ("httpStatus", v_num(404.0)),
            ],
        ),
        "4xx document 應以 warn 記錄"
    );
}

// 原測試：非相關的 4xx 與成功的 media/圖片不記錄
#[tokio::test(start_paused = true)]
async fn irrelevant_four_hundred_and_successful_media_not_logged() {
    let page = MockPageAdapter::new("channel");
    let context = MockContextAdapter::new(vec![Ok(page.clone())]);
    let logger = FakeLogger::new();
    let launcher = MockLauncher::new(launch_results(vec![Ok(MockBrowserAdapter::new_with(
        context,
    ))]));
    let manager = make_manager(
        create_config(),
        BrowserManagerDependencies {
            launcher: Some(launcher as Arc<dyn BrowserLauncher>),
            logger: Some(logger.clone() as Arc<dyn BrowserManagerLogger>),
            ..BrowserManagerDependencies::default()
        },
    );
    manager.start().await.unwrap();
    manager.create_page("channel").await.unwrap();

    page.emit_response(BrowserHttpResponseDiagnostic {
        url: "https://video-weaver.atl01.hls.ttvnw.net/v1/playlist/segment.ts".to_string(),
        method: "GET".to_string(),
        resource_type: "fetch".to_string(),
        status: 403,
        status_text: "Forbidden".to_string(),
        graphql_operation_names: None,
    });
    page.emit_response(BrowserHttpResponseDiagnostic {
        url: "https://analytics.example.com/track".to_string(),
        method: "POST".to_string(),
        resource_type: "fetch".to_string(),
        status: 200,
        status_text: "OK".to_string(),
        graphql_operation_names: None,
    });

    assert_eq!(logger.call_count(LogLevel::Warn), 0);
    assert_eq!(logger.call_count(LogLevel::Debug), 0);
}

// 原測試：成功的 Twitch bootstrap 回應以 debug 記錄
#[tokio::test(start_paused = true)]
async fn successful_twitch_bootstrap_response_logged_as_debug() {
    let page = MockPageAdapter::new("channel");
    let context = MockContextAdapter::new(vec![Ok(page.clone())]);
    let logger = FakeLogger::new();
    let launcher = MockLauncher::new(launch_results(vec![Ok(MockBrowserAdapter::new_with(
        context,
    ))]));
    let manager = make_manager(
        create_config(),
        BrowserManagerDependencies {
            launcher: Some(launcher as Arc<dyn BrowserLauncher>),
            logger: Some(logger.clone() as Arc<dyn BrowserManagerLogger>),
            ..BrowserManagerDependencies::default()
        },
    );
    manager.start().await.unwrap();
    manager.create_page("channel").await.unwrap();

    page.emit_response(BrowserHttpResponseDiagnostic {
        url: "https://gql.twitch.tv/gql".to_string(),
        method: "POST".to_string(),
        resource_type: "fetch".to_string(),
        status: 200,
        status_text: "OK".to_string(),
        graphql_operation_names: Some(vec!["PlaybackAccessToken".to_string()]),
    });

    assert!(
        logger.called_with(
            LogLevel::Debug,
            "browser_http_response",
            &[
                ("channel", v_str("channel")),
                ("endpointCategory", v_str("twitch_graphql")),
                ("httpStatus", v_num(200.0)),
                (
                    "graphQlOperationNames",
                    SanitizedValue::Array(vec![v_str("PlaybackAccessToken")])
                ),
            ],
        ),
        "成功的 bootstrap 回應應以 debug 記錄"
    );
}

// 原測試：console error 對應 warn、warning 對應 debug、log 不記錄
#[tokio::test(start_paused = true)]
async fn console_levels_map_warn_debug_and_ignore_log() {
    let page = MockPageAdapter::new("channel");
    let context = MockContextAdapter::new(vec![Ok(page.clone())]);
    let logger = FakeLogger::new();
    let launcher = MockLauncher::new(launch_results(vec![Ok(MockBrowserAdapter::new_with(
        context,
    ))]));
    let manager = make_manager(
        create_config(),
        BrowserManagerDependencies {
            launcher: Some(launcher as Arc<dyn BrowserLauncher>),
            logger: Some(logger.clone() as Arc<dyn BrowserManagerLogger>),
            ..BrowserManagerDependencies::default()
        },
    );
    manager.start().await.unwrap();
    manager.create_page("channel").await.unwrap();

    page.emit_console(BrowserConsoleDiagnostic {
        r#type: "error".to_string(),
        text: "Failed to load resource: token=abc".to_string(),
        source_url: Some("https://static.twitchcdn.net/app.js?secret=1".to_string()),
        line_number: 12,
        column_number: 4,
    });
    page.emit_console(BrowserConsoleDiagnostic {
        r#type: "warning".to_string(),
        text: "Deprecated API usage".to_string(),
        source_url: Some("https://static.twitchcdn.net/legacy.js".to_string()),
        line_number: 7,
        column_number: 2,
    });
    page.emit_console(BrowserConsoleDiagnostic {
        r#type: "log".to_string(),
        text: "ordinary log".to_string(),
        source_url: None,
        line_number: 0,
        column_number: 0,
    });

    assert!(
        logger.called_with(
            LogLevel::Warn,
            "browser_console_message",
            &[
                ("channel", v_str("channel")),
                ("consoleType", v_str("error")),
                ("message", v_str("Failed to load resource: token=abc")),
                ("sourceHost", v_str("static.twitchcdn.net")),
                ("sourcePath", v_str("/app.js")),
                ("lineNumber", v_num(12.0)),
                ("columnNumber", v_num(4.0)),
            ],
        ),
        "console error 應以 warn 記錄"
    );
    assert!(
        logger.called_with(
            LogLevel::Debug,
            "browser_console_message",
            &[
                ("channel", v_str("channel")),
                ("consoleType", v_str("warning")),
                ("message", v_str("Deprecated API usage")),
                ("sourceHost", v_str("static.twitchcdn.net")),
                ("sourcePath", v_str("/legacy.js")),
            ],
        ),
        "console warning 應以 debug 記錄"
    );
    let serialized = logger.serialized();
    assert!(!serialized.contains("secret=1"), "{serialized}");
    assert!(!serialized.contains("ordinary log"), "{serialized}");
}

// 原測試：third_party 來源的 console error 與已知噪音 pattern 以 debug 記錄
#[tokio::test(start_paused = true)]
async fn third_party_console_error_and_noise_patterns_logged_as_debug() {
    let page = MockPageAdapter::new("channel");
    let context = MockContextAdapter::new(vec![Ok(page.clone())]);
    let logger = FakeLogger::new();
    let launcher = MockLauncher::new(launch_results(vec![Ok(MockBrowserAdapter::new_with(
        context,
    ))]));
    let manager = make_manager(
        create_config(),
        BrowserManagerDependencies {
            launcher: Some(launcher as Arc<dyn BrowserLauncher>),
            logger: Some(logger.clone() as Arc<dyn BrowserManagerLogger>),
            ..BrowserManagerDependencies::default()
        },
    );
    manager.start().await.unwrap();
    manager.create_page("channel").await.unwrap();

    page.emit_console(BrowserConsoleDiagnostic {
        r#type: "error".to_string(),
        text: "Failed to load resource: net::ERR_CONNECTION_REFUSED".to_string(),
        source_url: Some("https://sb.scorecardresearch.com/p".to_string()),
        line_number: 0,
        column_number: 0,
    });
    page.emit_console(BrowserConsoleDiagnostic {
        r#type: "error".to_string(),
        text: "SpadeClient send error -1 : Failed to fetch".to_string(),
        source_url: Some("https://assets.twitch.tv/amazon-ivs-wasmworker.js".to_string()),
        line_number: 0,
        column_number: 0,
    });

    assert_eq!(logger.call_count(LogLevel::Warn), 0);
    assert!(
        logger.called_with(
            LogLevel::Debug,
            "browser_console_message",
            &[
                ("channel", v_str("channel")),
                ("consoleType", v_str("error")),
                (
                    "message",
                    v_str("Failed to load resource: net::ERR_CONNECTION_REFUSED")
                ),
                ("sourceHost", v_str("sb.scorecardresearch.com")),
                ("sourcePath", v_str("/p")),
            ],
        ),
        "third_party 的 console error 應以 debug 記錄"
    );
    assert!(
        logger.called_with(
            LogLevel::Debug,
            "browser_console_message",
            &[
                ("channel", v_str("channel")),
                ("consoleType", v_str("error")),
                (
                    "message",
                    v_str("SpadeClient send error -1 : Failed to fetch")
                ),
                ("sourceHost", v_str("assets.twitch.tv")),
            ],
        ),
        "已知噪音樣式應以 debug 記錄"
    );
}

// 原測試：closePage 與 stop 會移除全部 diagnostics listener
#[tokio::test(start_paused = true)]
async fn close_page_and_stop_remove_all_diagnostics_listeners() {
    let closed_page = MockPageAdapter::new("closed");
    let stopped_page = MockPageAdapter::new("stopped");
    let context = MockContextAdapter::new(vec![Ok(closed_page.clone()), Ok(stopped_page.clone())]);
    let logger = FakeLogger::new();
    let launcher = MockLauncher::new(launch_results(vec![Ok(MockBrowserAdapter::new_with(
        context,
    ))]));
    let manager = make_manager(
        create_config(),
        BrowserManagerDependencies {
            launcher: Some(launcher as Arc<dyn BrowserLauncher>),
            logger: Some(logger.clone() as Arc<dyn BrowserManagerLogger>),
            ..BrowserManagerDependencies::default()
        },
    );
    manager.start().await.unwrap();
    manager.create_page("closed").await.unwrap();
    manager.create_page("stopped").await.unwrap();

    manager.close_page("closed").await.unwrap();
    closed_page.emit_request_failed(BrowserRequestFailureDiagnostic {
        url: "https://static.twitchcdn.net/after-close.js".to_string(),
        method: "GET".to_string(),
        resource_type: "script".to_string(),
        failure_text: None,
        graphql_operation_names: None,
    });
    closed_page.emit_console(BrowserConsoleDiagnostic {
        r#type: "error".to_string(),
        text: "after close".to_string(),
        source_url: None,
        line_number: 0,
        column_number: 0,
    });
    closed_page.emit_response(BrowserHttpResponseDiagnostic {
        url: "https://static.twitchcdn.net/after-close.js".to_string(),
        method: "GET".to_string(),
        resource_type: "script".to_string(),
        status: 500,
        status_text: "Error".to_string(),
        graphql_operation_names: None,
    });

    manager.stop().await.unwrap();
    stopped_page.emit_request_failed(BrowserRequestFailureDiagnostic {
        url: "https://static.twitchcdn.net/after-stop.js".to_string(),
        method: "GET".to_string(),
        resource_type: "script".to_string(),
        failure_text: None,
        graphql_operation_names: None,
    });
    stopped_page.emit_console(BrowserConsoleDiagnostic {
        r#type: "error".to_string(),
        text: "after stop".to_string(),
        source_url: None,
        line_number: 0,
        column_number: 0,
    });
    stopped_page.emit_response(BrowserHttpResponseDiagnostic {
        url: "https://static.twitchcdn.net/after-stop.js".to_string(),
        method: "GET".to_string(),
        resource_type: "script".to_string(),
        status: 500,
        status_text: "Error".to_string(),
        graphql_operation_names: None,
    });

    let diagnostic_events = [
        "browser_request_failed",
        "browser_http_response",
        "browser_console_message",
    ];
    for (level, event, _) in logger.entries.lock().unwrap().iter() {
        if matches!(level, LogLevel::Warn | LogLevel::Debug) {
            assert!(
                !diagnostic_events.contains(&event.as_str()),
                "診斷事件不應再記錄：{event}"
            );
        }
    }
}
