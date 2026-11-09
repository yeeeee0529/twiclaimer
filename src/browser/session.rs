//! 舊版 `src/browser/ChannelSession.ts` 的移植：單一頻道 session 的生命週期、
//! 定時維護與錯誤復原（S3.4）。
//!
//! 依賴形狀完全照舊版建構參數：config（`channels`＋`browser`）、BrowserManager
//! （經 [`SessionBrowserManager`] 注入；正式端由 [`ManagedSessionBrowserManager`]
//! 轉接 manager 模組的介面）、RewardClaimer（[`SessionRewardClaimer`]）、
//! playbackOptimizer、logger、健康檢查門檻、healthEvaluator、三個觀察者與
//! `now` 時鐘。頁面操作以本檔的 [`SessionPage`]／[`SessionLocator`] trait 注入
//! （D12；正式端 [`ManagedSessionPage`] 以 cdp `PageShared` 落地，測試端照
//! 舊測試的 fake page）。
//!
//! 執行緒模型（D13）：內部狀態集中在一把 `Mutex`；**不在持有鎖時 await 瀏覽器
//! 或網路操作**——await 前把所需值取出、鎖釋放後再等待。序列化沿用舊版的
//! promise-tail 語意（`lifecycleTail`／`pageOperationTail` → manager 模組的
//! [`ExclusiveQueue`]，FIFO、排隊位置同步登記）；timer 以帶 epoch 的背景任務
//! 實作舊版 `setTimeout` 鏈（`scheduleX()` 的再排程 = 迴圈下一輪重新判斷），
//! `clear_timers` 遞增 epoch 取消，未到點的 sleep 連同任務一起結束。
//!
//! 行為基準依 DEC-016 為舊版 `d49363d`：`login_required`／`login_missing` 的
//! 健康失敗不立即 reload（跳過 recovery reload、只累積連續失敗）；session 起始
//! 冷卻的套用（`channel_health_recovery_backoff`）在 S3.5 的 SessionManager。
#![allow(dead_code)]

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::StaticFormatDescription;
use time::macros::format_description;
use tokio_util::sync::CancellationToken;

use super::cdp::context::DriverError;
use super::cdp::locator::Locator;
use super::cdp::page::{PageShared, WaitUntil};
use super::channel_points_reader::{
    BalanceError, ChannelPointsBalancePage, channel_points_balance_script, read_balance_on_page,
};
use super::channel_url::{
    channel_pattern, create_channel_url, is_expected_channel_url, stable_jitter,
};
use super::health::{
    ChannelHealthEvaluator, ChannelHealthReason, ChannelHealthResult, ContentWarningAcceptResult,
    DefaultChannelHealthEvaluator, HealthLocator, HealthPage, accept_content_warning,
};
use super::manager::{BrowserManager, ExclusiveQueue};
use super::reward_claimer::RewardClaimResult;
use super::safe_logging::safe_error_message;
use super::selectors::COMMUNITY_POINTS_BALANCE_SELECTOR;
use super::side_nav::{
    SideNavCollapseResult, SideNavLocator, SideNavPage, collapse_side_nav_on_page,
};
use super::stream_playback_optimizer::{
    StreamPlaybackLocator, StreamPlaybackOptimizerConfig, optimize,
};
use super::types::{BrowserManagerLogger, BrowserPage, positive_integer};
use crate::config::model::BrowserConfig;
use crate::logging::logger::LogEvents;
use crate::logging::redact::SanitizedValue;

// ---------------------------------------------------------------------------
// 常數（舊版逐字搬遷，§7.6）
// ---------------------------------------------------------------------------

const DEFAULT_HEALTH_FAILURE_THRESHOLD: u64 = 3;
const MAX_TIMER_JITTER_MS: u64 = 5_000;
const MAX_PAGE_REFRESH_JITTER_MS: u64 = 60_000;
const MAX_TIMER_DELAY_MS: u64 = 2_147_483_647;
const DEFAULT_PAGE_OPERATION_DRAIN_TIMEOUT_MS: u64 = 5_000;
const DEFAULT_POINTS_CLAIM_RETRY_ATTEMPTS: u64 = 3;
const DEFAULT_POINTS_CLAIM_RETRY_DELAY_MS: u64 = 1_000;
/// 舊版 `REWARD_FAILURE_RECOVERY_THRESHOLD`。
pub(crate) const REWARD_FAILURE_RECOVERY_THRESHOLD: u64 = 10;

// ---------------------------------------------------------------------------
// 型別
// ---------------------------------------------------------------------------

/// 舊版 `ChannelSessionState`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChannelSessionState {
    Starting,
    Watching,
    Recovering,
    Stopping,
    Stopped,
    Failed,
}

impl ChannelSessionState {
    /// 舊版 union 的字面值（測試斷言與 `state` getter 用）。
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ChannelSessionState::Starting => "starting",
            ChannelSessionState::Watching => "watching",
            ChannelSessionState::Recovering => "recovering",
            ChannelSessionState::Stopping => "stopping",
            ChannelSessionState::Stopped => "stopped",
            ChannelSessionState::Failed => "failed",
        }
    }
}

/// 舊版 `ChannelSessionPointsResult`：
/// `ChannelPointsReadResult | { status: 'unavailable', reason: 'page_unavailable' }`。
///
/// Rust 契約（同 [`read_channel_points_balance`] 的移植遺留）：available 只回
/// `balance`，舊版同時回傳的 `displayValue` 在此省略（呼叫端不需要；見
/// channel_points_reader 的模組註解）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChannelSessionPointsResult {
    /// `{ status: 'available', balance }`。
    Available { balance: u64 },
    /// `{ status: 'unavailable', reason: 'not_found' }`。
    NotFound,
    /// `{ status: 'unavailable', reason: 'parse_failed' }`。
    ParseFailed,
    /// `{ status: 'unavailable', reason: 'page_unavailable' }`。
    PageUnavailable,
}

impl ChannelSessionPointsResult {
    /// 舊版 `reason` 的字面值（日誌與斷言用）；available 無對應值（`None`）。
    pub(crate) fn as_str(self) -> Option<&'static str> {
        match self {
            ChannelSessionPointsResult::Available { .. } => None,
            ChannelSessionPointsResult::NotFound => Some("not_found"),
            ChannelSessionPointsResult::ParseFailed => Some("parse_failed"),
            ChannelSessionPointsResult::PageUnavailable => Some("page_unavailable"),
        }
    }

    /// 舊版 `status` 的字面值。
    pub(crate) fn status_str(&self) -> &'static str {
        match self {
            ChannelSessionPointsResult::Available { .. } => "available",
            _ => "unavailable",
        }
    }
}

/// 舊版 `ContainerRestartRequest`。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ContainerRestartRequest {
    pub(crate) channel: String,
    pub(crate) reason: &'static str,
    pub(crate) consecutive_failures: u32,
    pub(crate) requested_at: String,
}

/// 舊版 `ChannelSessionRefreshReason`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChannelSessionRefreshReason {
    ScheduledRefresh,
    ManualRefresh,
    RewardClaimFailureThreshold,
}

impl ChannelSessionRefreshReason {
    /// 舊版 union 的字面值（日誌欄位與 observer 事件用）。
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ChannelSessionRefreshReason::ScheduledRefresh => "scheduled_refresh",
            ChannelSessionRefreshReason::ManualRefresh => "manual_refresh",
            ChannelSessionRefreshReason::RewardClaimFailureThreshold => {
                "reward_claim_failure_threshold"
            }
        }
    }
}

/// 舊版 `ChannelSessionRefreshEvent`。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ChannelSessionRefreshEvent {
    pub(crate) channel: String,
    pub(crate) reason: ChannelSessionRefreshReason,
    pub(crate) started_at: String,
}

/// 舊版 `ChannelSessionRefreshStatus`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ChannelSessionRefreshStatus {
    pub(crate) channel: String,
    pub(crate) enabled: bool,
    pub(crate) next_refresh_at: Option<String>,
    pub(crate) seconds_until_refresh: Option<u64>,
}

/// 舊版 `ChannelSessionLogger`（`Pick<Logger, 'debug'|'info'|'warn'|'error'>`）；
/// 沿用 manager 的日誌抽象（欄位值為消毒前的 [`SanitizedValue`]）。
/// 舊版預設 `NOOP_LOGGER`。
type SessionLogger = dyn BrowserManagerLogger;

/// 舊版 options 的 `now?: () => Date`：回傳 epoch 毫秒（`getTime()`；型別形狀同
/// manager 的 `ManagerClock`）。
pub(crate) type SessionClock = Arc<dyn Fn() -> u64 + Send + Sync>;

// ---------------------------------------------------------------------------
// 注入介面：頁面操作（對應舊版 Playwright Page 用到的子集）
// ---------------------------------------------------------------------------

/// 本檔使用的頁面 API（對應舊版 `Page` 用到的子集；正式端
/// [`ManagedSessionPage`] 包 manager 的 `BrowserPage`，測試端為 fake）。
#[async_trait]
pub(crate) trait SessionPage: Send + Sync {
    /// 舊版 `page.url()`。
    fn url(&self) -> String;

    /// 舊版 `page.isClosed()`。
    fn is_closed(&self) -> bool;

    /// 舊版 `page.setDefaultNavigationTimeout(ms)`。
    fn set_default_navigation_timeout(&self, timeout_ms: u64);

    /// 舊版 `page.goto(url, {waitUntil: 'domcontentloaded', signal})`。
    async fn goto_dom_content_loaded(
        &self,
        url: &str,
        abort: Option<Arc<AtomicBool>>,
    ) -> Result<(), DriverError>;

    /// 舊版 `page.reload({waitUntil: 'domcontentloaded', signal?})`。
    async fn reload_dom_content_loaded(
        &self,
        abort: Option<Arc<AtomicBool>>,
    ) -> Result<(), DriverError>;

    /// 舊版 `page.screenshot({type: 'png', fullPage: false})`。
    async fn screenshot_png(&self) -> Result<Vec<u8>, DriverError>;

    /// 舊版 `page.evaluate(fn, arg)`（X7 前綴 `page.evaluate`）。
    async fn evaluate(
        &self,
        fn_declaration: &str,
        arg: Option<Value>,
    ) -> Result<Value, DriverError>;

    /// 舊版 `page.locator(selector)`；builder 不失敗，操作時才回錯。
    fn locator(&self, selector: &str) -> Arc<dyn SessionLocator>;

    /// 舊版 `page.mouse.move(x, y)`（playback optimizer 的 reveal fallback 用）。
    async fn mouse_move(&self, x: f64, y: f64) -> Result<(), DriverError>;

    /// cdp 把手（正式碼有值；fake 回 `None` —— 同 `types::BrowserPage` 的約定）。
    fn as_page_shared(&self) -> Option<Arc<PageShared>>;
}

/// 本檔使用的 locator API（對應舊版各頁面操作模組共用的 `Locator` 子集）。
#[async_trait]
pub(crate) trait SessionLocator: Send + Sync {
    /// 舊版 `locator.first()`。
    fn first(&self) -> Arc<dyn SessionLocator>;

    /// 舊版 `locator.nth(index)`（playback optimizer 用）。
    fn nth(&self, index: usize) -> Arc<dyn SessionLocator>;

    /// 舊版 `locator.count()`。
    async fn count(&self) -> Result<usize, DriverError>;

    /// 舊版 `locator.isVisible()`。
    async fn is_visible(&self) -> Result<bool, DriverError>;

    /// 舊版 `locator.waitFor({state: 'visible', timeout})`。
    async fn wait_visible(&self, timeout: Option<u64>) -> Result<(), DriverError>;

    /// 舊版 `locator.click({timeout})`。
    async fn click(&self, timeout: Option<u64>) -> Result<(), DriverError>;

    /// 舊版 `locator.hover({timeout})`（playback optimizer 用）。
    async fn hover(&self, timeout: Option<u64>) -> Result<(), DriverError>;

    /// 舊版 `locator.evaluate(fn, arg)`。
    async fn evaluate(
        &self,
        fn_declaration: &str,
        arg: Option<Value>,
    ) -> Result<Value, DriverError>;

    /// 舊版 `locator.evaluateAll(fn)`。
    async fn evaluate_all(&self, fn_declaration: &str) -> Result<Value, DriverError>;
}

// 正式的 cdp `Locator` 直接滿足 trait（同名方法一對一轉接）。
#[async_trait]
impl SessionLocator for Locator {
    fn first(&self) -> Arc<dyn SessionLocator> {
        Arc::new(self.first())
    }

    fn nth(&self, index: usize) -> Arc<dyn SessionLocator> {
        Arc::new(self.nth(index as i64))
    }

    async fn count(&self) -> Result<usize, DriverError> {
        self.count().await
    }

    async fn is_visible(&self) -> Result<bool, DriverError> {
        self.is_visible().await
    }

    async fn wait_visible(&self, timeout: Option<u64>) -> Result<(), DriverError> {
        use super::cdp::locator::WaitForState;
        self.wait_for(WaitForState::Visible, timeout).await
    }

    async fn click(&self, timeout: Option<u64>) -> Result<(), DriverError> {
        self.click(timeout).await
    }

    async fn hover(&self, timeout: Option<u64>) -> Result<(), DriverError> {
        self.hover(timeout).await
    }

    async fn evaluate(
        &self,
        fn_declaration: &str,
        arg: Option<Value>,
    ) -> Result<Value, DriverError> {
        self.evaluate(fn_declaration, arg, None).await
    }

    async fn evaluate_all(&self, fn_declaration: &str) -> Result<Value, DriverError> {
        self.evaluate_all(fn_declaration).await
    }
}

/// 正式端的頁面把手：包 manager 回傳的 [`BrowserPage`]；頁面操作經
/// cdp `PageShared` 落地（X7 的 `<api>` 前綴由 cdp 層產生）。
pub(crate) struct ManagedSessionPage {
    page: Arc<dyn BrowserPage>,
}

impl ManagedSessionPage {
    pub(crate) fn new(page: Arc<dyn BrowserPage>) -> Self {
        Self { page }
    }
}

#[async_trait]
impl SessionPage for ManagedSessionPage {
    fn url(&self) -> String {
        self.page
            .as_page_shared()
            .map(|shared| shared.url())
            .unwrap_or_default()
    }

    fn is_closed(&self) -> bool {
        self.page.is_closed()
    }

    fn set_default_navigation_timeout(&self, timeout_ms: u64) {
        if let Some(shared) = self.page.as_page_shared() {
            shared.set_default_navigation_timeout(timeout_ms);
        }
    }

    async fn goto_dom_content_loaded(
        &self,
        url: &str,
        abort: Option<Arc<AtomicBool>>,
    ) -> Result<(), DriverError> {
        let shared = self
            .page
            .as_page_shared()
            // 不變式：正式 manager 建立的 cdp page 恆有 PageShared 把手。
            .expect("managed session page must expose a cdp page handle");
        shared
            .goto_abortable(url, WaitUntil::DomContentLoaded, abort)
            .await
    }

    async fn reload_dom_content_loaded(
        &self,
        abort: Option<Arc<AtomicBool>>,
    ) -> Result<(), DriverError> {
        let shared = self
            .page
            .as_page_shared()
            // 不變式：同上。
            .expect("managed session page must expose a cdp page handle");
        shared
            .reload_abortable(WaitUntil::DomContentLoaded, abort)
            .await
    }

    async fn screenshot_png(&self) -> Result<Vec<u8>, DriverError> {
        let shared = self
            .page
            .as_page_shared()
            // 不變式：同上。
            .expect("managed session page must expose a cdp page handle");
        shared.screenshot().await
    }

    async fn evaluate(
        &self,
        fn_declaration: &str,
        arg: Option<Value>,
    ) -> Result<Value, DriverError> {
        let shared = self
            .page
            .as_page_shared()
            // 不變式：同上。
            .expect("managed session page must expose a cdp page handle");
        shared.evaluate_function(fn_declaration, arg).await
    }

    fn locator(&self, selector: &str) -> Arc<dyn SessionLocator> {
        let shared = Arc::clone(
            &self
                .page
                .as_page_shared()
                // 不變式：同上。
                .expect("managed session page must expose a cdp page handle"),
        );
        Arc::new(shared.locator(selector))
    }

    async fn mouse_move(&self, x: f64, y: f64) -> Result<(), DriverError> {
        let shared = self
            .page
            .as_page_shared()
            // 不變式：同上。
            .expect("managed session page must expose a cdp page handle");
        shared.mouse_move(x, y).await
    }

    fn as_page_shared(&self) -> Option<Arc<PageShared>> {
        self.page.as_page_shared()
    }
}

// ---------------------------------------------------------------------------
// 注入介面：BrowserManager／RewardClaimer／playbackOptimizer／觀察者
// ---------------------------------------------------------------------------

/// 舊版 session 對 `BrowserManager` 的有效呼用面（`createPage`／`closePage`）。
/// 正式端 [`ManagedSessionBrowserManager`] 轉接 manager 模組的完整介面。
#[async_trait]
pub(crate) trait SessionBrowserManager: Send + Sync {
    async fn create_page(&self, channel: &str) -> Result<Arc<dyn SessionPage>, DriverError>;
    async fn close_page(&self, channel: &str) -> Result<(), DriverError>;
}

/// 正式端轉接：[`BrowserManager`]（manager 模組）→ [`SessionBrowserManager`]。
pub(crate) struct ManagedSessionBrowserManager {
    inner: Arc<dyn BrowserManager>,
}

impl ManagedSessionBrowserManager {
    pub(crate) fn new(inner: Arc<dyn BrowserManager>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl SessionBrowserManager for ManagedSessionBrowserManager {
    async fn create_page(&self, channel: &str) -> Result<Arc<dyn SessionPage>, DriverError> {
        let page = self.inner.create_page(channel).await?;
        Ok(Arc::new(ManagedSessionPage::new(page)) as Arc<dyn SessionPage>)
    }

    async fn close_page(&self, channel: &str) -> Result<(), DriverError> {
        self.inner.close_page(channel).await
    }
}

/// 舊版 `RewardClaimer` 介面在 session 的呼用面（`claimIfAvailable` 可能拋錯
/// → Rust 以 `Err` 表達；`hasClaimableBonus` 可能拋錯 → 同）。
#[async_trait]
pub(crate) trait SessionRewardClaimer: Send + Sync {
    async fn claim_if_available(
        &self,
        page: &dyn SessionPage,
        channel: &str,
    ) -> Result<RewardClaimResult, DriverError>;

    async fn has_claimable_bonus(&self, page: &dyn SessionPage) -> Result<bool, DriverError>;
}

/// 正式端轉接：reward_claimer 模組的具體 `RewardClaimer` 結構（該模組的
/// 實作永不拋錯：`claimIfAvailable` 的錯誤已折為 ClickFailed，`hasClaimableBonus`
/// 的錯誤折為 `false`）。
#[async_trait]
impl SessionRewardClaimer for super::reward_claimer::RewardClaimer {
    async fn claim_if_available(
        &self,
        page: &dyn SessionPage,
        channel: &str,
    ) -> Result<RewardClaimResult, DriverError> {
        let Some(shared) = page.as_page_shared() else {
            // 正式端 session 的頁面恆帶 cdp 把手；本分支僅在誤用 fake 時出現，
            // 以 ClickFailed 回報（對應舊版 locator API 失敗的路徑）。
            return Ok(RewardClaimResult::ClickFailed {
                channel: channel.to_string(),
                checked_at: format_iso_millis(default_now_ms()),
                error: safe_error_message(Some("managed page does not expose a cdp page handle")),
            });
        };
        let page_handle: Arc<dyn BrowserPage> = Arc::new(SharedBrowserPage { shared });
        Ok(super::reward_claimer::RewardClaimer::claim_if_available(
            self,
            page_handle.as_ref(),
            channel,
        )
        .await)
    }

    async fn has_claimable_bonus(&self, page: &dyn SessionPage) -> Result<bool, DriverError> {
        let Some(shared) = page.as_page_shared() else {
            // 同上：fake 頁沒有 cdp 把手，視為沒有可領按鈕（該模組對錯誤的折法）。
            return Ok(false);
        };
        let page_handle: Arc<dyn BrowserPage> = Arc::new(SharedBrowserPage { shared });
        Ok(
            super::reward_claimer::RewardClaimer::has_claimable_bonus(self, page_handle.as_ref())
                .await,
        )
    }
}

/// 把 cdp `PageShared` 包回 `BrowserPage` 的最小視圖（供共用頁面操作模組使用）。
struct SharedBrowserPage {
    shared: Arc<PageShared>,
}

#[async_trait]
impl BrowserPage for SharedBrowserPage {
    fn is_closed(&self) -> bool {
        self.shared.is_closed()
    }

    async fn close(&self) -> Result<(), DriverError> {
        PageShared::close(&self.shared).await
    }

    fn as_page_shared(&self) -> Option<Arc<PageShared>> {
        Some(Arc::clone(&self.shared))
    }
}

/// 舊版 `StreamPlaybackOptimizer` 介面在 session 的呼用面
/// （`optimize(page, channel)` 可能拋錯 → `Err`）。
#[async_trait]
pub(crate) trait SessionStreamPlaybackOptimizer: Send + Sync {
    async fn optimize(
        &self,
        page: &(dyn SessionPage + 'static),
        channel: &str,
    ) -> Result<(), DriverError>;
}

/// 舊版預設 `DefaultStreamPlaybackOptimizer(config.browser, logger)` 的轉接。
struct DefaultSessionPlaybackOptimizer {
    config: StreamPlaybackOptimizerConfig,
    logger: Arc<SessionLogger>,
}

#[async_trait]
impl SessionStreamPlaybackOptimizer for DefaultSessionPlaybackOptimizer {
    async fn optimize(
        &self,
        page: &(dyn SessionPage + 'static),
        channel: &str,
    ) -> Result<(), DriverError> {
        optimize(
            &PlaybackPageView::new(page),
            channel,
            self.config,
            self.logger.as_ref(),
        )
        .await;
        Ok(())
    }
}

/// session 的 fake-able page → playback optimizer 頁面的轉接（D12）。
struct PlaybackPageView<'a> {
    page: &'a (dyn SessionPage + 'static),
}

impl<'a> PlaybackPageView<'a> {
    fn new(page: &'a (dyn SessionPage + 'static)) -> Self {
        Self { page }
    }
}

#[async_trait]
impl super::stream_playback_optimizer::StreamPlaybackPage for PlaybackPageView<'_> {
    fn locator(&self, selector: &str) -> Arc<dyn StreamPlaybackLocator> {
        Arc::new(PlaybackLocatorView::new(self.page.locator(selector)))
    }

    async fn mouse_move(&self, x: f64, y: f64) -> Result<(), DriverError> {
        self.page.mouse_move(x, y).await
    }
}

struct PlaybackLocatorView(Arc<dyn SessionLocator>);

impl PlaybackLocatorView {
    fn new(inner: Arc<dyn SessionLocator>) -> Self {
        Self(inner)
    }
}

#[async_trait]
impl StreamPlaybackLocator for PlaybackLocatorView {
    fn first(&self) -> Arc<dyn StreamPlaybackLocator> {
        Arc::new(PlaybackLocatorView::new(self.0.first()))
    }

    fn nth(&self, index: usize) -> Arc<dyn StreamPlaybackLocator> {
        Arc::new(PlaybackLocatorView::new(self.0.nth(index)))
    }

    async fn count(&self) -> Result<usize, DriverError> {
        self.0.count().await
    }

    async fn wait_for(&self, timeout: Option<u64>) -> Result<(), DriverError> {
        self.0.wait_visible(timeout).await
    }

    async fn click(&self, timeout: Option<u64>) -> Result<(), DriverError> {
        self.0.click(timeout).await
    }

    async fn hover(&self, timeout: Option<u64>) -> Result<(), DriverError> {
        self.0.hover(timeout).await
    }

    async fn evaluate(
        &self,
        fn_declaration: &str,
        arg: Option<Value>,
    ) -> Result<Value, DriverError> {
        self.0.evaluate(fn_declaration, arg).await
    }

    async fn evaluate_all(&self, fn_declaration: &str) -> Result<Value, DriverError> {
        self.0.evaluate_all(fn_declaration).await
    }
}

/// 舊版 `ChannelSessionInvalidationObserver`（第二參數固定
/// `'health_failure_threshold'`，未建模為欄位）。觀察者可拋錯 → `Err`。
#[async_trait]
pub(crate) trait ChannelSessionInvalidationObserver: Send + Sync {
    async fn on_invalidated(&self, channel: &str) -> Result<(), DriverError>;
}

/// 舊版 `ContainerRestartRequestObserver`。
#[async_trait]
pub(crate) trait ContainerRestartRequestObserver: Send + Sync {
    async fn on_container_restart_requested(
        &self,
        request: &ContainerRestartRequest,
    ) -> Result<(), DriverError>;
}

/// 舊版 `ChannelSessionRefreshObserver`。
#[async_trait]
pub(crate) trait ChannelSessionRefreshObserver: Send + Sync {
    async fn on_page_refresh(&self, event: &ChannelSessionRefreshEvent) -> Result<(), DriverError>;
}

// ---------------------------------------------------------------------------
// options 與 factory
// ---------------------------------------------------------------------------

/// 舊版 `Pick<AppConfig, 'channels' | 'browser'>` 的形狀（channels 與完整
/// browser 設定；session 只用到其中幾個欄位）。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ChannelSessionConfig {
    pub(crate) channels: Vec<String>,
    pub(crate) browser: BrowserConfig,
}

/// 舊版 `DefaultChannelSessionFactoryOptions`。
pub(crate) struct DefaultChannelSessionFactoryOptions {
    pub(crate) config: ChannelSessionConfig,
    pub(crate) browser_manager: Arc<dyn SessionBrowserManager>,
    pub(crate) reward_claimer: Arc<dyn SessionRewardClaimer>,
    pub(crate) playback_optimizer: Option<Arc<dyn SessionStreamPlaybackOptimizer>>,
    pub(crate) logger: Option<Arc<SessionLogger>>,
    pub(crate) health_failure_threshold: Option<u64>,
    pub(crate) health_evaluator: Option<Arc<dyn ChannelHealthEvaluator>>,
    pub(crate) on_invalidated: Option<Arc<dyn ChannelSessionInvalidationObserver>>,
    pub(crate) on_page_refresh: Option<Arc<dyn ChannelSessionRefreshObserver>>,
    pub(crate) on_container_restart_requested: Option<Arc<dyn ContainerRestartRequestObserver>>,
    pub(crate) page_operation_drain_timeout_ms: Option<u64>,
    pub(crate) points_claim_retry_attempts: Option<u64>,
    pub(crate) points_claim_retry_delay_ms: Option<u64>,
    pub(crate) now: Option<SessionClock>,
}

/// 舊版 `DefaultChannelSessionOptions`（factory options ＋ channel）。
#[derive(Clone)]
pub(crate) struct DefaultChannelSessionOptions {
    pub(crate) channel: String,
    pub(crate) config: ChannelSessionConfig,
    pub(crate) browser_manager: Arc<dyn SessionBrowserManager>,
    pub(crate) reward_claimer: Arc<dyn SessionRewardClaimer>,
    pub(crate) playback_optimizer: Option<Arc<dyn SessionStreamPlaybackOptimizer>>,
    pub(crate) logger: Option<Arc<SessionLogger>>,
    pub(crate) health_failure_threshold: Option<u64>,
    pub(crate) health_evaluator: Option<Arc<dyn ChannelHealthEvaluator>>,
    pub(crate) on_invalidated: Option<Arc<dyn ChannelSessionInvalidationObserver>>,
    pub(crate) on_page_refresh: Option<Arc<dyn ChannelSessionRefreshObserver>>,
    pub(crate) on_container_restart_requested: Option<Arc<dyn ContainerRestartRequestObserver>>,
    pub(crate) page_operation_drain_timeout_ms: Option<u64>,
    pub(crate) points_claim_retry_attempts: Option<u64>,
    pub(crate) points_claim_retry_delay_ms: Option<u64>,
    pub(crate) now: Option<SessionClock>,
}

/// 舊版 `DefaultChannelSessionFactory`。
pub(crate) struct DefaultChannelSessionFactory {
    options: DefaultChannelSessionFactoryOptions,
}

impl DefaultChannelSessionFactory {
    pub(crate) fn new(options: DefaultChannelSessionFactoryOptions) -> Self {
        Self { options }
    }

    /// 舊版 `create(channel)`；驗證在 `DefaultChannelSession` 建構函式（同舊版）。
    pub(crate) fn create(&self, channel: &str) -> Result<DefaultChannelSession, DriverError> {
        DefaultChannelSession::new(DefaultChannelSessionOptions {
            channel: channel.to_string(),
            config: self.options.config.clone(),
            browser_manager: Arc::clone(&self.options.browser_manager),
            reward_claimer: Arc::clone(&self.options.reward_claimer),
            playback_optimizer: self.options.playback_optimizer.clone(),
            logger: self.options.logger.clone(),
            health_failure_threshold: self.options.health_failure_threshold,
            health_evaluator: self.options.health_evaluator.clone(),
            on_invalidated: self.options.on_invalidated.clone(),
            on_page_refresh: self.options.on_page_refresh.clone(),
            on_container_restart_requested: self.options.on_container_restart_requested.clone(),
            page_operation_drain_timeout_ms: self.options.page_operation_drain_timeout_ms,
            points_claim_retry_attempts: self.options.points_claim_retry_attempts,
            points_claim_retry_delay_ms: self.options.points_claim_retry_delay_ms,
            now: self.options.now.clone(),
        })
    }
}

// ---------------------------------------------------------------------------
// 內部實作（舊版 private 方法逐項對應）
// ---------------------------------------------------------------------------

impl Inner {
    // ===== 公開 API 對應的內部版 =====

    /// 舊版 `start()` 主體（包在 `runLifecycle`；成功前往發動與取消見
    /// `start_attempt`，失敗的路徑在 catch 分支同構）。
    async fn start(self: &Arc<Self>) -> Result<(), DriverError> {
        let this = Arc::clone(self);
        self.run_lifecycle(async move {
            // 舊版的 try 本體（留意：`failed` 狀態重複 start 是 no-op，照舊版）。
            let attempt = Inner::start_attempt(&this).await;
            match attempt {
                Ok(()) => Ok(()),
                Err(error) => {
                    {
                        let mut state = this
                            .state
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        state.current = ChannelSessionState::Failed;
                    }
                    // 舊版 catch：clearTimers → page = undefined → closePageForCleanup
                    // → 依 startCancellationReason 分流記錄。
                    this.clear_timers();
                    {
                        let mut state = this
                            .state
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        state.page = None;
                    }
                    this.close_page_for_cleanup("start_failure").await;
                    let cancellation_reason = this
                        .state
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .start_cancellation_reason
                        .clone();
                    match cancellation_reason {
                        Some(reason) => this.logger.debug(
                            "watch_start_aborted",
                            Some(&fields(vec![
                                ("channel", sv_str(&this.channel)),
                                ("reason", sv_str(&reason)),
                                ("outcome", sv_str("aborted")),
                                ("error", sv_str(safe_error_message(Some(&error.message)))),
                            ])),
                        ),
                        None => this.logger.error(
                            "watch_start_failed",
                            Some(&fields(vec![
                                ("channel", sv_str(&this.channel)),
                                ("error", sv_str(safe_error_message(Some(&error.message)))),
                            ])),
                        ),
                    }
                    Err(error)
                }
            }
        })
        .await
    }

    /// 舊版 `start()` 的 try 本體。
    async fn start_attempt(this: &Arc<Inner>) -> Result<(), DriverError> {
        {
            let mut state = this
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if matches!(
                state.current,
                ChannelSessionState::Starting
                    | ChannelSessionState::Watching
                    | ChannelSessionState::Recovering
                    | ChannelSessionState::Failed
            ) {
                return Ok(());
            }
            state.current = ChannelSessionState::Starting;
            state.start_cancellation_reason = None;
            state.operation_abort = Some(Arc::new(AtomicBool::new(false)));
            state.consecutive_health_failures = 0;
            reset_reward_failure_recovery(&mut state);
        }

        // 舊版：createPage → page 記住 → throwIfStartCancelled → …逐步檢查取消。
        let page = this.browser_manager.create_page(&this.channel).await?;
        {
            let mut state = this
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.page = Some(Arc::clone(&page));
        }
        throw_if_start_cancelled(&this.state)?;
        page.set_default_navigation_timeout(
            // 舊版的 navigationTimeoutMs 是 number；設定驗證保證為有限正值
            //（NaN／負值在 Rust 飽和為 0，行為同 Node 的 timer drink）。
            this.config.browser.navigation_timeout_ms as u64,
        );
        this.goto_page(page.as_ref()).await?;
        throw_if_start_cancelled(&this.state)?;
        if !is_expected_channel_url(&page.url(), &this.target_url) {
            return Err(DriverError::new("Error", "頻道頁面導向非預期 Twitch URL"));
        }
        this.accept_content_warning_if_present(page.as_ref(), "start")
            .await?;
        throw_if_start_cancelled(&this.state)?;
        this.collapse_side_nav_if_expanded(page.as_ref(), "start")
            .await;
        throw_if_start_cancelled(&this.state)?;
        this.optimize_playback(page.as_ref()).await;
        throw_if_start_cancelled(&this.state)?;

        {
            let mut state = this
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.current = ChannelSessionState::Watching;
        }
        this.start_timers();
        this.logger.info(
            LogEvents::WATCH_STARTED,
            Some(&fields(vec![
                ("channel", sv_str(&this.channel)),
                ("url", sv_str(&this.target_url)),
            ])),
        );
        Ok(())
    }

    /// 舊版 `page.goto(...)` 的包裝（signal 由 operationAbortController 取出）。
    async fn goto_page(&self, page: &(dyn SessionPage + 'static)) -> Result<(), DriverError> {
        let abort = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .operation_abort
            .clone();
        page.goto_dom_content_loaded(&self.target_url, abort).await
    }

    /// 舊版 `cancelStart(reason)`（不走 runLifecycle；老師可與進行中的 start 併發）。
    async fn cancel_start(self: &Arc<Self>, reason: &str) {
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if state.current != ChannelSessionState::Starting {
                return;
            }
            state.start_cancellation_reason = Some(reason.to_string());
        }
        self.abort_session_operations(reason);
        self.clear_timers();
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.page = None;
        }
        self.close_page_for_cleanup("start_cancelled").await;
    }

    /// 舊版 `stop(reason)`（包在 runLifecycle）。
    async fn stop(self: &Arc<Self>, reason: &str) -> Result<(), DriverError> {
        let this = Arc::clone(self);
        let reason = reason.to_string();
        self.run_lifecycle(async move { Inner::stop_locked(&this, &reason).await })
            .await
    }

    async fn stop_locked(this: &Arc<Inner>, reason: &str) -> Result<(), DriverError> {
        {
            let mut state = this
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if matches!(
                state.current,
                ChannelSessionState::Stopped | ChannelSessionState::Stopping
            ) {
                return Ok(());
            }
            state.current = ChannelSessionState::Stopping;
        }
        this.abort_session_operations(reason);
        this.clear_timers();
        let drained = this.drain_page_operations().await;
        if !drained {
            this.logger.warn(
                "session_page_operation_drain_timeout",
                Some(&fields(vec![
                    ("channel", sv_str(&this.channel)),
                    ("reason", sv_str(reason)),
                    (
                        "timeoutMs",
                        sv_num(this.page_operation_drain_timeout_ms as f64),
                    ),
                ])),
            );
        }
        {
            let mut state = this
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.page = None;
        }
        if let Err(error) = this.browser_manager.close_page(&this.channel).await {
            this.logger.warn(
                "watch_stop_cleanup_failed",
                Some(&fields(vec![
                    ("channel", sv_str(&this.channel)),
                    ("reason", sv_str(reason)),
                    ("error", sv_str(safe_error_message(Some(&error.message)))),
                ])),
            );
        }
        {
            let mut state = this
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.current = ChannelSessionState::Stopped;
            state.start_cancellation_reason = None;
            state.consecutive_health_failures = 0;
            reset_reward_failure_recovery(&mut state);
        }
        this.logger.info(
            LogEvents::WATCH_STOPPED,
            Some(&fields(vec![
                ("channel", sv_str(&this.channel)),
                ("reason", sv_str(reason)),
            ])),
        );
        Ok(())
    }

    // ===== 維護類 API（單一 flight 語意照舊版）=====

    /// 舊版 `checkHealth()`：`healthFlight` 的單一 flight（永不拋錯）。
    async fn check_health(self: &Arc<Self>) -> ChannelHealthResult {
        flight_take_existing(
            self,
            "health",
            |state, cell| claim_flight_slot(&mut state.health_flight, Arc::clone(cell)),
            |state, cell| release_flight_slot(&mut state.health_flight, cell),
            |this| async move { Inner::run_health_check(&this).await },
        )
        .await
    }

    /// 舊版 `tickRewardClaim()`：`rewardFlight` 的單一 flight（永不拋錯）。
    async fn tick_reward_claim(self: &Arc<Self>) -> RewardClaimResult {
        flight_take_existing(
            self,
            "reward",
            |state, cell| claim_flight_slot(&mut state.reward_flight, Arc::clone(cell)),
            |state, cell| release_flight_slot(&mut state.reward_flight, cell),
            |this| async move { Inner::run_reward_claim(&this).await },
        )
        .await
    }

    /// 舊版 `captureScreenshot()`：不做單一 flight（每次呼叫都排入 page operation）。
    async fn capture_screenshot(self: &Arc<Self>) -> Result<Vec<u8>, DriverError> {
        let this = Arc::clone(self);
        self.run_page_operation("screenshot", async move {
            let (page, should) = read_page_and_should(&this);
            let page = match page {
                Some(page) if !page.is_closed() && should => page,
                _ => return Err(DriverError::new("Error", "頻道頁面目前無法截圖")),
            };
            page.screenshot_png().await
        })
        .await
    }

    /// 舊版 `getChannelPoints()`：`pointsFlight` 的單一 flight。
    async fn get_channel_points(self: &Arc<Self>) -> ChannelSessionPointsResult {
        flight_take_existing(
            self,
            "points",
            |state, cell| claim_flight_slot(&mut state.points_flight, Arc::clone(cell)),
            |state, cell| release_flight_slot(&mut state.points_flight, cell),
            |this| async move { Inner::run_get_channel_points(&this).await },
        )
        .await
    }

    /// 舊版 `getRefreshStatus()`。
    fn get_refresh_status(&self) -> ChannelSessionRefreshStatus {
        let enabled = self.config.browser.page_refresh_interval_seconds > 0.0;
        let next_page_refresh_at_ms = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .next_page_refresh_at_ms;
        let Some(next_page_refresh_at_ms) = next_page_refresh_at_ms else {
            if !enabled {
                return ChannelSessionRefreshStatus {
                    channel: self.channel.clone(),
                    enabled: false,
                    next_refresh_at: None,
                    seconds_until_refresh: None,
                };
            }
            return ChannelSessionRefreshStatus {
                channel: self.channel.clone(),
                enabled: true,
                next_refresh_at: None,
                seconds_until_refresh: None,
            };
        };
        let now_ms = (self.now)();
        let delta_ms = next_page_refresh_at_ms as i64 - now_ms as i64;
        let seconds_until_refresh = if delta_ms <= 0 {
            0
        } else {
            (delta_ms as f64 / 1_000.0).ceil() as u64
        };
        ChannelSessionRefreshStatus {
            channel: self.channel.clone(),
            enabled: true,
            next_refresh_at: Some(format_iso_millis(next_page_refresh_at_ms)),
            seconds_until_refresh: Some(seconds_until_refresh),
        }
    }

    /// 舊版 `refreshNow()`。reload 失敗時照舊版把錯誤外拋（不記 page_refreshed）。
    async fn refresh_now(self: &Arc<Self>) -> Result<bool, DriverError> {
        let (page, should) = read_page_and_should(self);
        let page = match page {
            Some(page) if !page.is_closed() && should => page,
            _ => return Ok(false),
        };
        self.notify_page_refresh(ChannelSessionRefreshReason::ManualRefresh);
        self.reload_page(page, "manual_refresh").await?;
        self.logger.info(
            "page_refreshed",
            Some(&fields(vec![
                ("channel", sv_str(&self.channel)),
                (
                    "reason",
                    sv_str(ChannelSessionRefreshReason::ManualRefresh.as_str()),
                ),
            ])),
        );
        Ok(true)
    }

    // ===== 單一 flight 的通用實作與 page operation 佇列 =====

    /// 舊版 promise-tail 的 FIFO 串行（對應 `runLifecycle`）。
    async fn run_lifecycle<T, F>(self: &Arc<Self>, operation: F) -> T
    where
        T: Send,
        F: Future<Output = T> + Send,
    {
        let slot = self.lifecycle_queue.enqueue();
        let _guard = slot.acquire().await;
        operation.await
    }

    /// 舊版 `runPageOperation(task, operation)`：`pageOperationTail` 串行加
    /// `session_maintenance_completed` 記錄（queuedMs／durationMs 為 Math.round 的
    /// performance.now 差值）。
    async fn run_page_operation<T, F>(
        self: &Arc<Self>,
        task: &'static str,
        operation: F,
    ) -> Result<T, DriverError>
    where
        T: Send,
        F: Future<Output = Result<T, DriverError>> + Send,
    {
        let queued_at = Instant::now();
        let slot = self.page_operation_queue.enqueue();
        let _guard = slot.acquire().await;
        let started_at = Instant::now();
        let (outcome, result) = match operation.await {
            Ok(value) => ("completed", Ok(value)),
            Err(error) => ("failed", Err(error)),
        };
        self.record_maintenance_completed(task, outcome, queued_at, started_at);
        result
    }

    /// 永不拒絕的操作的 `runPageOperation`（health／reward／points；舊版同一
    /// 佇列與記錄，Promise 的 catch 分支在這幾個 op 上不可達）。
    async fn run_page_operation_infallible<T, F>(
        self: &Arc<Self>,
        task: &'static str,
        operation: F,
    ) -> T
    where
        T: Send,
        F: Future<Output = T> + Send,
    {
        let queued_at = Instant::now();
        let slot = self.page_operation_queue.enqueue();
        let _guard = slot.acquire().await;
        let started_at = Instant::now();
        let result = operation.await;
        self.record_maintenance_completed(task, "completed", queued_at, started_at);
        result
    }

    /// `session_maintenance_completed` 記錄（run_page_operation / _infallible 共用）。
    fn record_maintenance_completed(
        &self,
        task: &'static str,
        outcome: &'static str,
        queued_at: Instant,
        started_at: Instant,
    ) {
        let completed_at = Instant::now();
        self.logger.debug(
            "session_maintenance_completed",
            Some(&fields(vec![
                ("channel", sv_str(&self.channel)),
                ("task", sv_str(task)),
                ("outcome", sv_str(outcome)),
                (
                    "queuedMs",
                    sv_num(round_ms(started_at.duration_since(queued_at))),
                ),
                (
                    "durationMs",
                    sv_num(round_ms(completed_at.duration_since(started_at))),
                ),
            ])),
        );
    }

    // ===== 頁面刷新（reloadPage／reloadDuringPageOperation 的單一 flight）=====

    /// 舊版 `reloadPage(page, reason)`：`reloadFlight` 的單一 flight + page operation。
    async fn reload_page(
        self: &Arc<Self>,
        page: Arc<dyn SessionPage>,
        reason: &'static str,
    ) -> Result<(), DriverError> {
        let created = FlightCell::<Result<(), DriverError>>::new();
        let existing = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            claim_flight_slot(&mut state.reload_flight, Arc::clone(&created))
        };
        if let Some(existing) = existing {
            return existing.take().await;
        }
        let this = Arc::clone(self);
        let result = self
            .run_page_operation("reload", async move {
                Inner::reload_page_operation(&this, page, reason).await
            })
            .await;
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            release_flight_slot(&mut state.reload_flight, &created);
        }
        created.set(result.clone());
        result
    }

    /// 舊版 `reloadDuringPageOperation(page, reason)`：已在 reload 時靜默略過，
    /// 否則直接執行（不再進 page operation 佇列——本呼叫已在佇列內）。
    async fn reload_during_page_operation(
        self: &Arc<Self>,
        page: Arc<dyn SessionPage>,
        reason: &'static str,
    ) -> Result<(), DriverError> {
        let created = FlightCell::<Result<(), DriverError>>::new();
        let existing = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            claim_flight_slot(&mut state.reload_flight, Arc::clone(&created))
        };
        if existing.is_some() {
            // 呼叫端已持有 page operation 佇列，既有的 reload 必定排在佇列後方等待；
            // 在這裡等它會形成循環等待（它等我們釋放佇列）。直接略過，
            // 排隊的 reload 會在佇列釋放後照常執行。
            self.log_maintenance_skipped("reload", "reload_already_queued");
            return Ok(());
        }
        let result = Inner::run_page_reload(self, page, reason).await;
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            release_flight_slot(&mut state.reload_flight, &created);
        }
        created.set(result.clone());
        result
    }

    /// `reloadPage` 的 operation 本體（先判斷 page 是否還是同一個，再 reload）。
    async fn reload_page_operation(
        this: &Arc<Inner>,
        page: Arc<dyn SessionPage>,
        reason: &'static str,
    ) -> Result<(), DriverError> {
        let (current, should) = read_page_and_should(this);
        let valid = match current.as_ref() {
            // 舊版：`this.page !== page` 的識別相等檢查；Rust 以 Arc 資料指標比較。
            Some(current) => !current.is_closed() && should && session_page_same(current, &page),
            None => false,
        };
        if !valid {
            this.log_maintenance_skipped("reload", "page_unavailable");
            return Ok(());
        }
        Inner::run_page_reload(this, page, reason).await
    }

    /// 舊版 `runPageReload(page, reason)`。內容警示／側欄／playback 的失敗語意
    /// 照舊版個別 helper。
    async fn run_page_reload(
        this: &Arc<Inner>,
        page: Arc<dyn SessionPage>,
        reason: &'static str,
    ) -> Result<(), DriverError> {
        let abort = this
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .operation_abort
            .clone();
        page.reload_dom_content_loaded(abort).await?;
        this.accept_content_warning_if_present(page.as_ref(), reason)
            .await?;
        this.collapse_side_nav_if_expanded(page.as_ref(), reason)
            .await;
        this.optimize_playback(page.as_ref()).await;
        this.logger.debug(
            "page_reloaded",
            Some(&fields(vec![
                ("channel", sv_str(&this.channel)),
                ("reason", sv_str(reason)),
            ])),
        );
        Ok(())
    }

    // ===== 健康檢查 =====

    /// 舊版 `runHealthCheck()`：頁面缺失先記 skipped；評估後依 stopping／stopped、
    /// healthy／unhealthy 分流。
    async fn run_health_check(this: &Arc<Inner>) -> ChannelHealthResult {
        let (page, should) = read_page_and_should(this);
        let Some(page) = page else {
            let current = read_current(this);
            log_maintenance_skipped(this, "health", "page_missing");
            return ChannelHealthResult::failed(if current == ChannelSessionState::Failed {
                ChannelHealthReason::PageClosed
            } else {
                ChannelHealthReason::NotStarted
            });
        };
        if page.is_closed() || !should {
            let current = read_current(this);
            log_maintenance_skipped(this, "health", "page_missing");
            return ChannelHealthResult::failed(if current == ChannelSessionState::Failed {
                ChannelHealthReason::PageClosed
            } else {
                ChannelHealthReason::NotStarted
            });
        }

        let result = match this
            .health_evaluator
            .evaluate(&HealthPageView::new(page.as_ref()), &this.target_url)
            .await
        {
            Ok(result) => result,
            Err(error) => ChannelHealthResult::failed_with_error(
                ChannelHealthReason::HealthCheckError,
                safe_error_message(Some(&error.message)),
            ),
        };

        let current = read_current(this);
        if matches!(
            current,
            ChannelSessionState::Stopping | ChannelSessionState::Stopped
        ) {
            return result;
        }

        if result.healthy {
            let mut state = this
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.consecutive_health_failures = 0;
            state.current = ChannelSessionState::Watching;
            return result;
        }

        Inner::handle_health_failure(this, Arc::clone(&page), result.clone()).await;
        result
    }

    /// 舊版 `handleHealthFailure(page, result)`。
    async fn handle_health_failure(
        this: &Arc<Inner>,
        page: Arc<dyn SessionPage>,
        result: ChannelHealthResult,
    ) {
        // 舊版先計數再記錄，reason 錯誤欄位只在 `health_check_error` 有值。
        let consecutive = {
            let mut state = this
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.consecutive_health_failures += 1;
            state.consecutive_health_failures
        };
        {
            let mut entries = vec![
                ("channel", sv_str(&this.channel)),
                ("reason", sv_str(result.reason.as_str())),
                ("consecutiveFailures", sv_num(consecutive)),
            ];
            if let Some(error) = &result.error {
                entries.push(("error", sv_str(error)));
            }
            this.logger
                .warn(LogEvents::PAGE_HEALTH_FAILED, Some(&fields(entries)));
            // 欄位值（SanitizedValue 內含 Rc）在 await 前結束生命週期（D13）。
        }

        if consecutive as u64 >= this.health_failure_threshold {
            this.fail_session().await;
            return;
        }

        {
            let mut state = this
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.current = ChannelSessionState::Recovering;
        }
        // d49363d：登入問題或第一次失敗以外的失敗不立即 reload；頁面已關閉也略過。
        if matches!(
            result.reason,
            ChannelHealthReason::LoginRequired | ChannelHealthReason::LoginMissing
        ) || consecutive != 1
            || page.is_closed()
        {
            return;
        }

        if let Err(error) = this
            .reload_during_page_operation(page, "health_failure")
            .await
        {
            this.logger.warn(
                "page_reload_failed",
                Some(&fields(vec![
                    ("channel", sv_str(&this.channel)),
                    ("reason", sv_str("health_failure")),
                    ("error", sv_str(safe_error_message(Some(&error.message)))),
                ])),
            );
        }
    }

    /// 舊版 `failSession()`：狀態、abort、timers、頁面清理與失效通知。
    async fn fail_session(self: &Arc<Self>) {
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.current = ChannelSessionState::Failed;
        }
        self.abort_session_operations("health_failure_threshold");
        self.clear_timers();
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.page = None;
        }
        self.close_page_for_cleanup("health_failure_threshold")
            .await;
        self.notify_invalidated();
    }

    /// 舊版 `closePageForCleanup(reason)`：manager 的 closePage 失敗只記 warn。
    async fn close_page_for_cleanup(self: &Arc<Self>, reason: &str) {
        if let Err(error) = self.browser_manager.close_page(&self.channel).await {
            self.logger.warn(
                "watch_page_cleanup_failed",
                Some(&fields(vec![
                    ("channel", sv_str(&self.channel)),
                    ("reason", sv_str(reason)),
                    ("error", sv_str(safe_error_message(Some(&error.message)))),
                ])),
            );
        }
    }

    // ===== 忠誠點數 =====

    /// 舊版 `getChannelPoints()` 的 operation 本體。
    async fn run_get_channel_points(this: &Arc<Inner>) -> ChannelSessionPointsResult {
        let (page, should) = read_page_and_should(this);
        let available = match page.as_ref() {
            Some(page) => !page.is_closed() && should,
            None => false,
        };
        // 舊版：頁面不可用時先記 skipped 再回 page_unavailable。
        let Some(page) = page else {
            log_maintenance_skipped(this, "points", "page_unavailable");
            return ChannelSessionPointsResult::PageUnavailable;
        };
        if !available {
            log_maintenance_skipped(this, "points", "page_unavailable");
            return ChannelSessionPointsResult::PageUnavailable;
        }
        Inner::read_channel_points_with_claim_recovery(this, Arc::clone(&page)).await
    }

    /// 舊版 `readChannelPointsWithClaimRecovery(page)`：先讀；不可用時視 bonus
    /// 狀態先領取再重讀（重讀間隔照 options 的 retry delay）。
    async fn read_channel_points_with_claim_recovery(
        this: &Arc<Inner>,
        page: Arc<dyn SessionPage>,
    ) -> ChannelSessionPointsResult {
        let initial = read_channel_points_balance_result(page.as_ref()).await;
        if matches!(initial, ChannelSessionPointsResult::Available { .. }) {
            return initial;
        }

        let claimable = match this.reward_claimer.has_claimable_bonus(page.as_ref()).await {
            Ok(claimable) => claimable,
            Err(error) => {
                this.logger.debug(
                    "points_claimable_check_failed",
                    Some(&fields(vec![
                        ("channel", sv_str(&this.channel)),
                        ("error", sv_str(safe_error_message(Some(&error.message)))),
                    ])),
                );
                false
            }
        };
        if !claimable {
            return initial;
        }

        this.logger.info(
            "points_claim_before_read",
            Some(&fields(vec![
                ("channel", sv_str(&this.channel)),
                ("readFailure", sv_str(initial.as_str().unwrap_or_default())),
            ])),
        );

        if let Err(error) = this
            .reward_claimer
            .claim_if_available(page.as_ref(), &this.channel)
            .await
        {
            this.logger.warn(
                "points_claim_before_read_failed",
                Some(&fields(vec![
                    ("channel", sv_str(&this.channel)),
                    ("error", sv_str(safe_error_message(Some(&error.message)))),
                ])),
            );
        }

        let mut last_result = initial;
        for _attempt in 1..=this.points_claim_retry_attempts {
            // 舊版用全域 setTimeout；Rust 走 tokio 計時器（測試以 paused 時鐘驅動）。
            tokio::time::sleep(Duration::from_millis(this.points_claim_retry_delay_ms)).await;
            last_result = read_channel_points_balance_result(page.as_ref()).await;
            if matches!(last_result, ChannelSessionPointsResult::Available { .. }) {
                return last_result;
            }
        }
        last_result
    }

    // ===== playback／content warning／side nav（都吞錯或分流成日誌）=====

    /// 舊版 `optimizePlayback(page)`：任何錯誤只記 debug
    /// `stream_playback_optimization_failed`。
    async fn optimize_playback(&self, page: &(dyn SessionPage + 'static)) {
        if let Err(error) = self.playback_optimizer.optimize(page, &self.channel).await {
            self.logger.debug(
                "stream_playback_optimization_failed",
                Some(&fields(vec![
                    ("channel", sv_str(&self.channel)),
                    ("error", sv_str(safe_error_message(Some(&error.message)))),
                ])),
            );
        }
    }

    /// 舊版 `acceptContentWarningIfPresent(page, reason)`。
    async fn accept_content_warning_if_present(
        &self,
        page: &(dyn SessionPage + 'static),
        reason: &'static str,
    ) -> Result<(), DriverError> {
        let result = accept_content_warning(&HealthPageView::new(page)).await?;
        match result {
            ContentWarningAcceptResult::NotPresent => Ok(()),
            ContentWarningAcceptResult::Accepted => {
                self.logger.info(
                    "content_warning_accepted",
                    Some(&fields(vec![
                        ("channel", sv_str(&self.channel)),
                        ("reason", sv_str(reason)),
                    ])),
                );
                Ok(())
            }
            ContentWarningAcceptResult::Blocked => {
                self.logger.warn(
                    "content_warning_accept_failed",
                    Some(&fields(vec![
                        ("channel", sv_str(&self.channel)),
                        ("reason", sv_str(reason)),
                    ])),
                );
                Ok(())
            }
        }
    }

    /// 舊版 `collapseSideNavIfExpanded(page, reason)`。
    async fn collapse_side_nav_if_expanded(
        &self,
        page: &(dyn SessionPage + 'static),
        reason: &'static str,
    ) {
        match collapse_side_nav_on_page(&SideNavPageView::new(page)).await {
            SideNavCollapseResult::Collapsed => {
                self.logger.debug(
                    "side_nav_collapsed",
                    Some(&fields(vec![
                        ("channel", sv_str(&self.channel)),
                        ("reason", sv_str(reason)),
                    ])),
                );
            }
            SideNavCollapseResult::Failed => {
                self.logger.debug(
                    "side_nav_collapse_skipped",
                    Some(&fields(vec![
                        ("channel", sv_str(&self.channel)),
                        ("reason", sv_str(reason)),
                    ])),
                );
            }
            SideNavCollapseResult::AlreadyCollapsed | SideNavCollapseResult::NotFound => {}
        }
    }

    // ===== timers（舊版 setTimeout 鏈 → 世代化的背景任務） =====

    /// 舊版 `startTimers()`：一組 timer（health／reward／playback／page refresh）
    /// 只在「沒有任何 timer 存在」時啟動。`schedulePageRefresh()` 在舊版是同步
    /// 設 `nextPageRefreshAtMs`，這裡在 spawn 前同步設定同值（stable jitter 對
    /// 同一輸入恆同值）。
    fn start_timers(self: &Arc<Self>) {
        let (epoch, token, next_page_refresh_at_ms) = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if state.timers_running {
                return;
            }
            state.timers_running = true;
            state.timers_epoch += 1;
            let token = Arc::new(CancellationToken::new());
            state.timers_token = Some(Arc::clone(&token));
            let next_page_refresh_at_ms = if self.config.browser.page_refresh_interval_seconds > 0.0
            {
                Some((self.now)() + page_refresh_delay(self))
            } else {
                None
            };
            state.next_page_refresh_at_ms = next_page_refresh_at_ms;
            (state.timers_epoch, token, next_page_refresh_at_ms)
        };
        tokio::spawn(health_timer_loop(
            Arc::clone(self),
            epoch,
            Arc::clone(&token),
        ));
        tokio::spawn(reward_timer_loop(
            Arc::clone(self),
            epoch,
            Arc::clone(&token),
        ));
        tokio::spawn(playback_timer_loop(
            Arc::clone(self),
            epoch,
            Arc::clone(&token),
        ));
        tokio::spawn(page_refresh_timer_loop(
            Arc::clone(self),
            epoch,
            token,
            next_page_refresh_at_ms.is_some(),
        ));
    }

    /// 舊版 `clearTimers()`：取消再生成的 timer 任務（epoch 遞增）並清掉
    /// `nextPageRefreshAtMs`。
    fn clear_timers(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.timers_running = false;
        state.timers_epoch += 1;
        if let Some(token) = state.timers_token.take() {
            token.cancel();
        }
        state.next_page_refresh_at_ms = None;
    }

    /// 舊版 `drainPageOperations()`：等待 page op 佇列的龜宮（逾時回 false）。
    async fn drain_page_operations(self: &Arc<Self>) -> bool {
        let slot = self.page_operation_queue.enqueue();
        let timeout = Duration::from_millis(self.page_operation_drain_timeout_ms);
        match tokio::time::timeout(timeout, slot.acquire()).await {
            Ok(_guard) => true,
            Err(_) => false,
        }
    }

    /// 舊版 `logMaintenanceSkipped(task, reason)`。
    fn log_maintenance_skipped(&self, task: &str, reason: &str) {
        self.logger.debug(
            "session_maintenance_skipped",
            Some(&fields(vec![
                ("channel", sv_str(&self.channel)),
                ("task", sv_str(task)),
                ("reason", sv_str(reason)),
            ])),
        );
    }

    /// 舊版 `notifyInvalidated()`：fire-and-forget；observer 拋錯記
    /// `session_invalidation_failed`。
    fn notify_invalidated(&self) {
        let Some(observer) = self.on_invalidated.clone() else {
            return;
        };
        let channel = self.channel.clone();
        let logger = Arc::clone(&self.logger);
        tokio::spawn(async move {
            if let Err(error) = observer.on_invalidated(&channel).await {
                logger.warn(
                    "session_invalidation_failed",
                    Some(&fields(vec![
                        ("channel", sv_str(&channel)),
                        ("reason", sv_str("health_failure_threshold")),
                        ("error", sv_str(safe_error_message(Some(&error.message)))),
                    ])),
                );
            }
        });
    }

    /// 舊版 `notifyPageRefresh(reason)`：fire-and-forget；observer 拋錯記
    /// `page_refresh_notification_failed`。
    fn notify_page_refresh(&self, reason: ChannelSessionRefreshReason) {
        let Some(observer) = self.on_page_refresh.clone() else {
            return;
        };
        let event = ChannelSessionRefreshEvent {
            channel: self.channel.clone(),
            reason,
            started_at: format_iso_millis((self.now)()),
        };
        let logger = Arc::clone(&self.logger);
        tokio::spawn(async move {
            if let Err(error) = observer.on_page_refresh(&event).await {
                logger.warn(
                    "page_refresh_notification_failed",
                    Some(&fields(vec![
                        ("channel", sv_str(&event.channel)),
                        ("reason", sv_str(event.reason.as_str())),
                        ("error", sv_str(safe_error_message(Some(&error.message)))),
                    ])),
                );
            }
        });
    }

    /// 舊版 `abortSessionOperations(reason)`：第一次 abort 才記
    /// `session_operation_aborted`（`controller.signal.aborted` 的守衛）。
    fn abort_session_operations(&self, reason: &str) {
        let abort = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .operation_abort
            .clone();
        // 舊版：controller undefined（start 前的 abort 呼叫）直接返回。
        let Some(abort) = abort else {
            return;
        };
        if abort.load(Ordering::SeqCst) {
            return;
        }
        abort.store(true, Ordering::SeqCst);
        self.logger.debug(
            "session_operation_aborted",
            Some(&fields(vec![
                ("channel", sv_str(&self.channel)),
                ("reason", sv_str(reason)),
                ("outcome", sv_str("aborted")),
            ])),
        );
    }

    // ===== 獎勵領取 =====

    /// 舊版 `runRewardClaim()`。
    async fn run_reward_claim(this: &Arc<Inner>) -> RewardClaimResult {
        let (page, _should) = read_page_and_should(this);
        let state_now = read_current(this);
        let eligable = match page.as_ref() {
            Some(page) => {
                !page.is_closed()
                    && is_expected_channel_url(&page.url(), &this.target_url)
                    && matches!(
                        state_now,
                        ChannelSessionState::Watching | ChannelSessionState::Recovering
                    )
            }
            None => false,
        };
        // 舊版：page missing／closed／URL 偏離／非 watching・recovering → skipped。
        let Some(page) = page else {
            log_maintenance_skipped(this, "reward", "page_unavailable");
            return RewardClaimResult::NotFound {
                channel: this.channel.clone(),
                checked_at: format_iso_millis((this.now)()),
            };
        };
        if !eligable {
            log_maintenance_skipped(this, "reward", "page_unavailable");
            return RewardClaimResult::NotFound {
                channel: this.channel.clone(),
                checked_at: format_iso_millis((this.now)()),
            };
        }

        let result = match this
            .reward_claimer
            .claim_if_available(page.as_ref(), &this.channel)
            .await
        {
            Ok(result) => result,
            Err(error) => {
                let checked_at = format_iso_millis((this.now)());
                let safe_error = safe_error_message(Some(&error.message));
                this.logger.warn(
                    LogEvents::REWARD_CLAIM_FAILED,
                    Some(&fields(vec![
                        ("channel", sv_str(&this.channel)),
                        ("checkedAt", sv_str(&checked_at)),
                        ("error", sv_str(&safe_error)),
                    ])),
                );
                RewardClaimResult::ClickFailed {
                    channel: this.channel.clone(),
                    checked_at,
                    error: safe_error,
                }
            }
        };

        // 舊版：claim 之後再確認一次狀態才套用復原副作用。
        if should_schedule_work(
            &this
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        ) {
            Inner::handle_reward_claim_result(this, Arc::clone(&page), result.clone()).await;
        }
        result
    }

    /// 舊版 `handleRewardClaimResult(page, result)`。
    async fn handle_reward_claim_result(
        this: &Arc<Inner>,
        page: Arc<dyn SessionPage>,
        result: RewardClaimResult,
    ) {
        match &result {
            RewardClaimResult::Claimed { .. } | RewardClaimResult::NotFound { .. } => {
                let mut state = this
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                reset_reward_failure_recovery(&mut state);
            }
            RewardClaimResult::ClickFailed { error, .. } => {
                let (consecutive, refresh_attempted) = {
                    let mut state = this
                        .state
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    state.consecutive_reward_failures += 1;
                    (
                        state.consecutive_reward_failures,
                        state.reward_failure_refresh_attempted,
                    )
                };
                if (consecutive as u64) < REWARD_FAILURE_RECOVERY_THRESHOLD {
                    return;
                }
                this.logger.warn(
                    LogEvents::REWARD_CLAIM_FAILURE_THRESHOLD,
                    Some(&fields(vec![
                        ("channel", sv_str(&this.channel)),
                        ("consecutiveFailures", sv_num(consecutive)),
                        (
                            "threshold",
                            sv_num(REWARD_FAILURE_RECOVERY_THRESHOLD as f64),
                        ),
                        ("recoveryRefreshAttempted", sv_bool(refresh_attempted)),
                        ("error", sv_str(error)),
                    ])),
                );
                if !refresh_attempted {
                    Self::refresh_after_reward_failures(this, page).await;
                    return;
                }
                this.request_container_restart_after_reward_failures();
            }
        }
    }

    /// 舊版 `refreshAfterRewardFailures(page)`。
    async fn refresh_after_reward_failures(this: &Arc<Inner>, page: Arc<dyn SessionPage>) {
        {
            let mut state = this
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.reward_failure_refresh_attempted = true;
        }
        this.logger.warn(
            LogEvents::REWARD_CLAIM_FAILURE_RECOVERY_REFRESH,
            Some(&fields(vec![
                ("channel", sv_str(&this.channel)),
                (
                    "threshold",
                    sv_num(REWARD_FAILURE_RECOVERY_THRESHOLD as f64),
                ),
                ("reason", sv_str("reward_claim_failure_threshold")),
            ])),
        );

        if page.is_closed() {
            this.request_container_restart_after_reward_failures();
            return;
        }

        this.notify_page_refresh(ChannelSessionRefreshReason::RewardClaimFailureThreshold);
        match this
            .reload_during_page_operation(page, "reward_claim_failure_threshold")
            .await
        {
            Ok(()) => {
                {
                    let mut state = this
                        .state
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    state.consecutive_reward_failures = 0;
                }
                this.logger.info(
                    "page_refreshed",
                    Some(&fields(vec![
                        ("channel", sv_str(&this.channel)),
                        ("reason", sv_str("reward_claim_failure_threshold")),
                    ])),
                );
            }
            Err(error) => {
                this.logger.warn(
                    "page_reload_failed",
                    Some(&fields(vec![
                        ("channel", sv_str(&this.channel)),
                        ("reason", sv_str("reward_claim_failure_threshold")),
                        ("error", sv_str(safe_error_message(Some(&error.message)))),
                    ])),
                );
                this.request_container_restart_after_reward_failures();
            }
        }
    }

    /// 舊版 `requestContainerRestartAfterRewardFailures()`（observer 缺席時記
    /// canonical `container_restart_requested`，source＝channel_session_reward_no_observer）。
    fn request_container_restart_after_reward_failures(self: &Arc<Self>) {
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if state.container_restart_requested {
                return;
            }
            state.container_restart_requested = true;
        }
        let consecutive = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .consecutive_reward_failures;
        let request = ContainerRestartRequest {
            channel: self.channel.clone(),
            reason: "reward_claim_failure_after_refresh",
            consecutive_failures: consecutive,
            requested_at: format_iso_millis((self.now)()),
        };
        // Canonical container_restart_requested is emitted once by
        // ContainerRestartController; session only escalates via observer.
        // （session 只經 observer 升級，無 observer 時 fallback 記 error。）
        let Some(observer) = self.on_container_restart_requested.clone() else {
            self.logger.error(
                LogEvents::CONTAINER_RESTART_REQUESTED,
                Some(&fields(vec![
                    ("channel", sv_str(&request.channel)),
                    ("reason", sv_str(request.reason)),
                    ("consecutiveFailures", sv_num(request.consecutive_failures)),
                    ("requestedAt", sv_str(&request.requested_at)),
                    ("source", sv_str("channel_session_reward_no_observer")),
                ])),
            );
            return;
        };

        let logger = Arc::clone(&self.logger);
        let request_channel = request.channel.clone();
        tokio::spawn(async move {
            if let Err(error) = observer.on_container_restart_requested(&request).await {
                logger.error(
                    "container_restart_request_failed",
                    Some(&fields(vec![
                        ("channel", sv_str(&request_channel)),
                        ("reason", sv_str(request.reason)),
                        ("error", sv_str(safe_error_message(Some(&error.message)))),
                    ])),
                );
            }
        });
    }
}

/// 舊版 `ChannelSession` 介面（S3.5 的 SessionManager 以此呼叫）。
#[async_trait]
pub(crate) trait ChannelSession: Send + Sync {
    /// 舊版 `channel`。
    fn channel(&self) -> &str;

    /// 舊版 `get state()`。
    fn state(&self) -> ChannelSessionState;

    /// 舊版 `start()`。`cancelled` 的錯誤訊息同舊版各分支。
    async fn start(&self) -> Result<(), DriverError>;

    /// 舊版 `cancelStart?(reason)`。
    async fn cancel_start(&self, reason: &str);

    /// 舊版 `stop(reason)`。
    async fn stop(&self, reason: &str) -> Result<(), DriverError>;

    /// 舊版 `checkHealth()`（與舊版一樣永不拋錯）。
    async fn check_health(&self) -> ChannelHealthResult;

    /// 舊版 `tickRewardClaim()`（與舊版一樣永不拋錯）。
    async fn tick_reward_claim(&self) -> RewardClaimResult;

    /// 舊版 `captureScreenshot()`。
    async fn capture_screenshot(&self) -> Result<Vec<u8>, DriverError>;

    /// 舊版 `getChannelPoints()`。
    async fn get_channel_points(&self) -> ChannelSessionPointsResult;

    /// 舊版 `getRefreshStatus()`。
    fn get_refresh_status(&self) -> ChannelSessionRefreshStatus;

    /// 舊版 `refreshNow()`。
    async fn refresh_now(&self) -> Result<bool, DriverError>;
}

/// 可異動的 session 狀態；一律不在鎖內 await。
struct SessionState {
    /// 舊版 `currentState`（建構時為 `Stopped`）。
    current: ChannelSessionState,
    page: Option<Arc<dyn SessionPage>>,
    consecutive_health_failures: u32,
    consecutive_reward_failures: u32,
    reward_failure_refresh_attempted: bool,
    container_restart_requested: bool,
    /// 舊版四個 `*Timer !== undefined` 的合成（一組 timer 一起存在）。
    timers_running: bool,
    /// `clearTimers`／`startTimers` 遞增；timer 任務以 epoch 識別自己的世代。
    timers_epoch: u64,
    timers_token: Option<Arc<CancellationToken>>,
    next_page_refresh_at_ms: Option<u64>,
    start_cancellation_reason: Option<String>,
    /// 舊版 `operationAbortController`（`signal` 以共享旗標表達）。
    operation_abort: Option<Arc<AtomicBool>>,
    // 舊版 flight promise 的槽位（每個 kind 一個共用 cell）。
    health_flight: Option<Arc<FlightCell<ChannelHealthResult>>>,
    reward_flight: Option<Arc<FlightCell<RewardClaimResult>>>,
    points_flight: Option<Arc<FlightCell<ChannelSessionPointsResult>>>,
    reload_flight: Option<Arc<FlightCell<Result<(), DriverError>>>>,
}

struct Inner {
    // options（建構時凍結）
    channel: String,
    target_url: String,
    config: ChannelSessionConfig,
    browser_manager: Arc<dyn SessionBrowserManager>,
    reward_claimer: Arc<dyn SessionRewardClaimer>,
    playback_optimizer: Arc<dyn SessionStreamPlaybackOptimizer>,
    logger: Arc<SessionLogger>,
    health_failure_threshold: u64,
    health_evaluator: Arc<dyn ChannelHealthEvaluator>,
    on_invalidated: Option<Arc<dyn ChannelSessionInvalidationObserver>>,
    on_page_refresh: Option<Arc<dyn ChannelSessionRefreshObserver>>,
    on_container_restart_requested: Option<Arc<dyn ContainerRestartRequestObserver>>,
    page_operation_drain_timeout_ms: u64,
    points_claim_retry_attempts: u64,
    points_claim_retry_delay_ms: u64,
    now: SessionClock,

    state: Mutex<SessionState>,
    /// 舊版 `lifecycleTail`。
    lifecycle_queue: Arc<ExclusiveQueue>,
    /// 舊版 `pageOperationTail`。
    page_operation_queue: Arc<ExclusiveQueue>,
}

/// 舊版 `DefaultChannelSession`（`Clone` 供測試 spawn 的調用；內部僅 `Arc`）。
#[derive(Clone)]
pub(crate) struct DefaultChannelSession {
    inner: Arc<Inner>,
}

impl DefaultChannelSession {
    /// 舊版 `DefaultChannelSession` 建構函式（channel 驗證失敗拋 `Error` → Rust 回 `Err`）。
    pub(crate) fn new(options: DefaultChannelSessionOptions) -> Result<Self, DriverError> {
        let DefaultChannelSessionOptions {
            channel,
            config,
            browser_manager,
            reward_claimer,
            playback_optimizer,
            logger,
            health_failure_threshold,
            health_evaluator,
            on_invalidated,
            on_page_refresh,
            on_container_restart_requested,
            page_operation_drain_timeout_ms,
            points_claim_retry_attempts,
            points_claim_retry_delay_ms,
            now,
        } = options;
        validate_configured_channel(&channel, &config.channels)?;
        let target_url = create_channel_url(&channel)?;
        let logger = logger.unwrap_or_else(|| Arc::new(NoopSessionLogger) as Arc<SessionLogger>);
        let playback_optimizer = playback_optimizer.unwrap_or_else(|| {
            Arc::new(DefaultSessionPlaybackOptimizer {
                config: StreamPlaybackOptimizerConfig::from(&config.browser),
                logger: Arc::clone(&logger),
            }) as Arc<dyn SessionStreamPlaybackOptimizer>
        });
        let inner = Arc::new(Inner {
            channel,
            target_url,
            config,
            browser_manager,
            reward_claimer,
            playback_optimizer,
            logger,
            health_failure_threshold: positive_integer(
                health_failure_threshold,
                DEFAULT_HEALTH_FAILURE_THRESHOLD,
            ),
            health_evaluator: health_evaluator
                .unwrap_or_else(|| Arc::new(DefaultChannelHealthEvaluator)),
            on_invalidated,
            on_page_refresh,
            on_container_restart_requested,
            page_operation_drain_timeout_ms: positive_integer(
                page_operation_drain_timeout_ms,
                DEFAULT_PAGE_OPERATION_DRAIN_TIMEOUT_MS,
            ),
            points_claim_retry_attempts: positive_integer(
                points_claim_retry_attempts,
                DEFAULT_POINTS_CLAIM_RETRY_ATTEMPTS,
            ),
            points_claim_retry_delay_ms: positive_integer(
                points_claim_retry_delay_ms,
                DEFAULT_POINTS_CLAIM_RETRY_DELAY_MS,
            ),
            now: now.unwrap_or_else(|| Arc::new(default_now_ms) as SessionClock),
            state: Mutex::new(SessionState {
                current: ChannelSessionState::Stopped,
                page: None,
                consecutive_health_failures: 0,
                consecutive_reward_failures: 0,
                reward_failure_refresh_attempted: false,
                container_restart_requested: false,
                timers_running: false,
                timers_epoch: 0,
                timers_token: None,
                next_page_refresh_at_ms: None,
                start_cancellation_reason: None,
                operation_abort: None,
                health_flight: None,
                reward_flight: None,
                points_flight: None,
                reload_flight: None,
            }),
            lifecycle_queue: ExclusiveQueue::new(),
            page_operation_queue: ExclusiveQueue::new(),
        });
        Ok(Self { inner })
    }
}

#[async_trait]
impl ChannelSession for DefaultChannelSession {
    fn channel(&self) -> &str {
        &self.inner.channel
    }

    fn state(&self) -> ChannelSessionState {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .current
    }

    async fn start(&self) -> Result<(), DriverError> {
        Inner::start(&self.inner).await
    }

    async fn cancel_start(&self, reason: &str) {
        Inner::cancel_start(&self.inner, reason).await;
    }

    async fn stop(&self, reason: &str) -> Result<(), DriverError> {
        Inner::stop(&self.inner, reason).await
    }

    async fn check_health(&self) -> ChannelHealthResult {
        Inner::check_health(&self.inner).await
    }

    async fn tick_reward_claim(&self) -> RewardClaimResult {
        Inner::tick_reward_claim(&self.inner).await
    }

    async fn capture_screenshot(&self) -> Result<Vec<u8>, DriverError> {
        Inner::capture_screenshot(&self.inner).await
    }

    async fn get_channel_points(&self) -> ChannelSessionPointsResult {
        Inner::get_channel_points(&self.inner).await
    }

    fn get_refresh_status(&self) -> ChannelSessionRefreshStatus {
        Inner::get_refresh_status(&self.inner)
    }

    async fn refresh_now(&self) -> Result<bool, DriverError> {
        Inner::refresh_now(&self.inner).await
    }
}

// ---------------------------------------------------------------------------
// timer 迴圈（舊版 scheduleHealthCheck／scheduleRewardCheck／
// schedulePlaybackOptimization／schedulePageRefresh 的 setTimeout 鏈；再排程 =
// 迴圈下一輪、`!shouldScheduleWork()` 即鏈結束，世代以 epoch 識別）
// ---------------------------------------------------------------------------

/// 舊版 `scheduleHealthCheck()` 的鏈。callback 的 catch 分支
/// （`page_health_timer_failed`）在 Rust 的 `checkHealth` 永不拒絕下不可達（回報有註記）。
async fn health_timer_loop(inner: Arc<Inner>, epoch: u64, token: Arc<CancellationToken>) {
    loop {
        let delay = {
            let state = inner
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if state.timers_epoch != epoch || !should_schedule_work(&state) {
                break;
            }
            timer_delay(
                &inner,
                inner.config.browser.page_health_check_interval_seconds,
                "health",
            )
        };
        tokio::select! {
            _ = token.cancelled() => break,
            _ = tokio::time::sleep(Duration::from_millis(delay)) => {}
        }
        Inner::check_health(&inner).await;
    }
}

/// 舊版 `scheduleRewardCheck()` 的鏈（`reward_timer_failed` 分支不可達，同上）。
async fn reward_timer_loop(inner: Arc<Inner>, epoch: u64, token: Arc<CancellationToken>) {
    loop {
        let delay = {
            let state = inner
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if state.timers_epoch != epoch || !should_schedule_work(&state) {
                break;
            }
            timer_delay(
                &inner,
                inner.config.browser.reward_check_interval_seconds,
                "reward",
            )
        };
        tokio::select! {
            _ = token.cancelled() => break,
            _ = tokio::time::sleep(Duration::from_millis(delay)) => {}
        }
        Inner::tick_reward_claim(&inner).await;
    }
}

/// 舊版 `schedulePlaybackOptimization()` 的鏈：到點先讀 page；缺頁（或已關閉）
/// 記 skipped，否則排入 page operation（operation 內還會再次確認 page 世代）。
async fn playback_timer_loop(inner: Arc<Inner>, epoch: u64, token: Arc<CancellationToken>) {
    loop {
        let delay = {
            let state = inner
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if state.timers_epoch != epoch || !should_schedule_work(&state) {
                break;
            }
            timer_delay(
                &inner,
                inner.config.browser.enforce_stream_quality_seconds,
                "playback",
            )
        };
        tokio::select! {
            _ = token.cancelled() => break,
            _ = tokio::time::sleep(Duration::from_millis(delay)) => {}
        }
        let (page, _should) = read_page_and_should(&inner);
        match page {
            None => {
                inner.log_maintenance_skipped("playback", "page_unavailable");
            }
            Some(page) if page.is_closed() => {
                inner.log_maintenance_skipped("playback", "page_unavailable");
            }
            Some(page) => {
                let this = Arc::clone(&inner);
                let queued = Arc::clone(&this);
                let _ = Inner::run_page_operation(&this, "playback", async move {
                    let (current, should) = read_page_and_should(&queued);
                    let valid = match current.as_ref() {
                        // 舊版：`this.page !== page || page.isClosed() || !this.shouldScheduleWork()`
                        Some(current) => {
                            session_page_same(current, &page) && !current.is_closed() && should
                        }
                        None => false,
                    };
                    if !valid {
                        queued.log_maintenance_skipped("playback", "page_unavailable");
                        return Ok(());
                    }
                    queued.optimize_playback(page.as_ref()).await;
                    Ok(())
                })
                .await;
            }
        }
    }
}

/// 舊版 `schedulePageRefresh()` 的鏈：排程時先設 `nextPageRefreshAtMs`，到點
/// 清掉並跑 `refreshPage()`（失敗的 catch 記 `page_refresh_failed`）。
async fn page_refresh_timer_loop(
    inner: Arc<Inner>,
    epoch: u64,
    token: Arc<CancellationToken>,
    scheduled_at_start: bool,
) {
    // 舊版 `schedulePageRefresh` 在 startTimers 已同步設 `nextPageRefreshAtMs`
    // 與 timer；本迴圈的第一次 sleep 沿用同樣的 delay（stable jitter 同值）。
    // `first_iteration` 之後，下一輪的排程才回填（fire → finally → schedule）。
    let mut first_iteration = scheduled_at_start;
    loop {
        let delay = {
            let mut state = inner
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if state.timers_epoch != epoch {
                break;
            }
            if !should_schedule_work(&state) {
                break;
            }
            if inner.config.browser.page_refresh_interval_seconds <= 0.0 {
                break;
            }
            let delay = page_refresh_delay(&inner);
            if first_iteration {
                first_iteration = false;
            } else {
                state.next_page_refresh_at_ms = Some((inner.now)() + delay);
            }
            delay
        };
        tokio::select! {
            _ = token.cancelled() => break,
            _ = tokio::time::sleep(Duration::from_millis(delay)) => {}
        }
        {
            let mut state = inner
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if state.timers_epoch == epoch {
                state.next_page_refresh_at_ms = None;
            }
        }
        if let Err(error) = refresh_scheduled_page(&inner).await {
            inner.logger.warn(
                "page_refresh_failed",
                Some(&fields(vec![
                    ("channel", sv_str(&inner.channel)),
                    ("error", sv_str(safe_error_message(Some(&error.message)))),
                ])),
            );
        }
    }
}

/// 舊版 `refreshPage()`（scheduled 分支；page 不可用時靜默返回）。
async fn refresh_scheduled_page(this: &Arc<Inner>) -> Result<(), DriverError> {
    let (page, should) = read_page_and_should(this);
    let page = match page {
        Some(page) if !page.is_closed() && should => page,
        _ => return Ok(()),
    };
    this.notify_page_refresh(ChannelSessionRefreshReason::ScheduledRefresh);
    this.reload_page(page, "scheduled_refresh").await?;
    this.logger.info(
        "page_refreshed",
        Some(&fields(vec![
            ("channel", sv_str(&this.channel)),
            (
                "reason",
                sv_str(ChannelSessionRefreshReason::ScheduledRefresh.as_str()),
            ),
        ])),
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// free helpers（舊版 module 層函式與 Windows／欄位建構子）
// ---------------------------------------------------------------------------

/// flight slot 的種類（health／reward／points 的 [`OnceCell`] 型別不同，以
/// enum 分派共用程式）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FlightSlotKind {
    Health,
    Reward,
    Points,
}

/// 取得／登記 flight slot：`Some(existing)` 有進行中的 flight；`None` 表示
/// 登記權在手（cell 已寫回 slot）。
fn claim_flight_slot<T: Clone>(
    slot: &mut Option<Arc<FlightCell<T>>>,
    cell: Arc<FlightCell<T>>,
) -> Option<Arc<FlightCell<T>>> {
    if let Some(existing) = slot.as_ref() {
        return Some(Arc::clone(existing));
    }
    *slot = Some(cell);
    None
}

/// flight 完成的清槽（僅當 slot 仍是同一 cell；舊版 `if (this.X === flight)…`）。
fn release_flight_slot<T: Clone>(slot: &mut Option<Arc<FlightCell<T>>>, cell: &Arc<FlightCell<T>>) {
    if slot
        .as_ref()
        .map(|entry| Arc::ptr_eq(entry, cell))
        .unwrap_or(false)
    {
        *slot = None;
    }
}

/// 舊版 flight promise 的共用語意：第一個 executor 是唯一的 setter，後到者
/// 等待同一個結果（`watch` channel 的變化通知；`T: Clone`）。
pub(crate) struct FlightCell<T: Clone> {
    sender: tokio::sync::watch::Sender<Option<T>>,
}

impl<T: Clone> FlightCell<T> {
    fn new() -> Arc<Self> {
        let (sender, _receiver) = tokio::sync::watch::channel(None);
        Arc::new(Self { sender })
    }

    /// 等待 flight 的結果（對應 `await` 舊版的共用 promise；已發送時立即返回）。
    async fn take(&self) -> T {
        let mut receiver = self.sender.subscribe();
        loop {
            if let Some(value) = receiver.borrow().clone() {
                return value;
            }
            // `Err` 只在 sender 全部消失時發生；waiters 自持 Arc，sender 與
            // slot 至少活到 waiter 結束，故此分支理論上不可達（runtime 收尾前）。
            if receiver.changed().await.is_err() {
                continue;
            }
        }
    }

    /// 發送 flight 的結果（單一 executor；對應 promise resolve）。
    fn set(&self, value: T) {
        let _ = self.sender.send(Some(value));
    }
}

/// 鎖內快照：目前 page 把手與 `shouldScheduleWork()`。
fn read_page_and_should(this: &Inner) -> (Option<Arc<dyn SessionPage>>, bool) {
    let state = this
        .state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    (state.page.clone(), should_schedule_work(&state))
}

/// 鎖內快照：目前狀態。
fn read_current(this: &Inner) -> ChannelSessionState {
    this.state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .current
}

/// 舊版 `shouldScheduleWork()`：watching 或 recovering。
fn should_schedule_work(state: &SessionState) -> bool {
    matches!(
        state.current,
        ChannelSessionState::Watching | ChannelSessionState::Recovering
    )
}

/// 舊版 `this.page !== page` 的識別相等（Arc 資料指標）。
fn session_page_same(left: &Arc<dyn SessionPage>, right: &Arc<dyn SessionPage>) -> bool {
    Arc::ptr_eq(left, right)
}

/// 舊版 `resetRewardFailureRecovery()`。
fn reset_reward_failure_recovery(state: &mut SessionState) {
    state.consecutive_reward_failures = 0;
    state.reward_failure_refresh_attempted = false;
    state.container_restart_requested = false;
}

/// 舊版 `throwIfStartCancelled()`。
fn throw_if_start_cancelled(state: &Mutex<SessionState>) -> Result<(), DriverError> {
    let guard = state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(reason) = &guard.start_cancellation_reason {
        return Err(DriverError::new(
            "Error",
            format!("Session start cancelled: {reason}"),
        ));
    }
    Ok(())
}

/// free 版的 `logMaintenanceSkipped`（timer 迴圈以 `&Inner` 呼叫）。
fn log_maintenance_skipped(this: &Inner, task: &str, reason: &str) {
    this.log_maintenance_skipped(task, reason);
}

/// 舊版 `timerDelay(intervalSeconds, task)`。
fn timer_delay(inner: &Inner, interval_seconds: f64, task: &str) -> u64 {
    let interval_ms = interval_seconds * 1_000.0;
    let jitter_ms = stable_jitter(&format!("{}:{}", inner.channel, task), MAX_TIMER_JITTER_MS);
    cap_timer_delay(interval_ms + (jitter_ms as f64))
}

/// 舊版 `pageRefreshDelay()`。
fn page_refresh_delay(inner: &Inner) -> u64 {
    let interval_ms = inner.config.browser.page_refresh_interval_seconds * 1_000.0;
    // Math.min(60_000, Math.max(5_000, Math.floor(intervalMs * 0.2)))。
    let refresh_jitter_ms = (MAX_TIMER_JITTER_MS as f64)
        .max((interval_ms * 0.2).floor())
        .min(MAX_PAGE_REFRESH_JITTER_MS as f64) as u64;
    let jitter_ms = stable_jitter(
        &format!("{}:page-refresh", inner.channel),
        refresh_jitter_ms,
    );
    cap_timer_delay(interval_ms + (jitter_ms as f64))
}

/// 舊版 `Math.min(MAX_TIMER_DELAY_MS, value)`（Node timer 毫秒上限；設定驗證
/// 保證為有限正值，NaN 飽和為 0 保底）。
fn cap_timer_delay(value: f64) -> u64 {
    if value.is_nan() {
        return 0;
    }
    if value >= MAX_TIMER_DELAY_MS as f64 {
        return MAX_TIMER_DELAY_MS;
    }
    value.max(0.0) as u64
}

/// `health` 的頁面視圖（從 session 的 fake-able page 轉接，D12）。
struct HealthPageView<'a> {
    page: &'a (dyn SessionPage + 'static),
}

impl<'a> HealthPageView<'a> {
    fn new(page: &'a (dyn SessionPage + 'static)) -> Self {
        Self { page }
    }
}

#[async_trait]
impl HealthPage for HealthPageView<'_> {
    fn is_closed(&self) -> bool {
        self.page.is_closed()
    }

    fn url(&self) -> String {
        self.page.url()
    }

    async fn evaluate(
        &self,
        fn_declaration: &str,
        arg: Option<Value>,
    ) -> Result<Value, DriverError> {
        self.page.evaluate(fn_declaration, arg).await
    }

    fn locator(&self, selector: &str) -> Arc<dyn HealthLocator> {
        Arc::new(HealthLocatorView {
            inner: self.page.locator(selector),
        })
    }
}

struct HealthLocatorView {
    inner: Arc<dyn SessionLocator>,
}

#[async_trait]
impl HealthLocator for HealthLocatorView {
    fn first(&self) -> Arc<dyn HealthLocator> {
        Arc::new(HealthLocatorView {
            inner: self.inner.first(),
        })
    }

    async fn is_visible(&self) -> Result<bool, DriverError> {
        self.inner.is_visible().await
    }

    async fn click(&self, timeout: Option<u64>) -> Result<(), DriverError> {
        self.inner.click(timeout).await
    }

    async fn wait_visible(&self, timeout: Option<u64>) -> Result<(), DriverError> {
        self.inner.wait_visible(timeout).await
    }
}

/// `side_nav` 的頁面視圖。
struct SideNavPageView<'a> {
    page: &'a (dyn SessionPage + 'static),
}

impl<'a> SideNavPageView<'a> {
    fn new(page: &'a (dyn SessionPage + 'static)) -> Self {
        Self { page }
    }
}

impl SideNavPage for SideNavPageView<'_> {
    fn locator(&self, selector: &str) -> Arc<dyn SideNavLocator> {
        Arc::new(SideNavLocatorView {
            inner: self.page.locator(selector),
        })
    }
}

struct SideNavLocatorView {
    inner: Arc<dyn SessionLocator>,
}

#[async_trait]
impl SideNavLocator for SideNavLocatorView {
    fn first(&self) -> Arc<dyn SideNavLocator> {
        Arc::new(SideNavLocatorView {
            inner: self.inner.first(),
        })
    }

    async fn wait_for(&self, timeout: Option<u64>) -> Result<(), DriverError> {
        self.inner.wait_visible(timeout).await
    }

    async fn evaluate(
        &self,
        fn_declaration: &str,
        arg: Option<Value>,
    ) -> Result<Value, DriverError> {
        self.inner.evaluate(fn_declaration, arg).await
    }

    async fn click(&self, timeout: Option<u64>) -> Result<(), DriverError> {
        self.inner.click(timeout).await
    }
}

/// `channel_points_reader` 的頁面視圖（對應舊版 `page.evaluateAll` 的批次讀取）。
struct PointsBalanceView<'a> {
    page: &'a (dyn SessionPage + 'static),
}

impl<'a> PointsBalanceView<'a> {
    fn new(page: &'a (dyn SessionPage + 'static)) -> Self {
        Self { page }
    }
}

#[async_trait]
impl ChannelPointsBalancePage for PointsBalanceView<'_> {
    async fn evaluate_all(&self) -> Result<Value, DriverError> {
        self.page
            .locator(COMMUNITY_POINTS_BALANCE_SELECTOR)
            .evaluate_all(channel_points_balance_script())
            .await
    }
}

/// 舊版 `readChannelPointsBalance(page)` 的回傳值折為 session 的 points 結果
/// （Rust 契約只回 balance，`displayValue` 省略）。
async fn read_channel_points_balance_result(
    page: &(dyn SessionPage + 'static),
) -> ChannelSessionPointsResult {
    match read_balance_on_page(&PointsBalanceView::new(page)).await {
        Ok(balance) => ChannelSessionPointsResult::Available { balance },
        Err(BalanceError::NotFound) => ChannelSessionPointsResult::NotFound,
        Err(BalanceError::ParseFailed) => ChannelSessionPointsResult::ParseFailed,
    }
}

/// 舊版 `validateConfiguredChannel(channel, configuredChannels)`。
fn validate_configured_channel(
    channel: &str,
    configured_channels: &[String],
) -> Result<(), DriverError> {
    if !channel_pattern(channel) {
        return Err(DriverError::new(
            "Error",
            "Channel 必須是 1 到 25 字元的英數字或底線",
        ));
    }
    // 舊版 `toLocaleLowerCase('en-US')`：channel 已通過 pattern（ASCII）；已
    // 驗證清單照設定驗證的承諾亦為頻道名（此處用 locale 無關的 to_lowercase）。
    let normalized = channel.to_lowercase();
    let is_configured = configured_channels
        .iter()
        .any(|configured| configured.to_lowercase() == normalized);
    if !is_configured {
        return Err(DriverError::new(
            "Error",
            format!("Channel \"{channel}\" 不在已驗證的設定清單中"),
        ));
    }
    Ok(())
}

/// 舊版 flight promise 的共用語意：先到者執行並登記共用的 [`FlightCell`]，
/// 後到者等待同一個 cell（`T: Clone`）。slot 的讀寫由呼叫端以 closure 注入
/// （各自對應 health／reward／points 的不同結果型別）。
async fn flight_take_existing<T, Claim, Release, Fut>(
    inner: &Arc<Inner>,
    task: &'static str,
    claim_slot: Claim,
    release_slot: Release,
    operation: impl FnOnce(Arc<Inner>) -> Fut + Send,
) -> T
where
    T: Clone + Send,
    Claim: FnOnce(&mut SessionState, &Arc<FlightCell<T>>) -> Option<Arc<FlightCell<T>>> + Send,
    Release: FnOnce(&mut SessionState, &Arc<FlightCell<T>>) + Send,
    Fut: Future<Output = T> + Send,
{
    let cell = FlightCell::<T>::new();
    let existing = {
        let mut state = inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        claim_slot(&mut state, &cell)
    };
    if let Some(existing) = existing {
        return existing.take().await;
    }
    let this = Arc::clone(inner);
    let result =
        Inner::run_page_operation_infallible(inner, task, async move { operation(this).await })
            .await;
    {
        let mut state = inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        release_slot(&mut state, &cell);
    }
    cell.set(result.clone());
    result
}

/// 舊版預設 `now`（`() => new Date()`）：epoch 毫秒。
fn default_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

/// 舊版 `toISOString()` 的格式：UTC 的 `YYYY-MM-DDTHH:MM:SS.sssZ`。
const ISO_UTC_FORMAT: StaticFormatDescription =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z");

/// epoch 毫秒 → `toISOString()` 字串（`now` 注入的契約是 epoch 毫秒，
/// u64 全域皆在 time crate 可表示範圍，兩個 `expect` 為型別邊界的不變式）。
fn format_iso_millis(unix_millis: u64) -> String {
    let datetime = OffsetDateTime::from_unix_timestamp_nanos(i128::from(unix_millis) * 1_000_000)
        .expect("session clock returned an invalid timestamp");
    datetime
        .format(&ISO_UTC_FORMAT)
        .expect("session clock timestamp must be representable")
}

/// 舊版 performance.now 差值經 `Math.round` 的毫秒數。
fn round_ms(duration: Duration) -> f64 {
    (duration.as_secs_f64() * 1_000.0).round()
}

// ---------------------------------------------------------------------------
// 日誌欄位建構子（同 manager 模組的小型 helper；插入順序照呼叫端，R10）
// ---------------------------------------------------------------------------

fn sv_str(value: impl Into<String>) -> SanitizedValue {
    SanitizedValue::String(value.into())
}

fn sv_num(value: impl Into<f64>) -> SanitizedValue {
    SanitizedValue::Number(value.into())
}

fn sv_bool(value: bool) -> SanitizedValue {
    SanitizedValue::Bool(value)
}

/// 組日誌欄位物件（插入順序照舊版）。
fn fields(entries: Vec<(&'static str, SanitizedValue)>) -> SanitizedValue {
    SanitizedValue::Object(
        entries
            .into_iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect(),
    )
}

/// 舊版預設的 `NOOP_LOGGER`。
struct NoopSessionLogger;

impl BrowserManagerLogger for NoopSessionLogger {
    fn debug(&self, _event: &str, _fields: Option<&SanitizedValue>) {}
    fn info(&self, _event: &str, _fields: Option<&SanitizedValue>) {}
    fn warn(&self, _event: &str, _fields: Option<&SanitizedValue>) {}
    fn error(&self, _event: &str, _fields: Option<&SanitizedValue>) {}
}

#[cfg(test)]
mod tests;
