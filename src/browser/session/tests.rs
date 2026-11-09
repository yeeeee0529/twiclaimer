//! `DefaultChannelSession` 的單元測試：移植舊版 `test/unit/channel-session.test.ts`
//! 的 38 個舊測試（TSV 逐字標題），另含一個 TSV 未收錄的 `it.each`（註解標明來源行號）。
//!
//! fake 照舊測試的 `createMockPage()`／`createBrowserManager()`／
//! `createRewardClaimer()`／`createLogger()` 逐一對應；`now` 以 `SessionClock`
//! 固定值注入；時間相關的測試一律 `#[tokio::test(start_paused = true)]`。
use std::collections::VecDeque;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{
    ChannelSession, ChannelSessionConfig, ChannelSessionInvalidationObserver,
    ChannelSessionPointsResult, ChannelSessionRefreshEvent, ChannelSessionRefreshObserver,
    ChannelSessionState, ContainerRestartRequest, ContainerRestartRequestObserver,
    DefaultChannelSession, DefaultChannelSessionFactory, DefaultChannelSessionFactoryOptions,
    DefaultChannelSessionOptions, REWARD_FAILURE_RECOVERY_THRESHOLD, SessionBrowserManager,
    SessionLocator, SessionPage, SessionRewardClaimer,
};
use crate::browser::cdp::context::DriverError;
use crate::browser::health::{
    ChannelHealthEvaluator, ChannelHealthReason, ChannelHealthResult, HealthPage,
};
use crate::browser::reward_claimer::RewardClaimResult;
use crate::browser::selectors::{
    CHANNEL_HEALTH_SELECTORS, COMMUNITY_POINTS_BALANCE_SELECTOR, SIDE_NAV_TOGGLE_SELECTOR,
};
use crate::browser::types::BrowserManagerLogger;
use crate::config::model::{
    BrowserConfig, BrowserEngine, BrowserRecoveryConfig, BrowserSessionStartConfig,
    ResourceGuardConfig, ResourceGuardEffectiveThresholds, StreamQuality,
};
use crate::logging::logger::{LogEvents, LogLevel};
use crate::logging::redact::SanitizedValue;
use tokio::sync::oneshot;

// ---------------------------------------------------------------------------
// 共用常數與 helper（對應舊測試頂層）
// ---------------------------------------------------------------------------

/// 舊測試的 `CHANNEL`。
const CHANNEL: &str = "streamer_one";
/// 舊測試的 `TARGET_URL`。
const TARGET_URL: &str = "https://www.twitch.tv/streamer_one";
/// 舊測試 `NOW` 的 unix 毫秒（`new Date('2026-06-14T12:00:00.000Z')`）。
const NOW_UNIX_MILLIS: u64 = 1_781_438_400_000;
/// 舊測試 `NOW.toISOString()`。
const NOW_ISO: &str = "2026-06-14T12:00:00.000Z";

/// 舊測試的 `HealthMarker`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HealthMarker {
    LoginRequired,
    LiveContent,
    Error,
    Offline,
    ContentWarning,
}

impl HealthMarker {
    /// 舊測試 `CHANNEL_HEALTH_SELECTORS[marker]` 的取值。
    fn selector(self) -> &'static str {
        match self {
            HealthMarker::LoginRequired => CHANNEL_HEALTH_SELECTORS.login_required,
            HealthMarker::LiveContent => CHANNEL_HEALTH_SELECTORS.live_content,
            HealthMarker::Error => CHANNEL_HEALTH_SELECTORS.error,
            HealthMarker::Offline => CHANNEL_HEALTH_SELECTORS.offline,
            HealthMarker::ContentWarning => CHANNEL_HEALTH_SELECTORS.content_warning,
        }
    }
}

/// 舊 mock 的 `DOMException('Navigation aborted', 'AbortError')`。
fn aborted_error() -> DriverError {
    DriverError::new("AbortError", "Navigation aborted")
}

/// 對應舊測試 `vi.waitFor`：輪詢條件；paused 計時下 sleep 推進時鐘，背景任務
/// 在每輪之間執行。
/// advance 前的 warmup：讓 timer 迴圈的 sleep 在 tokio::test 的 paused 驅動下
/// 先行註冊（tokio 的 spawn 任務要等 runtime 閒置才會 poll；註冊不及的睡眠
/// 在 advance 之後才登記，deadline 會被推後）。
async fn timer_warmup() {
    for _timer in 0..5 {
        tokio::time::sleep(Duration::from_millis(5)).await;
        tokio::task::yield_now().await;
    }
}

async fn eventually(condition: impl Fn() -> bool) {
    for _ in 0..1_000 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
        tokio::task::yield_now().await;
    }
    panic!("vi.waitFor 條件未成立");
}

// ---------------------------------------------------------------------------
// mock page（對應舊測試的 createMockPage / MockPageControls）
// ---------------------------------------------------------------------------

/// mock 用的 pending／settled 導覽實作（gotoImplementation／reloadImplementation）。
type MockNavigationSource = Arc<
    dyn Fn() -> std::pin::Pin<Box<dyn Future<Output = Result<(), DriverError>> + Send>>
        + Send
        + Sync,
>;

/// mock 截圖實作（screenshotImplementation）。
type MockScreenshotSource = Arc<
    dyn Fn() -> std::pin::Pin<Box<dyn Future<Output = Result<Vec<u8>, DriverError>> + Send>>
        + Send
        + Sync,
>;

/// mock 的可變後備行為（舊測試 input 的 Error／Implementation／finalUrl 欄位）。
#[derive(Default)]
struct MockPageBehaviors {
    /// 舊測試 `input.gotoError`。
    goto_error: Option<DriverError>,
    /// 舊測試 `input.gotoImplementation`。
    goto_implementation: Option<MockNavigationSource>,
    /// 舊測試 `input.finalUrl`。
    final_url: Option<String>,
    /// 舊測試 `input.reloadError`。
    reload_error: Option<DriverError>,
    /// 舊測試 `input.reloadImplementation`。
    reload_implementation: Option<MockNavigationSource>,
    /// 舊測試 `input.screenshotImplementation`。
    screenshot_implementation: Option<MockScreenshotSource>,
    /// 舊測試 `input.contentWarningClickError`。
    content_warning_click_error: Option<DriverError>,
    /// 健康評估前等待的閘門（重現 page operation 交錯時序的回歸測試用）。
    evaluate_gate: Option<MockNavigationSource>,
}

/// mock page 的共用狀態（Rust 的 Mutex／Atomic 對應舊 mock 的 closure 變數）。
struct MockPageShared {
    marker: Mutex<Option<HealthMarker>>,
    url: Mutex<String>,
    closed: AtomicBool,
    channel_points_balance: Mutex<Option<String>>,
    side_nav_present: Mutex<bool>,
    side_nav_expanded: Mutex<bool>,
    side_nav_collapse_clicks: AtomicU32,
    content_warning_clicks: AtomicU32,
    goto_calls: AtomicU32,
    reload_calls: AtomicU32,
    screenshot_calls: AtomicU32,
    navigation_timeout_values: Mutex<Vec<f64>>,
    behaviors: Mutex<MockPageBehaviors>,
}

impl MockPageShared {
    fn marker(&self) -> Option<HealthMarker> {
        *self.marker.lock().unwrap()
    }
}

/// mock page 把手（SessionPage 的 fake；對應舊測試的 Playwright Page mock）。
struct MockPage {
    shared: Arc<MockPageShared>,
}

/// 舊測試 `createMockPage(input)` 的輸入。
#[derive(Default)]
struct MockPageInput {
    marker: Option<HealthMarker>,
    url: Option<String>,
    final_url: Option<String>,
    side_nav_expanded: Option<bool>,
    channel_points_balance: Option<String>,
    behaviors: MockPageBehaviors,
}

fn create_mock_page(input: MockPageInput) -> Arc<MockPage> {
    let MockPageInput {
        marker,
        url,
        final_url,
        side_nav_expanded,
        channel_points_balance,
        mut behaviors,
    } = input;
    behaviors.final_url = final_url;
    Arc::new(MockPage {
        shared: Arc::new(MockPageShared {
            marker: Mutex::new(marker),
            url: Mutex::new(url.unwrap_or_else(|| "about:blank".to_string())),
            closed: AtomicBool::new(false),
            channel_points_balance: Mutex::new(channel_points_balance),
            side_nav_present: Mutex::new(side_nav_expanded.is_some()),
            side_nav_expanded: Mutex::new(side_nav_expanded.unwrap_or(false)),
            side_nav_collapse_clicks: AtomicU32::new(0),
            content_warning_clicks: AtomicU32::new(0),
            goto_calls: AtomicU32::new(0),
            reload_calls: AtomicU32::new(0),
            screenshot_calls: AtomicU32::new(0),
            navigation_timeout_values: Mutex::new(Vec::new()),
            behaviors: Mutex::new(behaviors),
        }),
    })
}

/// 舊測試 `raceWithAbort(signal, promise)`：abort 時 reject。
///
/// 舊 mock 的語意：abort listener 在 `abort()` 的下一個 microtask reject，
/// 即使後續 promise resolve 也不影響結果；以「先檢查旗標再 poll future」的
/// 迴圈重現（mock 的 future 都是簡單的 oneshot／pending，重複 poll 無副作用）。
async fn race_with_abort(
    abort: Option<&Arc<AtomicBool>>,
    mut future: std::pin::Pin<Box<dyn Future<Output = Result<(), DriverError>> + Send>>,
) -> Result<(), DriverError> {
    use futures_util::FutureExt;
    let Some(flag) = abort else {
        return future.await;
    };
    if flag.load(Ordering::SeqCst) {
        return Err(aborted_error());
    }
    loop {
        if flag.load(Ordering::SeqCst) {
            return Err(aborted_error());
        }
        if let Some(outcome) = future.as_mut().now_or_never() {
            return outcome;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

/// 舊測試 goto／reload mock 的共同流程（signal 檢查 → impl race → error → url 更新）。
async fn mock_navigation(
    shared: &Arc<MockPageShared>,
    url: &str,
    abort: Option<Arc<AtomicBool>>,
    is_goto: bool,
) -> Result<(), DriverError> {
    if abort
        .as_ref()
        .is_some_and(|flag| flag.load(Ordering::SeqCst))
    {
        return Err(aborted_error());
    }
    let (implementation, error, final_url) = {
        let behaviors = shared.behaviors.lock().unwrap();
        let implementation = if is_goto {
            behaviors.goto_implementation.clone()
        } else {
            behaviors.reload_implementation.clone()
        };
        let error = if is_goto {
            behaviors.goto_error.clone()
        } else {
            behaviors.reload_error.clone()
        };
        let final_url = if is_goto {
            behaviors.final_url.clone()
        } else {
            None
        };
        (implementation, error, final_url)
    };
    if let Some(source) = implementation {
        race_with_abort(abort.as_ref(), source()).await?;
    }
    if let Some(error) = error {
        return Err(error);
    }
    if is_goto {
        let final_url = final_url.unwrap_or_else(|| url.to_string());
        *shared.url.lock().unwrap() = final_url;
    }
    Ok(())
}

#[async_trait]
impl SessionPage for MockPage {
    fn url(&self) -> String {
        self.shared.url.lock().unwrap().clone()
    }

    fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::SeqCst)
    }

    fn set_default_navigation_timeout(&self, timeout_ms: u64) {
        self.shared
            .navigation_timeout_values
            .lock()
            .unwrap()
            .push(timeout_ms as f64);
    }

    async fn goto_dom_content_loaded(
        &self,
        url: &str,
        abort: Option<Arc<AtomicBool>>,
    ) -> Result<(), DriverError> {
        self.shared.goto_calls.fetch_add(1, Ordering::SeqCst);
        mock_navigation(&self.shared, url, abort, true).await
    }

    async fn reload_dom_content_loaded(
        &self,
        abort: Option<Arc<AtomicBool>>,
    ) -> Result<(), DriverError> {
        self.shared.reload_calls.fetch_add(1, Ordering::SeqCst);
        mock_navigation(&self.shared, "about:blank", abort, false).await
    }

    async fn screenshot_png(&self) -> Result<Vec<u8>, DriverError> {
        self.shared.screenshot_calls.fetch_add(1, Ordering::SeqCst);
        let source = self
            .shared
            .behaviors
            .lock()
            .unwrap()
            .screenshot_implementation
            .clone();
        match source {
            Some(source) => source().await,
            None => Ok(b"png-image".to_vec()),
        }
    }

    async fn evaluate(
        &self,
        _fn_declaration: &str,
        _arg: Option<Value>,
    ) -> Result<Value, DriverError> {
        let gate = self.shared.behaviors.lock().unwrap().evaluate_gate.clone();
        if let Some(gate) = gate {
            gate().await?;
        }
        let marker = self.shared.marker();
        Ok(json!({
            "loginRequired": marker == Some(HealthMarker::LoginRequired),
            "error": marker == Some(HealthMarker::Error),
            "offline": marker == Some(HealthMarker::Offline),
            "liveContent": marker == Some(HealthMarker::LiveContent),
            "unsupportedPlayerError": false,
        }))
    }

    fn locator(&self, selector: &str) -> Arc<dyn SessionLocator> {
        Arc::new(MockLocator {
            selector: selector.to_string(),
            shared: Arc::clone(&self.shared),
        })
    }

    async fn mouse_move(&self, _x: f64, _y: f64) -> Result<(), DriverError> {
        // 舊 mock 沒有 mouse.move；optimizer 在 mock 上走不到（count 恆 0 → hover fallback）。
        Ok(())
    }

    fn as_page_shared(&self) -> Option<Arc<crate::browser::cdp::page::PageShared>> {
        None
    }
}

/// mock 的 `MockPageControls`：讓測試於執行中改狀態／檢查次數。
impl MockPage {
    fn set_marker(&self, marker: Option<HealthMarker>) {
        *self.shared.marker.lock().unwrap() = marker;
    }

    fn set_evaluate_gate(&self, gate: Option<MockNavigationSource>) {
        self.shared.behaviors.lock().unwrap().evaluate_gate = gate;
    }

    fn set_side_nav_expanded(&self, expanded: bool) {
        *self.shared.side_nav_present.lock().unwrap() = true;
        *self.shared.side_nav_expanded.lock().unwrap() = expanded;
    }

    fn set_url(&self, url: &str) {
        *self.shared.url.lock().unwrap() = url.to_string();
    }

    fn set_closed(&self, closed: bool) {
        self.shared.closed.store(closed, Ordering::SeqCst);
    }

    fn set_channel_points_balance(&self, balance: Option<&str>) {
        *self.shared.channel_points_balance.lock().unwrap() = balance.map(str::to_string);
    }

    fn content_warning_clicks(&self) -> u32 {
        self.shared.content_warning_clicks.load(Ordering::SeqCst)
    }

    fn side_nav_collapse_clicks(&self) -> u32 {
        self.shared.side_nav_collapse_clicks.load(Ordering::SeqCst)
    }

    fn goto_calls(&self) -> u32 {
        self.shared.goto_calls.load(Ordering::SeqCst)
    }

    fn reload_calls(&self) -> u32 {
        self.shared.reload_calls.load(Ordering::SeqCst)
    }

    fn screenshot_calls(&self) -> u32 {
        self.shared.screenshot_calls.load(Ordering::SeqCst)
    }

    fn navigation_timeouts(&self) -> Vec<f64> {
        self.shared
            .navigation_timeout_values
            .lock()
            .unwrap()
            .clone()
    }
}

/// mock locator（對應舊 mock 的 `locator(selector)` 回傳物件）。
struct MockLocator {
    selector: String,
    shared: Arc<MockPageShared>,
}

fn mock_locator_visible(shared: &MockPageShared, selector: &str) -> bool {
    let marker_match = shared
        .marker()
        .map(|marker| marker.selector() == selector)
        .unwrap_or(false);
    marker_match
        || (*shared.side_nav_present.lock().unwrap() && selector == SIDE_NAV_TOGGLE_SELECTOR)
}

#[async_trait]
impl SessionLocator for MockLocator {
    fn first(&self) -> Arc<dyn SessionLocator> {
        // 舊 mock：first() 回同一個 mockLocator。
        Arc::new(MockLocator {
            selector: self.selector.clone(),
            shared: Arc::clone(&self.shared),
        })
    }

    fn nth(&self, _index: usize) -> Arc<dyn SessionLocator> {
        Arc::new(MockLocator {
            selector: self.selector.clone(),
            shared: Arc::clone(&self.shared),
        })
    }

    async fn count(&self) -> Result<usize, DriverError> {
        // 舊 mock 沒有 count（呼叫會拋 TypeError）；Rust mock 對所有locator 回 0，
        // optimizer 的等效結果是「video 不存在」（同 prepareVideo catch 的 default state）。
        Ok(0)
    }

    async fn is_visible(&self) -> Result<bool, DriverError> {
        Ok(mock_locator_visible(&self.shared, &self.selector))
    }

    async fn wait_visible(&self, _timeout: Option<u64>) -> Result<(), DriverError> {
        if !mock_locator_visible(&self.shared, &self.selector) {
            return Err(DriverError::new("Error", "locator is not visible"));
        }
        Ok(())
    }

    async fn click(&self, _timeout: Option<u64>) -> Result<(), DriverError> {
        if self.selector == SIDE_NAV_TOGGLE_SELECTOR {
            if *self.shared.side_nav_expanded.lock().unwrap() {
                *self.shared.side_nav_expanded.lock().unwrap() = false;
                self.shared
                    .side_nav_collapse_clicks
                    .fetch_add(1, Ordering::SeqCst);
            }
            return Ok(());
        }
        if self.shared.marker() == Some(HealthMarker::ContentWarning)
            && self.selector == CHANNEL_HEALTH_SELECTORS.content_warning
        {
            let error = self
                .shared
                .behaviors
                .lock()
                .unwrap()
                .content_warning_click_error
                .clone();
            if let Some(error) = error {
                return Err(error);
            }
            self.shared
                .content_warning_clicks
                .fetch_add(1, Ordering::SeqCst);
            *self.shared.marker.lock().unwrap() = Some(HealthMarker::LiveContent);
        }
        Ok(())
    }

    async fn hover(&self, _timeout: Option<u64>) -> Result<(), DriverError> {
        // 舊 mock 沒有 hover；session 的 mock 流程走不到 hover。
        Ok(())
    }

    async fn evaluate(
        &self,
        _fn_declaration: &str,
        _arg: Option<Value>,
    ) -> Result<Value, DriverError> {
        // 舊 mock：任何 locator.evaluate 都回 sideNavExpanded。
        Ok(json!(*self.shared.side_nav_expanded.lock().unwrap()))
    }

    async fn evaluate_all(&self, _fn_declaration: &str) -> Result<Value, DriverError> {
        let balance = self.shared.channel_points_balance.lock().unwrap().clone();
        if self.selector == COMMUNITY_POINTS_BALANCE_SELECTOR {
            return Ok(match balance {
                Some(balance) => json!([
                    {"visible": true, "candidates": [balance]},
                ]),
                None => json!([]),
            });
        }
        Ok(json!([]))
    }
}

// ---------------------------------------------------------------------------
// mock browser manager（舊測試 createBrowserManager）
// ---------------------------------------------------------------------------

struct MockManagerShared {
    create_page_calls: AtomicU32,
    close_page_calls: Mutex<Vec<String>>,
}

/// mock 的 BrowserManager（只回固定 page；呼叫次數照舊測試斷言）。
struct MockBrowserManager {
    page: Arc<MockPage>,
    shared: Arc<MockManagerShared>,
}

fn create_browser_manager(page: &Arc<MockPage>) -> Arc<MockBrowserManager> {
    Arc::new(MockBrowserManager {
        page: Arc::clone(page),
        shared: Arc::new(MockManagerShared {
            create_page_calls: AtomicU32::new(0),
            close_page_calls: Mutex::new(Vec::new()),
        }),
    })
}

#[async_trait]
impl SessionBrowserManager for MockBrowserManager {
    async fn create_page(&self, _channel: &str) -> Result<Arc<dyn SessionPage>, DriverError> {
        self.shared.create_page_calls.fetch_add(1, Ordering::SeqCst);
        Ok(Arc::clone(&self.page) as Arc<dyn SessionPage>)
    }

    async fn close_page(&self, channel: &str) -> Result<(), DriverError> {
        self.shared
            .close_page_calls
            .lock()
            .unwrap()
            .push(channel.to_string());
        Ok(())
    }
}

impl MockBrowserManager {
    fn create_page_calls(&self) -> u32 {
        self.shared.create_page_calls.load(Ordering::SeqCst)
    }

    fn close_page_calls(&self) -> usize {
        self.shared.close_page_calls.lock().unwrap().len()
    }
}

// ---------------------------------------------------------------------------
// mock reward claimer（舊測試 createRewardClaimer）
// ---------------------------------------------------------------------------

/// 舊測試 `createRewardClaimer(implementation)` 的 claim 實作（可能拋錯）。
type MockClaimImplementation = Arc<
    dyn for<'a> Fn(
            &(dyn SessionPage + 'a),
            &'a str,
        ) -> std::pin::Pin<
            Box<dyn Future<Output = Result<RewardClaimResult, DriverError>> + Send + 'a>,
        > + Send
        + Sync,
>;

struct MockClaimerShared {
    has_claimable: Mutex<Result<bool, DriverError>>,
    claim_implementation: MockClaimImplementation,
    claim_calls: AtomicU32,
    has_claimable_calls: AtomicU32,
    last_claim_channel: Mutex<Option<String>>,
}

impl MockClaimerShared {
    fn claim_calls(&self) -> u32 {
        self.claim_calls.load(Ordering::SeqCst)
    }

    fn has_claimable_calls(&self) -> u32 {
        self.has_claimable_calls.load(Ordering::SeqCst)
    }

    fn set_has_claimable(&self, has_claimable: Result<bool, DriverError>) {
        *self.has_claimable.lock().unwrap() = has_claimable;
    }

    fn last_claim_channel(&self) -> Option<String> {
        self.last_claim_channel.lock().unwrap().clone()
    }
}

struct MockRewardClaimer {
    shared: Arc<MockClaimerShared>,
}

/// 對應舊測試 default：`not_found`（checkedAt 固定 NOW）。
fn default_claim_implementation() -> MockClaimImplementation {
    Arc::new(|_page: &(dyn SessionPage + '_), channel: &str| {
        Box::pin(async move {
            Ok(RewardClaimResult::NotFound {
                channel: channel.to_string(),
                checked_at: NOW_ISO.to_string(),
            }) as Result<RewardClaimResult, DriverError>
        })
    })
}

fn create_reward_claimer() -> MockRewardClaimer {
    create_reward_claimer_impl(default_claim_implementation(), Ok(false))
}

fn create_reward_claimer_impl(
    claim_implementation: MockClaimImplementation,
    has_claimable: Result<bool, DriverError>,
) -> MockRewardClaimer {
    MockRewardClaimer {
        shared: Arc::new(MockClaimerShared {
            has_claimable: Mutex::new(has_claimable),
            claim_implementation,
            claim_calls: AtomicU32::new(0),
            has_claimable_calls: AtomicU32::new(0),
            last_claim_channel: Mutex::new(Some(CHANNEL.to_string())),
        }),
    }
}

/// 舊測試的 click_failed 實作。
fn click_failed_claim_implementation() -> MockClaimImplementation {
    Arc::new(|_page: &(dyn SessionPage + '_), channel: &str| {
        Box::pin(async move {
            Ok(RewardClaimResult::ClickFailed {
                channel: channel.to_string(),
                checked_at: NOW_ISO.to_string(),
                error: "click failed".to_string(),
            }) as Result<RewardClaimResult, DriverError>
        })
    })
}

/// 依序回傳 outcomes 的 claim 實作（空了就拋錯；同舊測試斷言用）。
fn sequence_claim_implementation(outcomes: VecDeque<RewardClaimResult>) -> MockClaimImplementation {
    let outcomes = Arc::new(Mutex::new(outcomes));
    Arc::new(move |_page: &(dyn SessionPage + '_), _channel: &str| {
        let outcomes = Arc::clone(&outcomes);
        Box::pin(async move {
            let outcome = outcomes.lock().unwrap().pop_front();
            match outcome {
                Some(outcome) => Ok(outcome),
                None => Err(DriverError::new("Error", "unexpected reward claim")),
            }
        })
    })
}

#[async_trait]
impl SessionRewardClaimer for MockRewardClaimer {
    async fn claim_if_available(
        &self,
        page: &dyn SessionPage,
        channel: &str,
    ) -> Result<RewardClaimResult, DriverError> {
        self.shared.claim_calls.fetch_add(1, Ordering::SeqCst);
        *self.shared.last_claim_channel.lock().unwrap() = Some(channel.to_string());
        (self.shared.claim_implementation)(page, channel).await
    }

    async fn has_claimable_bonus(&self, _page: &dyn SessionPage) -> Result<bool, DriverError> {
        self.shared
            .has_claimable_calls
            .fetch_add(1, Ordering::SeqCst);
        self.shared.has_claimable.lock().unwrap().clone()
    }
}

impl MockRewardClaimer {
    fn claim_calls(&self) -> u32 {
        self.shared.claim_calls.load(Ordering::SeqCst)
    }

    fn set_has_claimable(&self, has_claimable: Result<bool, DriverError>) {
        *self.shared.has_claimable.lock().unwrap() = has_claimable;
    }
}

// ---------------------------------------------------------------------------
// mock logger（舊測試 createLogger）
// ---------------------------------------------------------------------------

/// 舊測試 `createLogger` 的收集 logger。
struct MockLogger {
    entries: Mutex<Vec<(LogLevel, String, Value)>>,
}

impl MockLogger {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            entries: Mutex::new(Vec::new()),
        })
    }

    fn fields_of(&self, level: LogLevel, event: &str) -> Vec<Value> {
        self.entries
            .lock()
            .unwrap()
            .iter()
            .filter(|(entry_level, entry_event, _)| entry_level == &level && entry_event == event)
            .map(|(_, _, fields)| fields.clone())
            .collect()
    }

    fn called_with(&self, level: LogLevel, event: &str, subset: Value) -> bool {
        self.fields_of(level, event)
            .iter()
            .any(|fields| object_contains(&subset, fields))
    }

    fn call_count(&self, level: LogLevel, event: &str) -> usize {
        self.fields_of(level, event).len()
    }
}

impl BrowserManagerLogger for MockLogger {
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

impl MockLogger {
    fn record(&self, level: LogLevel, event: &str, fields: Option<&SanitizedValue>) {
        let json = match fields {
            Some(fields) => serde_json::from_str(&fields.to_json_string()).unwrap_or(Value::Null),
            None => json!({}),
        };
        self.entries
            .lock()
            .unwrap()
            .push((level, event.to_string(), json));
    }
}

/// `toHaveBeenCalledWith` 的 partial match（subset 為 json!()；`null` 對應
/// `expect.any(Number)`）。
fn object_contains(subset: &Value, actual: &Value) -> bool {
    let (Value::Object(expected_entries), Value::Object(actual_entries)) = (subset, actual) else {
        return subset == actual;
    };
    expected_entries.iter().all(|(key, expected)| {
        if expected.is_null() {
            return actual_entries
                .get(key)
                .map(|value| value.is_number())
                .unwrap_or(false);
        }
        actual_entries.get(key) == Some(expected)
    })
}

// ---------------------------------------------------------------------------
// config（舊測試 createConfig + test-config helpers 的預設值）
// ---------------------------------------------------------------------------

/// 舊測試 `createConfig` 的 overrides（只覆蓋 session 用到的欄位）。
#[derive(Default)]
struct ConfigOverrides {
    navigation_timeout_ms: Option<f64>,
    page_health_check_interval_seconds: Option<f64>,
    reward_check_interval_seconds: Option<f64>,
    page_refresh_interval_seconds: Option<f64>,
    enforce_stream_quality_seconds: Option<f64>,
}

/// 舊 test-config 的 `createDefaultBrowserRecovery()`。
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

/// 舊 test-config 的 `createDefaultBrowserSessionStart()`。
fn create_default_browser_session_start() -> BrowserSessionStartConfig {
    BrowserSessionStartConfig {
        failure_backoff_seconds: vec![30.0, 60.0, 120.0],
        maximum_cooldown_seconds: 900.0,
    }
}

/// 舊測試 config 內嵌的 resource_guard 物件（不影響 session 行為）。
fn create_default_resource_guard() -> ResourceGuardConfig {
    ResourceGuardConfig {
        enabled: false,
        sample_interval_seconds: 2.0,
        startup_rate_grace_seconds: 120.0,
        scale_with_streams: true,
        baseline_streams: 3.0,
        base_memory_mib: 512.0,
        warning_memory_mib: 4_096.0,
        warning_reset_memory_mib: 3_840.0,
        browser_recycle_memory_mib: 4_608.0,
        browser_recycle_consecutive_samples: 2.0,
        emergency_memory_mib: 5_376.0,
        emergency_swap_mib: 768.0,
        fast_growth_mib: 1_024.0,
        fast_growth_window_seconds: 30.0,
        post_browser_restart_rate_grace_seconds: 120.0,
        post_recycle_observation_seconds: 20.0,
        post_recycle_target_memory_mib: 4_096.0,
        post_recycle_minimum_drop_mib: 512.0,
        effective: ResourceGuardEffectiveThresholds {
            max_concurrent_streams: 1.0,
            warning_memory_mib: 4_096.0,
            warning_reset_memory_mib: 3_840.0,
            browser_recycle_memory_mib: 4_608.0,
            emergency_memory_mib: 5_376.0,
            post_recycle_target_memory_mib: 4_096.0,
        },
    }
}

fn create_config(overrides: ConfigOverrides) -> ChannelSessionConfig {
    let ConfigOverrides {
        navigation_timeout_ms,
        page_health_check_interval_seconds,
        reward_check_interval_seconds,
        page_refresh_interval_seconds,
        enforce_stream_quality_seconds,
    } = overrides;
    let mut browser = BrowserConfig {
        engine: BrowserEngine::Chromium,
        navigation_timeout_ms: 30_000.0,
        page_health_check_interval_seconds: 60.0,
        reward_check_interval_seconds: 30.0,
        page_refresh_interval_seconds: 300.0,
        restart_on_crash: true,
        stream_quality: StreamQuality::P160,
        enforce_stream_quality_seconds: 120.0,
        viewport_width: 640.0,
        viewport_height: 360.0,
        mute_audio: true,
        block_images: true,
        block_fonts: true,
        block_known_tracking: false,
        disable_chat: true,
        resource_telemetry_interval_seconds: 300.0,
        recovery: create_default_browser_recovery(),
        session_start: create_default_browser_session_start(),
        resource_guard: create_default_resource_guard(),
    };
    if let Some(value) = navigation_timeout_ms {
        browser.navigation_timeout_ms = value;
    }
    if let Some(value) = page_health_check_interval_seconds {
        browser.page_health_check_interval_seconds = value;
    }
    if let Some(value) = reward_check_interval_seconds {
        browser.reward_check_interval_seconds = value;
    }
    if let Some(value) = page_refresh_interval_seconds {
        browser.page_refresh_interval_seconds = value;
    }
    if let Some(value) = enforce_stream_quality_seconds {
        browser.enforce_stream_quality_seconds = value;
    }
    ChannelSessionConfig {
        channels: vec![CHANNEL.to_string()],
        browser,
    }
}

/// 舊測試的固定時鐘注入（`now: () => NOW`）。
fn fixed_now() -> Arc<dyn Fn() -> u64 + Send + Sync> {
    Arc::new(|| NOW_UNIX_MILLIS)
}

// ---------------------------------------------------------------------------
// session 組裝 helper（對應舊測試 `new DefaultChannelSession({...})`）
// ---------------------------------------------------------------------------

struct SessionParts {
    page: Arc<MockPage>,
    manager: Arc<MockBrowserManager>,
    claimer: Arc<MockRewardClaimer>,

    logger: Option<Arc<MockLogger>>,
    evaluator: Option<Arc<dyn ChannelHealthEvaluator>>,
    on_invalidated: Option<Arc<MockInvalidationObserver>>,
    on_page_refresh: Option<Arc<MockRefreshObserver>>,
    on_container_restart_requested: Option<Arc<MockRestartObserver>>,
    page_operation_drain_timeout_ms: Option<u64>,
    points_claim_retry_attempts: Option<u64>,
    points_claim_retry_delay_ms: Option<u64>,
    config: Option<ChannelSessionConfig>,
}

impl SessionParts {
    fn new(
        page: &Arc<MockPage>,
        claimer: MockRewardClaimer,
        manager: &Arc<MockBrowserManager>,
    ) -> Self {
        Self {
            page: Arc::clone(page),
            manager: Arc::clone(manager),
            claimer: Arc::new(claimer),
            logger: None,
            evaluator: None,
            on_invalidated: None,
            on_page_refresh: None,
            on_container_restart_requested: None,
            page_operation_drain_timeout_ms: None,
            points_claim_retry_attempts: None,
            points_claim_retry_delay_ms: None,
            config: None,
        }
    }

    fn with_logger(mut self, logger: Arc<MockLogger>) -> Self {
        self.logger = Some(logger);
        self
    }

    fn with_evaluator(mut self, evaluator: Arc<dyn ChannelHealthEvaluator>) -> Self {
        self.evaluator = Some(evaluator);
        self
    }

    fn with_invalidated(mut self, observer: Arc<MockInvalidationObserver>) -> Self {
        self.on_invalidated = Some(observer);
        self
    }

    fn with_refresh_observer(mut self, observer: Arc<MockRefreshObserver>) -> Self {
        self.on_page_refresh = Some(observer);
        self
    }

    fn with_restart_observer(mut self, observer: Arc<MockRestartObserver>) -> Self {
        self.on_container_restart_requested = Some(observer);
        self
    }

    fn with_points_retry(mut self, attempts: u64, delay_ms: u64) -> Self {
        self.points_claim_retry_attempts = Some(attempts);
        self.points_claim_retry_delay_ms = Some(delay_ms);
        self
    }

    fn with_drain_timeout(mut self, timeout_ms: u64) -> Self {
        self.page_operation_drain_timeout_ms = Some(timeout_ms);
        self
    }

    fn with_config(mut self, config: ChannelSessionConfig) -> Self {
        self.config = Some(config);
        self
    }

    fn build(self) -> DefaultChannelSession {
        DefaultChannelSession::new(DefaultChannelSessionOptions {
            channel: CHANNEL.to_string(),
            config: self
                .config
                .unwrap_or_else(|| create_config(ConfigOverrides::default())),
            browser_manager: self.manager,
            reward_claimer: self.claimer.clone(),
            playback_optimizer: None,
            logger: self
                .logger
                .clone()
                .map(|logger| logger as Arc<dyn BrowserManagerLogger>),
            health_failure_threshold: None,
            health_evaluator: self.evaluator,
            on_invalidated: self
                .on_invalidated
                .map(|observer| observer as Arc<dyn ChannelSessionInvalidationObserver>),
            on_page_refresh: self
                .on_page_refresh
                .map(|observer| observer as Arc<dyn ChannelSessionRefreshObserver>),
            on_container_restart_requested: self
                .on_container_restart_requested
                .map(|observer| observer as Arc<dyn ContainerRestartRequestObserver>),
            page_operation_drain_timeout_ms: self.page_operation_drain_timeout_ms,
            points_claim_retry_attempts: self.points_claim_retry_attempts,
            points_claim_retry_delay_ms: self.points_claim_retry_delay_ms,
            now: Some(fixed_now()),
        })
        .unwrap()
    }
}

// 觀察者 fakes（對應舊測試的 `vi.fn(async () => undefined)`）

struct MockInvalidationObserver {
    calls: Mutex<Vec<String>>,
}

impl MockInvalidationObserver {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
        })
    }

    fn call_args(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl ChannelSessionInvalidationObserver for MockInvalidationObserver {
    async fn on_invalidated(&self, channel: &str) -> Result<(), DriverError> {
        self.calls.lock().unwrap().push(channel.to_string());
        Ok(())
    }
}

struct MockRefreshObserver {
    calls: Mutex<Vec<super::ChannelSessionRefreshEvent>>,
}

impl MockRefreshObserver {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
        })
    }

    fn call_records(&self) -> Vec<super::ChannelSessionRefreshEvent> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl ChannelSessionRefreshObserver for MockRefreshObserver {
    async fn on_page_refresh(&self, event: &ChannelSessionRefreshEvent) -> Result<(), DriverError> {
        self.calls.lock().unwrap().push(event.clone());
        Ok(())
    }
}

struct MockRestartObserver {
    calls: Mutex<Vec<super::ContainerRestartRequest>>,
}

impl MockRestartObserver {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
        })
    }

    fn call_records(&self) -> Vec<super::ContainerRestartRequest> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl ContainerRestartRequestObserver for MockRestartObserver {
    async fn on_container_restart_requested(
        &self,
        request: &ContainerRestartRequest,
    ) -> Result<(), DriverError> {
        self.calls.lock().unwrap().push(request.clone());
        Ok(())
    }
}

// 自訂 healthEvaluator fakes（舊測試 `healthEvaluator: async () => …`）

struct FixedHealthEvaluator {
    result: ChannelHealthResult,
}

impl FixedHealthEvaluator {
    fn new(result: ChannelHealthResult) -> Arc<Self> {
        Arc::new(Self { result })
    }
}

#[async_trait]
impl ChannelHealthEvaluator for FixedHealthEvaluator {
    async fn evaluate(
        &self,
        _page: &dyn HealthPage,
        _target_url: &str,
    ) -> Result<ChannelHealthResult, DriverError> {
        Ok(self.result.clone())
    }
}

/// 舊測試 deferred 的 health evaluator（`vi.fn(() => health.promise)`）。
struct DeferredHealthEvaluator {
    calls: AtomicU32,
    receiver: Mutex<Option<oneshot::Receiver<ChannelHealthResult>>>,
}

impl DeferredHealthEvaluator {
    fn new(receiver: oneshot::Receiver<ChannelHealthResult>) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicU32::new(0),
            receiver: Mutex::new(Some(receiver)),
        })
    }

    fn calls(&self) -> u32 {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ChannelHealthEvaluator for DeferredHealthEvaluator {
    async fn evaluate(
        &self,
        _page: &dyn HealthPage,
        _target_url: &str,
    ) -> Result<ChannelHealthResult, DriverError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let receiver = self.receiver.lock().unwrap().take();
        match receiver {
            Some(receiver) => receiver
                .await
                .map_err(|_| DriverError::new("Error", "deferred health evaluator dropped sender")),
            None => Err(DriverError::new(
                "Error",
                "deferred health evaluator reused",
            )),
        }
    }
}

// spawn 的 helper（session 的 Clone 進 static future；對應舊測試 `session.start()` 未 await 的 promise）。
fn spawn_start(
    session: &DefaultChannelSession,
) -> tokio::task::JoinHandle<Result<(), DriverError>> {
    let session = session.clone();
    tokio::spawn(async move { session.start().await })
}

fn spawn_check_health(
    session: &DefaultChannelSession,
) -> tokio::task::JoinHandle<ChannelHealthResult> {
    let session = session.clone();
    tokio::spawn(async move { session.check_health().await })
}

fn spawn_capture_screenshot(
    session: &DefaultChannelSession,
) -> tokio::task::JoinHandle<Result<Vec<u8>, DriverError>> {
    let session = session.clone();
    tokio::spawn(async move { session.capture_screenshot().await })
}

fn spawn_stop(
    session: &DefaultChannelSession,
    reason: &'static str,
) -> tokio::task::JoinHandle<Result<(), DriverError>> {
    let session = session.clone();
    tokio::spawn(async move { session.stop(reason).await })
}

// ---------------------------------------------------------------------------
// 測試本體（舊測試 38 個標題）
// ---------------------------------------------------------------------------

// 原測試：併發 start 只建立一次 page、設定 timeout 並導覽編碼後 URL
#[tokio::test(start_paused = true)]
async fn concurrent_start_creates_page_once_sets_timeout_and_goes_to_encoded_url() {
    let page = create_mock_page(MockPageInput::default());
    let manager = create_browser_manager(&page);
    let claimer = create_reward_claimer();
    let claimer_shared = Arc::clone(&claimer.shared);
    let parts =
        SessionParts::new(&page, claimer, &manager).with_config(create_config(ConfigOverrides {
            navigation_timeout_ms: Some(12_345.0),
            ..ConfigOverrides::default()
        }));
    let session = parts.build();

    let (first, second, third) = tokio::join!(session.start(), session.start(), session.start());
    assert!(first.is_ok(), "first start: {first:?}");
    assert!(second.is_ok(), "second start: {second:?}");
    assert!(third.is_ok(), "third start: {third:?}");

    assert_eq!(manager.create_page_calls(), 1);
    assert_eq!(page.navigation_timeouts(), vec![12_345.0]);
    assert_eq!(page.goto_calls(), 1);
    assert_eq!(session.state(), ChannelSessionState::Watching);

    session.stop("test_complete").await.unwrap();
    let (first, second) = tokio::join!(session.start(), session.start());
    assert!(first.is_ok());
    assert!(second.is_ok());
    assert_eq!(manager.create_page_calls(), 2);
    assert_eq!(claimer_shared.claim_calls(), 0);
    assert_eq!(session.state(), ChannelSessionState::Watching);
    session.stop("restart_complete").await.unwrap();
}

// 原測試：start 失敗會透過 manager 清理 page 並進入 failed
#[tokio::test(start_paused = true)]
async fn start_failure_cleans_page_via_manager_and_enters_failed() {
    let page = create_mock_page(MockPageInput {
        behaviors: MockPageBehaviors {
            goto_error: Some(DriverError::new("Error", "navigation failed")),
            ..MockPageBehaviors::default()
        },
        ..MockPageInput::default()
    });
    let manager = create_browser_manager(&page);
    let parts = SessionParts::new(&page, create_reward_claimer(), &manager);
    let session = parts.build();

    let outcome = session.start().await;
    let error = outcome.unwrap_err();
    assert_eq!(error.message, "navigation failed");
    assert_eq!(manager.close_page_calls(), 1);
    assert_eq!(session.state(), ChannelSessionState::Failed);
}

// 原測試：取消進行中的 start 會 abort 底層 goto、關閉 page 並阻止 late success
#[tokio::test(start_paused = true)]
async fn cancel_pending_start_aborts_goto_closes_page_and_blocks_late_success() {
    let navigation = oneshot::channel::<Result<(), DriverError>>();
    let (navigation_tx, navigation_rx) = navigation;
    let receiver = Arc::new(Mutex::new(Some(navigation_rx)));
    let navigation_source: MockNavigationSource = Arc::new(move || {
        let receiver = Arc::clone(&receiver);
        Box::pin(async move {
            let rx = receiver.lock().unwrap().take();
            match rx {
                Some(rx) => rx.await.unwrap_or(Err(aborted_error())),
                None => Err(DriverError::new("Error", "deferred navigation reused")),
            }
        })
    });
    let page = create_mock_page(MockPageInput {
        behaviors: MockPageBehaviors {
            goto_implementation: Some(navigation_source),
            ..MockPageBehaviors::default()
        },
        ..MockPageInput::default()
    });
    let manager = create_browser_manager(&page);
    let parts = SessionParts::new(&page, create_reward_claimer(), &manager);
    let session = parts.build();

    let start = spawn_start(&session);
    eventually(|| page.goto_calls() == 1).await;

    session.cancel_start("start_timeout").await;
    assert_eq!(manager.close_page_calls(), 1);

    let _ = navigation_tx.send(Ok(()));
    let outcome = start.await.unwrap();
    let error = outcome.unwrap_err();
    assert_eq!(error.message, "Navigation aborted");
    assert_eq!(session.state(), ChannelSessionState::Failed);
    assert_eq!(manager.close_page_calls(), 2);
}

// 原測試：start 被取消時即使 goto 永不返回，abort 也會讓 start 立即結束且不留下 orphan page
#[tokio::test(start_paused = true)]
async fn cancelled_start_finishes_via_abort_without_waiting_for_goto() {
    let navigation_source: MockNavigationSource = Arc::new(|| Box::pin(std::future::pending()));
    let page = create_mock_page(MockPageInput {
        behaviors: MockPageBehaviors {
            goto_implementation: Some(navigation_source),
            ..MockPageBehaviors::default()
        },
        ..MockPageInput::default()
    });
    let manager = create_browser_manager(&page);
    let logger = MockLogger::new();
    let parts =
        SessionParts::new(&page, create_reward_claimer(), &manager).with_logger(logger.clone());
    let session = parts.build();

    let start = spawn_start(&session);
    eventually(|| page.goto_calls() == 1).await;

    session.cancel_start("start_timeout").await;

    let outcome = start.await.unwrap();
    let error = outcome.unwrap_err();
    assert_eq!(error.message, "Navigation aborted");
    assert_eq!(session.state(), ChannelSessionState::Failed);
    // 舊測試的 `expect(browser.closePage).toHaveBeenCalledWith(CHANNEL)`：
    // 只斷言被呼叫過（取消清理與 catch 的清理各一次）。
    assert!(manager.close_page_calls() >= 1);
    assert!(logger.called_with(
        LogLevel::Debug,
        "watch_start_aborted",
        json!({
            "channel": CHANNEL,
            "reason": "start_timeout",
            "outcome": "aborted",
            // `expect.stringMatching(/aborted/iu)`：錯誤文字含 aborted（不分大小寫）。
            "error": json!("Navigation aborted"),
        }),
    ));
}

// 原測試：start 導向非預期 origin 時立即關閉 page 且不啟動 session
#[tokio::test(start_paused = true)]
async fn unexpected_origin_closes_page_immediately() {
    let page = create_mock_page(MockPageInput {
        final_url: Some("https://example.test/fake-channel".to_string()),
        ..MockPageInput::default()
    });
    let manager = create_browser_manager(&page);
    let claimer = create_reward_claimer();
    let claimer_shared = Arc::clone(&claimer.shared);
    let parts = SessionParts::new(&page, claimer, &manager);
    let session = parts.build();

    let outcome = session.start().await;
    let error = outcome.unwrap_err();
    assert!(error.message.contains("非預期 Twitch URL"), "{error:?}");
    assert_eq!(manager.close_page_calls(), 1);
    assert_eq!(claimer_shared.claim_calls(), 0);
    assert_eq!(session.state(), ChannelSessionState::Failed);
}

// 原測試：stop 可重複呼叫、關閉 page 並記錄停止原因
#[tokio::test(start_paused = true)]
async fn repeated_stop_closes_page_once_and_records_reason() {
    let page = create_mock_page(MockPageInput::default());
    let manager = create_browser_manager(&page);
    let logger = MockLogger::new();
    let parts =
        SessionParts::new(&page, create_reward_claimer(), &manager).with_logger(logger.clone());
    let session = parts.build();
    session.start().await.unwrap();

    let (first, second) = tokio::join!(
        session.stop("stream_offline"),
        session.stop("duplicate_stop")
    );
    assert!(first.is_ok());
    assert!(second.is_ok());

    assert_eq!(manager.close_page_calls(), 1);
    assert_eq!(session.state(), ChannelSessionState::Stopped);
    assert!(logger.called_with(
        LogLevel::Info,
        LogEvents::WATCH_STOPPED,
        json!({"channel": CHANNEL, "reason": "stream_offline"}),
    ));
}

// 原測試：正常 Twitch URL 且有播放器時為 healthy
#[tokio::test(start_paused = true)]
async fn healthy_with_live_player() {
    let page = create_mock_page(MockPageInput {
        marker: Some(HealthMarker::LiveContent),
        ..MockPageInput::default()
    });
    let manager = create_browser_manager(&page);
    let logger = MockLogger::new();
    let parts =
        SessionParts::new(&page, create_reward_claimer(), &manager).with_logger(logger.clone());
    let session = parts.build();
    session.start().await.unwrap();

    let result = session.check_health().await;
    assert_eq!(result, ChannelHealthResult::healthy());
    assert_eq!(session.state(), ChannelSessionState::Watching);
    assert!(logger.called_with(
        LogLevel::Debug,
        "session_maintenance_completed",
        json!({
            "channel": CHANNEL,
            "task": "health",
            "outcome": "completed",
            "queuedMs": json!(Value::Null),
            "durationMs": json!(Value::Null),
        }),
    ));

    session.stop("test_complete").await.unwrap();
    let _result = session.check_health().await;
    assert!(logger.called_with(
        LogLevel::Debug,
        "session_maintenance_skipped",
        json!({"channel": CHANNEL, "task": "health", "reason": "page_missing"}),
    ));
}

// 原測試：start 遇到 Twitch 內容警示時會先按 Start Watching 再進入 watching
#[tokio::test(start_paused = true)]
async fn start_clicks_content_warning_before_watching() {
    let page = create_mock_page(MockPageInput {
        marker: Some(HealthMarker::ContentWarning),
        ..MockPageInput::default()
    });
    let manager = create_browser_manager(&page);
    let logger = MockLogger::new();
    let parts =
        SessionParts::new(&page, create_reward_claimer(), &manager).with_logger(logger.clone());
    let session = parts.build();

    session.start().await.unwrap();

    assert_eq!(page.content_warning_clicks(), 1);
    assert_eq!(session.state(), ChannelSessionState::Watching);
    assert!(logger.called_with(
        LogLevel::Info,
        "content_warning_accepted",
        json!({"channel": CHANNEL, "reason": "start"}),
    ));

    session.stop("test_complete").await.unwrap();
}

// 原測試：start 與 reload 後會自動收合展開的 Twitch 左側欄
#[tokio::test(start_paused = true)]
async fn start_and_reload_collapse_expanded_side_nav() {
    let page = create_mock_page(MockPageInput {
        marker: Some(HealthMarker::LiveContent),
        side_nav_expanded: Some(true),
        ..MockPageInput::default()
    });
    let manager = create_browser_manager(&page);
    let logger = MockLogger::new();
    let parts =
        SessionParts::new(&page, create_reward_claimer(), &manager).with_logger(logger.clone());
    let session = parts.build();

    session.start().await.unwrap();
    assert_eq!(page.side_nav_collapse_clicks(), 1);
    assert!(logger.called_with(
        LogLevel::Debug,
        "side_nav_collapsed",
        json!({"channel": CHANNEL, "reason": "start"}),
    ));

    page.set_side_nav_expanded(true);
    let refreshed = session.refresh_now().await.unwrap();
    assert!(refreshed);
    eventually_assert(
        || page.side_nav_collapse_clicks() == 2,
        "collapse clicked twice",
    )
    .await;
    assert!(logger.called_with(
        LogLevel::Debug,
        "side_nav_collapsed",
        json!({"channel": CHANNEL, "reason": "manual_refresh"}),
    ));

    session.stop("test_complete").await.unwrap();
}

/// 車輪 helper：等待條件成立（佇列內 reload 的非同步落點）。
async fn eventually_assert(condition: impl Fn() -> bool, message: &str) {
    for _ in 0..1_000 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
        tokio::task::yield_now().await;
    }
    assert!(condition(), "{message}");
}

// 原測試：觀看中可直接從現有 page 擷取 viewport PNG
#[tokio::test(start_paused = true)]
async fn capture_screenshot_from_live_page() {
    let page = create_mock_page(MockPageInput {
        marker: Some(HealthMarker::LiveContent),
        ..MockPageInput::default()
    });
    let manager = create_browser_manager(&page);
    let parts = SessionParts::new(&page, create_reward_claimer(), &manager);
    let session = parts.build();
    session.start().await.unwrap();

    let screenshot = session.capture_screenshot().await.unwrap();
    assert_eq!(screenshot, b"png-image".to_vec());
    assert_eq!(page.screenshot_calls(), 1);

    session.stop("test_complete").await.unwrap();
    let outcome = session.capture_screenshot().await;
    let error = outcome.unwrap_err();
    assert_eq!(error.message, "頻道頁面目前無法截圖");
}

// 原測試：captureScreenshot 會排在既有 page operation 後方
#[tokio::test(start_paused = true)]
async fn screenshot_queued_behind_pending_page_operation() {
    let page = create_mock_page(MockPageInput {
        marker: Some(HealthMarker::LiveContent),
        ..MockPageInput::default()
    });
    let manager = create_browser_manager(&page);
    let (health_tx, health_rx) = oneshot::channel();
    let evaluator = DeferredHealthEvaluator::new(health_rx);
    let parts = SessionParts::new(&page, create_reward_claimer(), &manager)
        .with_evaluator(evaluator.clone());
    let session = parts.build();
    session.start().await.unwrap();

    let health = spawn_check_health(&session);
    eventually(|| evaluator.calls() == 1).await;
    let screenshot = spawn_capture_screenshot(&session);
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
    assert_eq!(page.screenshot_calls(), 0);

    let _ = health_tx.send(ChannelHealthResult::healthy());
    let result = health.await.unwrap();
    assert_eq!(result, ChannelHealthResult::healthy());
    let outcome = screenshot.await.unwrap().unwrap();
    assert_eq!(outcome, b"png-image".to_vec());
    assert_eq!(page.screenshot_calls(), 1);

    session.stop("test_complete").await.unwrap();
}

// 原測試：觀看中可讀取忠誠點數，停止後回報 page_unavailable
#[tokio::test(start_paused = true)]
async fn channel_points_then_page_unavailable_after_stop() {
    let page = create_mock_page(MockPageInput {
        marker: Some(HealthMarker::LiveContent),
        channel_points_balance: Some("3,500".to_string()),
        ..MockPageInput::default()
    });
    let manager = create_browser_manager(&page);
    let logger = MockLogger::new();
    let claimer = create_reward_claimer();
    let parts = SessionParts::new(&page, claimer, &manager).with_logger(logger.clone());
    let session = parts.build();
    session.start().await.unwrap();

    let result = session.get_channel_points().await;
    assert_eq!(
        result,
        ChannelSessionPointsResult::Available { balance: 3_500 }
    );
    assert!(logger.called_with(
        LogLevel::Debug,
        "session_maintenance_completed",
        json!({"task": "points", "outcome": "completed"}),
    ));

    session.stop("test_complete").await.unwrap();
    let result = session.get_channel_points().await;
    assert_eq!(result, ChannelSessionPointsResult::PageUnavailable);
}

// 原測試：讀取失敗且無可領取 bonus 時回傳原結果且不領取
#[tokio::test(start_paused = true)]
async fn unavailable_points_without_claimable_bonus() {
    let page = create_mock_page(MockPageInput {
        marker: Some(HealthMarker::LiveContent),
        ..MockPageInput::default()
    });
    let manager = create_browser_manager(&page);
    let claimer = create_reward_claimer();
    let claimer_shared = Arc::clone(&claimer.shared);
    let parts = SessionParts::new(&page, claimer, &manager);
    let session = parts.build();
    session.start().await.unwrap();

    let result = session.get_channel_points().await;
    assert_eq!(result, ChannelSessionPointsResult::NotFound);
    assert_eq!(claimer_shared.has_claimable_calls(), 1);
    assert_eq!(claimer_shared.claim_calls(), 0);

    session.stop("test_complete").await.unwrap();
}

// 原測試：hasClaimableBonus 檢查拋錯時視為不可領取
#[tokio::test(start_paused = true)]
async fn claimable_check_error_treated_as_no_bonus() {
    let page = create_mock_page(MockPageInput {
        marker: Some(HealthMarker::LiveContent),
        ..MockPageInput::default()
    });
    let manager = create_browser_manager(&page);
    let claimer = create_reward_claimer();
    let claimer_shared = Arc::clone(&claimer.shared);
    claimer_shared.set_has_claimable(Err(DriverError::new("Error", "boom")));
    let parts = SessionParts::new(&page, claimer, &manager);
    let session = parts.build();
    session.start().await.unwrap();

    let result = session.get_channel_points().await;
    assert_eq!(result, ChannelSessionPointsResult::NotFound);
    assert_eq!(claimer_shared.claim_calls(), 0);

    session.stop("test_complete").await.unwrap();
}

// 原測試：被 bonus 蓋住時先領取再重讀回傳餘額
#[tokio::test(start_paused = true)]
async fn claims_bonus_then_rereads_balance() {
    let page = create_mock_page(MockPageInput {
        marker: Some(HealthMarker::LiveContent),
        ..MockPageInput::default()
    });
    let manager = create_browser_manager(&page);
    let page_shared = Arc::clone(&page.shared);
    let claimer = create_reward_claimer_impl(
        Arc::new(move |_page: &(dyn SessionPage + '_), channel: &str| {
            let page_shared = Arc::clone(&page_shared);
            let channel = channel.to_string();
            Box::pin(async move {
                *page_shared.channel_points_balance.lock().unwrap() = Some("3,500".to_string());
                Ok(RewardClaimResult::Claimed {
                    channel,
                    claimed_at: NOW_ISO.to_string(),
                }) as Result<RewardClaimResult, DriverError>
            })
                as std::pin::Pin<
                    Box<dyn Future<Output = Result<RewardClaimResult, DriverError>> + Send>,
                >
        }),
        Ok(true),
    );
    let claimer_shared = Arc::clone(&claimer.shared);
    let parts = SessionParts::new(&page, claimer, &manager).with_points_retry(1, 1);
    let session = parts.build();
    session.start().await.unwrap();

    let result = session.get_channel_points().await;
    assert_eq!(
        result,
        ChannelSessionPointsResult::Available { balance: 3_500 }
    );
    assert_eq!(claimer_shared.has_claimable_calls(), 1);
    assert_eq!(claimer_shared.claim_calls(), 1);
    assert_eq!(
        claimer_shared.last_claim_channel(),
        Some(CHANNEL.to_string())
    );

    session.stop("test_complete").await.unwrap();
}

// 原測試：領取後重讀仍失敗時回傳最後一次 unavailable
#[tokio::test(start_paused = true)]
async fn reread_failure_returns_last_unavailable() {
    let page = create_mock_page(MockPageInput {
        marker: Some(HealthMarker::LiveContent),
        ..MockPageInput::default()
    });
    let manager = create_browser_manager(&page);
    let claimer = create_reward_claimer_impl(claim_requested_now(), Ok(true));
    let claimer_shared = Arc::clone(&claimer.shared);
    let parts = SessionParts::new(&page, claimer, &manager).with_points_retry(2, 1);
    let session = parts.build();
    session.start().await.unwrap();

    let result = session.get_channel_points().await;
    assert_eq!(result, ChannelSessionPointsResult::NotFound);
    assert_eq!(claimer_shared.claim_calls(), 1);

    session.stop("test_complete").await.unwrap();
}

// 原測試：claim 拋錯時仍嘗試重讀餘額
#[tokio::test(start_paused = true)]
async fn claim_error_still_rereads_balance() {
    let page = create_mock_page(MockPageInput {
        marker: Some(HealthMarker::LiveContent),
        ..MockPageInput::default()
    });
    let manager = create_browser_manager(&page);
    let claimer = create_reward_claimer_impl(
        Arc::new(|_page: &(dyn SessionPage + '_), _channel: &str| {
            Box::pin(async move {
                Err(DriverError::new("Error", "claim exploded"))
                    as Result<RewardClaimResult, DriverError>
            })
                as std::pin::Pin<
                    Box<dyn Future<Output = Result<RewardClaimResult, DriverError>> + Send>,
                >
        }),
        Ok(true),
    );
    let claimer_shared = Arc::clone(&claimer.shared);
    let parts = SessionParts::new(&page, claimer, &manager).with_points_retry(1, 1);
    let session = parts.build();
    session.start().await.unwrap();

    let result = session.get_channel_points().await;
    assert_eq!(result, ChannelSessionPointsResult::NotFound);
    assert_eq!(claimer_shared.claim_calls(), 1);

    session.stop("test_complete").await.unwrap();
}

/// 舊測試 `claimedAt: NOW.toISOString()` 的 claimed 實作。
fn claim_requested_now() -> MockClaimImplementation {
    Arc::new(|_page: &(dyn SessionPage + '_), channel: &str| {
        Box::pin(async move {
            Ok(RewardClaimResult::Claimed {
                channel: channel.to_string(),
                claimed_at: NOW_ISO.to_string(),
            }) as Result<RewardClaimResult, DriverError>
        })
    })
}

// 原測試：明確辨識 %s 頁面
// 舊測試位於 channel-session.test.ts:888 的 it.each([...])（`as const)` 形式，DEC-020 修正 extract-spec 後已進 TSV）；
// 案例表照抄：[loginRequired, login_required]、[error, error_page]、[offline, offline]。
#[tokio::test(start_paused = true)]
async fn clearly_identifies_marked_pages() {
    let cases = [
        (HealthMarker::LoginRequired, "login_required"),
        (HealthMarker::Error, "error_page"),
        (HealthMarker::Offline, "offline"),
    ];
    for (marker, reason) in cases {
        let page = create_mock_page(MockPageInput {
            marker: Some(marker),
            ..MockPageInput::default()
        });
        let manager = create_browser_manager(&page);
        let parts = SessionParts::new(&page, create_reward_claimer(), &manager);
        let session = parts.build();
        session.start().await.unwrap();

        let result = session.check_health().await;
        assert_eq!(result.reason.as_str(), reason);
        assert!(!result.healthy);

        session.stop("test_complete").await.unwrap();
    }
}

// 原測試：健康檢查遇到可確認的 Twitch 內容警示時會按掉並回到 healthy
#[tokio::test(start_paused = true)]
async fn health_check_clicks_confirmable_content_warning() {
    // 舊測試一開始就是 contentWarning：start 先按一次，setMarker 後再按一次。
    let page = create_mock_page(MockPageInput {
        marker: Some(HealthMarker::ContentWarning),
        ..MockPageInput::default()
    });
    let manager = create_browser_manager(&page);
    let parts = SessionParts::new(&page, create_reward_claimer(), &manager);
    let session = parts.build();
    session.start().await.unwrap();
    page.set_marker(Some(HealthMarker::ContentWarning));

    let result = session.check_health().await;
    assert_eq!(result, ChannelHealthResult::healthy());
    assert_eq!(page.content_warning_clicks(), 2);

    session.stop("test_complete").await.unwrap();
}

// 原測試：健康檢查遇到無法確認的 Twitch 內容警示時回報 content_warning
#[tokio::test(start_paused = true)]
async fn health_check_reports_blocked_content_warning() {
    let page = create_mock_page(MockPageInput {
        marker: Some(HealthMarker::ContentWarning),
        url: Some(TARGET_URL.to_string()),
        behaviors: MockPageBehaviors {
            content_warning_click_error: Some(DriverError::new("Error", "button blocked")),
            ..MockPageBehaviors::default()
        },
        ..MockPageInput::default()
    });

    let result = crate::browser::health::evaluate_channel_health(
        &super::HealthPageView::new(page.as_ref()),
        TARGET_URL,
    )
    .await;
    assert_eq!(
        result,
        ChannelHealthResult::failed(ChannelHealthReason::ContentWarning)
    );
}

// 原測試：URL mismatch 與 page closed 回傳明確原因
#[tokio::test(start_paused = true)]
async fn url_mismatch_and_page_closed_have_explicit_reasons() {
    let page = create_mock_page(MockPageInput {
        marker: Some(HealthMarker::LiveContent),
        ..MockPageInput::default()
    });
    page.set_url("https://www.twitch.tv/another_channel");
    let result = crate::browser::health::evaluate_channel_health(
        &super::HealthPageView::new(page.as_ref()),
        TARGET_URL,
    )
    .await;
    assert_eq!(
        result,
        ChannelHealthResult::failed(ChannelHealthReason::UrlMismatch)
    );

    page.set_closed(true);
    let result = crate::browser::health::evaluate_channel_health(
        &super::HealthPageView::new(page.as_ref()),
        TARGET_URL,
    )
    .await;
    assert_eq!(
        result,
        ChannelHealthResult::failed(ChannelHealthReason::PageClosed)
    );
}

// 原測試：登入失敗不 reload，下一次健康才恢復 watching
#[tokio::test(start_paused = true)]
async fn login_failure_skips_reload_and_recovers_on_next_check() {
    let page = create_mock_page(MockPageInput {
        marker: Some(HealthMarker::LoginRequired),
        ..MockPageInput::default()
    });
    let manager = create_browser_manager(&page);
    let parts = SessionParts::new(&page, create_reward_claimer(), &manager);
    let session = parts.build();
    session.start().await.unwrap();

    let result = session.check_health().await;
    assert_eq!(
        result,
        ChannelHealthResult::failed(ChannelHealthReason::LoginRequired)
    );
    assert_eq!(page.reload_calls(), 0);
    assert_eq!(session.state(), ChannelSessionState::Recovering);

    page.set_marker(Some(HealthMarker::LiveContent));
    let result = session.check_health().await;
    assert_eq!(result, ChannelHealthResult::healthy());
    assert_eq!(session.state(), ChannelSessionState::Watching);

    session.stop("test_complete").await.unwrap();
}

// 原測試：%s 不重整，仍在連續失敗門檻後關閉並通知失效
// （it.each 案例表：login_required／login_missing）
#[tokio::test(start_paused = true)]
async fn login_reasons_skip_reload_and_trigger_threshold_failure() {
    for reason in [
        ChannelHealthReason::LoginRequired,
        ChannelHealthReason::LoginMissing,
    ] {
        let page = create_mock_page(MockPageInput {
            marker: Some(HealthMarker::LiveContent),
            ..MockPageInput::default()
        });
        let manager = create_browser_manager(&page);
        let invalidated = MockInvalidationObserver::new();
        let parts = SessionParts::new(&page, create_reward_claimer(), &manager)
            .with_evaluator(FixedHealthEvaluator::new(ChannelHealthResult::failed(
                reason,
            )))
            .with_invalidated(Arc::clone(&invalidated));
        let session = parts.build();
        session.start().await.unwrap();

        for _check in 0..3 {
            let result = session.check_health().await;
            assert_eq!(result, ChannelHealthResult::failed(reason));
        }
        assert_eq!(page.reload_calls(), 0);
        assert_eq!(manager.close_page_calls(), 1);
        assert_eq!(session.state(), ChannelSessionState::Failed);
        eventually_assert(
            || invalidated.call_args() == vec![CHANNEL.to_string()],
            "invalidation observer sees (channel, health_failure_threshold)",
        )
        .await;
    }
}

// 回歸測試：健康檢查持有 page operation 佇列時，手動 reload 已登記 flight 並排在佇列後方；
// 健康檢查判定要 reload 時必須略過，而不是等待那個排隊中的 reload（循環等待）。
#[tokio::test(start_paused = true)]
async fn health_reload_skips_queued_manual_reload_without_deadlock() {
    let page = create_mock_page(MockPageInput {
        marker: Some(HealthMarker::LiveContent),
        ..MockPageInput::default()
    });
    let manager = create_browser_manager(&page);
    let logger = MockLogger::new();
    let parts =
        SessionParts::new(&page, create_reward_claimer(), &manager).with_logger(logger.clone());
    let session = parts.build();
    session.start().await.unwrap();
    let reloads_before = page.reload_calls();

    // 健康評估卡在閘門上（此時健康檢查已持有 page operation 佇列）。
    let entered = Arc::new(AtomicBool::new(false));
    let (release_tx, release_rx) = oneshot::channel::<()>();
    let release_rx = Arc::new(Mutex::new(Some(release_rx)));
    let gate: MockNavigationSource = {
        let entered = Arc::clone(&entered);
        Arc::new(move || {
            entered.store(true, Ordering::SeqCst);
            let receiver = release_rx.lock().unwrap().take();
            Box::pin(async move {
                if let Some(receiver) = receiver {
                    let _ = receiver.await;
                }
                Ok(())
            })
        })
    };
    page.set_marker(Some(HealthMarker::Offline));
    page.set_evaluate_gate(Some(gate));

    let health = {
        let session = session.clone();
        tokio::spawn(async move { session.check_health().await })
    };
    eventually_assert(
        || entered.load(Ordering::SeqCst),
        "health check reaches evaluate",
    )
    .await;

    // 手動 reload 登記 flight 後排到佇列後方。
    let refresh = {
        let session = session.clone();
        tokio::spawn(async move { session.refresh_now().await })
    };
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    page.set_evaluate_gate(None);
    let _ = release_tx.send(());

    let result = tokio::time::timeout(Duration::from_secs(30), health)
        .await
        .expect("health check must not wait for the queued reload")
        .unwrap();
    assert!(!result.healthy);
    let refreshed = tokio::time::timeout(Duration::from_secs(30), refresh)
        .await
        .expect("queued manual reload completes after the health check")
        .unwrap()
        .unwrap();
    assert!(refreshed);
    assert_eq!(page.reload_calls(), reloads_before + 1);
    assert!(logger.called_with(
        LogLevel::Debug,
        "session_maintenance_skipped",
        json!({"channel": CHANNEL, "task": "reload", "reason": "reload_already_queued"}),
    ));
}

// 原測試：連續三次失敗後停止 timers、關閉 page 並進入 failed
#[tokio::test(start_paused = true)]
async fn three_consecutive_failures_stop_timers_and_fail_session() {
    let page = create_mock_page(MockPageInput {
        marker: Some(HealthMarker::Offline),
        ..MockPageInput::default()
    });
    let manager = create_browser_manager(&page);
    let invalidated = MockInvalidationObserver::new();
    let parts = SessionParts::new(&page, create_reward_claimer(), &manager)
        .with_invalidated(Arc::clone(&invalidated));
    let session = parts.build();
    session.start().await.unwrap();

    let _first = session.check_health().await;
    let _second = session.check_health().await;
    let _third = session.check_health().await;

    assert_eq!(page.reload_calls(), 1);
    assert_eq!(manager.close_page_calls(), 1);
    assert_eq!(session.state(), ChannelSessionState::Failed);
    eventually_assert(
        || invalidated.call_args() == vec![CHANNEL.to_string()],
        "invalidation fires once",
    )
    .await;
    let result = session.check_health().await;
    assert_eq!(
        result,
        ChannelHealthResult::failed(ChannelHealthReason::PageClosed)
    );
}

// 原測試：reward click_failed 只回報結果，不停止 session
#[tokio::test(start_paused = true)]
async fn reward_click_failed_keeps_session_watching() {
    let page = create_mock_page(MockPageInput {
        marker: Some(HealthMarker::LiveContent),
        ..MockPageInput::default()
    });
    let manager = create_browser_manager(&page);
    let claimer = create_reward_claimer_impl(click_failed_claim_implementation(), Ok(false));
    let parts = SessionParts::new(&page, claimer, &manager);
    let session = parts.build();
    session.start().await.unwrap();

    let result = session.tick_reward_claim().await;
    assert!(matches!(result, RewardClaimResult::ClickFailed { .. }));
    assert_eq!(session.state(), ChannelSessionState::Watching);
    assert_eq!(manager.close_page_calls(), 0);

    session.stop("test_complete").await.unwrap();
}

// 原測試：reward 連續失敗滿門檻時重整該頻道頁面
#[tokio::test(start_paused = true)]
async fn reward_failure_threshold_refreshes_page() {
    let page = create_mock_page(MockPageInput {
        marker: Some(HealthMarker::LiveContent),
        ..MockPageInput::default()
    });
    let manager = create_browser_manager(&page);
    let logger = MockLogger::new();
    let refresh_observer = MockRefreshObserver::new();
    let restart_observer = MockRestartObserver::new();
    let parts = SessionParts::new(
        &page,
        create_reward_claimer_impl(click_failed_claim_implementation(), Ok(false)),
        &manager,
    )
    .with_logger(logger.clone())
    .with_refresh_observer(Arc::clone(&refresh_observer))
    .with_restart_observer(Arc::clone(&restart_observer));
    let session = parts.build();
    session.start().await.unwrap();

    let mut results = Vec::new();
    for _index in 0..REWARD_FAILURE_RECOVERY_THRESHOLD {
        results.push(session.tick_reward_claim().await);
    }
    assert!(
        results
            .iter()
            .all(|result| matches!(result, RewardClaimResult::ClickFailed { .. }))
    );

    assert_eq!(page.reload_calls(), 1);
    assert_eq!(session.state(), ChannelSessionState::Watching);
    assert_eq!(restart_observer.call_records().len(), 0);
    assert!(logger.called_with(
        LogLevel::Warn,
        LogEvents::REWARD_CLAIM_FAILURE_THRESHOLD,
        json!({
            "channel": CHANNEL,
            "consecutiveFailures": REWARD_FAILURE_RECOVERY_THRESHOLD,
        }),
    ));
    assert!(logger.called_with(
        LogLevel::Warn,
        LogEvents::REWARD_CLAIM_FAILURE_THRESHOLD,
        json!({
            "channel": CHANNEL,
            "consecutiveFailures": REWARD_FAILURE_RECOVERY_THRESHOLD,
        }),
    ));
    assert!(logger.called_with(
        LogLevel::Warn,
        LogEvents::REWARD_CLAIM_FAILURE_RECOVERY_REFRESH,
        json!({
            "channel": CHANNEL,
            "threshold": REWARD_FAILURE_RECOVERY_THRESHOLD,
            "reason": "reward_claim_failure_threshold",
        }),
    ));
    eventually_assert(
        || {
            refresh_observer.call_records().iter().any(|event| {
                event.channel == CHANNEL
                    && event.reason.as_str() == "reward_claim_failure_threshold"
                    && event.started_at == NOW_ISO
            })
        },
        "onPageRefresh sees the reward threshold event",
    )
    .await;

    session.stop("test_complete").await.unwrap();
}

// 原測試：reward 重整後再次連續失敗會要求容器重啟
#[tokio::test(start_paused = true)]
async fn second_reward_failure_threshold_requests_container_restart() {
    let page = create_mock_page(MockPageInput {
        marker: Some(HealthMarker::LiveContent),
        ..MockPageInput::default()
    });
    let manager = create_browser_manager(&page);
    let logger = MockLogger::new();
    let restart_observer = MockRestartObserver::new();
    let parts = SessionParts::new(
        &page,
        create_reward_claimer_impl(click_failed_claim_implementation(), Ok(false)),
        &manager,
    )
    .with_logger(logger.clone())
    .with_restart_observer(Arc::clone(&restart_observer));
    let session = parts.build();
    session.start().await.unwrap();

    for _index in 0..(REWARD_FAILURE_RECOVERY_THRESHOLD * 2) {
        let _ = session.tick_reward_claim().await;
    }
    let _ = session.tick_reward_claim().await;

    assert_eq!(page.reload_calls(), 1);
    eventually_assert(
        || restart_observer.call_records().len() == 1,
        "container restart requested once",
    )
    .await;
    let requests = restart_observer.call_records();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.channel, CHANNEL);
    assert_eq!(request.reason, "reward_claim_failure_after_refresh");
    assert_eq!(
        request.consecutive_failures,
        REWARD_FAILURE_RECOVERY_THRESHOLD as u32
    );
    assert_eq!(request.requested_at, NOW_ISO);
    // Canonical container_restart_requested is emitted by ContainerRestartController
    // (observer), not by the session itself when an observer is wired.
    assert_eq!(
        logger.call_count(LogLevel::Error, LogEvents::CONTAINER_RESTART_REQUESTED),
        0
    );

    session.stop("test_complete").await.unwrap();
}

// 原測試：reward 失敗後成功或 not_found 會重置復原狀態
#[tokio::test(start_paused = true)]
async fn reward_success_or_not_found_resets_recovery_state() {
    let page = create_mock_page(MockPageInput {
        marker: Some(HealthMarker::LiveContent),
        ..MockPageInput::default()
    });
    let manager = create_browser_manager(&page);
    let outcomes: VecDeque<RewardClaimResult> = {
        let mut outcomes = VecDeque::new();
        for _index in 0..(REWARD_FAILURE_RECOVERY_THRESHOLD - 1) {
            outcomes.push_back(RewardClaimResult::ClickFailed {
                channel: CHANNEL.to_string(),
                checked_at: NOW_ISO.to_string(),
                error: "click failed".to_string(),
            });
        }
        outcomes.push_back(RewardClaimResult::NotFound {
            channel: CHANNEL.to_string(),
            checked_at: NOW_ISO.to_string(),
        });
        for _index in 0..(REWARD_FAILURE_RECOVERY_THRESHOLD - 1) {
            outcomes.push_back(RewardClaimResult::ClickFailed {
                channel: CHANNEL.to_string(),
                checked_at: NOW_ISO.to_string(),
                error: "click failed".to_string(),
            });
        }
        outcomes.push_back(RewardClaimResult::Claimed {
            channel: CHANNEL.to_string(),
            claimed_at: NOW_ISO.to_string(),
        });
        outcomes
    };
    let restart_observer = MockRestartObserver::new();
    let parts = SessionParts::new(
        &page,
        create_reward_claimer_impl(sequence_claim_implementation(outcomes), Ok(false)),
        &manager,
    )
    .with_restart_observer(Arc::clone(&restart_observer));
    let session = parts.build();
    session.start().await.unwrap();

    // 舊測試：`while (outcomes.length > 0)`。
    for _index in 0..(2 * REWARD_FAILURE_RECOVERY_THRESHOLD) {
        let _ = session.tick_reward_claim().await;
    }

    assert_eq!(page.reload_calls(), 0);
    assert_eq!(restart_observer.call_records().len(), 0);

    session.stop("test_complete").await.unwrap();
}

// 原測試：獎勵操作前 URL 已偏離目標時不呼叫 RewardClaimer
#[tokio::test(start_paused = true)]
async fn reward_skipped_when_url_drifted() {
    let page = create_mock_page(MockPageInput {
        marker: Some(HealthMarker::LiveContent),
        ..MockPageInput::default()
    });
    let manager = create_browser_manager(&page);
    let claimer = create_reward_claimer();
    let claimer_shared = Arc::clone(&claimer.shared);
    let parts = SessionParts::new(&page, claimer, &manager);
    let session = parts.build();
    session.start().await.unwrap();
    page.set_url("https://example.test/fake-channel");

    let result = session.tick_reward_claim().await;
    match result {
        RewardClaimResult::NotFound {
            channel,
            checked_at,
        } => {
            assert_eq!(channel, CHANNEL);
            assert_eq!(checked_at, NOW_ISO);
        }
        other => panic!("expected not_found, got {other:?}"),
    }
    assert_eq!(claimer_shared.claim_calls(), 0);

    session.stop("test_complete").await.unwrap();
}

// 原測試：timer 工作不重入，且同一頁面的維護操作依序執行
#[tokio::test(start_paused = true)]
async fn timer_works_do_not_reenter_and_page_operations_serialize() {
    let page = create_mock_page(MockPageInput {
        marker: Some(HealthMarker::LiveContent),
        ..MockPageInput::default()
    });
    let manager = create_browser_manager(&page);
    let (health_tx, health_rx) = oneshot::channel::<ChannelHealthResult>();
    let (reward_tx, reward_rx) = oneshot::channel::<Result<RewardClaimResult, DriverError>>();
    let health_evaluator = DeferredHealthEvaluator::new(health_rx);
    let reward_receiver = Arc::new(Mutex::new(Some(reward_rx)));
    let reward_cl: MockClaimImplementation =
        Arc::new(move |_page: &(dyn SessionPage + '_), _channel: &str| {
            let reward_receiver = Arc::clone(&reward_receiver);
            Box::pin(async move {
                let rx = reward_receiver.lock().unwrap().take();
                match rx {
                    Some(rx) => match rx.await {
                        Ok(outcome) => outcome,
                        Err(_) => Err(DriverError::new("Error", "deferred reward dropped")),
                    },
                    None => Err(DriverError::new("Error", "deferred reward reused")),
                }
            })
        });
    let claimer = create_reward_claimer_impl(reward_cl, Ok(false));
    let claimer_shared = Arc::clone(&claimer.shared);
    let parts = SessionParts::new(&page, claimer, &manager)
        .with_config(create_config(ConfigOverrides {
            page_health_check_interval_seconds: Some(1.0),
            reward_check_interval_seconds: Some(1.0),
            ..ConfigOverrides::default()
        }))
        .with_evaluator(health_evaluator.clone());
    let session = parts.build();
    session.start().await.unwrap();

    timer_warmup().await;
    tokio::time::advance(Duration::from_secs(6)).await;
    eventually(|| health_evaluator.calls() == 1).await;
    assert_eq!(claimer_shared.claim_calls(), 0);

    tokio::time::advance(Duration::from_secs(5)).await;
    tokio::task::yield_now().await;
    assert_eq!(health_evaluator.calls(), 1);
    assert_eq!(claimer_shared.claim_calls(), 0);

    let _ = health_tx.send(ChannelHealthResult::healthy());
    eventually(|| claimer_shared.claim_calls() == 1).await;

    let stop = spawn_stop(&session, "shutdown");
    tokio::task::yield_now().await;
    assert_eq!(manager.close_page_calls(), 0);

    let _ = reward_tx.send(Ok(RewardClaimResult::NotFound {
        channel: CHANNEL.to_string(),
        checked_at: NOW_ISO.to_string(),
    }));
    let outcome = stop.await.unwrap();
    assert!(outcome.is_ok(), "stop resolves: {outcome:?}");
    assert_eq!(manager.close_page_calls(), 1);

    tokio::time::advance(Duration::from_secs(5)).await;
    tokio::task::yield_now().await;
    assert_eq!(health_evaluator.calls(), 1);
    assert_eq!(claimer_shared.claim_calls(), 1);
}

// 原測試：依設定定時刷新頁面並在 reload 後維持 watching
#[tokio::test(start_paused = true)]
async fn scheduled_page_refresh_keeps_watching() {
    let page = create_mock_page(MockPageInput {
        marker: Some(HealthMarker::LiveContent),
        ..MockPageInput::default()
    });
    let manager = create_browser_manager(&page);
    let logger = MockLogger::new();
    let refresh_observer = MockRefreshObserver::new();
    let parts = SessionParts::new(&page, create_reward_claimer(), &manager)
        .with_logger(logger.clone())
        .with_refresh_observer(Arc::clone(&refresh_observer))
        .with_config(create_config(ConfigOverrides {
            page_refresh_interval_seconds: Some(1.0),
            ..ConfigOverrides::default()
        }));
    let session = parts.build();
    session.start().await.unwrap();

    let status = session.get_refresh_status();
    assert_eq!(status.channel, CHANNEL);
    assert!(status.enabled);
    assert!(status.next_refresh_at.is_some());
    assert!(status.seconds_until_refresh.is_some());

    timer_warmup().await;
    tokio::time::advance(Duration::from_secs(61)).await;
    eventually(|| page.reload_calls() == 1).await;

    assert_eq!(session.state(), ChannelSessionState::Watching);
    eventually(|| {
        logger.called_with(
            LogLevel::Info,
            "page_refreshed",
            json!({"channel": CHANNEL, "reason": "scheduled_refresh"}),
        )
    })
    .await;
    eventually(|| {
        refresh_observer
            .call_records()
            .iter()
            .any(|event| event.reason.as_str() == "scheduled_refresh")
    })
    .await;

    session.stop("test_complete").await.unwrap();
}

// 原測試：重整後遇到 Twitch 內容警示時會重新按 Start Watching
#[tokio::test(start_paused = true)]
async fn refresh_clicks_content_warning_again() {
    let page = create_mock_page(MockPageInput {
        marker: Some(HealthMarker::LiveContent),
        ..MockPageInput::default()
    });
    let manager = create_browser_manager(&page);
    let parts = SessionParts::new(&page, create_reward_claimer(), &manager);
    let session = parts.build();
    session.start().await.unwrap();

    page.set_marker(Some(HealthMarker::ContentWarning));
    let refreshed = session.refresh_now().await.unwrap();
    assert!(refreshed);

    assert_eq!(page.content_warning_clicks(), 1);
    let result = session.check_health().await;
    assert_eq!(result, ChannelHealthResult::healthy());

    session.stop("test_complete").await.unwrap();
}

// 原測試：page_refresh_interval_seconds 為 0 時不排程定時刷新
#[tokio::test(start_paused = true)]
async fn zero_refresh_interval_disables_schedule() {
    let page = create_mock_page(MockPageInput {
        marker: Some(HealthMarker::LiveContent),
        ..MockPageInput::default()
    });
    let manager = create_browser_manager(&page);
    let parts = SessionParts::new(&page, create_reward_claimer(), &manager).with_config(
        create_config(ConfigOverrides {
            page_refresh_interval_seconds: Some(0.0),
            ..ConfigOverrides::default()
        }),
    );
    let session = parts.build();
    session.start().await.unwrap();

    let status = session.get_refresh_status();
    assert_eq!(status.channel, CHANNEL);
    assert!(!status.enabled);
    assert_eq!(status.next_refresh_at, None);
    assert_eq!(status.seconds_until_refresh, None);

    timer_warmup().await;
    tokio::time::advance(Duration::from_secs(3_600)).await;
    tokio::task::yield_now().await;

    assert_eq!(page.reload_calls(), 0);
    session.stop("test_complete").await.unwrap();
}

// 原測試：refreshNow 會手動重整頁面並送出 manual_refresh 事件
#[tokio::test(start_paused = true)]
async fn manual_refresh_posts_manual_refresh_event() {
    let page = create_mock_page(MockPageInput {
        marker: Some(HealthMarker::LiveContent),
        ..MockPageInput::default()
    });
    let manager = create_browser_manager(&page);
    let refresh_observer = MockRefreshObserver::new();
    let parts = SessionParts::new(&page, create_reward_claimer(), &manager)
        .with_refresh_observer(Arc::clone(&refresh_observer));
    let session = parts.build();
    session.start().await.unwrap();

    let refreshed = session.refresh_now().await.unwrap();
    assert!(refreshed);

    assert_eq!(page.reload_calls(), 1);
    eventually(|| {
        refresh_observer.call_records().iter().any(|event| {
            event.channel == CHANNEL
                && event.reason.as_str() == "manual_refresh"
                && event.started_at == NOW_ISO
        })
    })
    .await;

    session.stop("test_complete").await.unwrap();
}

// 原測試：stop 會 abort 進行中的 reload 並快速完成清理
#[tokio::test(start_paused = true)]
async fn stop_aborts_pending_reload_and_cleans_quickly() {
    let (reload_tx, reload_rx) = oneshot::channel::<Result<(), DriverError>>();
    let receiver = Arc::new(Mutex::new(Some(reload_rx)));
    let reload_source: MockNavigationSource = Arc::new(move || {
        let receiver = Arc::clone(&receiver);
        Box::pin(async move {
            let rx = receiver.lock().unwrap().take();
            match rx {
                Some(rx) => rx.await.unwrap_or(Err(aborted_error())),
                None => Err(DriverError::new("Error", "deferred reload reused")),
            }
        })
    });
    let page = create_mock_page(MockPageInput {
        marker: Some(HealthMarker::LiveContent),
        behaviors: MockPageBehaviors {
            reload_implementation: Some(reload_source),
            ..MockPageBehaviors::default()
        },
        ..MockPageInput::default()
    });
    let manager = create_browser_manager(&page);
    let logger = MockLogger::new();
    let parts =
        SessionParts::new(&page, create_reward_claimer(), &manager).with_logger(logger.clone());
    let session = parts.build();
    session.start().await.unwrap();

    let refresh = {
        let session = session.clone();
        tokio::spawn(async move { session.refresh_now().await })
    };
    eventually(|| page.reload_calls() == 1).await;

    let stopped = session.stop("shutdown").await;
    assert!(stopped.is_ok(), "stop resolves: {stopped:?}");
    assert_eq!(manager.close_page_calls(), 1);
    assert_eq!(session.state(), ChannelSessionState::Stopped);
    assert!(logger.called_with(
        LogLevel::Debug,
        "session_operation_aborted",
        json!({"channel": CHANNEL, "reason": "shutdown", "outcome": "aborted"}),
    ));

    let _ = reload_tx.send(Ok(()));
    let outcome = refresh.await.unwrap();
    let error = outcome.unwrap_err();
    assert_eq!(error.message, "Navigation aborted");
}

// 原測試：stop 等待無法 abort 的 page operation 超時後會強制關閉 page
#[tokio::test(start_paused = true)]
async fn stop_times_out_unabordable_operation_and_force_closes_page() {
    let screenshot_source: MockScreenshotSource = Arc::new(|| Box::pin(std::future::pending()));
    let page = create_mock_page(MockPageInput {
        marker: Some(HealthMarker::LiveContent),
        behaviors: MockPageBehaviors {
            screenshot_implementation: Some(screenshot_source),
            ..MockPageBehaviors::default()
        },
        ..MockPageInput::default()
    });
    let manager = create_browser_manager(&page);
    let parts =
        SessionParts::new(&page, create_reward_claimer(), &manager).with_drain_timeout(1_000);
    let session = parts.build();
    session.start().await.unwrap();

    let screenshot = spawn_capture_screenshot(&session);
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
    assert_eq!(page.screenshot_calls(), 1);

    let stop = spawn_stop(&session, "shutdown");
    tokio::task::yield_now().await;
    assert_eq!(manager.close_page_calls(), 0);

    tokio::time::advance(Duration::from_millis(999)).await;
    assert_eq!(manager.close_page_calls(), 0);
    tokio::time::advance(Duration::from_millis(1)).await;
    eventually(|| manager.close_page_calls() == 1).await;
    let outcome = stop.await.unwrap();
    assert!(outcome.is_ok(), "stop resolves: {outcome:?}");
    assert_eq!(session.state(), ChannelSessionState::Stopped);

    // 舊測試：`void screenshotPromise.catch(() => undefined)`——放任永不結束的 future。
    screenshot.abort();
}

// 原測試：RewardClaimer 拋錯時轉為 click_failed 且避免 timer rejection
#[tokio::test(start_paused = true)]
async fn claimer_error_maps_to_click_failed_without_timer_rejection() {
    let page = create_mock_page(MockPageInput {
        marker: Some(HealthMarker::LiveContent),
        ..MockPageInput::default()
    });
    let manager = create_browser_manager(&page);
    let logger = MockLogger::new();
    let parts = SessionParts::new(
        &page,
        create_reward_claimer_impl(
            Arc::new(|_page: &(dyn SessionPage + '_), _channel: &str| {
                Box::pin(async move {
                    Err(DriverError::new("Error", "access_token=secret-value"))
                        as Result<RewardClaimResult, DriverError>
                })
                    as std::pin::Pin<
                        Box<dyn Future<Output = Result<RewardClaimResult, DriverError>> + Send>,
                    >
            }),
            Ok(false),
        ),
        &manager,
    )
    .with_logger(logger.clone());
    let session = parts.build();
    session.start().await.unwrap();

    let result = session.tick_reward_claim().await;
    match result {
        RewardClaimResult::ClickFailed { error, .. } => {
            assert!(
                !error.contains("secret-value"),
                "error must be redacted: {error:?}"
            );
        }
        other => panic!("expected click_failed, got {other:?}"),
    }
    assert_eq!(session.state(), ChannelSessionState::Watching);
    assert!(logger.called_with(
        LogLevel::Warn,
        LogEvents::REWARD_CLAIM_FAILED,
        json!({"channel": CHANNEL}),
    ));

    session.stop("test_complete").await.unwrap();
}

// 原測試：只接受設定模組已驗證且存在於 channels 的名稱
#[tokio::test(start_paused = true)]
async fn factory_only_accepts_validated_configured_channel_names() {
    let page = create_mock_page(MockPageInput::default());
    let manager = create_browser_manager(&page);
    let claimer = create_reward_claimer();
    let factory = DefaultChannelSessionFactory::new(DefaultChannelSessionFactoryOptions {
        config: create_config(ConfigOverrides::default()),
        browser_manager: manager,
        reward_claimer: Arc::new(claimer),
        playback_optimizer: None,
        logger: None,
        health_failure_threshold: None,
        health_evaluator: None,
        on_invalidated: None,
        on_page_refresh: None,
        on_container_restart_requested: None,
        page_operation_drain_timeout_ms: None,
        points_claim_retry_attempts: None,
        points_claim_retry_delay_ms: None,
        now: Some(fixed_now()),
    });

    let session = factory.create(CHANNEL).unwrap();
    assert_eq!(session.channel(), CHANNEL);

    let error = factory.create("../escape").map(|_| ()).unwrap_err();
    assert_eq!(error.message, "Channel 必須是 1 到 25 字元的英數字或底線");
    let error = factory.create("not_configured").map(|_| ()).unwrap_err();
    assert!(
        error.message.contains("不在已驗證的設定清單中"),
        "{error:?}"
    );
}
