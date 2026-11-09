//! CDP 頁面：target 初始化、事件泵、生命週期追蹤與 Page 最小 API
//! （§7.3.4、§7.3.5、§7.3.6 的攔截決策）。
//!
//! 初始化照 `crPage.ts` 的 `FrameSession::_initialize` 錄製順序（G2 目標，
//! 以 `doc/spec/traces/*.seq` 為準）：`Browser.getWindowForTarget` → `Page.enable`
//! → `Page.getFrameTree` → `Log.enable` → `Page.setLifecycleEventsEnabled` →
//! `Runtime.enable` → `Network.enable` →（資源阻擋時 `Network.setCacheDisabled`、
//! `Fetch.enable`）→ `Target.setAutoAttach` → `Emulation.setFocusEmulationEnabled`
//! → `Browser.setWindowBounds` → `Emulation.setDeviceMetricsOverride` →
//! `Page.setFontFamilies` → `Emulation.setEmulatedMedia` → 每個 init script 一個
//! `Page.addScriptToEvaluateOnNewDocument {source, runImmediately: true}` →
//! `Runtime.runIfWaitingForDebugger`。
//!
//! 事件泵（每個 session 一個 task，頁面本體與其 worker／OOPIF 共用同一組處理器）：
//! 更新主框架 URL／loaderId／生命週期旗標、維護 Network 請求對照表、把
//! `Fetch.requestPaused` 依攔截種類 continue／fail／fulfill（照 `_sendMayFail`
//! 不等回應），並把診斷事件轉成 [`PageDiagnostic`] 派給 listener（listener 內的
//! 錯誤一律吞掉，§7.3.5）。iframe（OOPIF）與 worker 的 console／網路事件因此
//! 歸屬所屬頁面（與 Playwright 相同）。
//!
//! 以下 API 尚無正式呼叫端，等對應步驟接上後移除 allow：
//! - `PageAdapter::reload`／`on_crash`／`on_close`（S1.5、S1.8）。
//! - 診斷 listener 的取消函式（S1.6 接 `BrowserManager`）。
//! - `PageDiagnostic` 的完整欄位（S1.6 精修消毒與分級）。
#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use base64::Engine as _;
use regex::Regex;
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};

use super::connection::Event;
use super::context::{ContextShared, DriverError, InterceptionKind};
use super::injected::InjectedScript;
use super::launcher::{CancelListener, LaunchedBrowser};
use crate::browser::diagnostics::extract_graph_ql_operation_names;

/// 導覽等待的預設逾時（§7.4：30 000 ms；`setDefaultNavigationTimeout` 覆寫）。
const DEFAULT_NAVIGATION_TIMEOUT_MS: u64 = 30_000;

/// 診斷文字的截斷長度（照舊版 `browser-diagnostics.ts`）。
const MAX_HOST_LENGTH: usize = 128;
const MAX_PATH_LENGTH: usize = 256;
const MAX_MESSAGE_LENGTH: usize = 500;
const MAX_FAILURE_TEXT_LENGTH: usize = 300;
/// 請求對照表的上限：終止事件（loadingFinished／loadingFailed）是可丟棄的診斷事件，
/// 遺失時對照不會被移除；超過上限就整批清除（只影響進行中請求的診斷輸出）。
const MAX_TRACKED_REQUESTS: usize = 1024;

/// `browser-diagnostics.ts` 的 `REDACTED_URL`。
const REDACTED_URL: &str = "[REDACTED]";

/// 頁面生命週期事件名（`waitUntil` 支援的兩種，照舊版用法）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WaitUntil {
    DomContentLoaded,
    Load,
}

impl WaitUntil {
    fn lifecycle_name(self) -> &'static str {
        match self {
            WaitUntil::DomContentLoaded => "DOMContentLoaded",
            WaitUntil::Load => "load",
        }
    }
}

/// 頁面的監看快照：事件泵更新、API 端以 watch 等待。
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct PageSnapshot {
    /// 主框架目前的 URL（`frameNavigated`／`navigatedWithinDocument` 追蹤）。
    pub(crate) url: String,
    /// 目前文件的 loaderId（生命週期事件以此對應文件）。
    pub(crate) loader_id: String,
    pub(crate) domcontentloaded: bool,
    pub(crate) load: bool,
    pub(crate) closed: bool,
    pub(crate) crashed: bool,
    /// 主框架 main world 的 execution context id（`evaluate` 用；導覽會摧毀）。
    pub(crate) context_id: Option<i64>,
    /// 主框架「跨文件 commit」的累計次數（`frameNavigated`；判斷導覽被取代用）。
    pub(crate) commit_count: u64,
    /// 同文件導覽的累計次數（`page.goto` 的同文件等待用）。
    pub(crate) same_document_commits: u64,
    /// 主框架被請求導覽的累計次數（`Page.frameRequestedNavigation`；click 的
    /// `waitForSignalsCreatedBy` 觀察動作觸發的導覽用）。
    pub(crate) navigation_requests: u64,
}

/// 診斷事件（S1.4：照 §7.5 的 CDP 來源翻譯；S1.6 再精修消毒與分級）。
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum PageDiagnostic {
    RequestFailed {
        url: String,
        method: String,
        resource_type: String,
        failure_text: Option<String>,
        graphql_operation_names: Vec<String>,
    },
    Response {
        url: String,
        method: String,
        resource_type: String,
        status: u32,
        status_text: String,
        graphql_operation_names: Vec<String>,
    },
    Console {
        r#type: String,
        text: String,
        source_url: Option<String>,
        line_number: i64,
        column_number: i64,
    },
}

/// listener 清單（鎖保護；fire 時 clone 出清單，鎖外呼叫）。
type ListenerList<E> = Vec<Arc<dyn Fn(&E) + Send + Sync>>;

/// listener 集合：註冊回傳取消函式；fire 時逐一呼叫並攔 panic
/// （§7.3.5：listener 的錯誤不得影響其他 listener 與頁面生命週期）。
pub(crate) struct ListenerSet<E> {
    listeners: Arc<Mutex<ListenerList<E>>>,
}

impl<E> Default for ListenerSet<E> {
    fn default() -> Self {
        ListenerSet {
            listeners: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl<E: 'static> ListenerSet<E> {
    /// 註冊 listener，回傳取消函式。
    fn subscribe(&self, listener: impl Fn(&E) + Send + Sync + 'static) -> CancelListener {
        let entry: Arc<dyn Fn(&E) + Send + Sync> = Arc::new(listener);
        self.listeners
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(Arc::clone(&entry));
        let listeners = Arc::clone(&self.listeners);
        Box::new(move || {
            listeners
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .retain(|registered| !Arc::ptr_eq(registered, &entry));
        })
    }

    /// 觸發所有 listener（鎖外逐一呼叫；panic 一律攔下）。
    fn fire(&self, event: &E) {
        let listeners = self
            .listeners
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        for listener in listeners {
            // listener 是外部提供的程式碼：攔下 panic 讓事件泵繼續。
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| listener(event)));
        }
    }
}

/// Network 請求對照表的單筆（診斷事件與攔截決策共用）。
#[derive(Clone)]
struct RequestRecord {
    url: String,
    method: String,
    /// CDP 事件的 `type` 轉小寫；沒有時為 `other`（§7.5）。
    resource_type: String,
    /// `postDataEntries` 各段 base64 解碼後串接的 UTF-8 文字（沒有時 None）。
    post_data: Option<String>,
}

/// 頁面 session 的網路狀態：攔截種類、請求對照表與等待配對的事件
/// （照 `crNetworkManager.ts`：`requestPaused` 與 `requestWillBeSent` 的先後
/// 不固定，用 `networkId` 對應兩者）。
#[derive(Default)]
struct NetworkState {
    kind: InterceptionKind,
    /// networkId → 已建立的請求（等 response／failed 派 diag）。
    requests: HashMap<String, RequestRecord>,
    /// networkId → `Network.requestWillBeSent` 事件原文（等 `Fetch.requestPaused`）。
    awaiting_paused: HashMap<String, Value>,
    /// networkId → 攔截 id（`interception-job-…`，等 `Network.requestWillBeSent`）。
    paused: HashMap<String, String>,
    /// 已嘗試提供認證的 interceptionId（`_attemptedAuthentications`）。
    attempted_auth: HashSet<String>,
}

impl NetworkState {
    /// 登記（或覆寫）請求對照；新請求會讓表超過上限時先整批清除。
    fn track_request(&mut self, network_id: String, record: RequestRecord) {
        if !self.requests.contains_key(&network_id) && self.requests.len() >= MAX_TRACKED_REQUESTS {
            self.requests.clear();
        }
        self.requests.insert(network_id, record);
    }
}

/// 頁面的共用狀態（事件泵是唯一寫入者；API 端以 watch 等待）。
pub(crate) struct PageShared {
    target_id: String,
    session_id: String,
    context: Arc<ContextShared>,
    browser: Arc<super::context::BrowserShared>,
    snapshot: watch::Sender<PageSnapshot>,
    /// 永不讀取的接收端：tokio watch 的 `send` 在沒有接收端時**不會儲存新值**
    /// （返回 Err 即丟棄），等待者晚於事件訂閱會錯過歷史更新；持有這個 keeper
    /// 讓 watch 永遠有接收端，`send` 一定成功並儲存。
    #[allow(dead_code)]
    snapshot_keeper: watch::Receiver<PageSnapshot>,
    /// 初始化結果（`None` = 初始化中）。
    init_done: watch::Sender<Option<Result<(), DriverError>>>,
    /// 同 [`PageShared::snapshot_keeper`]（`init_done` 的 keeper）。
    #[allow(dead_code)]
    init_done_keeper: watch::Receiver<Option<Result<(), DriverError>>>,
    /// 主框架 frame id（getFrameTree 初始化時設定；`Page.navigate` 用）。
    main_frame_id: Mutex<String>,
    /// 導覽逾時（`setDefaultNavigationTimeout` 設定的值）。
    navigation_timeout_ms: AtomicU64,
    /// 攔截與網路狀態（鎖內不做 await）。
    network: Mutex<NetworkState>,
    /// 主框架 main world 的 injected script 把手（X8；事件泵在 context 變動時清空）。
    injected: InjectedScript,
    closed: AtomicBool,
    crashed: AtomicBool,
    /// 初始化成功的頁面（照 Playwright 的 `initializedOrUndefined`）。
    initialized: AtomicBool,
    is_storage_state_page: bool,
    /// opener 頁面（popup 用；初始化完成時觸發其 popup listener）。
    opener: Option<Weak<PageShared>>,
    close_listeners: ListenerSet<()>,
    crash_listeners: ListenerSet<()>,
    popup_listeners: ListenerSet<Arc<PageShared>>,
    diag_listeners: ListenerSet<PageDiagnostic>,
}

/// `PageInit`：頁面建立的情境（照 `CRPage` 建構的 bits 與 opener）。
pub(crate) struct PageInit {
    /// storageState 的暫時頁：不做視窗／模擬／init script 初始化。
    pub(crate) is_storage_state_page: bool,
}

impl PageShared {
    pub(crate) fn new(
        browser: Arc<super::context::BrowserShared>,
        context: Arc<ContextShared>,
        target_id: String,
        session_id: String,
        opener: Option<Weak<PageShared>>,
        init: PageInit,
    ) -> Self {
        let (snapshot, snapshot_keeper) = watch::channel(PageSnapshot::default());
        let (init_done, init_done_keeper) = watch::channel(None);
        PageShared {
            target_id,
            session_id,
            context,
            browser,
            snapshot,
            snapshot_keeper,
            init_done,
            init_done_keeper,
            main_frame_id: Mutex::new(String::new()),
            navigation_timeout_ms: AtomicU64::new(DEFAULT_NAVIGATION_TIMEOUT_MS),
            network: Mutex::new(NetworkState::default()),
            injected: InjectedScript::default(),
            closed: AtomicBool::new(false),
            crashed: AtomicBool::new(false),
            initialized: AtomicBool::new(false),
            is_storage_state_page: init.is_storage_state_page,
            opener,
            close_listeners: ListenerSet::default(),
            crash_listeners: ListenerSet::default(),
            popup_listeners: ListenerSet::default(),
            diag_listeners: ListenerSet::default(),
        }
    }

    pub(crate) fn target_id(&self) -> &str {
        &self.target_id
    }

    pub(crate) fn is_storage_state_page(&self) -> bool {
        self.is_storage_state_page
    }

    pub(crate) fn is_initialized(&self) -> bool {
        self.initialized.load(Ordering::SeqCst)
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    pub(crate) fn is_crashed(&self) -> bool {
        self.crashed.load(Ordering::SeqCst)
    }

    /// 目前的 URL（`page.url()`）。
    pub(crate) fn url(&self) -> String {
        self.snapshot.borrow().url.clone()
    }

    /// 狀態 watch 的訂閱端（click 的導覽等待用）。
    pub(crate) fn subscribe_snapshot(&self) -> watch::Receiver<PageSnapshot> {
        self.snapshot.subscribe()
    }

    /// 導覽的累計計數（requested、跨文件 commit、同文件 commit；
    /// click 的 `waitForSignalsCreatedBy` 觀察動作觸發的導覽用）。
    pub(crate) fn navigation_counters(&self) -> (u64, u64, u64) {
        let snapshot = self.snapshot.borrow();
        (
            snapshot.navigation_requests,
            snapshot.commit_count,
            snapshot.same_document_commits,
        )
    }

    /// 是否有「已請求但尚未收尾」的導覽（照 `frame._pendingDocument`：
    /// requested 計數超過已收尾的導覽數）。
    pub(crate) fn has_pending_navigation(&self) -> bool {
        let snapshot = self.snapshot.borrow();
        snapshot.navigation_requests
            > snapshot
                .commit_count
                .saturating_add(snapshot.same_document_commits)
    }

    /// 主框架 main world 的 injected script 把手（locator／evaluate 用）。
    pub(crate) fn injected(&self) -> &InjectedScript {
        &self.injected
    }

    /// 導覽逾時（毫秒）。
    fn navigation_timeout(&self) -> Duration {
        Duration::from_millis(self.navigation_timeout_ms.load(Ordering::SeqCst).max(1))
    }

    /// `page.setDefaultNavigationTimeout(ms)`（只影響 `goto`／`reload`）。
    pub(crate) fn set_default_navigation_timeout(&self, ms: u64) {
        self.navigation_timeout_ms.store(ms, Ordering::SeqCst);
    }

    /// 註冊 close listener（§7.3.5）。
    pub(crate) fn on_close(&self, listener: impl Fn() + Send + Sync + 'static) -> CancelListener {
        self.close_listeners.subscribe(move |_| listener())
    }

    /// 註冊 crash listener（§7.3.5）。
    pub(crate) fn on_crash(&self, listener: impl Fn() + Send + Sync + 'static) -> CancelListener {
        self.crash_listeners.subscribe(move |_| listener())
    }

    /// 註冊 popup listener（opener 頁面用；popup 初始化完成時觸發）。
    pub(crate) fn on_popup(
        &self,
        listener: impl Fn(&Arc<PageShared>) + Send + Sync + 'static,
    ) -> CancelListener {
        self.popup_listeners.subscribe(listener)
    }

    /// 註冊診斷 listener。
    pub(crate) fn on_diagnostic(
        &self,
        listener: impl Fn(&PageDiagnostic) + Send + Sync + 'static,
    ) -> CancelListener {
        self.diag_listeners.subscribe(listener)
    }

    /// 送出一則頁面 session 的指令（等回應）。
    ///
    /// 頁面已關閉時不送出、立即回已關閉錯誤（照 Playwright 的
    /// LongStandingScope：close 之後的指令一律 `Target page, context or
    /// browser has been closed`——不然 Chromium 會回「Session with given id
    /// not found」，訊息不同）。
    pub(crate) async fn send(&self, method: &str, params: Value) -> Result<Value, DriverError> {
        if self.is_closed() {
            return Err(DriverError::target_closed());
        }
        self.browser
            .launched
            .send_command(Some(&self.session_id), method, params)
            .await
            .map_err(DriverError::from)
    }

    /// 送出一則指令但不等回應（攔截的 continue／fail／fulfill，照 `_sendMayFail`）。
    ///
    /// 指令在 Router 鎖內排隊，順序與事件泵的處理順序一致；不等回應避免堵住
    /// 事件泵（瀏覽器要收到 continue 才會繼續送事件）。
    pub(crate) fn send_may_fail(&self, method: &str, params: Value) {
        let launched = Arc::clone(&self.browser.launched);
        let session = self.session_id.clone();
        let method = method.to_string();
        tokio::spawn(async move {
            let _ = launched.send_command(Some(&session), &method, params).await;
        });
    }

    // ===== 初始化（§7.3.4）=====

    /// 依錄製順序初始化頁面 session，最後 `Runtime.runIfWaitingForDebugger`。
    pub(crate) async fn run_init(self: Arc<Self>) -> Result<(), DriverError> {
        let storage_state_page = self.is_storage_state_page;
        let session = Some(self.session_id.as_str());
        let launched = Arc::clone(&self.browser.launched);

        // 照 crPage：暫時頁不送 `Browser.getWindowForTarget`；一般頁失敗時照
        // Playwright 吞掉（沒有 windowId 就不送 `Browser.setWindowBounds`）。
        let mut window_id: Option<i64> = None;
        if !storage_state_page
            && let Ok(reply) = launched
                .send_command(session, "Browser.getWindowForTarget", json!({}))
                .await
        {
            window_id = reply.get("windowId").and_then(Value::as_i64);
        }

        launched
            .send_command(session, "Page.enable", json!({}))
            .await?;
        let frame_tree = launched
            .send_command(session, "Page.getFrameTree", json!({}))
            .await?;
        let frame = frame_tree
            .get("frameTree")
            .and_then(|tree| tree.get("frame"))
            .cloned()
            .unwrap_or(Value::Null);
        *self
            .main_frame_id
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = frame
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let url = frame
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let loader_id = frame
            .get("loaderId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        // 初始快照（getFrameTree 反映 target 建立時已 commit 的初始文件）。
        let _ = self.snapshot.send(PageSnapshot {
            url,
            loader_id,
            ..PageSnapshot::default()
        });

        launched
            .send_command(session, "Log.enable", json!({}))
            .await?;
        launched
            .send_command(
                session,
                "Page.setLifecycleEventsEnabled",
                json!({"enabled": true}),
            )
            .await?;
        launched
            .send_command(session, "Runtime.enable", json!({}))
            .await?;
        launched
            .send_command(session, "Network.enable", json!({}))
            .await?;

        // 資源阻擋在初始化時套用（照錄製：b1 的 page#2／page#3 在 `Network.enable`
        // 之後、`Target.setAutoAttach` 之前）；暫時頁的攔截在建立後才啟用
        // （照 setStorageState 的順序）。
        let interception = {
            let mut network = self
                .network
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let kind = if storage_state_page {
                InterceptionKind::None
            } else if self.context.blocking_any_enabled() {
                InterceptionKind::ResourceBlocking
            } else {
                InterceptionKind::None
            };
            network.kind = kind;
            kind
        };
        if interception != InterceptionKind::None {
            self.enable_interception_commands(session, &launched)
                .await?;
        }

        launched
            .send_command(
                session,
                "Target.setAutoAttach",
                json!({"autoAttach": true, "flatten": true, "waitForDebuggerOnStart": true}),
            )
            .await?;

        if !storage_state_page {
            launched
                .send_command(
                    session,
                    "Emulation.setFocusEmulationEnabled",
                    json!({"enabled": true}),
                )
                .await?;
            if let Some(window_id) = window_id {
                let (width, height) = self.context.viewport();
                launched
                    .send_command(
                        session,
                        "Browser.setWindowBounds",
                        json!({
                            "windowId": window_id,
                            "bounds": {"width": width, "height": height},
                        }),
                    )
                    .await?;
            }
            let (width, height) = self.context.viewport();
            launched
                .send_command(
                    session,
                    "Emulation.setDeviceMetricsOverride",
                    json!({
                        "mobile": false,
                        "width": width,
                        "height": height,
                        "screenWidth": width,
                        "screenHeight": height,
                        "deviceScaleFactor": 1,
                        "screenOrientation": {"angle": 0, "type": "landscapePrimary"},
                    }),
                )
                .await?;
            launched
                .send_command(
                    session,
                    "Page.setFontFamilies",
                    json!(default_font_families(launched.platform())),
                )
                .await?;
            launched
                .send_command(
                    session,
                    "Emulation.setEmulatedMedia",
                    json!({
                        "media": "",
                        "features": [
                            {"name": "prefers-color-scheme", "value": "light"},
                            {"name": "prefers-reduced-motion", "value": "no-preference"},
                            {"name": "forced-colors", "value": "none"},
                            {"name": "prefers-contrast", "value": "no-preference"},
                        ],
                    }),
                )
                .await?;
            // 每個 init script 一個 `Page.addScriptToEvaluateOnNewDocument`
            // {source, runImmediately: true}（照 `allInitScripts()` 的順序）。
            for script in self.context.init_scripts() {
                launched
                    .send_command(
                        session,
                        "Page.addScriptToEvaluateOnNewDocument",
                        json!({"source": script.as_str(), "runImmediately": true}),
                    )
                    .await?;
            }
        }

        launched
            .send_command(session, "Runtime.runIfWaitingForDebugger", json!({}))
            .await?;
        Ok(())
    }

    /// 攔截的兩條指令：`Network.setCacheDisabled {cacheDisabled: true}` +
    /// `Fetch.enable {handleAuthRequests: true, patterns: [{urlPattern: "*", requestStage: "Request"}]}`。
    async fn enable_interception_commands(
        &self,
        session: Option<&str>,
        launched: &Arc<LaunchedBrowser>,
    ) -> Result<(), DriverError> {
        launched
            .send_command(
                session,
                "Network.setCacheDisabled",
                json!({"cacheDisabled": true}),
            )
            .await?;
        launched
            .send_command(
                session,
                "Fetch.enable",
                json!({
                    "handleAuthRequests": true,
                    "patterns": [{"urlPattern": "*", "requestStage": "Request"}],
                }),
            )
            .await?;
        Ok(())
    }

    /// 對已存在的頁面補送攔截（`configureResourceBlocking` 對既有頁面用）。
    ///
    /// 已有攔截（如暫時頁的 fulfill 攔截）時不重送：照錄製結果，b0／b1 的
    /// page#1 段落完全相同。
    pub(crate) async fn ensure_interception(
        &self,
        kind: InterceptionKind,
    ) -> Result<(), DriverError> {
        let already = {
            let mut network = self
                .network
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if network.kind != InterceptionKind::None {
                true
            } else {
                network.kind = kind;
                false
            }
        };
        if already {
            return Ok(());
        }
        self.enable_interception_commands(Some(&self.session_id), &self.browser.launched)
            .await
    }

    /// 對既有頁面補送一個 init script（`doAddInitScript`）。
    pub(crate) async fn add_init_script_to_session(
        &self,
        source: &Arc<String>,
    ) -> Result<(), DriverError> {
        self.send(
            "Page.addScriptToEvaluateOnNewDocument",
            json!({"source": source.as_str(), "runImmediately": true}),
        )
        .await
        .map(|_| ())
    }

    /// 初始化完成的回報：resolve 等待者；成功且 opener 存在時觸發 opener 的
    /// popup listener（§7.3.4：初始化完成後對 opener 觸發 popup；popup 也套用
    /// context 的 init script 與資源阻擋——由 run_init 依 context 狀態套用）。
    pub(crate) fn mark_initialized(self: &Arc<Self>, result: Result<(), DriverError>) {
        self.initialized.store(result.is_ok(), Ordering::SeqCst);
        let _ = self.init_done.send(Some(result.clone()));
        if result.is_ok()
            && let Some(opener) = self.opener.as_ref().and_then(Weak::upgrade)
        {
            // popup 初始化完成：對 opener 觸發 popup listener（fire 內已攔 panic）。
            opener.popup_listeners.fire(&Arc::clone(self));
        }
    }

    /// 等 `Target.createTarget` 的頁面初始化完成（`new_page` 用）。
    pub(crate) async fn wait_initialized(&self) -> Result<(), DriverError> {
        let mut init_done = self.init_done.subscribe();
        loop {
            if let Some(result) = init_done.borrow_and_update().clone() {
                return result;
            }
            if init_done.changed().await.is_err() {
                return Err(DriverError::target_closed());
            }
        }
    }

    // ===== 生命週期（§7.3.5）=====

    /// 頁面 target 結束（`Target.detachedFromTarget` 或 `close()` 完成）：標記
    /// 關閉並觸發 close listener（重複呼叫只觸發一次）。
    pub(crate) fn did_close_target(self: &Arc<Self>) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        self.publish_snapshot();
        self.close_listeners.fire(&());
    }

    /// crash（`Inspector.targetCrashed`）。
    pub(crate) fn did_crash(self: &Arc<Self>) {
        if self.crashed.swap(true, Ordering::SeqCst) {
            return;
        }
        self.publish_snapshot();
        self.crash_listeners.fire(&());
    }

    /// 把目前狀態廣播出去（watch send 失敗代表沒有等待者，忽略）。
    fn publish_snapshot(&self) {
        let mut snapshot = self.snapshot.borrow().clone();
        snapshot.closed = self.is_closed();
        snapshot.crashed = self.is_crashed();
        let _ = self.snapshot.send(snapshot);
    }

    // ===== 事件泵（§7.3.5）=====

    /// 事件泵：消費一個 session 的事件並交給共用處理器。
    ///
    /// 頁面本體、worker、OOPIF 各一個泵；`main` 指出是否為頁面主 session
    /// （只有主 session 更新頁面狀態）。session 結束（router 解除登記）時
    /// channel 關閉、任務自然結束。
    pub(crate) async fn event_pump(
        page: Arc<PageShared>,
        mut events: mpsc::Receiver<Event>,
        main: bool,
    ) {
        while let Some(event) = events.recv().await {
            PageShared::handle_event(&page, &event, main);
        }
    }

    /// 單一事件的處理分派（同步；需要 await 的部分另起任務或 fire-and-forget）。
    fn handle_event(page: &Arc<PageShared>, event: &Event, main: bool) {
        match event.method.as_str() {
            // 主框架 URL 與生命週期（只有頁面主 session 的主框架事件會更新狀態）。
            "Page.frameNavigated" => {
                let frame = event.params.get("frame").cloned().unwrap_or(Value::Null);
                if main
                    && frame.get("id").and_then(Value::as_str)
                        == Some(page.main_frame_id().as_str())
                {
                    page.on_main_frame_navigated(&frame);
                }
            }
            "Page.navigatedWithinDocument" => {
                let frame_id = event.params.get("frameId").and_then(Value::as_str);
                let url = event
                    .params
                    .get("url")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if main && frame_id == Some(page.main_frame_id().as_str()) {
                    page.on_main_frame_navigated_within_document(url);
                }
            }
            "Page.lifecycleEvent" => {
                let name = event
                    .params
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let frame_id = event.params.get("frameId").and_then(Value::as_str);
                let loader_id = event.params.get("loaderId").and_then(Value::as_str);
                if main && frame_id == Some(page.main_frame_id().as_str()) {
                    page.on_lifecycle_event(name, loader_id);
                }
            }
            // 主框架被請求的導覽（click 的 waitForSignalsCreatedBy 等它收尾）。
            "Page.frameRequestedNavigation" => {
                let frame_id = event.params.get("frameId").and_then(Value::as_str);
                if main && frame_id == Some(page.main_frame_id().as_str()) {
                    let mut snapshot = page.snapshot.borrow().clone();
                    snapshot.navigation_requests += 1;
                    let _ = page.snapshot.send(snapshot);
                }
            }
            // execution context（evaluate 用；worker 的第一個 context 也走這裡）。
            "Runtime.executionContextCreated" => {
                let context = event.params.get("context").cloned().unwrap_or(Value::Null);
                let is_default = context
                    .get("auxData")
                    .and_then(|aux| aux.get("isDefault"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let context_frame = context
                    .get("auxData")
                    .and_then(|aux| aux.get("frameId"))
                    .and_then(Value::as_str);
                let context_id = context.get("id").and_then(Value::as_i64);
                if is_default
                    && context_frame == Some(page.main_frame_id().as_str())
                    && let Some(context_id) = context_id
                {
                    // 舊 context 的 injected 物件已失效：對不上新 id 時清空（X8）。
                    page.injected.invalidate_on_new_context(context_id);
                    let mut snapshot = page.snapshot.borrow().clone();
                    snapshot.context_id = Some(context_id);
                    let _ = page.snapshot.send(snapshot);
                }
            }
            "Runtime.executionContextDestroyed" | "Runtime.executionContextsCleared" => {
                let destroyed = event
                    .params
                    .get("executionContextId")
                    .and_then(Value::as_i64);
                // injected script 隨 execution context 摧毀（X8）：丟棄建立的物件，
                // 下次使用重建（只有頁面主 session 的事件決定主框架的 context）。
                if main {
                    match event.method.as_str() {
                        "Runtime.executionContextDestroyed" => page.injected.invalidate(destroyed),
                        _ => page.injected.invalidate(None),
                    }
                }
                let mut snapshot = page.snapshot.borrow().clone();
                let matches = match (event.method.as_str(), destroyed, snapshot.context_id) {
                    ("Runtime.executionContextDestroyed", Some(id), Some(current)) => id == current,
                    ("Runtime.executionContextsCleared", _, Some(_)) => true,
                    _ => false,
                };
                if matches {
                    snapshot.context_id = None;
                    let _ = page.snapshot.send(snapshot);
                }
            }
            // 診斷事件（§7.5）：網路三件套與 console（含子 session 事件泵進來者）。
            "Network.requestWillBeSent" => PageShared::on_request_will_be_sent(page, &event.params),
            "Network.responseReceived" => PageShared::on_response_received(page, &event.params),
            "Network.loadingFinished" => PageShared::on_loading_finished(page, &event.params),
            "Network.loadingFailed" => PageShared::on_loading_failed(page, &event.params),
            "Fetch.requestPaused" => PageShared::on_request_paused(page, &event.params),
            "Fetch.authRequired" => PageShared::on_auth_required(page, &event.params),
            "Runtime.consoleAPICalled" => PageShared::on_console_api_called(page, &event.params),
            "Log.entryAdded" => PageShared::on_log_entry_added(page, &event.params),
            // crash（§7.3.5 的 on_crash 來源）：照 `crPage._onTargetCrashed` →
            // `crConnection.CRSession._markAsCrashed()`——先擋住後續指令（同步），
            // pending 的拒絕交給背景任務（事件泵內不持鎖時不再鎖同一鎖）。
            "Inspector.targetCrashed" if main => {
                page.browser.launched.mark_session_crashed(&page.session_id);
                let launched = Arc::clone(&page.browser.launched);
                let session = page.session_id.clone();
                tokio::spawn(async move {
                    launched.fail_crashed_session_pending(&session).await;
                });
                Arc::clone(page).did_crash();
            }
            // 非 main session（worker 等）的 crash：不影響頁面本體的狀態。
            "Inspector.targetCrashed" => {}
            // 嵌套 auto-attach：worker 與 OOPIF（§7.3.4）。
            "Target.attachedToTarget" if !main => {}
            "Target.attachedToTarget" => {
                PageShared::on_nested_attached_to_target(page, &event.params)
            }
            "Target.detachedFromTarget" if !main => {}
            "Target.detachedFromTarget" => {
                PageShared::on_nested_detached_from_target(page, &event.params)
            }
            // 其餘事件（頁面狀態之外的 protocol 事件）忽略。
            _ => {}
        }
    }

    /// 目前的主框架 frame id（`handle_event` 內比對用）。
    fn main_frame_id(&self) -> String {
        self.main_frame_id
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// 主框架導覽 commit（新文件）：更新 URL／loaderId、重設生命週期旗標。
    fn on_main_frame_navigated(self: &Arc<Self>, frame: &Value) {
        let url = format!(
            "{}{}",
            frame.get("url").and_then(Value::as_str).unwrap_or_default(),
            frame
                .get("urlFragment")
                .and_then(Value::as_str)
                .unwrap_or_default()
        );
        let loader_id = frame
            .get("loaderId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let mut snapshot = self.snapshot.borrow().clone();
        snapshot.url = url;
        snapshot.loader_id = loader_id;
        snapshot.domcontentloaded = false;
        snapshot.load = false;
        // 每個跨文件 commit 都計一次：等待端據此判斷「導覽被取代」。
        snapshot.commit_count = snapshot.commit_count.wrapping_add(1);
        let _ = self.snapshot.send(snapshot);
    }

    /// 同文件導覽 commit：更新 URL 並遞增同文件 commit 計數。
    fn on_main_frame_navigated_within_document(self: &Arc<Self>, url: &str) {
        let mut snapshot = self.snapshot.borrow().clone();
        snapshot.url = url.to_string();
        snapshot.same_document_commits += 1;
        let _ = self.snapshot.send(snapshot);
    }

    /// 生命週期事件（`DOMContentLoaded`／`load`；loaderId 需對應目前文件）。
    fn on_lifecycle_event(self: &Arc<Self>, name: &str, loader_id: Option<&str>) {
        if name != "DOMContentLoaded" && name != "load" {
            return;
        }
        let mut snapshot = self.snapshot.borrow().clone();
        // 舊文件的殘餘事件（新文件 commit 後才到）不能污染目前狀態。
        if let Some(loader_id) = loader_id
            && (loader_id.is_empty() || loader_id != snapshot.loader_id)
        {
            return;
        }
        if name == "DOMContentLoaded" {
            snapshot.domcontentloaded = true;
        } else {
            snapshot.load = true;
        }
        let _ = self.snapshot.send(snapshot);
    }

    // ===== Network 診斷與攔截（§7.3.6、§7.5）=====

    /// `Network.requestWillBeSent`：建立請求對照；redirect 時先派前一個 response。
    ///
    /// 攔截啟用時等 `Fetch.requestPaused` 配對再決策（§7.3.6）；未啟用時僅登記
    /// （供 response／requestfailed 的診斷）。
    fn on_request_will_be_sent(page: &Arc<PageShared>, params: &Value) {
        let network_id = params
            .get("requestId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let redirect_response = params.get("redirectResponse");
        let request = params.get("request").cloned().unwrap_or(Value::Null);
        {
            let mut network = page
                .network
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if redirect_response.is_some()
                && let Some(previous) = network.requests.remove(&network_id)
            {
                // redirect：前一個請求的 response（§7.3.5）。
                let status = redirect_response
                    .and_then(|response| response.get("status"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let status_text = redirect_response
                    .and_then(|response| response.get("statusText"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                page.diag_listeners.fire(&PageDiagnostic::Response {
                    url: previous.url.clone(),
                    method: previous.method.clone(),
                    resource_type: previous.resource_type.clone(),
                    status: status as u32,
                    status_text,
                    graphql_operation_names: extract_graph_ql_operation_names(
                        previous.post_data.as_deref(),
                    ),
                });
            }
            // 登記（或以 redirect 的新請求覆寫）。
            network.track_request(
                network_id.clone(),
                RequestRecord {
                    url: request
                        .get("url")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    method: request
                        .get("method")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    resource_type: to_resource_type(params.get("type")),
                    post_data: post_data_text(&request),
                },
            );
            // 攔截中：等 requestPaused 配對再決策；data: URL 不攔截（Network Service）。
            let url = request
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if network.kind != InterceptionKind::None && !url.starts_with("data:") {
                if let Some(paused) = network.paused.remove(&network_id) {
                    // requestPaused 先到：現在決策（kind 先取出，避免鎖內重入）。
                    let kind = network.kind;
                    let paused_params = params.clone();
                    drop(network);
                    PageShared::route_paused_request(page, &paused_params, &paused, kind);
                } else {
                    network
                        .awaiting_paused
                        .insert(network_id.clone(), params.clone());
                }
            }
        }
    }

    /// `Network.responseReceived`：派 response 診斷（§7.5）。
    fn on_response_received(page: &Arc<PageShared>, params: &Value) {
        let network_id = params
            .get("requestId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let response = params.get("response").cloned().unwrap_or(Value::Null);
        let record = page
            .network
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .requests
            .get(&network_id)
            .cloned();
        let Some(record) = record else {
            return; // 對照不到的請求不派（照 Playwright：file upload 等特殊情況）
        };
        page.diag_listeners.fire(&PageDiagnostic::Response {
            url: record.url.clone(),
            method: record.method.clone(),
            resource_type: record.resource_type.clone(),
            status: response.get("status").and_then(Value::as_u64).unwrap_or(0) as u32,
            status_text: response
                .get("statusText")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            graphql_operation_names: extract_graph_ql_operation_names(record.post_data.as_deref()),
        });
    }

    /// 目前仍在對照表中的請求數（測試觀測用）。
    #[cfg(test)]
    pub(crate) fn tracked_request_count(&self) -> usize {
        self.network
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .requests
            .len()
    }

    /// `Network.loadingFinished`：請求成功結束，移除對照（不派診斷）。
    fn on_loading_finished(page: &Arc<PageShared>, params: &Value) {
        let Some(network_id) = params.get("requestId").and_then(Value::as_str) else {
            return;
        };
        page.network
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .requests
            .remove(network_id);
    }

    /// `Network.loadingFailed`：派 requestfailed 診斷（§7.5）。
    fn on_loading_failed(page: &Arc<PageShared>, params: &Value) {
        let network_id = params
            .get("requestId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let record = page
            .network
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .requests
            .remove(&network_id);
        let Some(record) = record else {
            return;
        };
        let failure_text = params
            .get("errorText")
            .and_then(Value::as_str)
            .or_else(|| params.get("blockedReason").and_then(Value::as_str))
            .unwrap_or_default();
        let failure_text = if failure_text.is_empty() {
            None
        } else {
            Some(truncate_text(
                &crate::logging::redact::redact_sensitive_string(failure_text),
                MAX_FAILURE_TEXT_LENGTH,
            ))
        };
        page.diag_listeners.fire(&PageDiagnostic::RequestFailed {
            url: record.url.clone(),
            method: record.method.clone(),
            resource_type: record.resource_type.clone(),
            failure_text,
            graphql_operation_names: extract_graph_ql_operation_names(record.post_data.as_deref()),
        });
    }

    /// `Fetch.requestPaused`：等 `Network.requestWillBeSent` 配對後決策
    /// （照 `_onRequestPaused`；沒有 networkId 或 data: 的處理與舊版相同）。
    fn on_request_paused(page: &Arc<PageShared>, params: &Value) {
        let interception_id = params
            .get("requestId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let network_id = params
            .get("networkId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if network_id.is_empty() {
            // Fetch 攔不到 Network 事件的請求：continue 以免影響（照舊版註解）。
            page.send_may_fail(
                "Fetch.continueRequest",
                json!({"requestId": interception_id}),
            );
            return;
        }
        if params
            .get("request")
            .and_then(|request| request.get("url"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .starts_with("data:")
        {
            return; // data: URL 不攔截
        }
        let mut network = page
            .network
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(request_will_be_sent) = network.awaiting_paused.remove(&network_id) {
            // kind 先取出：決策在鎖外執行（`route_paused_request` 不得再鎖）。
            let kind = network.kind;
            drop(network);
            PageShared::route_paused_request(page, &request_will_be_sent, &interception_id, kind);
        } else {
            network.paused.insert(network_id, interception_id);
        }
    }

    /// 攔截決策：`Fetch.fulfillRequest`（暫時頁）或依 `shouldBlockRequest`
    /// `failRequest`／`continueRequest`（資源阻擋）。呼叫端不可持有 network 鎖。
    fn route_paused_request(
        page: &Arc<PageShared>,
        request_will_be_sent: &Value,
        interception_id: &str,
        kind: InterceptionKind,
    ) {
        let request = request_will_be_sent
            .get("request")
            .cloned()
            .unwrap_or(Value::Null);
        let resource_type = to_resource_type(request_will_be_sent.get("type"));
        let url = request
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        match kind {
            InterceptionKind::FulfillDocument => {
                // 暫時頁的導覽攔截：fulfill 回 `<html></html>`（照 Playwright 的
                // route.fulfill({body: '<html></html>'})）。
                let body = base64::engine::general_purpose::STANDARD.encode("<html></html>");
                page.send_may_fail(
                    "Fetch.fulfillRequest",
                    json!({
                        "requestId": interception_id,
                        "responseCode": 200,
                        "responsePhrase": "OK",
                        "responseHeaders": [],
                        "body": body,
                    }),
                );
            }
            InterceptionKind::ResourceBlocking => {
                // 舊版 shouldBlockRequest → abort('blockedbyclient')／continue()。
                if page
                    .context
                    .blocking()
                    .should_block_request(&resource_type, &url)
                {
                    page.send_may_fail(
                        "Fetch.failRequest",
                        json!({"requestId": interception_id, "errorReason": "BlockedByClient"}),
                    );
                } else {
                    page.send_may_fail(
                        "Fetch.continueRequest",
                        json!({"requestId": interception_id}),
                    );
                }
            }
            InterceptionKind::None => {}
        }
    }

    /// `Fetch.authRequired`（照 `crNetworkManager.ts` 的 `_onAuthRequired`；本專案
    /// 沒有 HTTP credentials：首次回 `Default`，重複挑戰回 `CancelAuth`）。
    fn on_auth_required(page: &Arc<PageShared>, params: &Value) {
        let interception_id = params
            .get("requestId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let mut network = page
            .network
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let response = if network.attempted_auth.contains(&interception_id) {
            "CancelAuth"
        } else {
            network.attempted_auth.insert(interception_id.clone());
            "Default"
        };
        page.send_may_fail(
            "Fetch.continueWithAuth",
            json!({
                "requestId": interception_id,
                "authChallengeResponse": {"response": response},
            }),
        );
    }

    // ===== console 診斷（§7.5）=====

    /// `Runtime.consoleAPICalled`：文字以 `renderPreview` 串接（§7.5）。
    fn on_console_api_called(page: &Arc<PageShared>, params: &Value) {
        let args = params
            .get("args")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let text = args
            .iter()
            .map(render_preview)
            .collect::<Vec<_>>()
            .join(" ");
        let location = stack_trace_location(params.get("stackTrace"));
        page.diag_listeners.fire(&PageDiagnostic::Console {
            r#type: params
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            text: truncate_text(
                &crate::logging::redact::redact_sensitive_string(&text),
                MAX_MESSAGE_LENGTH,
            ),
            source_url: optional_source_url(&location.0),
            line_number: location.1,
            column_number: location.2,
        });
    }

    /// `Log.entryAdded`（照 `crPage.ts` 的 `_onLogEntryAdded`：type 為 level、
    /// 欄號為 0；`source === 'worker'` 的條目略過）。
    fn on_log_entry_added(page: &Arc<PageShared>, params: &Value) {
        let entry = params.get("entry").cloned().unwrap_or(Value::Null);
        let source = entry
            .get("source")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if source == "worker" {
            return;
        }
        let location = (
            entry
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            entry.get("lineNumber").and_then(Value::as_i64).unwrap_or(0),
            0,
        );
        page.diag_listeners.fire(&PageDiagnostic::Console {
            r#type: entry
                .get("level")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            text: truncate_text(
                &crate::logging::redact::redact_sensitive_string(
                    entry
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                ),
                MAX_MESSAGE_LENGTH,
            ),
            source_url: optional_source_url(&location.0),
            line_number: location.1,
            column_number: location.2,
        });
    }

    // ===== 嵌套 auto-attach（§7.3.4）=====

    /// 頁面 session 上的嵌套 attach（worker／OOPIF，照 `crPage.ts` 的
    /// `_onAttachedToTarget`）。
    fn on_nested_attached_to_target(page: &Arc<PageShared>, params: &Value) {
        let session_id = params
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let target_info = params.get("targetInfo").cloned().unwrap_or(Value::Null);
        let target_type = target_info
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        match target_type.as_str() {
            "worker" => {
                // worker 的 session 事件泵到頁面；初始化照 crPage：Runtime.enable →
                // Network.enable →（阻擋時 setCacheDisabled，無 Fetch.enable）→
                // Runtime.runIfWaitingForDebugger → Target.setAutoAttach。
                let page_for_task = Arc::clone(page);
                let session_for_task = session_id.clone();
                tokio::spawn(async move {
                    let events = page_for_task
                        .browser
                        .launched
                        .register_session(&session_for_task)
                        .await;
                    init_worker_session(&page_for_task, &session_for_task).await;
                    PageShared::event_pump(page_for_task, events, false).await;
                });
            }
            "iframe" => {
                // OOPIF frame session（照 crPage 的 FrameSession；headless shell
                // 不拆 OOPIF 時不會出現）。
                let page_for_task = Arc::clone(page);
                let session_for_task = session_id.clone();
                tokio::spawn(async move {
                    let events = page_for_task
                        .browser
                        .launched
                        .register_session(&session_for_task)
                        .await;
                    init_frame_session(&page_for_task, &session_for_task).await;
                    PageShared::event_pump(page_for_task, events, false).await;
                });
            }
            // 其餘照 crPage：resume 後 detach（session 已結束）。
            _ => {
                let launched = Arc::clone(&page.browser.launched);
                let session = session_id.clone();
                tokio::spawn(async move {
                    let _ = launched
                        .send_command(Some(&session), "Runtime.runIfWaitingForDebugger", json!({}))
                        .await;
                    let _ = launched
                        .send_command(
                            None,
                            "Target.detachFromTarget",
                            json!({"sessionId": session}),
                        )
                        .await;
                });
            }
        }
    }

    /// 頁面 session 上的嵌套 detach（worker／OOPIF 結束）。
    fn on_nested_detached_from_target(page: &Arc<PageShared>, params: &Value) {
        let session_id = params
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if session_id.is_empty() {
            return;
        }
        let launched = Arc::clone(&page.browser.launched);
        let session = session_id.clone();
        tokio::spawn(async move {
            launched.unregister_session(&session).await;
        });
    }

    // ===== 導覽等待（§7.4 的生命週期語意）=====

    /// 等待「指定 loaderId 的文件」commit 並 fire 指定生命週期事件。
    ///
    /// 照 Playwright `frames.ts` 的 `gotoImpl`：先等這個文件 commit
    /// （`frameNavigated` 的 loaderId 對上），再等生命週期；等待期間若**另一個**
    /// 文件 commit（commit 計數超過等待起點），代表這次導覽被取代，照 Playwright
    /// 回 interrupted 錯誤。生命週期旗標在 commit 時重設，因此只會被**這個**
    /// 文件自己的事件置真——啟始 about:blank 的殘餘旗標不會誤導判斷。
    async fn wait_document_lifecycle(
        &self,
        loader_id: &str,
        wait_until: WaitUntil,
    ) -> Result<(), DriverError> {
        let mut snapshot_rx = self.snapshot.subscribe();
        let baseline = snapshot_rx.borrow().commit_count;
        loop {
            let snapshot = snapshot_rx.borrow().clone();
            if snapshot.closed || snapshot.crashed {
                return Err(self.lifecycle_stop_error());
            }
            if snapshot.loader_id == loader_id {
                if lifecycle_fired(&snapshot, wait_until) {
                    return Ok(());
                }
            } else if snapshot.commit_count > baseline {
                // 另一個文件已 commit 並取代這次導覽（照 Playwright 回錯誤）。
                return Err(DriverError::new(
                    "Error",
                    format!(
                        "Navigation to the given URL is interrupted by another navigation to \"{}\"",
                        snapshot.url
                    ),
                ));
            }
            if snapshot_rx.changed().await.is_err() {
                return Err(DriverError::target_closed());
            }
        }
    }

    /// reload 的等待：等 loader 換掉（新文件）且生命週期已 fire。
    async fn wait_new_document_lifecycle(
        &self,
        previous_loader: &str,
        wait_until: WaitUntil,
    ) -> Result<(), DriverError> {
        let mut snapshot_rx = self.snapshot.subscribe();
        loop {
            let snapshot = snapshot_rx.borrow().clone();
            if snapshot.closed || snapshot.crashed {
                return Err(self.lifecycle_stop_error());
            }
            if snapshot.loader_id != previous_loader && lifecycle_fired(&snapshot, wait_until) {
                return Ok(());
            }
            if snapshot_rx.changed().await.is_err() {
                return Err(DriverError::target_closed());
            }
        }
    }

    /// 等同文件導覽 commit（`navigatedWithinDocument`；計數只增不減，
    /// navigate 回應之後的事件才會被算進等待）。
    async fn wait_same_document_commit(&self) -> Result<(), DriverError> {
        let mut snapshot_rx = self.snapshot.subscribe();
        let baseline = snapshot_rx.borrow().same_document_commits;
        loop {
            let snapshot = snapshot_rx.borrow().clone();
            if snapshot.closed || snapshot.crashed {
                return Err(self.lifecycle_stop_error());
            }
            if snapshot.same_document_commits > baseline {
                return Ok(());
            }
            if snapshot_rx.changed().await.is_err() {
                return Err(DriverError::target_closed());
            }
        }
    }

    /// 導覽等待在頁面停止時的錯誤（crash → `Target crashed `；關閉 → 已關閉訊息）。
    pub(crate) fn lifecycle_stop_error(&self) -> DriverError {
        if self.is_crashed() {
            DriverError::crashed()
        } else {
            DriverError::target_closed()
        }
    }

    // ===== evaluate（storageState 與 S1.5 用）=====

    /// 主框架 main world 的 `Runtime.evaluate`（storageState 的 localStorage 寫入；
    /// S1.5 的 evaluate 沿用）。X7：訊息前綴 `page.evaluate`。
    pub(crate) async fn evaluate(&self, expression: String) -> Result<Value, DriverError> {
        self.evaluate_expression(expression)
            .await
            .map_err(|error| error.with_api("page.evaluate"))
    }

    /// [`PageShared::evaluate`] 的本體（前綴由包裝補上）。
    async fn evaluate_expression(&self, expression: String) -> Result<Value, DriverError> {
        let context_id = self.wait_main_world_context().await?;
        let reply = self
            .send(
                "Runtime.evaluate",
                json!({
                    "expression": expression,
                    "contextId": context_id,
                    "returnByValue": true,
                }),
            )
            .await?;
        if reply.get("exceptionDetails").is_some() {
            return Err(DriverError::new(
                "Error",
                get_exception_message(&reply["exceptionDetails"]),
            ));
        }
        Ok(reply.get("result").cloned().unwrap_or(Value::Null))
    }
    /// `page.evaluate(fn, arg)`（§7.4）：主框架 main world 以 `Runtime.callFunctionOn`
    /// 呼叫函式（照 `crExecutionContext.ts` 的 `evaluateExpression`：函式宣告 +
    /// `arguments: [{value: arg}]`、`returnByValue: true`、`awaitPromise: true`）。
    ///
    /// 例外照 `getExceptionMessage`（`Error: boom`）→ 只保留第一行（X7）；
    /// 協定錯誤（非已關閉、非 crash）照 `rewriteError` 視為 context 被導覽摧毀；
    /// 訊息前綴 `page.evaluate`。
    pub(crate) async fn evaluate_function(
        &self,
        fn_declaration: &str,
        arg: Option<Value>,
    ) -> Result<Value, DriverError> {
        self.evaluate_function_body(fn_declaration, arg)
            .await
            .map_err(|error| error.with_api("page.evaluate"))
    }

    /// [`PageShared::evaluate_function`] 的本體（前綴由包裝補上）。
    async fn evaluate_function_body(
        &self,
        fn_declaration: &str,
        arg: Option<Value>,
    ) -> Result<Value, DriverError> {
        let context_id = self.wait_main_world_context().await?;
        // arg 為 None 時不帶 value 項（照 JSON.stringify 丟棄 undefined 的行為）。
        let arguments = match arg {
            Some(arg) => json!([{"value": arg}]),
            None => json!([]),
        };
        let reply = self
            .send(
                "Runtime.callFunctionOn",
                json!({
                    "functionDeclaration": fn_declaration,
                    "executionContextId": context_id,
                    "arguments": arguments,
                    "returnByValue": true,
                    "awaitPromise": true,
                    "userGesture": true,
                }),
            )
            .await;
        let reply = match reply {
            Ok(reply) => reply,
            Err(error) if error == DriverError::target_closed() => return Err(error),
            // crash 的錯誤照原樣上拋（`Target crashed `）。
            Err(error) if error.is_crashed() => return Err(error),
            // 照 crExecutionContext 的 `rewriteError`：協定錯誤一律視為 context 摧毀。
            Err(_) => {
                return Err(DriverError::new(
                    "Error",
                    "Execution context was destroyed, most likely because of a navigation.",
                ));
            }
        };
        if let Some(details) = reply.get("exceptionDetails") {
            return Err(DriverError::new(
                "Error",
                crate::browser::cdp::injected::first_line_of(&get_exception_message(details)),
            ));
        }
        // `returnByValue: true`：值在 `result.value`（undefined 回傳時無 value 欄）。
        Ok(reply
            .get("result")
            .and_then(|result| result.get("value"))
            .cloned()
            .unwrap_or(Value::Null))
    }

    /// 等主框架 main world 的 execution context（導覽後重建；30 秒防線）。
    pub(crate) async fn wait_main_world_context(&self) -> Result<i64, DriverError> {
        let mut snapshot_rx = self.snapshot.subscribe();
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(context_id) = snapshot_rx.borrow().context_id {
                return Ok(context_id);
            }
            let snapshot = snapshot_rx.borrow().clone();
            if snapshot.closed {
                return Err(DriverError::target_closed());
            }
            if snapshot.crashed {
                return Err(DriverError::crashed());
            }
            match tokio::time::timeout_at(deadline.into(), snapshot_rx.changed()).await {
                Ok(Ok(())) => {}
                _ => {
                    return Err(DriverError::new(
                        "Error",
                        "Execution context was destroyed, most likely because of a navigation.",
                    ));
                }
            }
        }
    }

    // ===== 生命週期（關閉與截圖）=====

    /// `page.goto(url, {waitUntil})`（§7.4）：`Page.navigate` → `errorText` 失敗 →
    /// 等新文件的 DOMContentLoaded／load；同文件導覽交給同程序（等待）。
    ///
    /// 整個流程受導覽逾時限制（照 Playwright 的 progress）；X7：訊息前綴
    /// `page.goto`。
    pub(crate) async fn goto(&self, url: &str, wait_until: WaitUntil) -> Result<(), DriverError> {
        self.goto_body(url, wait_until)
            .await
            .map_err(|error| error.with_api("page.goto"))
    }

    /// [`PageShared::goto`] 的本體（前綴由包裝補上）。
    async fn goto_body(&self, url: &str, wait_until: WaitUntil) -> Result<(), DriverError> {
        let timeout = self.navigation_timeout();
        let navigation = async {
            let frame_id = self.main_frame_id();
            let reply = self
                .send(
                    "Page.navigate",
                    json!({"url": url, "frameId": frame_id, "referrerPolicy": "unsafeUrl"}),
                )
                .await?;
            if let Some(error_text) = reply.get("errorText").and_then(Value::as_str) {
                return Err(DriverError::new("Error", format!("{error_text} at {url}")));
            }
            let loader_id = reply
                .get("loaderId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if loader_id.is_empty() {
                // 同文件導覽：等一個同文件 commit（`navigatedWithinDocument`）。
                return self.wait_same_document_commit().await;
            }
            self.wait_document_lifecycle(&loader_id, wait_until).await
        };
        tokio::time::timeout(timeout, navigation)
            .await
            .map_err(|_| DriverError::timeout(timeout.as_millis()))?
    }

    /// storageState 的頁面導覽：同 `page.goto`，但等到 `load`（照 recordSession
    /// 的預設）。
    pub(crate) async fn goto_for_storage(&self, url: &str) -> Result<(), DriverError> {
        self.goto(url, WaitUntil::Load).await
    }

    /// `page.goto` 的可中止版本（§7.4 的 `signal`）：`abort` 旗標在 select 點
    /// 輪詢（10 ms），設真時回 `AbortError`（name=`AbortError`、
    /// `page.goto: This operation was aborted`，X7 的前綴）。
    pub(crate) async fn goto_abortable(
        &self,
        url: &str,
        wait_until: WaitUntil,
        abort: Option<Arc<AtomicBool>>,
    ) -> Result<(), DriverError> {
        match abort {
            Some(flag) => {
                tokio::select! {
                    result = self.goto(url, wait_until) => result,
                    // 偵測中止：10 ms 輪詢（等待迴圈的輪詢點檢查）。
                    _ = abort_wait(&flag) => Err(DriverError::new(
                        "AbortError",
                        "This operation was aborted",
                    ).with_api("page.goto")),
                }
            }
            None => self.goto(url, wait_until).await,
        }
    }

    /// `page.reload({waitUntil})`（§7.4）：`Page.reload` 之後同 `goto` 等**新文件**
    /// 的生命週期（照 `page.ts` 的 `reload`：waitForNavigation(newDocument) 與
    /// `Page.reload` 並行——watch 訂閱先建立，指令後送，效果相同）。
    /// X7：訊息前綴 `page.reload`。
    pub(crate) async fn reload(&self, wait_until: WaitUntil) -> Result<(), DriverError> {
        self.reload_abortable(wait_until, None).await
    }

    /// `page.reload` 的可中止版本（同 [`PageShared::goto_abortable`]）。
    pub(crate) async fn reload_abortable(
        &self,
        wait_until: WaitUntil,
        abort: Option<Arc<AtomicBool>>,
    ) -> Result<(), DriverError> {
        let run = async {
            let previous_loader = self.snapshot.borrow().loader_id.clone();
            self.send("Page.reload", json!({})).await?;
            self.wait_new_document_lifecycle(&previous_loader, wait_until)
                .await
        };
        let timeout = self.navigation_timeout();
        // 逾時包在裡層、abort 在外層監看；兩臂各自收斂成 `Result<(), DriverError>`
        // （abort 不可被誤判成逾時——X7 的 `AbortError` 與 `TimeoutError` 不同）。
        let result = match abort {
            Some(flag) => {
                tokio::select! {
                    inner = tokio::time::timeout(timeout, run) => match inner {
                        Ok(inner) => inner,
                        Err(_) => Err(DriverError::timeout(timeout.as_millis())),
                    },
                    _ = abort_wait(&flag) => Err(DriverError::new(
                        "AbortError",
                        "This operation was aborted",
                    )),
                }
            }
            None => match tokio::time::timeout(timeout, run).await {
                Ok(inner) => inner,
                Err(_) => Err(DriverError::timeout(timeout.as_millis())),
            },
        };
        result.map_err(|error| error.with_api("page.reload"))
    }

    /// `page.mouse.move(x, y)`（§7.4）：單一 `Input.dispatchMouseEvent
    /// {type: 'mouseMoved'}`（照 crInput 的 move：無按鍵時 mask 0、force 0）。
    pub(crate) async fn mouse_move(&self, x: f64, y: f64) -> Result<(), DriverError> {
        self.send(
            "Input.dispatchMouseEvent",
            json!({
                "type": "mouseMoved",
                "button": "none",
                "buttons": 0,
                "x": x,
                "y": y,
                "modifiers": 0,
                "force": 0,
            }),
        )
        .await
        .map(|_| ())
    }

    /// click 的 `mousePressed`（照 `input.ts` Mouse.down：button left、按下後的
    /// buttons mask 1、clickCount 1、force 0.5）。
    pub(crate) async fn mouse_press(&self, x: f64, y: f64) -> Result<(), DriverError> {
        self.send(
            "Input.dispatchMouseEvent",
            json!({
                "type": "mousePressed",
                "button": "left",
                "buttons": 1,
                "x": x,
                "y": y,
                "modifiers": 0,
                "clickCount": 1,
                "force": 0.5,
            }),
        )
        .await
        .map(|_| ())
    }

    /// click 的 `mouseReleased`（照 `input.ts` Mouse.up：放開後的 buttons mask 0、
    /// clickCount 1；無 force 欄）。
    pub(crate) async fn mouse_release(&self, x: f64, y: f64) -> Result<(), DriverError> {
        self.send(
            "Input.dispatchMouseEvent",
            json!({
                "type": "mouseReleased",
                "button": "left",
                "buttons": 0,
                "x": x,
                "y": y,
                "modifiers": 0,
                "clickCount": 1,
            }),
        )
        .await
        .map(|_| ())
    }

    /// `page.close()`（§7.3.4）：browser session 送 `Target.closeTarget`，等 target
    /// 結束（照 Playwright 的 `closedPromise`；照 `crBrowser._closePage` 一律送出，
    /// crash 的 target 也會以 `detachedFromTarget` 結束）。
    pub(crate) async fn close(self: &Arc<Self>) -> Result<(), DriverError> {
        if self.is_closed() {
            return Ok(());
        }
        let _ = self
            .browser
            .launched
            .send_command(
                None,
                "Target.closeTarget",
                json!({"targetId": self.target_id}),
            )
            .await;
        // 等結束通知（detachedFromTarget → did_close_target）。瀏覽器已結束時
        // 監看任務會標記關閉，這裡不會卡住。
        let mut snapshot_rx = self.snapshot.subscribe();
        while !snapshot_rx.borrow().closed {
            if snapshot_rx.changed().await.is_err() {
                // watch 發送端消失（瀏覽器結束）：監看任務會觸發 did_close_target，
                // 以短暫輪詢兜底。
                tokio::time::sleep(Duration::from_millis(10)).await;
                if self.is_closed() {
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    /// `page.screenshot({type: 'png', fullPage: false})`：viewport 範圍的 PNG
    /// （照 crPage 的 takeScreenshot：以 visualViewport 組 clip，
    /// `captureBeyondViewport: false`）。X7：訊息前綴 `page.screenshot`。
    pub(crate) async fn screenshot(&self) -> Result<Vec<u8>, DriverError> {
        self.screenshot_body()
            .await
            .map_err(|error| error.with_api("page.screenshot"))
    }

    /// [`PageShared::screenshot`] 的本體（前綴由包裝補上）。
    async fn screenshot_body(&self) -> Result<Vec<u8>, DriverError> {
        let metrics = self.send("Page.getLayoutMetrics", json!({})).await?;
        let visual = metrics
            .get("visualViewport")
            .cloned()
            .unwrap_or(Value::Null);
        let page_x = visual.get("pageX").and_then(Value::as_f64).unwrap_or(0.0);
        let page_y = visual.get("pageY").and_then(Value::as_f64).unwrap_or(0.0);
        let scale = visual.get("scale").and_then(Value::as_f64).unwrap_or(1.0);
        let (viewport_width, viewport_height) = self.context.viewport();
        // documentRect：pageX/Y + viewport（viewportRect 原點 0,0），大小除以 scale。
        let document_rect = json!({
            "x": page_x,
            "y": page_y,
            "width": viewport_width as f64 / scale,
            "height": viewport_height as f64 / scale,
        });
        // scale: 'css' → clip.scale /= deviceScaleFactor（本專案固定 1）。
        let mut clip = document_rect.clone();
        clip["scale"] = json!(scale);
        let reply = self
            .send(
                "Page.captureScreenshot",
                json!({
                    "format": "png",
                    "clip": clip,
                    "captureBeyondViewport": false,
                }),
            )
            .await?;
        let data = reply.get("data").and_then(Value::as_str).ok_or_else(|| {
            DriverError::new("Error", "Page.captureScreenshot response is missing 'data'")
        })?;
        base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|error| DriverError::new("Error", format!("Invalid screenshot data: {error}")))
    }
}

/// 中止旗標的等待：設真即返回（10 ms 輪詢；沒有旗標時永不返回）。
async fn abort_wait(flag: &Arc<AtomicBool>) {
    loop {
        if flag.load(Ordering::SeqCst) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// 生命週期旗標是否已 fire。
fn lifecycle_fired(snapshot: &PageSnapshot, wait_until: WaitUntil) -> bool {
    match wait_until {
        WaitUntil::DomContentLoaded => snapshot.domcontentloaded,
        WaitUntil::Load => snapshot.load,
    }
}

/// `stackTrace.callFrames[0]` 的位置（§7.5；沒有時為 0／空）。
fn stack_trace_location(stack_trace: Option<&Value>) -> (String, i64, i64) {
    let frame = stack_trace
        .and_then(|trace| trace.get("callFrames"))
        .and_then(Value::as_array)
        .and_then(|frames| frames.first());
    (
        frame
            .and_then(|frame| frame.get("url"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        frame
            .and_then(|frame| frame.get("lineNumber"))
            .and_then(Value::as_i64)
            .unwrap_or(0),
        frame
            .and_then(|frame| frame.get("columnNumber"))
            .and_then(Value::as_i64)
            .unwrap_or(0),
    )
}

/// console 的 sourceUrl：位置 URL 非空才輸出（§7.5）。
fn optional_source_url(location_url: &str) -> Option<String> {
    if location_url.is_empty() {
        return None;
    }
    let sanitized = join_sanitized_url(&sanitize_browser_url(location_url));
    if sanitized.is_empty() {
        None
    } else {
        Some(sanitized)
    }
}

/// `request.postDataEntries` → UTF-8 文字（§7.5；沒有時 None）。
fn post_data_text(request: &Value) -> Option<String> {
    let entries = request
        .get("postDataEntries")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(|entry| {
            let value = entry.get("bytes").and_then(Value::as_str)?;
            base64::engine::general_purpose::STANDARD
                .decode(value)
                .ok()
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        })
        .collect::<Vec<_>>();
    if entries.is_empty() {
        return None;
    }
    Some(entries.join(""))
}

/// 把值轉成 JS `String(value)` 的形式（console preview 與診斷文字用）。
fn js_stringify(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(inner) => inner.to_string(),
        Value::Number(inner) => {
            if let Some(as_f64) = inner.as_f64() {
                crate::logging::redact::js_number_to_string(as_f64)
            } else {
                inner.to_string()
            }
        }
        Value::String(inner) => inner.clone(),
        Value::Array(_) | Value::Object(_) => "[object Object]".to_string(),
    }
}

/// 照 `crExecutionContext.ts` 的 `renderPreview`（console 文字與 handle 預覽）。
fn render_preview(object: &Value) -> String {
    let object_type = object
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if object_type == "undefined" {
        return "undefined".to_string();
    }
    if let Some(value) = object.get("value") {
        return js_stringify(value);
    }
    if let Some(unserializable) = object.get("unserializableValue").and_then(Value::as_str) {
        // 照 JSHandle 的建構：parse 成功的值照 `String(value)`；沒有對應
        //（如 `10n`）的 unserializableValue 沒有 objectId → String(undefined)。
        return match unserializable {
            "NaN" => "NaN".to_string(),
            "Infinity" => "Infinity".to_string(),
            "-Infinity" => "-Infinity".to_string(),
            "-0" => "0".to_string(),
            _ => "undefined".to_string(),
        };
    }
    if object_type == "object" && object.get("subtype").and_then(Value::as_str) == Some("node") {
        return "JSHandle@node".to_string();
    }
    let description = object
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if description == "Object"
        && let Some(preview) = object.get("preview")
    {
        let tokens = preview
            .get("properties")
            .and_then(Value::as_array)
            .map(|properties| {
                properties
                    .iter()
                    .map(|property| {
                        format!(
                            "{}: {}",
                            property
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or_default(),
                            property.get("value").map(js_stringify).unwrap_or_default()
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        return format!("{{{}}}", tokens.join(", "));
    }
    if object.get("subtype").and_then(Value::as_str) == Some("array")
        && let Some(preview) = object.get("preview")
    {
        return sparse_array_to_string(preview.get("properties").and_then(Value::as_array));
    }
    if !description.is_empty() {
        return description.to_string();
    }
    // objectId 存在但沒有描述：`JSHandle@<subtype 或 type>`。
    format!(
        "JSHandle@{}",
        object
            .get("subtype")
            .or_else(|| object.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("object")
    )
}

/// `javascript.ts` 的 `sparseArrayToString`（稀疏陣列的預覽）。
fn sparse_array_to_string(properties: Option<&Vec<Value>>) -> String {
    let mut array_entries: Vec<(usize, String)> = properties
        .unwrap_or(&Vec::new())
        .iter()
        .filter_map(|property| {
            let name = property.get("name").and_then(Value::as_str)?;
            let index: f64 = name.parse().ok()?;
            if index < 0.0 {
                return None;
            }
            let value = property.get("value").map(js_stringify).unwrap_or_default();
            Some((index as usize, value))
        })
        .collect();
    array_entries.sort_by_key(|(index, _)| *index);
    let mut tokens: Vec<String> = Vec::new();
    let mut last_index: i64 = -1;
    for (index, value) in array_entries {
        let empty_items = index as i64 - last_index - 1;
        if empty_items == 1 {
            tokens.push("empty".to_string());
        } else if empty_items > 1 {
            tokens.push(format!("empty x {empty_items}"));
        }
        tokens.push(value);
        last_index = index as i64;
    }
    format!("[{}]", tokens.join(", "))
}

/// 舊版 `truncateText`。
fn truncate_text(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.to_string();
    }
    value.chars().take(max).collect()
}

/// 舊版 `sanitizeBrowserUrl`：只保留 hostname 與 pathname（port、query、fragment
/// 全丟）；回 `(host, path)`。
pub(crate) fn sanitize_browser_url(raw_url: &str) -> (String, String) {
    if raw_url.is_empty() {
        return (REDACTED_URL.to_string(), String::new());
    }
    // 舊版 parseUrl：絕對 URL 直接解析，否則補 https:// 前綴。
    let candidate = if absolute_url_prefix().is_match(raw_url) {
        raw_url.to_string()
    } else {
        format!("https://{raw_url}")
    };
    match url::Url::parse(&candidate) {
        Ok(parsed) => (
            truncate_text(parsed.host_str().unwrap_or_default(), MAX_HOST_LENGTH),
            truncate_text(parsed.path(), MAX_PATH_LENGTH),
        ),
        Err(_) => (REDACTED_URL.to_string(), String::new()),
    }
}

/// 舊版 parseUrl 的 `^[A-Za-z][A-Za-z0-9+.-]*:\/\//u`。
fn absolute_url_prefix() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        // 固定樣式編譯失敗視為實作錯誤（照專案規範不用 unwrap）。
        match Regex::new(r"^[A-Za-z][A-Za-z0-9+.-]*://") {
            Ok(regex) => regex,
            Err(error) => panic!("invalid built-in URL prefix pattern: {error}"),
        }
    })
}

/// 舊版 `joinSanitizedUrl`。
pub(crate) fn join_sanitized_url(endpoint: &(String, String)) -> String {
    let (host, path) = endpoint;
    if path.is_empty() {
        host.clone()
    } else {
        format!("{host}{path}")
    }
}

/// `Runtime.exceptionDetails` 的訊息（照 `crProtocolHelper.ts` 的 `getExceptionMessage`）。
pub(crate) fn get_exception_message(details: &Value) -> String {
    let exception = details.get("exception");
    if let Some(description) = exception
        .and_then(|exception| exception.get("description"))
        .and_then(Value::as_str)
        .filter(|description| !description.is_empty())
    {
        return description.to_string();
    }
    if let Some(object) = exception {
        if let Some(description) = object.get("description").and_then(Value::as_str) {
            return description.to_string();
        }
        if let Some(value) = object.get("value") {
            if let Some(text) = value.as_str() {
                return text.to_string();
            }
            return value.to_string();
        }
    }
    details
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("Evaluation failed")
        .to_string()
}

/// `requestWillBeSent`／`loadingFailed` 事件的 `type` → 小寫 resourceType
/// （照 `crNetworkManager.ts` 的 `toResourceType`）。
fn to_resource_type(event_type: Option<&Value>) -> String {
    let Some(event_type) = event_type.and_then(Value::as_str) else {
        return "other".to_string();
    };
    match event_type {
        "Document" => "document",
        "Stylesheet" => "stylesheet",
        "Image" => "image",
        "Media" => "media",
        "Font" => "font",
        "Script" => "script",
        "TextTrack" => "texttrack",
        "XHR" => "xhr",
        "Fetch" => "fetch",
        "EventSource" => "eventsource",
        "WebSocket" => "websocket",
        "Manifest" => "manifest",
        "Ping" => "ping",
        "CSPViolationReport" => "cspreport",
        _ => "other",
    }
    .to_string()
}

/// `defaultFontFamilies.ts` 依平台選擇的字型族表（錄製平台為 macOS）。
fn default_font_families(platform: &str) -> Value {
    match platform {
        "linux" => json!({
            "fontFamilies": {
                "standard": "Times New Roman",
                "fixed": "Monospace",
                "serif": "Times New Roman",
                "sansSerif": "Arial",
                "cursive": "Comic Sans MS",
                "fantasy": "Impact",
            }
        }),
        "win" => json!({
            "fontFamilies": {
                "standard": "Times New Roman",
                "fixed": "Consolas",
                "serif": "Times New Roman",
                "sansSerif": "Arial",
                "cursive": "Comic Sans MS",
                "fantasy": "Impact",
            },
            "forScripts": [
                {"script": "cyrl", "fontFamilies": {"standard": "Times New Roman", "fixed": "Courier New", "serif": "Times New Roman", "sansSerif": "Arial"}},
                {"script": "arab", "fontFamilies": {"fixed": "Courier New", "sansSerif": "Segoe UI"}},
                {"script": "grek", "fontFamilies": {"standard": "Times New Roman", "fixed": "Courier New", "serif": "Times New Roman", "sansSerif": "Arial"}},
                {"script": "jpan", "fontFamilies": {"standard": ",Meiryo,Yu Gothic", "fixed": "MS Gothic", "serif": ",Yu Mincho,MS PMincho", "sansSerif": ",Meiryo,Yu Gothic"}},
                {"script": "hang", "fontFamilies": {"standard": "Malgun Gothic", "fixed": "Gulimche", "serif": "Batang", "sansSerif": "Malgun Gothic", "cursive": "Gungsuh"}},
                {"script": "hans", "fontFamilies": {"standard": "Microsoft YaHei", "fixed": "NSimsun", "serif": "Simsun", "sansSerif": "Microsoft YaHei", "cursive": "KaiTi"}},
                {"script": "hant", "fontFamilies": {"standard": "Microsoft JhengHei", "fixed": "MingLiU", "serif": "PMingLiU", "sansSerif": "Microsoft JhengHei", "cursive": "DFKai-SB"}},
            ]
        }),
        // mac（錄製環境）。
        _ => json!({
            "fontFamilies": {
                "standard": "Times",
                "fixed": "Courier",
                "serif": "Times",
                "sansSerif": "Helvetica",
                "cursive": "Apple Chancery",
                "fantasy": "Papyrus",
            },
            "forScripts": [
                {"script": "jpan", "fontFamilies": {"standard": "Hiragino Kaku Gothic ProN", "fixed": "Osaka-Mono", "serif": "Hiragino Mincho ProN", "sansSerif": "Hiragino Kaku Gothic ProN"}},
                {"script": "hang", "fontFamilies": {"standard": "Apple SD Gothic Neo", "serif": "AppleMyungjo", "sansSerif": "Apple SD Gothic Neo"}},
                {"script": "hans", "fontFamilies": {"standard": ",PingFang SC,STHeiti", "serif": "Songti SC", "sansSerif": ",PingFang SC,STHeiti", "cursive": "Kaiti SC"}},
                {"script": "hant", "fontFamilies": {"standard": ",PingFang TC,Heiti TC", "serif": "Songti TC", "sansSerif": ",PingFang TC,Heiti TC", "cursive": "Kaiti TC"}},
            ]
        }),
    }
}

/// worker session 的初始化（照 `crPage.ts`：Runtime.enable → Network.enable →
/// （阻擋時 setCacheDisabled，無 Fetch.enable）→ runIfWaitingForDebugger →
/// Target.setAutoAttach）。
async fn init_worker_session(page: &Arc<PageShared>, session_id: &str) {
    let launched = Arc::clone(&page.browser.launched);
    let session = Some(session_id);
    let _ = launched
        .send_command(session, "Runtime.enable", json!({}))
        .await;
    let _ = launched
        .send_command(session, "Network.enable", json!({}))
        .await;
    if page.context.blocking_any_enabled() {
        let _ = launched
            .send_command(
                session,
                "Network.setCacheDisabled",
                json!({"cacheDisabled": true}),
            )
            .await;
    }
    let _ = launched
        .send_command(session, "Runtime.runIfWaitingForDebugger", json!({}))
        .await;
    let _ = launched
        .send_command(
            session,
            "Target.setAutoAttach",
            json!({"autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true}),
        )
        .await;
}

/// OOPIF frame session 的初始化（照 `crPage.ts` 的 FrameSession；非主框架：
/// 無視窗／焦點模擬，有字型族表、模擬媒體與 init script）。
async fn init_frame_session(page: &Arc<PageShared>, session_id: &str) {
    let launched = Arc::clone(&page.browser.launched);
    let session = Some(session_id);
    let _ = launched
        .send_command(session, "Page.enable", json!({}))
        .await;
    let _ = launched
        .send_command(session, "Page.getFrameTree", json!({}))
        .await;
    let _ = launched
        .send_command(session, "Log.enable", json!({}))
        .await;
    let _ = launched
        .send_command(
            session,
            "Page.setLifecycleEventsEnabled",
            json!({"enabled": true}),
        )
        .await;
    let _ = launched
        .send_command(session, "Runtime.enable", json!({}))
        .await;
    let _ = launched
        .send_command(session, "Network.enable", json!({}))
        .await;
    if page.context.blocking_any_enabled() {
        let _ = launched
            .send_command(
                session,
                "Network.setCacheDisabled",
                json!({"cacheDisabled": true}),
            )
            .await;
        let _ = launched
            .send_command(
                session,
                "Fetch.enable",
                json!({
                    "handleAuthRequests": true,
                    "patterns": [{"urlPattern": "*", "requestStage": "Request"}],
                }),
            )
            .await;
    }
    let _ = launched
        .send_command(
            session,
            "Target.setAutoAttach",
            json!({"autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true}),
        )
        .await;
    let _ = launched
        .send_command(
            session,
            "Page.setFontFamilies",
            json!(default_font_families(launched.platform())),
        )
        .await;
    let _ = launched
        .send_command(
            session,
            "Emulation.setEmulatedMedia",
            json!({
                "media": "",
                "features": [
                    {"name": "prefers-color-scheme", "value": "light"},
                    {"name": "prefers-reduced-motion", "value": "no-preference"},
                    {"name": "forced-colors", "value": "none"},
                    {"name": "prefers-contrast", "value": "no-preference"},
                ],
            }),
        )
        .await;
    for script in page.context.init_scripts() {
        let _ = launched
            .send_command(
                session,
                "Page.addScriptToEvaluateOnNewDocument",
                json!({"source": script.as_str(), "runImmediately": true}),
            )
            .await;
    }
    let _ = launched
        .send_command(session, "Runtime.runIfWaitingForDebugger", json!({}))
        .await;
}

/// 頁面 adapter（對應舊版 `PlaywrightBrowserPageAdapter` 的 page 側）。
pub(crate) struct PageAdapter {
    pub(crate) shared: Arc<PageShared>,
}

#[cfg(test)]
mod network_state_tests {
    use super::{MAX_TRACKED_REQUESTS, NetworkState, RequestRecord};

    fn record(url: &str) -> RequestRecord {
        RequestRecord {
            url: url.to_string(),
            method: "GET".to_string(),
            resource_type: "other".to_string(),
            post_data: None,
        }
    }

    #[test]
    fn tracked_requests_stay_bounded() {
        let mut network = NetworkState::default();
        for index in 0..(MAX_TRACKED_REQUESTS * 3) {
            network.track_request(format!("id-{index}"), record("https://example.com/"));
            assert!(network.requests.len() <= MAX_TRACKED_REQUESTS);
        }
    }

    #[test]
    fn overwriting_existing_request_does_not_clear() {
        let mut network = NetworkState::default();
        for index in 0..MAX_TRACKED_REQUESTS {
            network.track_request(format!("id-{index}"), record("https://example.com/"));
        }
        // redirect 以同一 networkId 覆寫：不算新請求，不觸發清除。
        network.track_request("id-0".to_string(), record("https://example.com/next"));
        assert_eq!(network.requests.len(), MAX_TRACKED_REQUESTS);
        assert_eq!(network.requests["id-0"].url, "https://example.com/next");
    }
}
