//! 瀏覽器抽象（D12）：trait 對應舊版 `src/browser/types.ts` 的介面，逐項照形狀。
//!
//! 舊版依賴 Playwright 的 `Page` 型別；Rust 版以 [`BrowserPage`] trait 抽象頁面
//! 把手（只含本層用到的 API：`isClosed`／`close`，D12 的「`Page` trait 只含舊版
//! 用到的 Playwright API」原則；之後的頁面操作模組可經由
//! [`BrowserPage::as_page_shared`] 取得 cdp 把手）。錯誤一律用 cdp 層的
//! [`DriverError`]（`name`＋`message`，對應舊版 `Error`）。
//!
//! trait 的正式實作（impl 區塊集中在本檔，不動 `cdp/*` 與 `page_adapter.rs`）：
//! - [`BrowserPage`] ← [`CdpPage`]（包 cdp `PageShared`）
//! - [`BrowserPageAdapter`] ← [`CdpPageAdapter`]（生命週期 listener 來自
//!   `PageShared`、診斷 listener 來自 S1.6 的 [`PlaywrightBrowserPageAdapter`]）
//! - [`BrowserContextAdapter`] ← cdp `BrowserContextAdapter`
//! - [`BrowserAdapter`] ← [`CdpBrowserAdapter`]（包 cdp `BrowserAdapter`；cdp 型別
//!   未直接暴露 `isConnected`／`getVersion`，由本層以 listener 自記連線狀態並在
//!   建構時快取版本）
//! - [`BrowserLauncher`] ← cdp `BrowserLauncher`（headless 由錄製參數固定，D1）
//! - [`BrowserManagerLogger`] ← [`JsonLineLogger`]
//! - [`SleepFn`] ← [`TokioSleep`]
//!
//! 舊版的 `BrowserManager` 介面與 `DefaultBrowserManager` 實作在
//! [`super::manager`]。以下 API 的正式呼叫端在後續步驟（S3.4–S3.6）接上，故
//! 標記 allow(dead_code)。
#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;

use super::cdp::context::{
    BrowserAdapter as CdpBrowser, BrowserContextAdapter as CdpContext, DriverError,
    ResourceBlockingOptions,
};
use super::cdp::launcher::{BrowserLauncher as CdpLauncher, CancelListener};
use super::cdp::page::PageShared;
use super::page_adapter::{
    BrowserConsoleDiagnostic, BrowserHttpResponseDiagnostic, BrowserRequestFailureDiagnostic,
    DiagnosticEventSource, PlaywrightBrowserPageAdapter,
};
use crate::logging::logger::JsonLineLogger;
use crate::logging::redact::SanitizedValue;

/// 舊版 `BrowserLaunchOptions`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BrowserLaunchOptions {
    pub(crate) headless: bool,
}

/// 舊版 `BrowserContextOptions.viewport`。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct BrowserViewport {
    pub(crate) width: f64,
    pub(crate) height: f64,
}

/// 舊版 `BrowserContextOptions`。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct BrowserContextOptions {
    pub(crate) storage_state: String,
    pub(crate) viewport: BrowserViewport,
}

/// 舊版 Playwright `Page` 把手的抽象（D12）：正式碼用 [`CdpPage`]，測試用 fake。
///
/// manager 需要的頁面操作只有 popup 的 `isClosed`／`close`；其他頁面 API 由
/// 呼叫端經 [`BrowserPage::as_page_shared`] 使用 cdp 層。
#[async_trait]
pub(crate) trait BrowserPage: Send + Sync {
    fn is_closed(&self) -> bool;

    /// 舊版 `page.close()`。
    async fn close(&self) -> Result<(), DriverError>;

    /// cdp 層的頁面把手（正式碼有值；fake 沒有對應實體，回 `None`）。
    fn as_page_shared(&self) -> Option<Arc<PageShared>>;
}

/// 舊版 `BrowserPageAdapter`：頁面＋生命週期與診斷 listener。
#[async_trait]
pub(crate) trait BrowserPageAdapter: Send + Sync {
    /// 舊版 `page` 屬性：頁面把手（同一 adapter 恆回同一把手，識別相等）。
    fn page(&self) -> Arc<dyn BrowserPage>;

    /// 舊版 `close()`：可能失敗（呼叫端以 [`CloseOutcome`] 處理）。
    async fn close(&self) -> Result<(), DriverError>;

    /// 舊版 `isClosed()`。
    fn is_closed(&self) -> bool;

    /// 舊版 `onCrash(listener)`，回傳取消函式。
    fn on_crash(&self, listener: Box<dyn Fn() + Send + Sync>) -> CancelListener;

    /// 舊版 `onClose(listener)`，回傳取消函式。
    fn on_close(&self, listener: Box<dyn Fn() + Send + Sync>) -> CancelListener;

    /// 舊版 `onPopup(listener)`，回傳取消函式。
    fn on_popup(&self, listener: Box<dyn Fn(Arc<dyn BrowserPage>) + Send + Sync>)
    -> CancelListener;

    /// 舊版 `onRequestFailed(listener)`，回傳取消函式。
    fn on_request_failed(
        &self,
        listener: Box<dyn Fn(&BrowserRequestFailureDiagnostic) + Send + Sync>,
    ) -> CancelListener;

    /// 舊版 `onResponse(listener)`，回傳取消函式。
    fn on_response(
        &self,
        listener: Box<dyn Fn(&BrowserHttpResponseDiagnostic) + Send + Sync>,
    ) -> CancelListener;

    /// 舊版 `onConsole(listener)`，回傳取消函式。
    fn on_console(
        &self,
        listener: Box<dyn Fn(&BrowserConsoleDiagnostic) + Send + Sync>,
    ) -> CancelListener;
}

/// 舊版 `BrowserContextAdapter`。
#[async_trait]
pub(crate) trait BrowserContextAdapter: Send + Sync {
    /// 舊版 `configureResourceBlocking(options)`。
    async fn configure_resource_blocking(
        &self,
        options: ResourceBlockingOptions,
    ) -> Result<(), DriverError>;

    /// 舊版 `configureChatBlocking(enabled)`。
    async fn configure_chat_blocking(&self, enabled: bool) -> Result<(), DriverError>;

    /// 舊版 `newPage()`。
    async fn new_page(&self) -> Result<Arc<dyn BrowserPageAdapter>, DriverError>;

    /// 舊版 `close()`。
    async fn close(&self) -> Result<(), DriverError>;
}

/// 舊版 `BrowserAdapter`。
#[async_trait]
pub(crate) trait BrowserAdapter: Send + Sync {
    /// 舊版 `newContext(options)`。
    async fn new_context(
        &self,
        options: BrowserContextOptions,
    ) -> Result<Arc<dyn BrowserContextAdapter>, DriverError>;

    /// 舊版 `close()`。
    async fn close(&self) -> Result<(), DriverError>;

    /// 舊版 `onDisconnected(listener)`，回傳取消函式。
    fn on_disconnected(&self, listener: Box<dyn Fn() + Send + Sync>) -> CancelListener;

    /// 舊版 `isConnected()`：與瀏覽器程序的連線是否還活著。
    fn is_connected(&self) -> bool;

    /// 舊版 `getVersion()`：安全的瀏覽器版本字串。
    fn get_version(&self) -> String;
}

/// 舊版 `BrowserLauncher`。
#[async_trait]
pub(crate) trait BrowserLauncher: Send + Sync {
    /// 舊版 `launch(options)`。
    async fn launch(
        &self,
        options: BrowserLaunchOptions,
    ) -> Result<Arc<dyn BrowserAdapter>, DriverError>;
}

/// 舊版 `CloseOutcome`：一次定時資源關閉的結果；逾時永遠不算成功。
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum CloseOutcome {
    Closed,
    AlreadyClosed,
    Failed { error: DriverError },
    TimedOut,
}

impl CloseOutcome {
    /// 舊版 `status` 的字串（日誌欄位用）。
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            CloseOutcome::Closed => "closed",
            CloseOutcome::AlreadyClosed => "already_closed",
            CloseOutcome::Failed { .. } => "failed",
            CloseOutcome::TimedOut => "timed_out",
        }
    }
}

/// 舊版 `BrowserTeardownResult`。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct BrowserTeardownResult {
    pub(crate) browser_terminated: bool,
    pub(crate) page_close_timed_out: bool,
    pub(crate) browser_close_timed_out: bool,
    pub(crate) browser_close_failed: bool,
}

/// 舊版 `BrowserNavigationOutcome` 的 `status`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BrowserNavigationStatus {
    Succeeded,
    TimedOut,
}

impl BrowserNavigationStatus {
    /// 舊版 union 的字面值（日誌欄位用）。
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            BrowserNavigationStatus::Succeeded => "succeeded",
            BrowserNavigationStatus::TimedOut => "timed_out",
        }
    }
}

/// 舊版 `BrowserNavigationOutcome`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BrowserNavigationOutcome {
    pub(crate) channel: String,
    pub(crate) status: BrowserNavigationStatus,
}

/// 舊版 `BrowserInvalidationReason`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BrowserInvalidationReason {
    PageCrashed,
    PageClosed,
    BrowserDisconnected,
    BrowserRestarted,
}

impl BrowserInvalidationReason {
    /// 舊版 union 的字面值（事件名稱與日誌欄位用）。
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            BrowserInvalidationReason::PageCrashed => "page_crashed",
            BrowserInvalidationReason::PageClosed => "page_closed",
            BrowserInvalidationReason::BrowserDisconnected => "browser_disconnected",
            BrowserInvalidationReason::BrowserRestarted => "browser_restarted",
        }
    }
}

/// 舊版 `BrowserInvalidation`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BrowserInvalidation {
    pub(crate) channel: String,
    pub(crate) reason: BrowserInvalidationReason,
}

/// 舊版 `BrowserInvalidationObserver`。
///
/// 舊版觀察者可為 async 或 sync、可拋錯；Rust 以 `Err` 表達拋錯（呼叫端
/// 捕捉後記 `browser_invalidation_observer_failed`），行為等價於舊版的
/// try/catch。
#[async_trait]
pub(crate) trait BrowserInvalidationObserver: Send + Sync {
    async fn on_invalidated(&self, invalidation: BrowserInvalidation) -> Result<(), DriverError>;
}

/// 舊版 `BrowserRestartedEvent.mode`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BrowserRestartedMode {
    Automatic,
    Manual,
}

impl BrowserRestartedMode {
    /// 舊版 union 的字面值（日誌欄位用）。
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            BrowserRestartedMode::Automatic => "automatic",
            BrowserRestartedMode::Manual => "manual",
        }
    }
}

/// 舊版 `BrowserRestartedEvent`：成功重啟／回收（手動或自動）後觸發。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BrowserRestartedEvent {
    pub(crate) mode: BrowserRestartedMode,
}

/// 舊版 `BrowserRestartedObserver`（同步觀察者）。
pub(crate) trait BrowserRestartedObserver: Send + Sync {
    fn on_browser_restarted(&self, event: BrowserRestartedEvent) -> Result<(), DriverError>;
}

/// 舊版 `BrowserFatalRecoveryObserver` 的參數（`{reason, fields?}`）。
///
/// 復原耗盡或無法證明終止時呼叫；`fields` 為 `None` 表示舊版省略欄位。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FatalRecoveryRequest {
    pub(crate) reason: String,
    pub(crate) fields: Option<SanitizedValue>,
}

/// 舊版 `BrowserFatalRecoveryObserver`（同 invalidation observer：以 `Err` 表達拋錯）。
#[async_trait]
pub(crate) trait BrowserFatalRecoveryObserver: Send + Sync {
    async fn on_fatal_recovery(&self, request: FatalRecoveryRequest) -> Result<(), DriverError>;
}

/// 舊版 `BrowserManagerLogger`（`Pick<Logger, 'debug'|'info'|'warn'|'error'>`）。
///
/// 欄位值為消毒前的 [`SanitizedValue`]（正式 logger 內部會再消毒）。
pub(crate) trait BrowserManagerLogger: Send + Sync {
    fn debug(&self, event: &str, fields: Option<&SanitizedValue>);
    fn info(&self, event: &str, fields: Option<&SanitizedValue>);
    fn warn(&self, event: &str, fields: Option<&SanitizedValue>);
    fn error(&self, event: &str, fields: Option<&SanitizedValue>);
}

/// 正式 logger 的轉接（同 loader 的 `ConfigLogger` 前例）。
impl BrowserManagerLogger for JsonLineLogger {
    fn debug(&self, event: &str, fields: Option<&SanitizedValue>) {
        // inherent 的 debug 同簽名；以完整路徑呼叫避免誤呼本 impl 造成遞迴。
        JsonLineLogger::debug(self, event, fields);
    }

    fn info(&self, event: &str, fields: Option<&SanitizedValue>) {
        JsonLineLogger::info(self, event, fields);
    }

    fn warn(&self, event: &str, fields: Option<&SanitizedValue>) {
        JsonLineLogger::warn(self, event, fields);
    }

    fn error(&self, event: &str, fields: Option<&SanitizedValue>) {
        JsonLineLogger::error(self, event, fields);
    }
}

/// 舊版 `BrowserManagerDependencies.sleep`：`sleep?: (milliseconds) => Promise<void>`。
///
/// 舊版注入的 sleep 可能 reject（`browser_restart_backoff_failed` 分支）；
/// Rust 版不會失敗，該分支不可達（見 manager 的註解）。
#[async_trait]
pub(crate) trait SleepFn: Send + Sync {
    async fn sleep(&self, milliseconds: u64);
}

/// 正式的 sleep：tokio 計時器（對應舊版 setTimeout 的預設）。
pub(crate) struct TokioSleep;

#[async_trait]
impl SleepFn for TokioSleep {
    async fn sleep(&self, milliseconds: u64) {
        tokio::time::sleep(Duration::from_millis(milliseconds)).await;
    }
}

/// 舊版 `BrowserManagerDependencies.now`：`now?: () => number`，epoch 毫秒
/// （型別形狀同 sessions 的 `RecoveryClock` 前例）。
pub(crate) type ManagerClock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// 舊版 `BrowserManagerConfig`（`Pick<AppConfig, …> & {browser: Pick<…>}`）。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct BrowserManagerConfig {
    pub(crate) headless: bool,
    pub(crate) storage_state_path: String,
    pub(crate) browser: BrowserManagerBrowserConfig,
}

/// 舊版 `BrowserManagerConfig` 內嵌的 `browser.recovery`（`BrowserRecoveryConfig`
/// 的欄位形狀；與 config 模組的同名欄位一一對應）。bin crate（probe）不帶 config
/// 模組，故 types.rs 以自有定義承載相同形狀；組裝端（S3.6）由正式設定轉換。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct BrowserManagerRecoveryConfig {
    pub(crate) page_crash_backoff_seconds: Vec<f64>,
    pub(crate) channel_crash_window_seconds: f64,
    pub(crate) channel_quarantine_threshold: f64,
    pub(crate) channel_quarantine_seconds: f64,
    pub(crate) stable_reset_seconds: f64,
    pub(crate) multi_channel_crash_window_seconds: f64,
    pub(crate) multi_channel_crash_threshold: f64,
    pub(crate) browser_failure_window_seconds: f64,
    pub(crate) browser_failure_container_threshold: f64,
}

/// 舊版 `BrowserManagerConfig.browser` 的子集。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct BrowserManagerBrowserConfig {
    pub(crate) restart_on_crash: bool,
    pub(crate) viewport_width: f64,
    pub(crate) viewport_height: f64,
    pub(crate) block_images: bool,
    pub(crate) block_fonts: bool,
    pub(crate) block_known_tracking: bool,
    pub(crate) disable_chat: bool,
    pub(crate) recovery: BrowserManagerRecoveryConfig,
}

/// 舊版 `BrowserManagerDependencies`；`None` 欄位用預設值（與舊版 `??` 相同）。
#[derive(Default)]
pub(crate) struct BrowserManagerDependencies {
    /// 舊版 `launcher`；預設 cdp 啟動器（僅 chromium，D1）。
    pub(crate) launcher: Option<Arc<dyn BrowserLauncher>>,
    /// 舊版 `logger`；預設 no-op。
    pub(crate) logger: Option<Arc<dyn BrowserManagerLogger>>,
    /// 舊版 `onInvalidated`。
    pub(crate) on_invalidated: Option<Arc<dyn BrowserInvalidationObserver>>,
    /// 舊版 `onFatalRecovery`：復原耗盡或無法證明終止時呼叫。
    pub(crate) on_fatal_recovery: Option<Arc<dyn BrowserFatalRecoveryObserver>>,
    /// 舊版 `onBrowserRestarted`：瀏覽器程序成功替換後呼叫。
    pub(crate) on_browser_restarted: Option<Arc<dyn BrowserRestartedObserver>>,
    /// 舊版 `sleep`；預設 [`TokioSleep`]。
    pub(crate) sleep: Option<Arc<dyn SleepFn>>,
    /// 舊版 `now`；預設系統時鐘（epoch 毫秒）。
    pub(crate) now: Option<ManagerClock>,
    /// 舊版 `resourceCloseTimeoutMs`。
    pub(crate) resource_close_timeout_ms: Option<u64>,
    /// 舊版 `restartBackoffMs`。
    pub(crate) restart_backoff_ms: Option<u64>,
    /// 舊版 `restartBackoffMaxMs`。
    pub(crate) restart_backoff_max_ms: Option<u64>,
    /// 舊版 `maxAutomaticRestartAttempts`。
    pub(crate) max_automatic_restart_attempts: Option<u64>,
    /// 舊版 `restartAttemptResetMs`。
    pub(crate) restart_attempt_reset_ms: Option<u64>,
}

/// 舊版 `PageEntry`：registry 的每頁記錄。
///
/// 六個取消函式在 Rust 合成一個清單（detach 時逐一呼叫並清空，等價於舊版
/// 逐欄呼叫後換成 no-op）。
pub(crate) struct PageEntry {
    pub(crate) adapter: Arc<dyn BrowserPageAdapter>,
    pub(crate) browser_generation: u64,
    pub(crate) page_generation: u64,
    pub(crate) unsubscribes: Vec<CancelListener>,
}

/// 舊版 `DetachedResources`：從 manager 狀態 detach 後待清理的資源。
pub(crate) struct DetachedResources {
    pub(crate) browser: Option<Arc<dyn BrowserAdapter>>,
    pub(crate) context: Option<Arc<dyn BrowserContextAdapter>>,
    pub(crate) pages: Vec<Arc<dyn BrowserPageAdapter>>,
    pub(crate) unsubscribe_browser: Option<CancelListener>,
}

/// 舊版 `RestartSchedule`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RestartSchedule {
    pub(crate) attempt: u64,
    pub(crate) delay_ms: u64,
    pub(crate) recovery_epoch: u64,
}

/// 舊版 `positiveInteger`：值為正整數時採用，否則 fallback
///（`Number.isInteger` 與 `> 0` 的檢查；u64 天然整數，0 視為未提供）。
pub(crate) fn positive_integer(value: Option<u64>, fallback: u64) -> u64 {
    match value {
        Some(value) if value > 0 => value,
        _ => fallback,
    }
}

/// 舊版 `evictOldTimestamps`：保留 `timestamp >= nowMs - windowMs` 的項目。
pub(crate) fn evict_old_timestamps(timestamps: Vec<u64>, now_ms: u64, window_ms: u64) -> Vec<u64> {
    let cutoff = now_ms.saturating_sub(window_ms);
    timestamps
        .into_iter()
        .filter(|&timestamp| timestamp >= cutoff)
        .collect()
}

/// 舊版 `normalizeChannel`：`channel.trim().toLocaleLowerCase('en-US')`。
/// 頻道名稱經設定驗證後只含 ASCII，`to_ascii_lowercase` 與 en-US 語意相同。
pub(crate) fn normalize_channel(channel: &str) -> String {
    channel.trim().to_ascii_lowercase()
}

// ---------------------------------------------------------------------------
// 正式碼的 trait 實作（cdp 型別的轉接；不動 cdp 既有檔案）
// ---------------------------------------------------------------------------

/// 正式的頁面把手：包 cdp 的 [`PageShared`]（對應舊版 Playwright `Page`）。
pub(crate) struct CdpPage {
    shared: Arc<PageShared>,
}

#[async_trait]
impl BrowserPage for CdpPage {
    fn is_closed(&self) -> bool {
        self.shared.is_closed()
    }

    async fn close(&self) -> Result<(), DriverError> {
        // inherent 的 close 收 `&Arc<Self>`；以完整路徑呼叫。
        PageShared::close(&self.shared).await
    }

    fn as_page_shared(&self) -> Option<Arc<PageShared>> {
        Some(Arc::clone(&self.shared))
    }
}

/// 正式的頁面 adapter（對應舊版 `PlaywrightBrowserPageAdapter`）：生命週期
/// listener 來自 [`PageShared`]，診斷 listener 來自 S1.6 的
/// [`PlaywrightBrowserPageAdapter`]（消毒已在該層完成）。
pub(crate) struct CdpPageAdapter {
    shared: Arc<PageShared>,
    /// 穩定的頁面把手（`page()` 恆回同一個，識別相等）。
    page: Arc<dyn BrowserPage>,
}

impl CdpPageAdapter {
    pub(crate) fn new(shared: Arc<PageShared>) -> Self {
        let page: Arc<dyn BrowserPage> = Arc::new(CdpPage {
            shared: Arc::clone(&shared),
        });
        Self { shared, page }
    }
}

#[async_trait]
impl BrowserPageAdapter for CdpPageAdapter {
    fn page(&self) -> Arc<dyn BrowserPage> {
        Arc::clone(&self.page)
    }

    async fn close(&self) -> Result<(), DriverError> {
        PageShared::close(&self.shared).await
    }

    fn is_closed(&self) -> bool {
        self.shared.is_closed()
    }

    fn on_crash(&self, listener: Box<dyn Fn() + Send + Sync>) -> CancelListener {
        self.shared.on_crash(listener)
    }

    fn on_close(&self, listener: Box<dyn Fn() + Send + Sync>) -> CancelListener {
        self.shared.on_close(listener)
    }

    fn on_popup(
        &self,
        listener: Box<dyn Fn(Arc<dyn BrowserPage>) + Send + Sync>,
    ) -> CancelListener {
        self.shared.on_popup(move |popup: &Arc<PageShared>| {
            listener(Arc::new(CdpPage {
                shared: Arc::clone(popup),
            }) as Arc<dyn BrowserPage>)
        })
    }

    fn on_request_failed(
        &self,
        listener: Box<dyn Fn(&BrowserRequestFailureDiagnostic) + Send + Sync>,
    ) -> CancelListener {
        // 舊版 adapter 的診斷層（S1.6）：消毒與省略條件在該層完成。
        PlaywrightBrowserPageAdapter::new(
            Arc::clone(&self.shared) as Arc<dyn DiagnosticEventSource>
        )
        .on_request_failed(listener)
    }

    fn on_response(
        &self,
        listener: Box<dyn Fn(&BrowserHttpResponseDiagnostic) + Send + Sync>,
    ) -> CancelListener {
        PlaywrightBrowserPageAdapter::new(
            Arc::clone(&self.shared) as Arc<dyn DiagnosticEventSource>
        )
        .on_response(listener)
    }

    fn on_console(
        &self,
        listener: Box<dyn Fn(&BrowserConsoleDiagnostic) + Send + Sync>,
    ) -> CancelListener {
        PlaywrightBrowserPageAdapter::new(
            Arc::clone(&self.shared) as Arc<dyn DiagnosticEventSource>
        )
        .on_console(listener)
    }
}

/// 正式的 context adapter：cdp 的 [`CdpContext`] 直接滿足 trait（同名方法一對一）。
#[async_trait]
impl BrowserContextAdapter for CdpContext {
    async fn configure_resource_blocking(
        &self,
        options: ResourceBlockingOptions,
    ) -> Result<(), DriverError> {
        CdpContext::configure_resource_blocking(self, options).await
    }

    async fn configure_chat_blocking(&self, enabled: bool) -> Result<(), DriverError> {
        CdpContext::configure_chat_blocking(self, enabled).await
    }

    async fn new_page(&self) -> Result<Arc<dyn BrowserPageAdapter>, DriverError> {
        let page = CdpContext::new_page(self).await?;
        Ok(Arc::new(CdpPageAdapter::new(page.shared)) as Arc<dyn BrowserPageAdapter>)
    }

    async fn close(&self) -> Result<(), DriverError> {
        CdpContext::close(self).await
    }
}

/// 正式的瀏覽器 adapter（對應舊版 `PlaywrightBrowserAdapter`）：包 cdp 的
/// [`CdpBrowser`]。cdp 型別未直接暴露 `isConnected`／`getVersion`（不改其檔案），
/// 連線狀態以 listener 自記（對應 Playwright `browser.isConnected()`：斷線事件
/// 之後即為 false）、版本在建構時快取。
pub(crate) struct CdpBrowserAdapter {
    inner: Arc<CdpBrowser>,
    /// 與 [`CdpBrowserAdapter::on_disconnected`] 的登記共用；斷線事件後翻 false。
    connected: Arc<AtomicBool>,
    /// 建構時快取的 `Browser.getVersion` 版本。
    version: String,
}

impl CdpBrowserAdapter {
    /// 包裝 launch 成功的 cdp 瀏覽器 adapter（版本取自 `LaunchedBrowser`）。
    pub(crate) fn new(version: String, inner: Arc<CdpBrowser>) -> Arc<Self> {
        let connected = Arc::new(AtomicBool::new(true));
        // 自記連線狀態：斷線事件（含程序結束、pipe EOF）即翻 false。
        // 取消函式不保留（listener 與瀏覽器同生命週期）；以 let _ 壓制未使用警告。
        {
            let connected = Arc::clone(&connected);
            let _cancel = inner.on_disconnected(move || connected.store(false, Ordering::SeqCst));
        }
        Arc::new(Self {
            inner,
            connected,
            version,
        })
    }
}

#[async_trait]
impl BrowserAdapter for CdpBrowserAdapter {
    async fn new_context(
        &self,
        options: BrowserContextOptions,
    ) -> Result<Arc<dyn BrowserContextAdapter>, DriverError> {
        // 舊版把 `storageState` 字串直接交給 Playwright；正式設定驗證保證非空，
        // 空字串視為不帶 storageState（對應 Option）。
        let storage_state = (!options.storage_state.is_empty())
            .then(|| PathBuf::from(options.storage_state.as_str()));
        let viewport = (
            options.viewport.width as u32,
            options.viewport.height as u32,
        );
        let context =
            CdpBrowser::new_context(self.inner.as_ref(), storage_state.as_deref(), viewport)
                .await?;
        Ok(Arc::new(context) as Arc<dyn BrowserContextAdapter>)
    }

    async fn close(&self) -> Result<(), DriverError> {
        // cdp 的 close 不回錯（程序導向清理）；逾時由呼叫端負責。
        CdpBrowser::close(self.inner.as_ref()).await;
        Ok(())
    }

    fn on_disconnected(&self, listener: Box<dyn Fn() + Send + Sync>) -> CancelListener {
        self.inner.on_disconnected(listener)
    }

    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    fn get_version(&self) -> String {
        self.version.clone()
    }
}

/// 正式的啟動器（對應舊版 `PlaywrightBrowserLauncher` 的 chromium 預設）：
/// 以 cdp 啟動 headless shell（`headless` 由錄製參數固定，D1）。
pub(crate) struct CdpLauncherAdapter {
    inner: CdpLauncher,
}

impl CdpLauncherAdapter {
    pub(crate) fn new() -> Self {
        Self {
            inner: CdpLauncher::new(),
        }
    }
}

impl Default for CdpLauncherAdapter {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl BrowserLauncher for CdpLauncherAdapter {
    async fn launch(
        &self,
        _options: BrowserLaunchOptions,
    ) -> Result<Arc<dyn BrowserAdapter>, DriverError> {
        // LaunchError 只帶訊息（cdp 層，§7.3.1）；以 `Error` 名稱包裝。
        let launched = CdpLauncher::launch(&self.inner)
            .await
            .map_err(|error| DriverError::new("Error", error.message().to_string()))?;
        let version = launched.get_version().to_string();
        Ok(CdpBrowserAdapter::new(version, CdpBrowser::new(launched)) as Arc<dyn BrowserAdapter>)
    }
}
