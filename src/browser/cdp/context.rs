//! CDP Browser／BrowserContext adapter（§7.3.3）：以 [`LaunchedBrowser`] 為底，
//! 管理 browser context 的建立（`disposeOnDetach`、下載目錄、storageState）、
//! init script（聊天室封鎖）與資源阻擋設定，並消費 attach／detach 通知：
//! 新 `page` target 初始化成 [`PageShared`]（popup 對 opener 觸發 popup listener）、
//! service worker 照 `CRServiceWorker` resume、不感興趣的 target 照
//! `crBrowser.ts` detach。
//!
//! 呼叫流程照錄製結果（§7.3.3）：
//! `Target.createBrowserContext {disposeOnDetach: true}` →
//! `Browser.setDownloadBehavior {behavior: "allowAndName", …}` →
//! `Storage.setCookies`（cookies）→（有 localStorage 時）開暫時頁逐 origin
//! 寫入後 `Target.closeTarget`。`close` 送 `Target.disposeBrowserContext`。
//!
//! 以下 API 尚無正式呼叫端，等對應步驟接上後移除 allow：
//! - `BrowserAdapter::on_disconnected`／`close`（S1.6 的 listener 與 probe 收尾）。
#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};

use super::connection::{CdpError, SessionNotice};
use super::launcher::{CancelListener, LaunchedBrowser};
use super::page::{PageAdapter, PageInit, PageShared};

/// `network.ts` 的 `kMaxCookieExpiresDateInSeconds`（Rollover to 5-digit year）。
const MAX_COOKIE_EXPIRES_DATE_IN_SECONDS: f64 = 253402300799.0;

/// `Target.createTarget` 的 attach 通知等待上限（瀏覽器異常時的防線）。
const PAGE_INIT_WAIT: Duration = Duration::from_secs(30);

/// 驅動層錯誤（X7）：`name` 與 `message`；`message` 的 `<api>: <內容>` 前綴由
/// 呼叫端（Page／Locator API）補上。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DriverError {
    /// 對應 Playwright 的錯誤 `name`（`Error`、`TimeoutError`…）。
    pub(crate) name: String,
    /// 對應 Playwright 的錯誤 `message`（不含 API 前綴的內容）。
    pub(crate) message: String,
}

impl DriverError {
    pub(crate) fn new(name: &str, message: impl Into<String>) -> Self {
        DriverError {
            name: name.to_string(),
            message: message.into(),
        }
    }

    /// 已關閉（session 或連線結束）的錯誤。
    pub(crate) fn target_closed() -> Self {
        DriverError::new("Error", "Target page, context or browser has been closed")
    }

    /// 逾時（X7：`Timeout <ms>ms exceeded.`；`TimeoutError` 名稱）。
    pub(crate) fn timeout(ms: u128) -> Self {
        DriverError::new("TimeoutError", format!("Timeout {ms}ms exceeded."))
    }

    /// 加上 `<api>: ` 前綴（X7：`message` 為 `<api>: <內容>`，只保留第一行——
    /// 內容在產生時已是第一行，這裡只補前綴）。
    pub(crate) fn with_api(mut self, api: &str) -> Self {
        self.message = format!("{api}: {}", self.message);
        self
    }

    /// execution context 被導覽摧毀（frames.ts 重試迴圈視為可重試：換新 context
    /// 後元素可能回來）。
    pub(crate) fn is_context_destroyed(&self) -> bool {
        self.name == "Error"
            && self.message
                == "Execution context was destroyed, most likely because of a navigation."
    }
}

impl std::fmt::Display for DriverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<CdpError> for DriverError {
    fn from(error: CdpError) -> Self {
        match error {
            CdpError::Protocol { message, .. } => DriverError::new("Error", message),
            CdpError::TargetClosed => DriverError::target_closed(),
        }
    }
}

/// 資源阻擋的攔截種類（同一 session 只會有一種；暫時頁的 fulfill 攔截以先
/// 註冊者為準，資源阻擋不重送——照錄製結果 b0／b1 的 page#1 段落相同）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum InterceptionKind {
    /// 未啟用攔截。
    #[default]
    None,
    /// storageState 的導覽攔截：一律 `Fetch.fulfillRequest` 回 `<html></html>`。
    FulfillDocument,
    /// 資源阻擋（`context.route('**/*')`）：依 `shouldBlockRequest` 決定 fail／continue。
    ResourceBlocking,
}

/// 資源阻擋選項（舊版 `configureResourceBlocking` 的三個旗標）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) struct ResourceBlockingOptions {
    pub(crate) block_images: bool,
    pub(crate) block_fonts: bool,
    pub(crate) block_known_tracking: bool,
}

impl ResourceBlockingOptions {
    /// 三個選項都關閉時 `configureResourceBlocking` 不做事（照舊版提前 return）。
    fn any_enabled(&self) -> bool {
        self.block_images || self.block_fonts || self.block_known_tracking
    }

    /// 舊版 `shouldBlockRequest`：`image`／`font` 依旗標；Twitch 統計端點依
    /// `block_known_tracking`。
    pub(crate) fn should_block_request(&self, resource_type: &str, url: &str) -> bool {
        if self.block_images && resource_type == "image" {
            return true;
        }
        if self.block_fonts && resource_type == "font" {
            return true;
        }
        if !self.block_known_tracking {
            return false;
        }
        match host_name_of_url(url) {
            Some(host) => TRACKING_HOSTNAMES.contains(&host.as_str()),
            None => false,
        }
    }
}

/// 舊版 `PlaywrightBrowserContextAdapter` 的 `TRACKING_HOSTNAMES`。
const TRACKING_HOSTNAMES: [&str; 2] = ["spade.twitch.tv", "science.twitch.tv"];

/// 取 URL 的 hostname（解析失敗回 None；照舊版 `shouldBlockRequest` 的 try/catch）。
fn host_name_of_url(url: &str) -> Option<String> {
    url::Url::parse(url).ok()?.host_str().map(str::to_string)
}

/// 瀏覽器共用狀態：connection 把手、context／page 對照表與 pages 變動的 epoch。
pub(crate) struct BrowserShared {
    pub(crate) launched: Arc<LaunchedBrowser>,
    /// 下載暫存目錄（`Browser.setDownloadBehavior` 的 `downloadPath`，X9）。
    artifacts_dir: PathBuf,
    /// browserContextId → context（attach 派送時判斷 target 歸屬）。
    contexts: Mutex<HashMap<String, Weak<ContextShared>>>,
    /// targetId → page（opener 對照、關閉清理與 popup 判定用）。
    pages: Mutex<HashMap<String, Weak<PageShared>>>,
    /// pages 變動的事件鐘：`new_page` 等待 attach 通知時觀察（避免輪詢）。
    page_epoch: watch::Sender<u64>,
    /// 永不讀取的接收端：讓 epoch 的 `send` 在沒有等待者時也會儲存新值
    /// （同 [`PageShared::snapshot_keeper`] 的理由）。
    #[allow(dead_code)]
    page_epoch_keeper: watch::Receiver<u64>,
}

/// 瀏覽器 adapter（取代舊版 `PlaywrightBrowserAdapter` 的瀏覽器層）：
/// 持有連線、消費 attach／detach 通知、建立 context。
pub(crate) struct BrowserAdapter {
    shared: Arc<BrowserShared>,
}

impl BrowserAdapter {
    /// 包裝 launch 成功的瀏覽器：取走 attach／detach 通知並啟動監看任務。
    pub(crate) fn new(mut launched: LaunchedBrowser) -> Arc<Self> {
        let notices = launched.take_notices();
        let artifacts_dir = launched.temp_dir_paths().1.to_path_buf();
        let launched = Arc::new(launched);
        let (page_epoch, page_epoch_keeper) = watch::channel(0u64);
        let shared = Arc::new(BrowserShared {
            launched,
            artifacts_dir,
            contexts: Mutex::new(HashMap::new()),
            pages: Mutex::new(HashMap::new()),
            page_epoch,
            page_epoch_keeper,
        });
        let supervisor = Arc::clone(&shared);
        tokio::spawn(supervision_task(supervisor, notices));
        Arc::new(BrowserAdapter { shared })
    }

    /// 建立新的 browser context（§7.3.3 的錄製順序）。
    ///
    /// `storage_state` 為 storageState 檔路徑；`viewport` 記在 context 上供頁面
    /// 初始化使用。
    pub(crate) async fn new_context(
        &self,
        storage_state: Option<&Path>,
        viewport: (u32, u32),
    ) -> Result<BrowserContextAdapter, DriverError> {
        let launched = &self.shared.launched;
        // 1. `Target.createBrowserContext {disposeOnDetach: true}`。
        let reply = launched
            .send_command(
                None,
                "Target.createBrowserContext",
                json!({"disposeOnDetach": true}),
            )
            .await?;
        let context_id = reply
            .get("browserContextId")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                DriverError::new(
                    "Error",
                    "Target.createBrowserContext response is missing 'browserContextId'",
                )
            })?
            .to_string();

        // 2. `Browser.setDownloadBehavior`（acceptDownloads 預設 accept → allowAndName）。
        launched
            .send_command(
                None,
                "Browser.setDownloadBehavior",
                json!({
                    "behavior": "allowAndName",
                    "browserContextId": context_id,
                    "downloadPath": self.shared.artifacts_dir.display().to_string(),
                    "eventsEnabled": true,
                }),
            )
            .await?;

        let context = Arc::new(ContextShared {
            browser: Arc::clone(&self.shared),
            context_id: context_id.clone(),
            viewport,
            init_scripts: Mutex::new(Vec::new()),
            blocking: Mutex::new(ResourceBlockingOptions::default()),
            creating_storage_state_page: AtomicBool::new(false),
        });
        self.shared
            .contexts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(context_id.clone(), Arc::downgrade(&context));

        // 3. storageState：cookies 與 localStorage。
        if let Some(state_path) = storage_state {
            let state = read_storage_state(state_path)?;
            if let Some(cookies) = state.get("cookies").filter(|cookies| cookies.is_array()) {
                let cookie_params = cookies
                    .as_array()
                    .map(|cookies| {
                        cookies
                            .iter()
                            .map(chromium_cookie_param)
                            .collect::<Result<Vec<_>, _>>()
                    })
                    .transpose()?;
                launched
                    .send_command(
                        None,
                        "Storage.setCookies",
                        json!({"cookies": cookie_params, "browserContextId": context_id}),
                    )
                    .await?;
            }
            // 4. 有 localStorage 的 origin：開暫時頁逐 origin 寫入（照 Playwright 的
            //    setStorageState：fulfill 攔截導覽 → 導向 origin → 寫 localStorage）。
            let origins = state
                .get("origins")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if !origins.is_empty() {
                let page = context.create_target_page(true).await?;
                page.ensure_interception(InterceptionKind::FulfillDocument)
                    .await?;
                let result = restore_origins(&page, &origins).await;
                // 照 Playwright：暫時頁一定關閉（成功與失敗都是）。
                let _ = page.close().await;
                result?;
            }
        }

        Ok(BrowserContextAdapter { shared: context })
    }

    /// 註冊斷線 listener（§7.3.1 的 `on_disconnected`）。
    pub(crate) fn on_disconnected(
        &self,
        listener: impl Fn() + Send + Sync + 'static,
    ) -> CancelListener {
        self.shared.launched.on_disconnected(listener)
    }

    /// 關閉瀏覽器（§7.3.1）。
    pub(crate) async fn close(&self) {
        self.shared.launched.close().await;
    }
}

/// storageState 的 origins 還原（§7.3.3 第 4 步）。
///
/// 每個 origin：`Page.navigate`（被攔截後 fulfill 回 `<html></html>`）→ 等文件
/// commit → `Runtime.evaluate` 寫入 localStorage。照 Playwright 的 `gotoImpl`
/// 預設等到 `load`。
async fn restore_origins(page: &Arc<PageShared>, origins: &[Value]) -> Result<(), DriverError> {
    for origin in origins {
        let origin_url = origin
            .get("origin")
            .and_then(Value::as_str)
            .ok_or_else(|| DriverError::new("Error", "storageState origin is missing 'origin'"))?;
        let items = origin
            .get("localStorage")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        page.goto_for_storage(origin_url).await?;
        let script = format!(
            "(() => {{ try {{ sessionStorage.clear(); }} catch {{}} localStorage.clear(); \
             for (const {{ name, value }} of {}) localStorage.setItem(name, value); }})()",
            serde_json::to_string(&items).unwrap_or_else(|_| "[]".to_string())
        );
        page.evaluate(script).await?;
    }
    Ok(())
}

/// 讀取 storageState 檔（§7.3.3；錯誤訊息照 Playwright client）。
fn read_storage_state(path: &Path) -> Result<Value, DriverError> {
    let text = std::fs::read_to_string(path).map_err(|error| {
        DriverError::new(
            "Error",
            format!(
                "Error reading storage state from {}:\n{}",
                path.display(),
                error
            ),
        )
    })?;
    serde_json::from_str(&text).map_err(|error| {
        DriverError::new(
            "Error",
            format!(
                "Error reading storage state from {}:\n{}",
                path.display(),
                error
            ),
        )
    })
}

/// storageState 檔的 cookie → CDP 的 CookieParam（照 `network.ts` 的
/// `rewriteCookies` 與 `crBrowser.ts` 的 `toChromiumCookie`）。
///
/// `url` 形式換算出 domain／path／secure；`expires: -1` 照原樣傳入；
/// 沒有的欄位照 `JSON.stringify` 的行為省略（undefined 不出現在 wire 上）。
pub(crate) fn chromium_cookie_param(cookie: &Value) -> Result<Value, DriverError> {
    let get = |key: &str| cookie.get(key);
    let url = get("url").and_then(Value::as_str);
    let domain = get("domain").and_then(Value::as_str);
    let path = get("path").and_then(Value::as_str);

    // rewriteCookies 的 assert（訊息照 Playwright 原文）。
    if url.is_none() && (domain.is_none() || path.is_none()) {
        return Err(DriverError::new(
            "Error",
            "Cookie should have a url or a domain/path pair",
        ));
    }
    if url.is_some() && domain.is_some() {
        return Err(DriverError::new(
            "Error",
            "Cookie should have either url or domain",
        ));
    }
    if url.is_some() && path.is_some() {
        return Err(DriverError::new(
            "Error",
            "Cookie should have either url or path",
        ));
    }
    if let Some(expires) = get("expires").and_then(Value::as_f64) {
        if expires != 0.0 && expires < 0.0 && expires != -1.0 {
            return Err(DriverError::new(
                "Error",
                "Cookie should have a valid expires, only -1 or a positive number for the unix timestamp in seconds is allowed",
            ));
        }
        if expires > MAX_COOKIE_EXPIRES_DATE_IN_SECONDS {
            return Err(DriverError::new(
                "Error",
                "Cookie should have a valid expires, only -1 or a positive number for the unix timestamp in seconds is allowed",
            ));
        }
    }

    let mut param = serde_json::Map::new();
    let insert = |param: &mut serde_json::Map<String, Value>, key: &str, value: Option<&Value>| {
        if let Some(value) = value.filter(|value| !value.is_null()) {
            param.insert(key.to_string(), value.clone());
        }
    };
    insert(&mut param, "name", get("name"));
    insert(&mut param, "value", get("value"));
    if let Some(url) = url {
        if url == "about:blank" {
            return Err(DriverError::new(
                "Error",
                format!(
                    "Blank page can not have cookie \"{}\"",
                    str_field(cookie, "name")
                ),
            ));
        }
        if url.starts_with("data:") {
            return Err(DriverError::new(
                "Error",
                format!(
                    "Data URL page can not have cookie \"{}\"",
                    str_field(cookie, "name")
                ),
            ));
        }
        let parsed = url::Url::parse(url).map_err(|error| {
            DriverError::new("Error", format!("Cookie should have a valid url: {error}"))
        })?;
        // copy.domain = url.hostname；copy.path = pathname 到最後一個 `/`（含）；
        // copy.secure = protocol === 'https:'。
        param.insert(
            "domain".to_string(),
            json!(parsed.host_str().unwrap_or_default()),
        );
        let raw_path = parsed.path();
        let cut = raw_path.rfind('/').map(|index| index + 1).unwrap_or(0);
        param.insert("path".to_string(), json!(&raw_path[..cut]));
        param.insert(
            "secure".to_string(),
            json!(parsed.scheme() == "https" || parsed.scheme() == "wss"),
        );
        insert(&mut param, "expires", get("expires"));
        insert(&mut param, "httpOnly", get("httpOnly"));
        insert(&mut param, "sameSite", get("sameSite"));
    } else {
        insert(&mut param, "domain", get("domain"));
        insert(&mut param, "path", get("path"));
        insert(&mut param, "expires", get("expires"));
        insert(&mut param, "httpOnly", get("httpOnly"));
        insert(&mut param, "secure", get("secure"));
        insert(&mut param, "sameSite", get("sameSite"));
    }
    // CHIPS 的 partitionKey：`{topLevelSite, hasCrossSiteAncestor: true}`（照
    // toChromiumCookie 的預設）。
    if let Some(top_level_site) = get("partitionKey").and_then(Value::as_str) {
        let has_cross_site_ancestor = cookie
            .get("_crHasCrossSiteAncestor")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        param.insert(
            "partitionKey".to_string(),
            json!({"topLevelSite": top_level_site, "hasCrossSiteAncestor": has_cross_site_ancestor}),
        );
    }
    Ok(Value::Object(param))
}

/// 取 cookie 的字串欄位（錯誤訊息組字用）。
fn str_field(cookie: &Value, key: &str) -> String {
    cookie
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// browser context adapter（取代舊版 `PlaywrightBrowserContextAdapter`）。
pub(crate) struct BrowserContextAdapter {
    shared: Arc<ContextShared>,
}

pub(crate) struct ContextShared {
    browser: Arc<BrowserShared>,
    pub(crate) context_id: String,
    /// 之後建立頁面時套用的 viewport（寬、高）。
    viewport: (u32, u32),
    /// init scripts（`configureChatBlocking` 加入；`addInitScript` 的包裝在送出時做）。
    init_scripts: Mutex<Vec<Arc<String>>>,
    /// 資源阻擋選項（全關表示未啟用）。
    blocking: Mutex<ResourceBlockingOptions>,
    /// 進行中的暫時頁建立（`setStorageState` 用）：`create_target_page(true)` 在送出
    /// `Target.createTarget` 前設立，attach 派送時取走決定 `PageInit`。照 Playwright 的
    /// `_creatingStorageStatePage`：旗標在 attach 前就緒（Chromium 先送 attach 再回
    /// createTarget，錄製確認），之後取走時以 swap 归零。
    creating_storage_state_page: AtomicBool,
}

impl ContextShared {
    pub(crate) fn viewport(&self) -> (u32, u32) {
        self.viewport
    }

    /// 取走「正在建立暫時頁」旗標（attach 派送時用；swap 歸零）。
    fn take_creating_storage_state_page(&self) -> bool {
        self.creating_storage_state_page
            .swap(false, Ordering::SeqCst)
    }

    /// 資源阻擋選項（任一開啟時啟用攔截）。
    pub(crate) fn blocking(&self) -> ResourceBlockingOptions {
        *self
            .blocking
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(crate) fn blocking_any_enabled(&self) -> bool {
        let blocking = self.blocking();
        blocking.block_images || blocking.block_fonts || blocking.block_known_tracking
    }

    /// 目前的 init scripts（頁面初始化時逐一套用）。
    pub(crate) fn init_scripts(&self) -> Vec<Arc<String>> {
        self.init_scripts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// `Target.createTarget {url: "about:blank"}` 並等 auto-attach 的初始化完成。
    ///
    /// `storage_state_page` 為 true 時（`setStorageState` 的暫時頁），在送出
    /// createTarget 前先在 context 上設旗標，attach 派送（可能早於 createTarget
    /// 回應）才會以 `PageInit { is_storage_state_page: true }` 初始化。
    pub(crate) async fn create_target_page(
        &self,
        storage_state_page: bool,
    ) -> Result<Arc<PageShared>, DriverError> {
        if storage_state_page {
            self.creating_storage_state_page
                .store(true, Ordering::SeqCst);
        }
        let launched = &self.browser.launched;
        let mut epoch = self.browser.page_epoch.subscribe();
        let reply = launched
            .send_command(
                None,
                "Target.createTarget",
                json!({"url": "about:blank", "browserContextId": self.context_id}),
            )
            .await?;
        let target_id = reply
            .get("targetId")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                DriverError::new(
                    "Error",
                    "Target.createTarget response is missing 'targetId'",
                )
            })?
            .to_string();
        let deadline = tokio::time::Instant::now() + PAGE_INIT_WAIT;
        loop {
            let page = self
                .browser
                .pages
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(&target_id)
                .and_then(|weak| weak.upgrade());
            if let Some(page) = page {
                page.wait_initialized().await?;
                // 照 Playwright：初始化完成後才發現已關閉的頁面回錯誤。
                if page.is_closed() {
                    return Err(DriverError::new("Error", "Page has been closed."));
                }
                return Ok(page);
            }
            if tokio::time::timeout_at(deadline, epoch.changed())
                .await
                .is_err()
            {
                return Err(DriverError::new(
                    "TimeoutError",
                    format!("Timeout {}ms exceeded.", PAGE_INIT_WAIT.as_millis()),
                ));
            }
        }
    }
}

impl BrowserContextAdapter {
    /// 資源阻擋（§7.3.6）：三個選項都關閉時不做任何事（照舊版提前 return）。
    ///
    /// 開啟時：對 context 內所有已初始化的頁面／popup 補送攔截（照 Playwright 的
    /// `doUpdateRequestInterception`），之後建立的頁面在初始化時套用。
    pub(crate) async fn configure_resource_blocking(
        &self,
        options: ResourceBlockingOptions,
    ) -> Result<(), DriverError> {
        if !(options.block_images || options.block_fonts || options.block_known_tracking) {
            return Ok(());
        }
        *self
            .shared
            .blocking
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = options;
        // 對既有頁面補送；暫時頁（已關閉）不在 pages 之列。
        for page in initialized_pages(&self.shared) {
            page.ensure_interception(InterceptionKind::ResourceBlocking)
                .await?;
        }
        Ok(())
    }

    /// 聊天室封鎖（§7.3.6）：把 init script 加入 context（照舊版
    /// `context.addInitScript(buildChatBlockInitScript())`）。
    ///
    /// 之後建立的每個頁面、popup、OOPIF 在初始化時套用；已存在的頁面照
    /// Playwright 的 `doAddInitScript` 補送。
    pub(crate) async fn configure_chat_blocking(&self, enabled: bool) -> Result<(), DriverError> {
        if !enabled {
            return Ok(());
        }
        let source = Arc::new(wrapped_init_script_source());
        self.shared
            .init_scripts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(Arc::clone(&source));
        // 對既有（已初始化）頁面補送；暫時頁在流程裡已關閉，通常沒有頁面。
        for page in initialized_pages(&self.shared) {
            page.add_init_script_to_session(&source).await?;
        }
        Ok(())
    }

    /// 建立頁面（§7.3.4）：`Target.createTarget {browserContextId, url: "about:blank"}`
    /// 並等 auto-attach 的初始化完成才回傳。
    pub(crate) async fn new_page(&self) -> Result<PageAdapter, DriverError> {
        let page = self.shared.create_target_page(false).await?;
        Ok(PageAdapter { shared: page })
    }

    /// 關閉 context（§7.3.3）：`Target.disposeBrowserContext`。
    pub(crate) async fn close(&self) -> Result<(), DriverError> {
        self.shared
            .browser
            .launched
            .send_command(
                None,
                "Target.disposeBrowserContext",
                json!({"browserContextId": self.shared.context_id}),
            )
            .await?;
        self.shared
            .browser
            .contexts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&self.shared.context_id);
        Ok(())
    }
}

/// 目前已初始化、非暫時頁的頁面（照 `pages()`）。
fn initialized_pages(context: &Arc<ContextShared>) -> Vec<Arc<PageShared>> {
    let pages = context
        .browser
        .pages
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    pages
        .values()
        .filter_map(Weak::upgrade)
        .filter(|page| page.is_initialized() && !page.is_storage_state_page())
        .collect()
}

/// init script 的最終 source：照 Playwright `InitScript` 的包裝
/// `(() => {\n      <source>\n    })();`，內容為剝除註解行後的聊天室封鎖腳本
/// （DEC-004；G2 的 `source` 欄位以 sha256 遮蔽比對，必須逐位元組相同）。
pub(crate) fn wrapped_init_script_source() -> String {
    format!("(() => {{\n      {}\n    }})();", chat_block_script())
}

/// `include_str!` 載入 `chat-block-init.js` 後剝除開頭連續的 `//` 註解行，
/// 再 trim 前後空白（DEC-004）。
pub(crate) fn chat_block_script() -> &'static str {
    static SCRIPT: OnceLock<String> = OnceLock::new();
    SCRIPT.get_or_init(|| {
        let raw = include_str!("../../browser/js/chat-block-init.js");
        let mut header_lines = 0usize;
        for line in raw.split('\n') {
            if line.trim_start().starts_with("//") {
                header_lines += 1;
                continue;
            }
            break;
        }
        let lines: Vec<&str> = raw.split('\n').collect();
        lines[header_lines..].join("\n").trim().to_string()
    })
}

/// attach／detach 監看任務：把通知派送給對應的 context／page（§7.3.4）。
async fn supervision_task(shared: Arc<BrowserShared>, mut notices: mpsc::Receiver<SessionNotice>) {
    while let Some(notice) = notices.recv().await {
        match notice {
            SessionNotice::Attached {
                session_id,
                events,
                params,
            } => {
                handle_attached(&shared, &session_id, events, &params).await;
            }
            SessionNotice::Detached {
                session_id,
                target_id,
            } => {
                // 頁面 target 結束（§7.3.5 的 on_close 來源之一）：pages 對照表以
                // targetId 為鍵；targetId 缺席時退回 sessionId（同 target 的 attach
                // 也以 targetId 登記，理論上不會走到 fallback）。
                let page = {
                    let mut pages = shared
                        .pages
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    if let Some(target_id) = target_id.as_deref() {
                        pages.remove(target_id)
                    } else {
                        pages.remove(&session_id)
                    }
                    .and_then(|weak| weak.upgrade())
                };
                if let Some(page) = page {
                    page.did_close_target();
                    bump_epoch(&shared);
                }
            }
        }
    }
    // 通知流結束（pipe EOF／連線收尾）：所有頁面標記關閉（§7.3.1 斷線語意）。
    let pages: Vec<Arc<PageShared>> = shared
        .pages
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .drain()
        .filter_map(|(_, weak)| weak.upgrade())
        .collect();
    for page in pages {
        page.did_close_target();
    }
}

/// 頁面對照表變動：廣播 epoch（等待者觀察）。
fn bump_epoch(shared: &Arc<BrowserShared>) {
    let current = *shared.page_epoch.borrow();
    let _ = shared.page_epoch.send(current + 1);
}

/// 處理 browser session 上的 attach（照 `crBrowser.ts` 的 `_onAttachedToTarget`）。
async fn handle_attached(
    shared: &Arc<BrowserShared>,
    session_id: &str,
    events: mpsc::Receiver<super::connection::Event>,
    params: &Value,
) {
    let target_info = params.get("targetInfo").cloned().unwrap_or(Value::Null);
    let target_type = target_info
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    match target_type.as_str() {
        // 照 crBrowser：type 'browser' 不理會（不建立 session，事件接收端直接釋放）。
        "browser" => drop(events),
        "page" => {
            let context_id = target_info
                .get("browserContextId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let context = shared
                .contexts
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(&context_id)
                .and_then(|weak| weak.upgrade());
            let Some(context) = context else {
                // 不屬於任何 context：照 crBrowser detach（先 resume 再 detach）。
                detach_browser_target(shared, session_id).await;
                drop(events);
                return;
            };
            let target_id = target_info
                .get("targetId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            // opener：`Target.attachedToTarget` 的 openerId 指向既有頁面（popup）。
            let opener = target_info
                .get("openerId")
                .and_then(Value::as_str)
                .and_then(|opener_id| {
                    shared
                        .pages
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .get(opener_id)
                        .and_then(|weak| weak.upgrade())
                });
            // 暫時頁旗標：`create_target_page(true)` 在 createTarget 前設立。
            let is_storage_state_page = context.take_creating_storage_state_page();
            spawn_page(
                shared,
                context,
                target_id,
                session_id.to_string(),
                opener,
                PageInit {
                    is_storage_state_page,
                },
                events,
            );
        }
        "service_worker" => {
            // 照 CRServiceWorker（網路檢查預設關閉）：resume 即可，事件接收端釋放。
            let launched = &shared.launched;
            let _ = launched
                .send_command(Some(session_id), "Runtime.enable", json!({}))
                .await;
            let _ = launched
                .send_command(
                    Some(session_id),
                    "Runtime.runIfWaitingForDebugger",
                    json!({}),
                )
                .await;
            drop(events);
        }
        // 照 crBrowser：不感興趣的 target 一律 detach（先 resume，避免 worker 卡住）。
        _ => {
            detach_browser_target(shared, session_id).await;
            drop(events);
        }
    }
}

/// 對（不處理的）子 target 送 resume 與 detach（照 `crConnection.ts` 的 `detach`）。
async fn detach_browser_target(shared: &Arc<BrowserShared>, session_id: &str) {
    let launched = &shared.launched;
    // 順序照 crConnection.ts：先 runIfWaitingForDebugger 再 detach（Chromium bug 註解）。
    let _ = launched
        .send_command(
            Some(session_id),
            "Runtime.runIfWaitingForDebugger",
            json!({}),
        )
        .await;
    let _ = launched
        .send_command(
            None,
            "Target.detachFromTarget",
            json!({"sessionId": session_id}),
        )
        .await;
}

/// 建立頁面共用狀態並啟動初始化任務與事件泵；回傳頁面把手。
///
/// 初始化完成時由 `mark_initialized` resolve 等待者／觸發 opener 的 popup listener。
fn spawn_page(
    shared: &Arc<BrowserShared>,
    context: Arc<ContextShared>,
    target_id: String,
    session_id: String,
    opener: Option<Arc<PageShared>>,
    init: PageInit,
    events: mpsc::Receiver<super::connection::Event>,
) -> Arc<PageShared> {
    let page = Arc::new(PageShared::new(
        Arc::clone(shared),
        context,
        target_id.clone(),
        session_id,
        opener.map(|page| Arc::downgrade(&page)),
        init,
    ));
    shared
        .pages
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(target_id.clone(), Arc::downgrade(&page));

    // 初始化任務：依錄製順序送出頁面 session 的指令，完成後 resolve 等待者並
    // 對 opener 觸發 popup listener（§7.3.4）。
    let init_page = Arc::clone(&page);
    tokio::spawn(async move {
        let result = PageShared::run_init(Arc::clone(&init_page)).await;
        init_page.mark_initialized(result);
    });

    // 事件泵：消費頁面 session 的事件，更新狀態與派送 listener（§7.3.5）。
    tokio::spawn(PageShared::event_pump(Arc::clone(&page), events, true));
    bump_epoch(shared);
    page
}
