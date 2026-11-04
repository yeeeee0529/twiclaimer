//! `DefaultSessionManager` 的單元測試:移植舊版 `test/unit/session-manager.test.ts`
//! 的 34 個案例(標題依 `doc/spec/ts-test-titles.tsv` 逐字)。
//!
//! fake 照舊測試 helper 逐一對應:`createSession`/`createFactory`/`createLogger`/
//! `createGate`/`createRecoveryPolicy`。時間的對應:舊測試的 `vi.useFakeTimers`
//! → tokio 的 paused 計時(逾時 timer 會即時推進,單元測試不等真實時間);
//! `vi.waitFor` → [`eventually`] 輪詢;`now` 注入 → 共用時鐘的
//! `AtomicU64`。日誌欄位斷言:`toEqual` → [`object_equals`](鍵序不敏感)、
//! `expect.objectContaining` → [`object_contains`]。
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use tokio::sync::watch;

use super::{
    BrowserNavigationOutcome, BrowserNavigationStatus, DefaultSessionManager,
    SessionChannelSessionFactory, SessionManager, SessionManagerDependencies,
    SessionNavigationOutcomeObserver, SessionOperationTimeoutError,
};
use crate::browser::cdp::context::DriverError;
use crate::browser::health::ChannelHealthResult;
use crate::browser::reward_claimer::RewardClaimResult;
use crate::browser::session::{
    ChannelSession, ChannelSessionPointsResult, ChannelSessionRefreshStatus, ChannelSessionState,
};
use crate::browser::types::{BrowserManagerLogger, SleepFn};
use crate::config::model::{BrowserRecoveryConfig, BrowserSessionStartConfig};
use crate::logging::logger::LogLevel;
use crate::logging::redact::SanitizedValue;
use crate::sessions::manager::{
    SessionChannelPointsResult, SessionChannelPointsUnavailableReason, SessionRefreshResult,
    SessionRefreshStatus, SessionScreenshot,
};
use crate::sessions::recovery::{
    ChannelRecoveryPolicy, ChannelRecoveryPolicyOptions, ChannelRecoveryState,
};

// ---------------------------------------------------------------------------
// 斷言輔助(同 manager/session 模組測試的寫法)
// ---------------------------------------------------------------------------

fn v_str(value: impl Into<String>) -> SanitizedValue {
    SanitizedValue::String(value.into())
}

fn v_num(value: impl Into<f64>) -> SanitizedValue {
    SanitizedValue::Number(value.into())
}

/// 對應 `expect(value).toEqual({...})`:物件鍵序不敏感的完整相等。
fn object_equals(expected: &[(&'static str, SanitizedValue)], actual: &serde_json::Value) -> bool {
    let Some(entries) = actual.as_object() else {
        return false;
    };
    entries.len() == expected.len()
        && expected
            .iter()
            .all(|(key, value)| entries.get(*key) == Some(&to_json(value)))
}

/// 對應 `expect.objectContaining({...})`:expected 的每個鍵值都要出現。
fn object_contains(
    expected: &[(&'static str, SanitizedValue)],
    actual: &serde_json::Value,
) -> bool {
    let Some(entries) = actual.as_object() else {
        return false;
    };
    expected
        .iter()
        .all(|(key, value)| entries.get(*key) == Some(&to_json(value)))
}

/// 斷言側的值:SanitizedValue → JSON 值(與 logger 儲存的形狀相同)。
fn to_json(value: &SanitizedValue) -> serde_json::Value {
    serde_json::from_str(&value.to_json_string()).unwrap_or(serde_json::Value::Null)
}

/// 對應舊測試的 `vi.waitFor`:輪詢條件;paused 計時下 sleep 立即推進,
/// 背景任務在每輪之間執行。
async fn eventually(condition: impl Fn() -> bool) -> bool {
    for _ in 0..1_000 {
        if condition() {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        tokio::task::yield_now().await;
    }
    false
}

async fn assert_eventually(message: &str, condition: impl Fn() -> bool) {
    assert!(eventually(condition).await, "{message}");
}

// ---------------------------------------------------------------------------
// 日誌收集(對應舊測試的 createLogger)
// ---------------------------------------------------------------------------

struct FakeLogger {
    entries: Mutex<Vec<(LogLevel, String, serde_json::Value)>>,
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

    fn fields_of(&self, level: LogLevel, event: &str) -> Vec<serde_json::Value> {
        self.entries
            .lock()
            .unwrap()
            .iter()
            .filter(|(entry_level, entry_event, _)| *entry_level == level && entry_event == event)
            .map(|(_, _, fields)| fields.clone())
            .collect()
    }

    fn called_with(
        &self,
        level: LogLevel,
        event: &str,
        expected: &[(&'static str, SanitizedValue)],
    ) -> bool {
        self.fields_of(level, event)
            .iter()
            .any(|fields| object_contains(expected, fields))
    }

    fn called_exactly_with(
        &self,
        level: LogLevel,
        event: &str,
        expected: &[(&'static str, SanitizedValue)],
    ) -> bool {
        self.fields_of(level, event)
            .iter()
            .any(|fields| object_equals(expected, fields))
    }

    fn level_call_count(&self, level: LogLevel) -> usize {
        self.entries
            .lock()
            .unwrap()
            .iter()
            .filter(|(entry_level, _, _)| *entry_level == level)
            .count()
    }

    /// 全部內容的序列化(對應 `JSON.stringify(mock.calls)` 的洩漏檢查)。
    fn serialized(&self) -> String {
        format!("{:?}", self.entries.lock().unwrap())
    }
}

impl BrowserManagerLogger for FakeLogger {
    fn debug(&self, event: &str, fields: Option<&SanitizedValue>) {
        self.record(LogLevel::Debug, event, fields);
    }

    fn info(&self, event: &str, fields: Option<&SanitizedValue>) {
        // 舊版 SessionManagerLogger 沒有 info;正式碼不會經此呼叫。
        self.record(LogLevel::Info, event, fields);
    }

    fn warn(&self, event: &str, fields: Option<&SanitizedValue>) {
        self.record(LogLevel::Warn, event, fields);
    }

    fn error(&self, event: &str, fields: Option<&SanitizedValue>) {
        self.record(LogLevel::Error, event, fields);
    }
}

fn create_logger() -> Arc<FakeLogger> {
    FakeLogger::new()
}

// ---------------------------------------------------------------------------
// 閘門(對應舊測試的 createGate;watch 訊號讓 future 內可 clone 接收端)
// ---------------------------------------------------------------------------

struct Gate {
    sender: watch::Sender<bool>,
}

impl Gate {
    /// 對應 `gate.release()`。
    fn release(&self) {
        let _ = self.sender.send(true);
    }
}

fn create_gate() -> (Arc<Gate>, watch::Receiver<bool>) {
    let (sender, receiver) = watch::channel(false);
    (Arc::new(Gate { sender }), receiver)
}

/// 等 gate 釋放(對應 `await gate.promise`)。
async fn wait_gate(mut receiver: watch::Receiver<bool>) {
    while !*receiver.borrow_and_update() {
        if receiver.changed().await.is_err() {
            // 發送端消失(任務被丟棄):視為已釋放。
            return;
        }
    }
}

// ---------------------------------------------------------------------------
// fake ChannelSession(對應舊測試的 createSession)
// ---------------------------------------------------------------------------

/// 舊測試 options 的 closure 型別:start/cancel/stop 的行為注入。
///
/// hook 的建構一律走 [`new_start_source`] 等轉接(封閉的回傳型別讓 closure 內部
/// 的具體 future 自然轉成動態型別)。
type StartSource =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Result<(), DriverError>> + Send>> + Send + Sync>;
type CancelSource = Arc<dyn Fn(&str) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;
type StopSource = Arc<
    dyn Fn(&str) -> Pin<Box<dyn Future<Output = Result<(), DriverError>> + Send>> + Send + Sync,
>;
type RefreshNowSource =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Result<bool, DriverError>> + Send>> + Send + Sync>;
type ScreenshotOutcome = Result<Vec<u8>, DriverError>;

fn new_start_source(
    hook: impl Fn() -> Pin<Box<dyn Future<Output = Result<(), DriverError>> + Send>>
    + Send
    + Sync
    + 'static,
) -> StartSource {
    Arc::new(hook)
}

fn new_cancel_source(
    hook: impl Fn(&str) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync + 'static,
) -> CancelSource {
    Arc::new(hook)
}

fn new_stop_source(
    hook: impl Fn(&str) -> Pin<Box<dyn Future<Output = Result<(), DriverError>> + Send>>
    + Send
    + Sync
    + 'static,
) -> StopSource {
    Arc::new(hook)
}

/// 立刻成功/失敗的 start hook(對應舊測試常見的常數 mock)。
fn start_source_always(outcome: Result<(), DriverError>) -> StartSource {
    new_start_source(move || {
        let outcome = outcome.clone();
        Box::pin(async move { outcome })
    })
}

#[derive(Default)]
struct FakeSessionOptions {
    start: Option<StartSource>,
    cancel_start: Option<CancelSource>,
    stop: Option<StopSource>,
    /// 舊版 `screenshot?: Buffer`;`mockRejectedValue` 對應 Err。
    screenshot: Option<ScreenshotOutcome>,
    /// 舊版 `channelPoints?`。
    points: Option<ChannelSessionPointsResult>,
    /// 舊版 `refreshNow?`。
    refresh_now: Option<RefreshNowSource>,
    /// 舊版 `refreshStatus?: { enabled, nextRefreshAt?, secondsUntilRefresh? }`。
    refresh_status: Option<(bool, Option<String>, Option<u64>)>,
}

/// 對映舊測試的 TestSession;`secret` 是舊測試「不洩漏 session」對照既有的
/// 資料形狀(本體從不進 log,R10/F10 由欄位建構保證)。
struct FakeSession {
    #[allow(dead_code)]
    secret: String,
    channel: String,
    start_calls: AtomicUsize,
    start_source: Option<StartSource>,
    cancel_start_calls: Mutex<Vec<String>>,
    cancel_source: Option<CancelSource>,
    stop_calls: Mutex<Vec<String>>,
    stop_source: Option<StopSource>,
    refresh_now_calls: AtomicUsize,
    refresh_now_source: Option<RefreshNowSource>,
    screenshot: Mutex<ScreenshotOutcome>,
    points: ChannelSessionPointsResult,
    refresh_status: Option<(bool, Option<String>, Option<u64>)>,
}

impl FakeSession {
    fn new(channel: &str, options: FakeSessionOptions) -> Arc<Self> {
        let default_screenshot = format!("screenshot:{channel}").into_bytes();
        Arc::new(Self {
            secret: format!("session-secret-{channel}"),
            channel: channel.to_string(),
            start_calls: AtomicUsize::new(0),
            start_source: options.start,
            cancel_start_calls: Mutex::new(Vec::new()),
            cancel_source: options.cancel_start,
            stop_calls: Mutex::new(Vec::new()),
            stop_source: options.stop,
            refresh_now_calls: AtomicUsize::new(0),
            refresh_now_source: options.refresh_now,
            screenshot: Mutex::new(options.screenshot.unwrap_or(Ok(default_screenshot))),
            points: options
                .points
                .unwrap_or(ChannelSessionPointsResult::NotFound),
            refresh_status: options.refresh_status,
        })
    }

    fn start_call_count(&self) -> usize {
        self.start_calls.load(Ordering::SeqCst)
    }

    fn cancel_start_calls(&self) -> Vec<String> {
        self.cancel_start_calls.lock().unwrap().clone()
    }

    fn stop_calls(&self) -> Vec<String> {
        self.stop_calls.lock().unwrap().clone()
    }

    fn refresh_now_call_count(&self) -> usize {
        self.refresh_now_calls.load(Ordering::SeqCst)
    }

    /// 轉成 manager 皮下的 trait object。
    fn as_session(self: &Arc<Self>) -> Arc<dyn ChannelSession> {
        Arc::clone(self) as Arc<dyn ChannelSession>
    }
}

#[async_trait]
impl ChannelSession for FakeSession {
    fn channel(&self) -> &str {
        &self.channel
    }

    fn state(&self) -> ChannelSessionState {
        // 舊 mock 沒有 state getter 的用途;以 stopped 表達。
        ChannelSessionState::Stopped
    }

    async fn start(&self) -> Result<(), DriverError> {
        self.start_calls.fetch_add(1, Ordering::SeqCst);
        match &self.start_source {
            Some(source) => source().await,
            None => Ok(()),
        }
    }

    async fn cancel_start(&self, reason: &str) {
        self.cancel_start_calls
            .lock()
            .unwrap()
            .push(reason.to_string());
        if let Some(source) = &self.cancel_source {
            source(reason).await;
        }
    }

    async fn stop(&self, reason: &str) -> Result<(), DriverError> {
        self.stop_calls.lock().unwrap().push(reason.to_string());
        match &self.stop_source {
            Some(source) => source(reason).await,
            None => Ok(()),
        }
    }

    async fn check_health(&self) -> ChannelHealthResult {
        // 舊 mock:`checkHealth: async () => ({ healthy: true, reason: 'live' })`。
        ChannelHealthResult::healthy()
    }

    async fn tick_reward_claim(&self) -> RewardClaimResult {
        RewardClaimResult::NotFound {
            channel: self.channel.clone(),
            checked_at: "2026-06-14T12:00:00.000Z".to_string(),
        }
    }

    async fn capture_screenshot(&self) -> Result<Vec<u8>, DriverError> {
        self.screenshot.lock().unwrap().clone()
    }

    async fn get_channel_points(&self) -> ChannelSessionPointsResult {
        self.points
    }

    fn get_refresh_status(&self) -> ChannelSessionRefreshStatus {
        match &self.refresh_status {
            None => ChannelSessionRefreshStatus {
                channel: self.channel.clone(),
                enabled: false,
                next_refresh_at: None,
                seconds_until_refresh: None,
            },
            Some((enabled, next_refresh_at, seconds_until_refresh)) => {
                ChannelSessionRefreshStatus {
                    channel: self.channel.clone(),
                    enabled: *enabled,
                    next_refresh_at: next_refresh_at.clone(),
                    seconds_until_refresh: *seconds_until_refresh,
                }
            }
        }
    }

    async fn refresh_now(&self) -> Result<bool, DriverError> {
        self.refresh_now_calls.fetch_add(1, Ordering::SeqCst);
        match &self.refresh_now_source {
            Some(source) => source().await,
            None => Ok(true),
        }
    }
}

/// 舊測試 `createSession(channel, options)`。
fn create_session(channel: &str) -> Arc<FakeSession> {
    create_session_with(channel, FakeSessionOptions::default())
}

fn create_session_with(channel: &str, options: FakeSessionOptions) -> Arc<FakeSession> {
    FakeSession::new(channel, options)
}

// ---------------------------------------------------------------------------
// fake factory(對應舊測試的 createFactory)
// ---------------------------------------------------------------------------

type FakeBuildFn =
    Arc<dyn Fn(&str, usize) -> Result<Arc<dyn ChannelSession>, DriverError> + Send + Sync>;

struct FakeFactory {
    builder: FakeBuildFn,
    calls: Mutex<Vec<String>>,
    /// 下一個 creationIndex;builder 回 Err 時不遞增(照舊 mock 的順序語意)。
    next_index: AtomicUsize,
}

impl FakeFactory {
    fn new(
        builder: impl Fn(&str, usize) -> Result<Arc<dyn ChannelSession>, DriverError>
        + Send
        + Sync
        + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            builder: Arc::new(builder),
            calls: Mutex::new(Vec::new()),
            next_index: AtomicUsize::new(0),
        })
    }

    fn created(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl SessionChannelSessionFactory for FakeFactory {
    fn create(&self, channel: &str) -> Result<Arc<dyn ChannelSession>, DriverError> {
        let index = self.next_index.load(Ordering::SeqCst);
        self.calls.lock().unwrap().push(channel.to_string());
        let builder = Arc::clone(&self.builder);
        match builder(channel, index) {
            Ok(session) => {
                self.next_index
                    .compare_exchange(index, index + 1, Ordering::SeqCst, Ordering::SeqCst)
                    .ok();
                Ok(session)
            }
            Err(error) => Err(error),
        }
    }
}

fn create_factory() -> Arc<FakeFactory> {
    FakeFactory::new(|channel, _index| Ok(create_session(channel).as_session()))
}

// ---------------------------------------------------------------------------
// recovery policy(對應舊測試的 createRecoveryPolicy;數值照舊 helper)
// ---------------------------------------------------------------------------

fn create_default_browser_recovery() -> BrowserRecoveryConfig {
    BrowserRecoveryConfig {
        page_crash_backoff_seconds: vec![30.0, 60.0, 120.0],
        channel_crash_window_seconds: 600.0,
        channel_quarantine_threshold: 4.0,
        channel_quarantine_seconds: 900.0,
        stable_reset_seconds: 1_800.0,
        multi_channel_crash_window_seconds: 15.0,
        multi_channel_crash_threshold: 2.0,
        browser_failure_window_seconds: 600.0,
        browser_failure_container_threshold: 3.0,
    }
}

fn create_default_browser_session_start() -> BrowserSessionStartConfig {
    BrowserSessionStartConfig {
        failure_backoff_seconds: vec![30.0, 60.0, 120.0],
        maximum_cooldown_seconds: 900.0,
    }
}

fn create_recovery_policy() -> (Arc<Mutex<ChannelRecoveryPolicy>>, Arc<AtomicU64>) {
    let now_ms = Arc::new(AtomicU64::new(0));
    let clock = Arc::clone(&now_ms);
    let policy = ChannelRecoveryPolicy::new(ChannelRecoveryPolicyOptions {
        recovery: create_default_browser_recovery(),
        session_start: create_default_browser_session_start(),
        now: Some(Box::new(move || clock.load(Ordering::SeqCst))),
    });
    (Arc::new(Mutex::new(policy)), now_ms)
}

fn state_of(policy: &Arc<Mutex<ChannelRecoveryPolicy>>, channel: &str) -> ChannelRecoveryState {
    policy.lock().unwrap().get_state(channel)
}

fn channels(list: &[&str]) -> Vec<String> {
    list.iter().map(|channel| channel.to_string()).collect()
}

/// 組好的 `DefaultSessionManager`(對映 `new DefaultSessionManager(factory, deps)`)。
fn make_new_manager(
    factory: Arc<dyn SessionChannelSessionFactory>,
    dependencies: SessionManagerDependencies,
) -> DefaultSessionManager {
    DefaultSessionManager::new(factory, dependencies)
}

/// 舊測試注入的 `sleep: async (ms) => { sleeps.push(ms); }`:只記錄、立即返回。
struct SleepRecorder {
    records: Arc<Mutex<Vec<u64>>>,
}

#[async_trait]
impl SleepFn for SleepRecorder {
    async fn sleep(&self, milliseconds: u64) {
        self.records.lock().unwrap().push(milliseconds);
    }
}

/// 舊測試 stagger 案例的 sleep:把 `sleep:<ms>` 推進共用的 events 序列。
struct SleepEventRecorder {
    events: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl SleepFn for SleepEventRecorder {
    async fn sleep(&self, milliseconds: u64) {
        self.events
            .lock()
            .unwrap()
            .push(format!("sleep:{milliseconds}"));
    }
}

/// 舊測試的 `onNavigationOutcomes: async (outcomes) => { … }`:記錄批次,
/// 並可在呼叫後跑一段同步 hook(如 events.push('observer'))。
struct RecordingOutcomeObserver {
    outcomes: Arc<Mutex<Vec<BrowserNavigationOutcome>>>,
    hook: Option<Arc<dyn Fn() + Send + Sync>>,
}

#[async_trait]
impl SessionNavigationOutcomeObserver for RecordingOutcomeObserver {
    async fn on_navigation_outcomes(
        &self,
        outcomes: &[BrowserNavigationOutcome],
    ) -> Result<(), DriverError> {
        self.outcomes
            .lock()
            .unwrap()
            .extend(outcomes.iter().cloned());
        if let Some(hook) = &self.hook {
            hook();
        }
        Ok(())
    }
}

/// 舊測試「navigation outcome observer 可在 reconcile 鎖外失效 session」:
/// observer 拿到 manager 反手 invalidate(登記在 OnceLock,事後接線)。
struct InvalidatingObserver {
    manager: Arc<OnceLock<Arc<DefaultSessionManager>>>,
    channel: &'static str,
    reason: &'static str,
}

#[async_trait]
impl SessionNavigationOutcomeObserver for InvalidatingObserver {
    async fn on_navigation_outcomes(
        &self,
        _outcomes: &[BrowserNavigationOutcome],
    ) -> Result<(), DriverError> {
        let manager = self
            .manager
            .get()
            .expect("manager must be registered before reconcile");
        manager.invalidate(self.channel, self.reason).await;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 移植的舊測試(標題逐字見 doc/spec/ts-test-titles.tsv)
// ---------------------------------------------------------------------------

// 原測試：依輸入順序新增 session 並回報 active channels
#[tokio::test(start_paused = true)]
async fn add_sessions_in_input_order_and_report_active_channels() {
    let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let events_for_builder = Arc::clone(&events);
    let factory = FakeFactory::new(move |channel, _index| {
        let channel_owned = channel.to_string();
        let events = Arc::clone(&events_for_builder);
        Ok(create_session_with(
            channel,
            FakeSessionOptions {
                start: Some(new_start_source(move || {
                    let events = Arc::clone(&events);
                    let channel = channel_owned.clone();
                    Box::pin(async move {
                        events.lock().unwrap().push(format!("start:{channel}"));
                        Ok(())
                    })
                })),
                ..FakeSessionOptions::default()
            },
        )
        .as_session())
    });
    let manager = make_new_manager(
        factory.clone() as Arc<dyn SessionChannelSessionFactory>,
        SessionManagerDependencies::default(),
    );

    manager.reconcile(&channels(&["first", "second"])).await;

    assert_eq!(*events.lock().unwrap(), ["start:first", "start:second"]);
    assert_eq!(manager.get_active_channels(), ["first", "second"]);
}

// 原測試：先依 registry 順序移除，再依 activeChannels 順序新增
#[tokio::test(start_paused = true)]
async fn remove_then_add_by_registry_and_input_orders() {
    let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let events_for_builder = Arc::clone(&events);
    let factory = FakeFactory::new(move |channel, _index| {
        let channel_owned = channel.to_string();
        let channel_for_start = channel_owned.clone();
        let channel_for_stop = channel_owned.clone();
        let events_start = Arc::clone(&events_for_builder);
        let events_stop = Arc::clone(&events_for_builder);
        Ok(create_session_with(
            channel,
            FakeSessionOptions {
                start: Some(new_start_source(move || {
                    let events = Arc::clone(&events_start);
                    let channel = channel_for_start.clone();
                    Box::pin(async move {
                        events.lock().unwrap().push(format!("start:{channel}"));
                        Ok(())
                    })
                })),
                stop: Some(new_stop_source(move |_reason| {
                    let events = Arc::clone(&events_stop);
                    let channel = channel_for_stop.clone();
                    let reason = _reason.to_string();
                    Box::pin(async move {
                        events
                            .lock()
                            .unwrap()
                            .push(format!("stop:{channel}:{reason}"));
                        Ok(())
                    })
                })),
                ..FakeSessionOptions::default()
            },
        )
        .as_session())
    });
    let manager = make_new_manager(
        factory as Arc<dyn SessionChannelSessionFactory>,
        SessionManagerDependencies::default(),
    );
    manager.reconcile(&channels(&["first", "second"])).await;
    events.lock().unwrap().clear();

    manager.reconcile(&channels(&["second", "third"])).await;

    assert_eq!(
        *events.lock().unwrap(),
        ["stop:first:inactive", "start:third"]
    );
    assert_eq!(manager.get_active_channels(), ["second", "third"]);
}

// 原測試：保持既有 session、不重建，並依最新輸入順序回報
#[tokio::test(start_paused = true)]
async fn keep_existing_sessions_and_report_by_latest_input_order() {
    let factory = create_factory();
    let manager = make_new_manager(
        factory.clone() as Arc<dyn SessionChannelSessionFactory>,
        SessionManagerDependencies::default(),
    );
    manager.reconcile(&channels(&["first", "second"])).await;

    manager
        .reconcile(&channels(&["second", "first", "first"]))
        .await;

    assert_eq!(factory.created().len(), 2);
    assert_eq!(manager.get_active_channels(), ["second", "first"]);
}

// 原測試：可擷取預設或指定中的 active session 截圖
#[tokio::test(start_paused = true)]
async fn capture_screenshot_default_or_by_requested_channel() {
    let factory = create_factory();
    let manager = make_new_manager(
        factory as Arc<dyn SessionChannelSessionFactory>,
        SessionManagerDependencies::default(),
    );
    manager.reconcile(&channels(&["first", "second"])).await;

    assert_eq!(
        manager.capture_screenshot(None).await,
        Some(SessionScreenshot {
            channel: "first".to_string(),
            image: "screenshot:first".as_bytes().to_vec(),
        })
    );
    assert_eq!(
        manager.capture_screenshot(Some("SECOND")).await,
        Some(SessionScreenshot {
            channel: "second".to_string(),
            image: "screenshot:second".as_bytes().to_vec(),
        })
    );
    assert_eq!(manager.capture_screenshot(Some("missing")).await, None);
}

// 原測試：可取得全部或指定 active session 的忠誠點數
#[tokio::test(start_paused = true)]
async fn get_channel_points_default_or_by_requested_channel() {
    let factory = FakeFactory::new(|channel, _index| {
        let points = if channel == "first" {
            ChannelSessionPointsResult::Available { balance: 3_500 }
        } else {
            ChannelSessionPointsResult::NotFound
        };
        Ok(create_session_with(
            channel,
            FakeSessionOptions {
                points: Some(points),
                ..FakeSessionOptions::default()
            },
        )
        .as_session())
    });
    let manager = make_new_manager(
        factory as Arc<dyn SessionChannelSessionFactory>,
        SessionManagerDependencies::default(),
    );
    manager.reconcile(&channels(&["first", "second"])).await;

    assert_eq!(
        manager.get_channel_points(None).await,
        vec![
            SessionChannelPointsResult::Available {
                channel: "first".to_string(),
                balance: 3_500.0,
            },
            SessionChannelPointsResult::Unavailable {
                channel: "second".to_string(),
                reason: SessionChannelPointsUnavailableReason::NotFound,
            },
        ]
    );
    assert_eq!(
        manager.get_channel_points(Some("FIRST")).await,
        vec![SessionChannelPointsResult::Available {
            channel: "first".to_string(),
            balance: 3_500.0,
        }]
    );
    assert_eq!(manager.get_channel_points(Some("missing")).await, []);
}

// 原測試：依 active session 順序回報頁面重整狀態
#[tokio::test(start_paused = true)]
async fn report_refresh_statuses_in_active_session_order() {
    let factory = FakeFactory::new(|channel, _index| {
        let refresh_status = (
            true,
            Some(format!(
                "2026-06-14T12:0{}:00.000Z",
                if channel == "first" { "1" } else { "2" }
            )),
            Some(if channel == "first" { 60 } else { 120 }),
        );
        Ok(create_session_with(
            channel,
            FakeSessionOptions {
                refresh_status: Some(refresh_status),
                ..FakeSessionOptions::default()
            },
        )
        .as_session())
    });
    let manager = make_new_manager(
        factory as Arc<dyn SessionChannelSessionFactory>,
        SessionManagerDependencies::default(),
    );
    manager.reconcile(&channels(&["first", "second"])).await;

    assert_eq!(
        manager.get_refresh_statuses(),
        vec![
            ChannelSessionRefreshStatus {
                channel: "first".to_string(),
                enabled: true,
                next_refresh_at: Some("2026-06-14T12:01:00.000Z".to_string()),
                seconds_until_refresh: Some(60),
            },
            ChannelSessionRefreshStatus {
                channel: "second".to_string(),
                enabled: true,
                next_refresh_at: Some("2026-06-14T12:02:00.000Z".to_string()),
                seconds_until_refresh: Some(120),
            },
        ]
    );
}

// 原測試：可手動重整全部或指定 active session
#[tokio::test(start_paused = true)]
async fn refresh_pages_default_or_by_requested_channel() {
    let sessions: Arc<Mutex<Vec<Arc<FakeSession>>>> = Arc::new(Mutex::new(Vec::new()));
    let registry = Arc::clone(&sessions);
    let factory = FakeFactory::new(move |channel, _index| {
        let session = create_session(channel);
        registry.lock().unwrap().push(Arc::clone(&session));
        Ok(session.as_session())
    });
    let manager = make_new_manager(
        factory as Arc<dyn SessionChannelSessionFactory>,
        SessionManagerDependencies::default(),
    );
    manager.reconcile(&channels(&["first", "second"])).await;

    assert_eq!(
        manager.refresh_pages(None).await,
        vec![
            SessionRefreshResult {
                channel: "first".to_string(),
                status: SessionRefreshStatus::Refreshed,
                error: None,
            },
            SessionRefreshResult {
                channel: "second".to_string(),
                status: SessionRefreshStatus::Refreshed,
                error: None,
            },
        ]
    );
    assert_eq!(
        manager.refresh_pages(Some("SECOND")).await,
        vec![SessionRefreshResult {
            channel: "second".to_string(),
            status: SessionRefreshStatus::Refreshed,
            error: None,
        }]
    );
    assert_eq!(manager.refresh_pages(Some("missing")).await, []);

    let registry = sessions.lock().unwrap().clone();
    let first = registry
        .iter()
        .find(|session| session.channel() == "first")
        .unwrap();
    let second = registry
        .iter()
        .find(|session| session.channel() == "second")
        .unwrap();
    assert_eq!(first.refresh_now_call_count(), 1);
    assert_eq!(second.refresh_now_call_count(), 2);
}

// 原測試：start failure 不留 registry，且不影響其他頻道啟動
#[tokio::test(start_paused = true)]
async fn start_failure_leaves_no_registry_entry_and_continues_other_channels() {
    let failed = create_session_with(
        "failed",
        FakeSessionOptions {
            start: Some(start_source_always(Err(DriverError::new(
                "Error",
                "start failed",
            )))),
            ..FakeSessionOptions::default()
        },
    );
    let healthy = create_session("healthy");
    let failed_handle = Arc::clone(&failed);
    let healthy_handle = Arc::clone(&healthy);
    let factory = FakeFactory::new(move |channel, _index| {
        if channel == "failed" {
            Ok(Arc::clone(&failed_handle).as_session())
        } else {
            Ok(Arc::clone(&healthy_handle).as_session())
        }
    });
    let logger = create_logger();
    let manager = make_new_manager(
        factory as Arc<dyn SessionChannelSessionFactory>,
        SessionManagerDependencies {
            logger: Some(logger.clone() as Arc<dyn BrowserManagerLogger>),
            ..SessionManagerDependencies::default()
        },
    );

    manager.reconcile(&channels(&["failed", "healthy"])).await;

    assert_eq!(manager.get_active_channels(), ["healthy"]);
    assert_eq!(failed.stop_calls(), ["start_failed"]);
    assert_eq!(healthy.start_call_count(), 1);
    assert!(logger.called_with(
        LogLevel::Error,
        "session_start_failed",
        &[("channel", v_str("failed"))]
    ));
}

// 原測試：page crash 只對該頻道 backoff，其他 active session 保持運作
#[tokio::test(start_paused = true)]
async fn page_crash_backs_off_only_that_channel() {
    let (policy, now_ms) = create_recovery_policy();
    let policy_for_specs = Arc::clone(&policy);
    let logger = create_logger();
    let factory = create_factory();
    let manager = make_new_manager(
        factory as Arc<dyn SessionChannelSessionFactory>,
        SessionManagerDependencies {
            logger: Some(logger.clone() as Arc<dyn BrowserManagerLogger>),
            recovery_policy: Some(policy),
            configured_channels: channels(&["a", "b", "c"]),
            ..SessionManagerDependencies::default()
        },
    );
    manager.reconcile(&channels(&["a", "b", "c"])).await;

    manager.invalidate("a", "page_crashed").await;
    manager.reconcile(&channels(&["a", "b", "c"])).await;

    assert_eq!(manager.get_active_channels(), ["b", "c"]);
    assert!(logger.called_with(
        LogLevel::Debug,
        "channel_recovery_deferred",
        &[("channel", v_str("a")), ("retryAtMs", v_num(30_000.0))]
    ));

    now_ms.store(30_000, Ordering::SeqCst);
    manager.reconcile(&channels(&["a", "b", "c"])).await;
    assert_eq!(manager.get_active_channels(), ["a", "b", "c"]);
    assert!(logger.called_with(
        LogLevel::Debug,
        "channel_recovery_probe_started",
        &[("channel", v_str("a")), ("recoveryGeneration", v_num(1.0)),]
    ));

    now_ms.fetch_add(1_800_000, Ordering::SeqCst);
    manager.reconcile(&channels(&["a", "b", "c"])).await;
    assert_eq!(state_of(&policy_for_specs, "a").consecutive_page_crashes, 0);
    assert!(logger.called_exactly_with(
        LogLevel::Debug,
        "channel_recovery_reset",
        &[("channel", v_str("a")), ("recoveryGeneration", v_num(1.0))]
    ));
}

// 原測試：健康失敗逐步冷卻，重複失效不重算，其他頻道不受影響且穩定後重設
#[tokio::test(start_paused = true)]
async fn health_failures_step_cooldowns_and_reset_once_stable() {
    let (policy, now_ms) = create_recovery_policy();
    let policy_for_specs = Arc::clone(&policy);
    let factory = create_factory();
    let logger = create_logger();
    let manager = make_new_manager(
        factory.clone() as Arc<dyn SessionChannelSessionFactory>,
        SessionManagerDependencies {
            logger: Some(logger.clone() as Arc<dyn BrowserManagerLogger>),
            recovery_policy: Some(policy),
            configured_channels: channels(&["a", "b"]),
            ..SessionManagerDependencies::default()
        },
    );
    manager.reconcile(&channels(&["a", "b"])).await;

    for cooldown_ms in [30_000_u64, 60_000, 120_000, 900_000] {
        manager.invalidate("a", "health_failure_threshold").await;
        let current = now_ms.load(Ordering::SeqCst);
        let retry_at_ms = current + cooldown_ms;
        assert_eq!(
            state_of(&policy_for_specs, "a").next_retry_at_ms,
            Some(retry_at_ms)
        );
        assert_eq!(state_of(&policy_for_specs, "a").consecutive_page_crashes, 0);
        manager.invalidate("a", "health_failure_threshold").await;
        assert_eq!(
            state_of(&policy_for_specs, "a").next_retry_at_ms,
            Some(retry_at_ms)
        );
        let creations = factory.created().len();
        now_ms.store(retry_at_ms - 1, Ordering::SeqCst);
        manager.reconcile(&channels(&["a", "b"])).await;
        assert_eq!(manager.get_active_channels(), ["b"]);
        assert_eq!(factory.created().len(), creations);
        now_ms.store(retry_at_ms, Ordering::SeqCst);
        manager.reconcile(&channels(&["a", "b"])).await;
        assert_eq!(manager.get_active_channels(), ["a", "b"]);
    }
    assert!(logger.called_with(
        LogLevel::Warn,
        "channel_health_recovery_backoff",
        &[("channel", v_str("a")), ("consecutiveFailures", v_num(4.0)),]
    ));
    now_ms.fetch_add(1_800_000, Ordering::SeqCst);
    manager.reconcile(&channels(&["a", "b"])).await;
    assert_eq!(
        state_of(&policy_for_specs, "a").consecutive_start_failures,
        0
    );
    let new_current = now_ms.load(Ordering::SeqCst);
    manager.invalidate("a", "health_failure_threshold").await;
    assert_eq!(
        state_of(&policy_for_specs, "a").next_retry_at_ms,
        Some(new_current + 30_000)
    );
    manager.stop_all("test_complete").await;
}

// 原測試：quarantine 到期前不啟動，到期後只啟動一個 probe
#[tokio::test(start_paused = true)]
async fn quarantine_defers_start_then_allows_single_probe() {
    let (policy, now_ms) = create_recovery_policy();
    for crash_at_ms in [0_u64, 30_000, 90_000, 210_000] {
        now_ms.store(crash_at_ms, Ordering::SeqCst);
        policy.lock().unwrap().record_page_crash("channel");
    }
    let factory = create_factory();
    let logger = create_logger();
    let manager = make_new_manager(
        factory.clone() as Arc<dyn SessionChannelSessionFactory>,
        SessionManagerDependencies {
            logger: Some(logger.clone() as Arc<dyn BrowserManagerLogger>),
            recovery_policy: Some(Arc::clone(&policy)),
            configured_channels: channels(&["channel"]),
            ..SessionManagerDependencies::default()
        },
    );

    manager.reconcile(&channels(&["channel"])).await;
    assert_eq!(factory.created().len(), 0);
    assert!(logger.called_with(
        LogLevel::Warn,
        "channel_recovery_quarantined",
        &[("channel", v_str("channel"))]
    ));

    now_ms.store(1_110_000, Ordering::SeqCst);
    let manager_a = manager.clone();
    let manager_b = manager.clone();
    let (first_reconcile, second_reconcile) = tokio::join!(
        async move { manager_a.reconcile(&channels(&["channel"])).await },
        async move { manager_b.reconcile(&channels(&["channel"])).await }
    );
    let _: () = first_reconcile;
    let _: () = second_reconcile;
    assert_eq!(factory.created().len(), 1);
    assert_eq!(manager.get_active_channels(), ["channel"]);
}

// 原測試：recoverable start failure 進入 cooldown，fatal prerequisite 不計入
#[tokio::test(start_paused = true)]
async fn recoverable_start_failure_enters_cooldown_fatal_prerequisite_not_counted() {
    let (recoverable_policy, _recoverable_clock) = create_recovery_policy();
    let recoverable_factory = FakeFactory::new(|_channel, _index| {
        Ok(create_session_with(
            "recoverable",
            FakeSessionOptions {
                start: Some(start_source_always(Err(DriverError::new(
                    "Error",
                    "Target page, context or browser has been closed",
                )))),
                ..FakeSessionOptions::default()
            },
        )
        .as_session())
    });
    let recoverable = make_new_manager(
        recoverable_factory.clone() as Arc<dyn SessionChannelSessionFactory>,
        SessionManagerDependencies {
            recovery_policy: Some(Arc::clone(&recoverable_policy)),
            configured_channels: channels(&["recoverable"]),
            ..SessionManagerDependencies::default()
        },
    );
    recoverable.reconcile(&channels(&["recoverable"])).await;
    recoverable.reconcile(&channels(&["recoverable"])).await;
    assert_eq!(recoverable_factory.created().len(), 1);
    assert_eq!(
        state_of(&recoverable_policy, "recoverable").consecutive_start_failures,
        1
    );

    let (fatal_policy, _fatal_clock) = create_recovery_policy();
    let fatal_factory = FakeFactory::new(|_channel, _index| {
        Ok(create_session_with(
            "fatal",
            FakeSessionOptions {
                start: Some(start_source_always(Err(DriverError::new(
                    "Error",
                    "authentication failed",
                )))),
                ..FakeSessionOptions::default()
            },
        )
        .as_session())
    });
    let fatal = make_new_manager(
        fatal_factory.clone() as Arc<dyn SessionChannelSessionFactory>,
        SessionManagerDependencies {
            recovery_policy: Some(Arc::clone(&fatal_policy)),
            configured_channels: channels(&["fatal"]),
            ..SessionManagerDependencies::default()
        },
    );
    fatal.reconcile(&channels(&["fatal"])).await;
    fatal.reconcile(&channels(&["fatal"])).await;
    assert_eq!(fatal_factory.created().len(), 2);
    assert_eq!(
        state_of(&fatal_policy, "fatal").consecutive_start_failures,
        0
    );
}

// 原測試：只有從 configured channels 移除時才清除 recovery state
#[tokio::test(start_paused = true)]
async fn remove_recovery_state_only_when_removed_from_configured_channels() {
    let (policy, _now_ms) = create_recovery_policy();
    policy.lock().unwrap().record_page_crash("removed");
    let policy_for_specs = Arc::clone(&policy);
    let factory = create_factory();
    let manager = make_new_manager(
        factory as Arc<dyn SessionChannelSessionFactory>,
        SessionManagerDependencies {
            recovery_policy: Some(Arc::clone(&policy)),
            configured_channels: channels(&["removed", "kept"]),
            ..SessionManagerDependencies::default()
        },
    );

    manager.reconcile(&channels(&[])).await;
    assert_eq!(
        state_of(&policy_for_specs, "removed").consecutive_page_crashes,
        1
    );
    manager.update_configured_channels(&channels(&["kept"]));
    assert_eq!(
        state_of(&policy_for_specs, "removed").consecutive_page_crashes,
        0
    );
}

// 原測試：session start 卡住時會 timeout 並繼續啟動後續頻道
#[tokio::test(start_paused = true)]
async fn stuck_start_times_out_and_subsequent_channels_still_start() {
    let (start_gate, start_signal) = create_gate();
    let lifecycle_events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let events_for_start = Arc::clone(&lifecycle_events);
    let events_for_cancel = Arc::clone(&lifecycle_events);
    let events_for_stop = Arc::clone(&lifecycle_events);
    let stuck = create_session_with(
        "stuck",
        FakeSessionOptions {
            start: Some(new_start_source(move || {
                let events = Arc::clone(&events_for_start);
                let signal = start_signal.clone();
                Box::pin(async move {
                    wait_gate(signal).await;
                    events
                        .lock()
                        .unwrap()
                        .push("late_start_completed".to_string());
                    Ok(())
                })
            })),
            cancel_start: Some(new_cancel_source(move |reason| {
                let events = Arc::clone(&events_for_cancel);
                let gate = Arc::clone(&start_gate);
                let reason = reason.to_string();
                Box::pin(async move {
                    events.lock().unwrap().push(format!("cancel:{reason}"));
                    gate.release();
                })
            })),
            stop: Some(new_stop_source(move |reason| {
                let events = Arc::clone(&events_for_stop);
                let reason = reason.to_string();
                Box::pin(async move {
                    events.lock().unwrap().push(format!("stop:{reason}"));
                    Ok(())
                })
            })),
            ..FakeSessionOptions::default()
        },
    );
    let healthy = create_session("healthy");
    let stuck_handle = Arc::clone(&stuck);
    let healthy_handle = Arc::clone(&healthy);
    let factory = FakeFactory::new(move |channel, _index| {
        if channel == "stuck" {
            Ok(Arc::clone(&stuck_handle).as_session())
        } else {
            Ok(Arc::clone(&healthy_handle).as_session())
        }
    });
    let logger = create_logger();
    let manager = make_new_manager(
        factory.clone() as Arc<dyn SessionChannelSessionFactory>,
        SessionManagerDependencies {
            logger: Some(logger.clone() as Arc<dyn BrowserManagerLogger>),
            session_operation_timeout_ms: Some(1_000),
            ..SessionManagerDependencies::default()
        },
    );

    let manager_task = manager.clone();
    let task_channels = channels(&["stuck", "healthy"]);
    let reconcile = tokio::spawn(async move { manager_task.reconcile(&task_channels).await });
    assert_eventually("stuck.start 應已被呼叫", || {
        stuck.start_call_count() == 1
    })
    .await;
    assert_eq!(healthy.start_call_count(), 0);

    reconcile.await.unwrap();

    assert_eq!(manager.get_active_channels(), ["healthy"]);
    assert_eq!(healthy.start_call_count(), 1);
    assert_eq!(stuck.cancel_start_calls(), ["start_timeout"]);
    assert_eq!(stuck.stop_calls(), ["start_failed"]);
    assert_eq!(
        *lifecycle_events.lock().unwrap(),
        [
            "cancel:start_timeout",
            "late_start_completed",
            "stop:start_failed"
        ]
    );
    assert!(logger.called_exactly_with(
        LogLevel::Warn,
        "session_start_timeout",
        &[
            ("channel", v_str("stuck")),
            ("attempt", v_num(1.0)),
            ("timeoutMs", v_num(1_000.0)),
        ]
    ));
    assert!(logger.called_with(
        LogLevel::Error,
        "session_start_failed",
        &[("channel", v_str("stuck"))]
    ));
}

// 原測試：啟動失敗為瀏覽器關閉時會清理舊 session 並短重試
#[tokio::test(start_paused = true)]
async fn browser_closed_start_failure_cleans_up_and_retries() {
    let failed = create_session_with(
        "channel",
        FakeSessionOptions {
            start: Some(start_source_always(Err(DriverError::new(
                "Error",
                "page.goto: Target page, context or browser has been closed",
            )))),
            ..FakeSessionOptions::default()
        },
    );
    let healthy = create_session("channel");
    let failed_handle = Arc::clone(&failed);
    let healthy_handle = Arc::clone(&healthy);
    let factory = FakeFactory::new(move |_channel, index| {
        if index == 0 {
            Ok(Arc::clone(&failed_handle).as_session())
        } else {
            Ok(Arc::clone(&healthy_handle).as_session())
        }
    });
    let logger = create_logger();
    let sleeps: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
    let sleeps_for_manager = Arc::clone(&sleeps);
    let manager = make_new_manager(
        factory.clone() as Arc<dyn SessionChannelSessionFactory>,
        SessionManagerDependencies {
            logger: Some(logger.clone() as Arc<dyn BrowserManagerLogger>),
            start_retry_attempts: Some(1),
            start_retry_delay_ms: Some(2_000),
            sleep: Some(Arc::new(SleepRecorder {
                records: sleeps_for_manager,
            }) as Arc<dyn SleepFn>),
            ..SessionManagerDependencies::default()
        },
    );

    manager.reconcile(&channels(&["channel"])).await;

    assert_eq!(manager.get_active_channels(), ["channel"]);
    assert_eq!(factory.created().len(), 2);
    assert_eq!(failed.stop_calls(), ["start_failed"]);
    assert_eq!(healthy.start_call_count(), 1);
    assert_eq!(*sleeps.lock().unwrap(), [2_000]);
    assert!(logger.called_with(
        LogLevel::Warn,
        "session_start_retry_scheduled",
        &[
            ("channel", v_str("channel")),
            ("attempt", v_num(1.0)),
            ("retryInMs", v_num(2_000.0)),
        ]
    ));
    assert_eq!(logger.level_call_count(LogLevel::Error), 0);
}

// 原測試：啟動失敗為 Twitch 導覽逾時時會清理舊 session 並短重試
#[tokio::test(start_paused = true)]
async fn navigation_timeout_start_failure_cleans_up_and_retries() {
    let failed = create_session_with(
        "channel",
        FakeSessionOptions {
            start: Some(start_source_always(Err(DriverError::new(
                "TimeoutError",
                "page.goto: Timeout 30000ms exceeded.\nCall log:\n  - navigating to \"https://www.twitch.tv/channel\", waiting until \"load\"",
            )))),
            ..FakeSessionOptions::default()
        },
    );
    let healthy = create_session("channel");
    let failed_handle = Arc::clone(&failed);
    let healthy_handle = Arc::clone(&healthy);
    let factory = FakeFactory::new(move |_channel, index| {
        if index == 0 {
            Ok(Arc::clone(&failed_handle).as_session())
        } else {
            Ok(Arc::clone(&healthy_handle).as_session())
        }
    });
    let logger = create_logger();
    let sleeps: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
    let sleeps_for_manager = Arc::clone(&sleeps);
    let navigation_outcomes: Arc<Mutex<Vec<BrowserNavigationOutcome>>> =
        Arc::new(Mutex::new(Vec::new()));
    let manager = make_new_manager(
        factory.clone() as Arc<dyn SessionChannelSessionFactory>,
        SessionManagerDependencies {
            logger: Some(logger.clone() as Arc<dyn BrowserManagerLogger>),
            start_retry_attempts: Some(1),
            start_retry_delay_ms: Some(2_000),
            sleep: Some(Arc::new(SleepRecorder {
                records: sleeps_for_manager,
            }) as Arc<dyn SleepFn>),
            on_navigation_outcomes: Some(Arc::new(RecordingOutcomeObserver {
                outcomes: Arc::clone(&navigation_outcomes),
                hook: None,
            })
                as Arc<dyn SessionNavigationOutcomeObserver>),
            ..SessionManagerDependencies::default()
        },
    );

    manager.reconcile(&channels(&["channel"])).await;

    assert_eq!(manager.get_active_channels(), ["channel"]);
    assert_eq!(factory.created().len(), 2);
    assert_eq!(failed.stop_calls(), ["start_failed"]);
    assert_eq!(healthy.start_call_count(), 1);
    assert_eq!(*sleeps.lock().unwrap(), [2_000]);
    assert_eq!(
        *navigation_outcomes.lock().unwrap(),
        [BrowserNavigationOutcome {
            channel: "channel".to_string(),
            status: BrowserNavigationStatus::Succeeded,
        }]
    );
    assert!(logger.called_with(
        LogLevel::Warn,
        "session_start_retry_scheduled",
        &[
            ("channel", v_str("channel")),
            ("attempt", v_num(1.0)),
            ("retryInMs", v_num(2_000.0)),
        ]
    ));
    assert_eq!(logger.level_call_count(LogLevel::Error), 0);
}

// 原測試：導覽短重試仍逾時時只在最終失敗後回報一次
#[tokio::test(start_paused = true)]
async fn timeout_after_short_retries_reports_outcome_once_after_final_failure() {
    const TIMEOUT_ERROR: &str = "page.goto: Timeout 30000ms exceeded.\nCall log:\n  - navigating to \"https://www.twitch.tv/channel\", waiting until \"domcontentloaded\"";
    let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let events_for_builder = Arc::clone(&events);
    let factory = FakeFactory::new(move |_channel, creation_index| {
        let events_for_stop = Arc::clone(&events_for_builder);
        let index = creation_index;
        Ok(create_session_with(
            "channel",
            FakeSessionOptions {
                start: Some(start_source_always(Err(DriverError::new(
                    "TimeoutError",
                    TIMEOUT_ERROR,
                )))),
                stop: Some(new_stop_source(move |_reason| {
                    let events = Arc::clone(&events_for_stop);
                    let marker = format!("stop:{index}");
                    Box::pin(async move {
                        events.lock().unwrap().push(marker);
                        Ok(())
                    })
                })),
                ..FakeSessionOptions::default()
            },
        )
        .as_session())
    });
    let navigation_outcomes: Arc<Mutex<Vec<BrowserNavigationOutcome>>> =
        Arc::new(Mutex::new(Vec::new()));
    let observer_events = Arc::clone(&events);
    let manager = make_new_manager(
        factory.clone() as Arc<dyn SessionChannelSessionFactory>,
        SessionManagerDependencies {
            start_retry_attempts: Some(1),
            start_retry_delay_ms: Some(0),
            on_navigation_outcomes: Some(Arc::new(RecordingOutcomeObserver {
                outcomes: Arc::clone(&navigation_outcomes),
                hook: Some(Arc::new(move || {
                    observer_events.lock().unwrap().push("observer".to_string());
                })),
            })
                as Arc<dyn SessionNavigationOutcomeObserver>),
            ..SessionManagerDependencies::default()
        },
    );

    manager.reconcile(&channels(&["channel"])).await;

    assert_eq!(manager.get_active_channels(), Vec::<String>::new());
    assert_eq!(factory.created().len(), 2);
    assert_eq!(*events.lock().unwrap(), ["stop:0", "stop:1", "observer"]);
    assert_eq!(
        *navigation_outcomes.lock().unwrap(),
        [BrowserNavigationOutcome {
            channel: "channel".to_string(),
            status: BrowserNavigationStatus::TimedOut,
        }]
    );
}

// 原測試：非導覽逾時的啟動失敗不回報 navigation outcome
#[tokio::test(start_paused = true)]
async fn non_navigation_timeout_failure_reports_no_outcome() {
    /// 舊測試的 `const onNavigationOutcomes = vi.fn()`:只數呼叫次數。
    struct CountingObserver {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl SessionNavigationOutcomeObserver for CountingObserver {
        async fn on_navigation_outcomes(
            &self,
            _outcomes: &[BrowserNavigationOutcome],
        ) -> Result<(), DriverError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    let calls = Arc::new(AtomicUsize::new(0));
    let factory = FakeFactory::new(|_channel, _index| {
        Ok(create_session_with(
            "channel",
            FakeSessionOptions {
                start: Some(start_source_always(Err(DriverError::new(
                    "Error",
                    "authentication failed",
                )))),
                ..FakeSessionOptions::default()
            },
        )
        .as_session())
    });
    let manager = make_new_manager(
        factory as Arc<dyn SessionChannelSessionFactory>,
        SessionManagerDependencies {
            on_navigation_outcomes: Some(Arc::new(CountingObserver {
                calls: Arc::clone(&calls),
            })
                as Arc<dyn SessionNavigationOutcomeObserver>),
            ..SessionManagerDependencies::default()
        },
    );

    manager.reconcile(&channels(&["channel"])).await;

    assert_eq!(manager.get_active_channels(), Vec::<String>::new());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

// 原測試：navigation outcome observer 可在 reconcile 鎖外失效 session
#[tokio::test(start_paused = true)]
async fn outcome_observer_can_invalidate_session_outside_reconcile_lock() {
    let session = create_session("channel");
    let session_handle = Arc::clone(&session);
    let manager_slot: Arc<OnceLock<Arc<DefaultSessionManager>>> = Arc::new(OnceLock::new());
    let factory =
        FakeFactory::new(move |_channel, _index| Ok(Arc::clone(&session_handle).as_session()));
    let manager = Arc::new(make_new_manager(
        factory as Arc<dyn SessionChannelSessionFactory>,
        SessionManagerDependencies {
            on_navigation_outcomes: Some(Arc::new(InvalidatingObserver {
                manager: Arc::clone(&manager_slot),
                channel: "channel",
                reason: "browser_restarted",
            })
                as Arc<dyn SessionNavigationOutcomeObserver>),
            ..SessionManagerDependencies::default()
        },
    ));
    manager_slot
        .set(Arc::clone(&manager))
        .ok()
        .expect("manager registers once");

    manager.reconcile(&channels(&["channel"])).await;

    assert_eq!(manager.get_active_channels(), Vec::<String>::new());
    assert_eq!(session.stop_calls(), ["browser_restarted"]);
}

// 原測試：新增多個 session 時會在後續啟動前套用間隔
#[tokio::test(start_paused = true)]
async fn stagger_is_applied_before_subsequent_starts() {
    let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let events_for_builder = Arc::clone(&events);
    let factory = FakeFactory::new(move |channel, _index| {
        let channel_owned = channel.to_string();
        let events_for_start = Arc::clone(&events_for_builder);
        Ok(create_session_with(
            channel,
            FakeSessionOptions {
                start: Some(new_start_source(move || {
                    let events = Arc::clone(&events_for_start);
                    let channel = channel_owned.clone();
                    Box::pin(async move {
                        events.lock().unwrap().push(format!("start:{channel}"));
                        Ok(())
                    })
                })),
                ..FakeSessionOptions::default()
            },
        )
        .as_session())
    });
    let events_for_sleep = Arc::clone(&events);
    let manager = make_new_manager(
        factory as Arc<dyn SessionChannelSessionFactory>,
        SessionManagerDependencies {
            start_stagger_ms: Some(1_000),
            sleep: Some(Arc::new(SleepEventRecorder {
                events: events_for_sleep,
            }) as Arc<dyn SleepFn>),
            ..SessionManagerDependencies::default()
        },
    );

    manager
        .reconcile(&channels(&["first", "second", "third"]))
        .await;

    assert_eq!(
        *events.lock().unwrap(),
        [
            "start:first",
            "sleep:1000",
            "start:second",
            "sleep:1000",
            "start:third",
        ]
    );
    assert_eq!(manager.get_active_channels(), ["first", "second", "third"]);
}

// 原測試：factory failure 不影響後續頻道，下一輪可重試
#[tokio::test(start_paused = true)]
async fn factory_failure_does_not_block_later_channels() {
    let failed_once = Arc::new(AtomicUsize::new(0));
    let failed_once_builder = Arc::clone(&failed_once);
    let factory = FakeFactory::new(move |channel, _index| {
        if channel == "first" && failed_once_builder.fetch_or(1, Ordering::SeqCst) == 0 {
            Err(DriverError::new("Error", "factory failed"))
        } else {
            Ok(create_session(channel).as_session())
        }
    });
    let manager = make_new_manager(
        factory.clone() as Arc<dyn SessionChannelSessionFactory>,
        SessionManagerDependencies::default(),
    );

    manager.reconcile(&channels(&["first", "second"])).await;
    assert_eq!(manager.get_active_channels(), ["second"]);

    manager.reconcile(&channels(&["first", "second"])).await;
    assert_eq!(manager.get_active_channels(), ["first", "second"]);
    assert_eq!(factory.created().len(), 3);
}

// 原測試：stop failure 仍移除舊 session 並繼續新增
#[tokio::test(start_paused = true)]
async fn stop_failure_still_removes_old_session_and_continues_starting() {
    let failed_stop = create_session_with(
        "first",
        FakeSessionOptions {
            stop: Some(new_stop_source(|_reason| {
                Box::pin(async { Err(DriverError::new("Error", "stop failed")) })
            })),
            ..FakeSessionOptions::default()
        },
    );
    let healthy = create_session("second");
    let failed_handle = Arc::clone(&failed_stop);
    let healthy_handle = Arc::clone(&healthy);
    let factory = FakeFactory::new(move |channel, _index| {
        if channel == "first" {
            Ok(Arc::clone(&failed_handle).as_session())
        } else {
            Ok(Arc::clone(&healthy_handle).as_session())
        }
    });
    let logger = create_logger();
    let manager = make_new_manager(
        factory as Arc<dyn SessionChannelSessionFactory>,
        SessionManagerDependencies {
            logger: Some(logger.clone() as Arc<dyn BrowserManagerLogger>),
            ..SessionManagerDependencies::default()
        },
    );
    manager.reconcile(&channels(&["first"])).await;

    manager.reconcile(&channels(&["second"])).await;

    assert_eq!(manager.get_active_channels(), ["second"]);
    assert_eq!(healthy.start_call_count(), 1);
    assert!(logger.called_with(
        LogLevel::Warn,
        "session_stop_failed",
        &[("channel", v_str("first")), ("reason", v_str("inactive")),]
    ));
}

// 原測試：截圖失敗時回傳 undefined 並記錄可診斷錯誤
#[tokio::test(start_paused = true)]
async fn screenshot_failure_returns_none_and_logs_error() {
    let logger = create_logger();
    let stale = create_session_with(
        "stale",
        FakeSessionOptions {
            screenshot: Some(Err(DriverError::new(
                "Error",
                "Target page has been closed",
            ))),
            ..FakeSessionOptions::default()
        },
    );
    let healthy = create_session("healthy");
    let stale_handle = Arc::clone(&stale);
    let healthy_handle = Arc::clone(&healthy);
    let factory = FakeFactory::new(move |channel, _index| {
        if channel == "stale" {
            Ok(Arc::clone(&stale_handle).as_session())
        } else {
            Ok(Arc::clone(&healthy_handle).as_session())
        }
    });
    let manager = make_new_manager(
        factory as Arc<dyn SessionChannelSessionFactory>,
        SessionManagerDependencies {
            logger: Some(logger.clone() as Arc<dyn BrowserManagerLogger>),
            ..SessionManagerDependencies::default()
        },
    );
    manager.reconcile(&channels(&["stale", "healthy"])).await;

    assert_eq!(
        manager.capture_screenshot(None).await,
        Some(SessionScreenshot {
            channel: "healthy".to_string(),
            image: "screenshot:healthy".as_bytes().to_vec(),
        })
    );
    assert_eq!(manager.capture_screenshot(Some("stale")).await, None);
    assert_eq!(
        manager.capture_screenshot(Some("healthy")).await,
        Some(SessionScreenshot {
            channel: "healthy".to_string(),
            image: "screenshot:healthy".as_bytes().to_vec(),
        })
    );
    assert!(logger.called_exactly_with(
        LogLevel::Warn,
        "session_screenshot_failed",
        &[
            ("channel", v_str("stale")),
            ("error", v_str("Target page has been closed")),
        ]
    ));
}

// 原測試：stopAll 隔離 stop failure、清空 registry 且可重複呼叫
#[tokio::test(start_paused = true)]
async fn stop_all_isolates_stop_failures_clears_registry_and_repeats() {
    let first = create_session_with(
        "first",
        FakeSessionOptions {
            stop: Some(new_stop_source(|_reason| {
                Box::pin(async { Err(DriverError::new("Error", "stop failed")) })
            })),
            ..FakeSessionOptions::default()
        },
    );
    let second = create_session("second");
    let first_handle = Arc::clone(&first);
    let second_handle = Arc::clone(&second);
    let factory = FakeFactory::new(move |channel, _index| {
        if channel == "first" {
            Ok(Arc::clone(&first_handle).as_session())
        } else {
            Ok(Arc::clone(&second_handle).as_session())
        }
    });
    let manager = make_new_manager(
        factory as Arc<dyn SessionChannelSessionFactory>,
        SessionManagerDependencies::default(),
    );
    manager.reconcile(&channels(&["first", "second"])).await;

    let manager_a = manager.clone();
    let manager_b = manager.clone();
    let (stop_all_a, stop_all_b) = tokio::join!(
        async move { manager_a.stop_all("shutdown").await },
        async move { manager_b.stop_all("duplicate_shutdown").await }
    );
    let _: () = stop_all_a;
    let _: () = stop_all_b;

    assert_eq!(manager.get_active_channels(), Vec::<String>::new());
    assert_eq!(first.stop_calls(), ["shutdown"]);
    assert_eq!(second.stop_calls(), ["shutdown"]);
}

// 原測試：invalidate 移除並停止 session，重複呼叫無副作用且可由 reconcile 重建
#[tokio::test(start_paused = true)]
async fn invalidate_removes_and_stops_and_reconcile_can_rebuild() {
    let created: Arc<Mutex<Vec<Arc<FakeSession>>>> = Arc::new(Mutex::new(Vec::new()));
    let created_builder = Arc::clone(&created);
    let factory = FakeFactory::new(move |channel, _index| {
        let session = create_session(channel);
        created_builder.lock().unwrap().push(Arc::clone(&session));
        Ok(session.as_session())
    });
    let manager = make_new_manager(
        factory.clone() as Arc<dyn SessionChannelSessionFactory>,
        SessionManagerDependencies::default(),
    );
    manager.reconcile(&channels(&["channel"])).await;

    manager.invalidate("channel", "page_crashed").await;
    manager.invalidate("channel", "duplicate_crash").await;

    assert_eq!(manager.get_active_channels(), Vec::<String>::new());
    let created_snapshot = created.lock().unwrap().clone();
    assert_eq!(created_snapshot.len(), 1);
    assert_eq!(created_snapshot[0].stop_calls(), ["page_crashed"]);

    manager.reconcile(&channels(&["channel"])).await;

    assert_eq!(manager.get_active_channels(), ["channel"]);
    assert_eq!(factory.created().len(), 2);
    let created_snapshot = created.lock().unwrap().clone();
    assert_eq!(created_snapshot[1].start_call_count(), 1);
}

// 原測試：stop failure 時 invalidate 仍移除，下一輪可重建
#[tokio::test(start_paused = true)]
async fn invalidate_removes_even_when_stop_fails() {
    let factory = FakeFactory::new(move |_channel, creation_index| {
        let stop = if creation_index == 0 {
            Some(new_stop_source(|_reason: &str| {
                Box::pin(async { Err(DriverError::new("Error", "stop failed")) })
            }))
        } else {
            None
        };
        Ok(create_session_with(
            "channel",
            FakeSessionOptions {
                stop,
                ..FakeSessionOptions::default()
            },
        )
        .as_session())
    });
    let manager = make_new_manager(
        factory.clone() as Arc<dyn SessionChannelSessionFactory>,
        SessionManagerDependencies::default(),
    );
    manager.reconcile(&channels(&["channel"])).await;

    manager.invalidate("channel", "browser_disconnected").await;
    manager.reconcile(&channels(&["channel"])).await;

    assert_eq!(manager.get_active_channels(), ["channel"]);
    assert_eq!(factory.created().len(), 2);
}

// 原測試：併發 reconcile 依呼叫順序序列化且不重複建立同頻道
#[tokio::test(start_paused = true)]
async fn concurrent_reconciles_serialize_by_call_order_without_duplicates() {
    let (allow_start_gate, allow_start_signal) = create_gate();
    let factory = FakeFactory::new(move |channel, _index| {
        let start = if channel == "first" {
            let allow_start_signal = allow_start_signal.clone();
            Some(new_start_source(move || {
                let allow_start_signal = allow_start_signal.clone();
                Box::pin(async move {
                    wait_gate(allow_start_signal).await;
                    Ok(())
                })
            }))
        } else {
            None
        };
        Ok(create_session_with(
            channel,
            FakeSessionOptions {
                start,
                ..FakeSessionOptions::default()
            },
        )
        .as_session())
    });
    let manager = make_new_manager(
        factory.clone() as Arc<dyn SessionChannelSessionFactory>,
        SessionManagerDependencies::default(),
    );

    let manager_task_a = manager.clone();
    let task_channels_a = channels(&["first"]);
    let first_reconcile =
        tokio::spawn(async move { manager_task_a.reconcile(&task_channels_a).await });
    // 對映 `await startEntered.promise`:start 建立出來(factory 已呼叫)。
    assert_eventually("first.start 應已建立", || {
        !manager.inner.starting_sessions.lock().unwrap().is_empty()
    })
    .await;
    let manager_task_b = manager.clone();
    let task_channels_b = channels(&["first", "second"]);
    let second_reconcile =
        tokio::spawn(async move { manager_task_b.reconcile(&task_channels_b).await });

    // 讓第二個 reconcile 的同步前段跑完(排隊登記在第一個的 exclusive 尾巴之後);
    // 舊測試此時 factory.create 只被呼叫過一次。
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert_eq!(factory.created(), ["first"]);
    allow_start_gate.release();
    first_reconcile.await.unwrap();
    second_reconcile.await.unwrap();

    assert_eq!(factory.created(), ["first", "second"]);
    assert_eq!(manager.get_active_channels(), ["first", "second"]);
}

// 原測試：stopAll 會等待先提交的 reconcile 完成後再清空
#[tokio::test(start_paused = true)]
async fn stop_all_waits_for_submitted_reconcile_before_clearing() {
    let (allow_start_gate, allow_start_signal) = create_gate();
    let factory = FakeFactory::new(move |_channel, _index| {
        let allow_start_signal = allow_start_signal.clone();
        Ok(create_session_with(
            "channel",
            FakeSessionOptions {
                start: Some(new_start_source(move || {
                    let allow_start_signal = allow_start_signal.clone();
                    Box::pin(async move {
                        wait_gate(allow_start_signal).await;
                        Ok(())
                    })
                })),
                ..FakeSessionOptions::default()
            },
        )
        .as_session())
    });
    let manager = make_new_manager(
        factory as Arc<dyn SessionChannelSessionFactory>,
        SessionManagerDependencies::default(),
    );

    let manager_task = manager.clone();
    let task_channels = channels(&["channel"]);
    let reconcile = tokio::spawn(async move { manager_task.reconcile(&task_channels).await });
    assert_eventually("start 應已進入", || {
        !manager.inner.starting_sessions.lock().unwrap().is_empty()
    })
    .await;
    let manager_task_stop = manager.clone();
    let stop_all = tokio::spawn(async move { manager_task_stop.stop_all("shutdown").await });
    // 讓 stop_all 的快照與排程先完成(對映舊 Promise 建立的同步前段)。
    tokio::task::yield_now().await;
    allow_start_gate.release();
    reconcile.await.unwrap();
    stop_all.await.unwrap();

    assert_eq!(manager.get_active_channels(), Vec::<String>::new());
}

// 原測試：invalidate 會等待先提交的 reconcile 完成後移除該 session
#[tokio::test(start_paused = true)]
async fn invalidate_waits_for_submitted_reconcile_before_removing() {
    let (allow_start_gate, allow_start_signal) = create_gate();
    let factory = FakeFactory::new(move |_channel, _index| {
        let allow_start_signal = allow_start_signal.clone();
        Ok(create_session_with(
            "channel",
            FakeSessionOptions {
                start: Some(new_start_source(move || {
                    let allow_start_signal = allow_start_signal.clone();
                    Box::pin(async move {
                        wait_gate(allow_start_signal).await;
                        Ok(())
                    })
                })),
                ..FakeSessionOptions::default()
            },
        )
        .as_session())
    });
    let manager = make_new_manager(
        factory as Arc<dyn SessionChannelSessionFactory>,
        SessionManagerDependencies::default(),
    );

    let manager_task = manager.clone();
    let task_channels = channels(&["channel"]);
    let reconcile = tokio::spawn(async move { manager_task.reconcile(&task_channels).await });
    assert_eventually("start 應已進入", || {
        !manager.inner.starting_sessions.lock().unwrap().is_empty()
    })
    .await;
    let manager_task_invalidate = manager.clone();
    let invalidate = tokio::spawn(async move {
        manager_task_invalidate
            .invalidate("channel", "page_crashed")
            .await
    });
    // 讓 invalidate 的登記先完成(在 release 之前,對映舊測試的呼叫順序)。
    tokio::task::yield_now().await;
    allow_start_gate.release();
    reconcile.await.unwrap();
    invalidate.await.unwrap();

    assert_eq!(manager.get_active_channels(), Vec::<String>::new());
}

// 原測試：單一頻道 start 卡住不會阻塞其他頻道 invalidate
#[tokio::test(start_paused = true)]
async fn stuck_start_does_not_block_other_channel_invalidate() {
    let (allow_start_gate, allow_start_signal) = create_gate();
    let sessions: Arc<Mutex<Vec<Arc<FakeSession>>>> = Arc::new(Mutex::new(Vec::new()));
    let registry = Arc::clone(&sessions);
    let allow_start_for_builder = allow_start_signal.clone();
    let factory = FakeFactory::new(move |channel, _index| {
        let start = if channel == "stuck" {
            let allow_start_signal = allow_start_for_builder.clone();
            Some(new_start_source(move || {
                let allow_start_signal = allow_start_signal.clone();
                Box::pin(async move {
                    wait_gate(allow_start_signal).await;
                    Ok(())
                })
            }))
        } else {
            None
        };
        let session = create_session_with(
            channel,
            FakeSessionOptions {
                start,
                ..FakeSessionOptions::default()
            },
        );
        registry.lock().unwrap().push(Arc::clone(&session));
        Ok(session.as_session())
    });
    let manager = make_new_manager(
        factory as Arc<dyn SessionChannelSessionFactory>,
        SessionManagerDependencies::default(),
    );
    manager.reconcile(&channels(&["active"])).await;

    let manager_task = manager.clone();
    let task_channels = channels(&["active", "stuck"]);
    let reconcile = tokio::spawn(async move { manager_task.reconcile(&task_channels).await });
    assert_eventually("stuck.start 應已進入", || {
        sessions
            .lock()
            .unwrap()
            .iter()
            .any(|session| session.channel() == "stuck" && session.start_call_count() == 1)
    })
    .await;
    manager.invalidate("active", "page_crashed").await;

    let registry = sessions.lock().unwrap().clone();
    let active = registry
        .iter()
        .find(|session| session.channel() == "active")
        .unwrap();
    assert_eq!(active.stop_calls(), ["page_crashed"]);
    assert_eq!(manager.get_active_channels(), Vec::<String>::new());

    allow_start_gate.release();
    reconcile.await.unwrap();
}

// 原測試：stopAll 會取消進行中的 start 後在有限時間內完成
#[tokio::test(start_paused = true)]
async fn stop_all_cancels_in_flight_start_and_finishes_in_bounded_time() {
    let (allow_start_gate, allow_start_signal) = create_gate();
    let session = create_session_with(
        "channel",
        FakeSessionOptions {
            start: Some(new_start_source(move || {
                let allow_start_signal = allow_start_signal.clone();
                Box::pin(async move {
                    wait_gate(allow_start_signal).await;
                    Ok(())
                })
            })),
            cancel_start: Some(new_cancel_source(move |_reason| {
                let allow_start_gate = Arc::clone(&allow_start_gate);
                Box::pin(async move {
                    allow_start_gate.release();
                })
            })),
            ..FakeSessionOptions::default()
        },
    );
    let session_handle = Arc::clone(&session);
    let factory =
        FakeFactory::new(move |_channel, _index| Ok(Arc::clone(&session_handle).as_session()));
    let manager = make_new_manager(
        factory as Arc<dyn SessionChannelSessionFactory>,
        SessionManagerDependencies {
            session_operation_timeout_ms: Some(1_000),
            ..SessionManagerDependencies::default()
        },
    );

    let manager_task = manager.clone();
    let task_channels = channels(&["channel"]);
    let reconcile = tokio::spawn(async move { manager_task.reconcile(&task_channels).await });
    assert_eventually("start 應已進入", || session.start_call_count() == 1).await;
    manager.stop_all("shutdown").await;
    reconcile.await.unwrap();

    assert_eq!(session.cancel_start_calls(), ["shutdown"]);
    assert_eq!(session.stop_calls().len(), 1);
    assert_eq!(manager.get_active_channels(), Vec::<String>::new());
}

// 原測試：cancelStart 未解除卡住操作時 stopAll 仍會 timeout 完成
#[tokio::test(start_paused = true)]
async fn stop_all_finishes_by_timeout_when_cannot_release_stuck_operation() {
    let (_never_gate, never_signal) = create_gate();
    let session = create_session_with(
        "channel",
        FakeSessionOptions {
            start: Some(new_start_source(move || {
                let never_signal = never_signal.clone();
                Box::pin(async move {
                    wait_gate(never_signal).await;
                    Ok(())
                })
            })),
            ..FakeSessionOptions::default()
        },
    );
    let session_handle = Arc::clone(&session);
    let factory =
        FakeFactory::new(move |_channel, _index| Ok(Arc::clone(&session_handle).as_session()));
    let manager = make_new_manager(
        factory as Arc<dyn SessionChannelSessionFactory>,
        SessionManagerDependencies {
            session_operation_timeout_ms: Some(1_000),
            ..SessionManagerDependencies::default()
        },
    );

    let manager_task = manager.clone();
    let task_channels = channels(&["channel"]);
    let reconcile = tokio::spawn(async move { manager_task.reconcile(&task_channels).await });
    assert_eventually("start 應已進入", || session.start_call_count() == 1).await;
    let manager_task_stop = manager.clone();
    let stop_all = tokio::spawn(async move { manager_task_stop.stop_all("shutdown").await });
    // 讓 stop_all 的快照/登記先完成;paused 時鐘隨後自動推進到逾時。
    tokio::task::yield_now().await;
    stop_all.await.unwrap();
    reconcile.await.unwrap();

    assert_eq!(session.cancel_start_calls(), ["shutdown"]);
    assert_eq!(manager.get_active_channels(), Vec::<String>::new());
}

// 原測試：輸入只對完全相同字串去重，大小寫不同視為不同頻道
#[tokio::test(start_paused = true)]
async fn dedupe_only_exact_strings_case_distinct_channels() {
    let factory = create_factory();
    let manager = make_new_manager(
        factory.clone() as Arc<dyn SessionChannelSessionFactory>,
        SessionManagerDependencies::default(),
    );

    manager
        .reconcile(&channels(&[
            "Channel", "Channel", "channel", "CHANNEL", "channel",
        ]))
        .await;

    assert_eq!(
        manager.get_active_channels(),
        ["Channel", "channel", "CHANNEL"]
    );
    assert_eq!(factory.created().len(), 3);
}

// 原測試：失敗日誌遮罩敏感錯誤內容且不傳 session object
#[tokio::test(start_paused = true)]
async fn failure_log_redacts_sensitive_content_and_omits_session_object() {
    let logger = create_logger();
    let factory = FakeFactory::new(|_channel, _index| {
        Ok(create_session_with(
            "channel",
            FakeSessionOptions {
                start: Some(start_source_always(Err(DriverError::new(
                    "Error",
                    "access_token=top-secret-value",
                )))),
                ..FakeSessionOptions::default()
            },
        )
        .as_session())
    });
    let manager = make_new_manager(
        factory as Arc<dyn SessionChannelSessionFactory>,
        SessionManagerDependencies {
            logger: Some(logger.clone() as Arc<dyn BrowserManagerLogger>),
            ..SessionManagerDependencies::default()
        },
    );

    manager.reconcile(&channels(&["channel"])).await;

    let error_calls = logger.fields_of(LogLevel::Error, "session_start_failed");
    assert_eq!(error_calls.len(), 1);
    let serialized = logger.serialized();
    assert!(!serialized.contains("top-secret-value"));
    let fields = &error_calls[0];
    let entries = fields.as_object().expect("欄位應為物件");
    assert!(!entries.contains_key("session"));
    assert!(object_equals(
        &[
            ("channel", v_str("channel")),
            ("error", v_str("access_token=[REDACTED]")),
        ],
        fields
    ));
}

// ---------------------------------------------------------------------------
// 補充測試(regex 轉換檢查;非舊測試移植,標題見各測試註解)
// ---------------------------------------------------------------------------

/// §7.7 轉換規則的分支檢查:is_recoverable/is_fatal/is_navigation_timeout 的
/// 樣式輸入(§7.4 的分類訊息;舊單元測試沒有涵蓋中文樣式,補上最小輸入)。
#[test]
fn session_error_classification_patterns() {
    // recoverable 的各分支(含中文訊息)。
    assert!(super::is_recoverable_session_start_error(
        "page.goto: Target page, context or browser has been closed"
    ));
    assert!(super::is_recoverable_session_start_error(
        "browser has been closed"
    ));
    assert!(super::is_recoverable_session_start_error(
        "browser manager has been closed"
    ));
    assert!(super::is_recoverable_session_start_error(
        "context unexpectedly closed"
    ));
    assert!(super::is_recoverable_session_start_error(
        "page unexpectedly closed"
    ));
    assert!(super::is_recoverable_session_start_error("target crashed"));
    assert!(super::is_recoverable_session_start_error(
        "page.goto: Timeout 30000ms exceeded"
    ));
    assert!(super::is_recoverable_session_start_error(
        "導向非預期 Twitch URL"
    ));
    assert!(super::is_recoverable_session_start_error(
        "Browser Manager 尚未啟動"
    ));
    // 非 recoverable。
    assert!(!super::is_recoverable_session_start_error("factory failed"));
    assert!(!super::is_recoverable_session_start_error("start failed"));
    // fatal prerequisite 的各分支(含 \s 展開的形狀)。
    assert!(super::is_fatal_prerequisite_error("authentication failed"));
    assert!(super::is_fatal_prerequisite_error("authorization failed"));
    assert!(super::is_fatal_prerequisite_error("auth failed"));
    assert!(super::is_fatal_prerequisite_error("configuration error"));
    assert!(super::is_fatal_prerequisite_error("configuration invalid"));
    assert!(super::is_fatal_prerequisite_error("storage state missing"));
    assert!(super::is_fatal_prerequisite_error("storage-state invalid"));
    assert!(super::is_fatal_prerequisite_error("storage_state error"));
    assert!(!super::is_fatal_prerequisite_error("factory failed"));
    // retrieable = 非 fatal 且 recoverable。
    assert!(super::is_retriable_session_start_error("target crashed"));
    assert!(!super::is_retriable_session_start_error(
        "authentication failed"
    ));
    assert!(!super::is_retriable_session_start_error("factory failed"));
    // 導覽逾時的專屬分類。
    assert!(super::is_navigation_timeout_error(
        "page.goto: Timeout 30000ms exceeded."
    ));
    assert!(!super::is_navigation_timeout_error("auth failed"));
}

/// 逾時錯誤的字面對應(舊版 `"${event} after ${timeoutMs}ms"`)。
#[test]
fn session_operation_timeout_error_message_shape() {
    let error = SessionOperationTimeoutError {
        event: "session_start_timeout",
        timeout_ms: 1_000,
    };
    assert_eq!(error.to_string(), "session_start_timeout after 1000ms");
}

/// `SessionChannelPointsUnavailableReason` 的字面值(舊版聯集的字面字串)。
#[test]
fn session_channel_points_unavailable_reason_literals() {
    use super::SessionChannelPointsUnavailableReason::{
        Failed, NotFound, PageUnavailable, ParseFailed,
    };
    assert_eq!(NotFound.as_str(), "not_found");
    assert_eq!(ParseFailed.as_str(), "parse_failed");
    assert_eq!(PageUnavailable.as_str(), "page_unavailable");
    assert_eq!(Failed.as_str(), "failed");
}

/// 補充:cancelPendingStarts 會升起 shutdownRequested(舊單元測試只經
/// stopAll 觀察此旗標;這裡直接對 TSV 以外的介面行為補一個最簡案例)。
#[tokio::test(start_paused = true)]
async fn cancel_pending_starts_raises_shutdown_requested() {
    let factory = create_factory();
    let manager = make_new_manager(
        factory as Arc<dyn SessionChannelSessionFactory>,
        SessionManagerDependencies::default(),
    );
    assert!(!manager.inner.shutdown_requested.load(Ordering::SeqCst));
    manager.cancel_pending_starts("manual").await;
    assert!(manager.inner.shutdown_requested.load(Ordering::SeqCst));
    // shutdownRequested 之後的 reconcile 直接返回(active 不變)。
    manager.reconcile(&channels(&["first"])).await;
    assert_eq!(manager.get_active_channels(), Vec::<String>::new());
}
