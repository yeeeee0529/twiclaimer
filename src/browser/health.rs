//! 舊版 `src/browser/ChannelHealthChecker.ts` 的移植：單頁健康判定（S3.4）。
//!
//! 頁面依賴以本檔的 [`HealthPage`]／[`HealthLocator`] trait 注入（D12；同
//! side_nav／stream_playback_optimizer 的做法：正式端由 [`CdpHealthPage`] 包
//! cdp `PageShared` 測試端注入舊測試同形的 fake page）。DOM snapshot 的 evaluate
//! 函式從舊版 `dist` 編譯輸出逐字複製在 `js/channel-health-snapshot.js`
//! （DEC-004：送進瀏覽器前剝除檔頭 `//` 註解行再 trim）。本檔不持任何鎖、
//! 不直接使用時鐘（逾時值照舊版逐字搬遷，§7.6）。
//!
//! 行為基準依 DEC-016 為舊版 `d49363d`：`login_required` 與一般未登入
//! （`login_missing`）是兩種不同的失敗原因。
#![allow(dead_code)]

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use serde_json::{Value, json};

use super::cdp::context::DriverError;
use super::cdp::locator::{Locator, WaitForState};
use super::cdp::page::PageShared;
use super::channel_url::is_expected_channel_url;
use super::safe_logging::safe_error_message;
use super::selectors::CHANNEL_HEALTH_SELECTORS;

/// 舊版 `CONTENT_WARNING_SETTLE_TIMEOUT_MS`。
const CONTENT_WARNING_SETTLE_TIMEOUT_MS: u64 = 3_000;

/// 舊版 `ChannelHealthFailureReason`（含成功分支的 `'live'` 字面值，故命名為
/// `ChannelHealthReason`；`as_str()` 逐字對應舊版字串）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChannelHealthReason {
    Live,
    NotStarted,
    PageClosed,
    UrlMismatch,
    LoginRequired,
    LoginMissing,
    ErrorPage,
    Offline,
    ContentWarning,
    LiveContentMissing,
    HealthCheckError,
}

impl ChannelHealthReason {
    /// 舊版 union 的字面值（日誌欄位與測試斷言用）。
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ChannelHealthReason::Live => "live",
            ChannelHealthReason::NotStarted => "not_started",
            ChannelHealthReason::PageClosed => "page_closed",
            ChannelHealthReason::UrlMismatch => "url_mismatch",
            ChannelHealthReason::LoginRequired => "login_required",
            ChannelHealthReason::LoginMissing => "login_missing",
            ChannelHealthReason::ErrorPage => "error_page",
            ChannelHealthReason::Offline => "offline",
            ChannelHealthReason::ContentWarning => "content_warning",
            ChannelHealthReason::LiveContentMissing => "live_content_missing",
            ChannelHealthReason::HealthCheckError => "health_check_error",
        }
    }
}

/// 舊版 `ChannelHealthResult`：成功時 `reason` 恆為 `'live'`、`error` 恆為 undefined。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ChannelHealthResult {
    pub(crate) healthy: bool,
    pub(crate) reason: ChannelHealthReason,
    /// 舊版只在 `health_check_error` 帶出 `safeErrorMessage(error)`。
    pub(crate) error: Option<String>,
}

impl ChannelHealthResult {
    /// 舊版 `{ healthy: true, reason: 'live' }`。
    pub(crate) fn healthy() -> Self {
        Self {
            healthy: true,
            reason: ChannelHealthReason::Live,
            error: None,
        }
    }

    /// 舊版 `{ healthy: false, reason }`。
    pub(crate) fn failed(reason: ChannelHealthReason) -> Self {
        Self {
            healthy: false,
            reason,
            error: None,
        }
    }

    /// 舊版 `{ healthy: false, reason, error }`。
    pub(crate) fn failed_with_error(reason: ChannelHealthReason, error: String) -> Self {
        Self {
            healthy: false,
            reason,
            error: Some(error),
        }
    }
}

/// 舊版 `ContentWarningAcceptResult`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ContentWarningAcceptResult {
    NotPresent,
    Accepted,
    Blocked,
}

impl ContentWarningAcceptResult {
    /// 舊版 union 的字面值。
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ContentWarningAcceptResult::NotPresent => "not_present",
            ContentWarningAcceptResult::Accepted => "accepted",
            ContentWarningAcceptResult::Blocked => "blocked",
        }
    }
}

/// 舊版 `ChannelHealthSnapshot`（page.evaluate 回傳值的 Rust 形狀）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ChannelHealthSnapshot {
    pub(crate) login_required: bool,
    pub(crate) logged_out: bool,
    pub(crate) error: bool,
    pub(crate) offline: bool,
    pub(crate) live_content: bool,
    pub(crate) unsupported_player_error: bool,
}

/// 本檔使用的頁面 API（對應舊版 `Page` 用到的子集）。
#[async_trait]
pub(crate) trait HealthPage: Send + Sync {
    /// 舊版 `page.isClosed()`。
    fn is_closed(&self) -> bool;

    /// 舊版 `page.url()`。
    fn url(&self) -> String;

    /// 舊版 `page.evaluate(fn, arg)`（X7 前綴 `page.evaluate`）。
    async fn evaluate(
        &self,
        fn_declaration: &str,
        arg: Option<Value>,
    ) -> Result<Value, DriverError>;

    /// 舊版 `page.locator(selector)`；builder 不失敗，操作時才回錯。
    fn locator(&self, selector: &str) -> Arc<dyn HealthLocator>;
}

/// 本檔使用的 locator API（對應舊版 `Locator` 用到的子集）。
#[async_trait]
pub(crate) trait HealthLocator: Send + Sync {
    /// 舊版 `locator.first()`。
    fn first(&self) -> Arc<dyn HealthLocator>;

    /// 舊版 `locator.isVisible()`（不等待；strict）。
    async fn is_visible(&self) -> Result<bool, DriverError>;

    /// 舊版 `locator.click({timeout})`。
    async fn click(&self, timeout: Option<u64>) -> Result<(), DriverError>;

    /// 舊版 `locator.waitFor({state: 'visible', timeout})`。
    async fn wait_visible(&self, timeout: Option<u64>) -> Result<(), DriverError>;
}

/// 正式端的頁面轉接：包 cdp 的 [`PageShared`]。
pub(crate) struct CdpHealthPage {
    shared: Arc<PageShared>,
}

impl CdpHealthPage {
    pub(crate) fn new(shared: Arc<PageShared>) -> Self {
        Self { shared }
    }
}

#[async_trait]
impl HealthPage for CdpHealthPage {
    fn is_closed(&self) -> bool {
        self.shared.is_closed()
    }

    fn url(&self) -> String {
        self.shared.url()
    }

    async fn evaluate(
        &self,
        fn_declaration: &str,
        arg: Option<Value>,
    ) -> Result<Value, DriverError> {
        self.shared.evaluate_function(fn_declaration, arg).await
    }

    fn locator(&self, selector: &str) -> Arc<dyn HealthLocator> {
        Arc::new(CdpHealthLocator {
            inner: self.shared.locator(selector),
        })
    }
}

struct CdpHealthLocator {
    inner: Locator,
}

#[async_trait]
impl HealthLocator for CdpHealthLocator {
    fn first(&self) -> Arc<dyn HealthLocator> {
        Arc::new(Self {
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
        self.inner.wait_for(WaitForState::Visible, timeout).await
    }
}

/// 舊版 `evaluateChannelHealth(page, targetUrl)`：包 try/catch 的對外入口，
/// 任何錯誤都折為 `health_check_error`（永不向外拋錯）。
pub(crate) async fn evaluate_channel_health(
    page: &dyn HealthPage,
    target_url: &str,
) -> ChannelHealthResult {
    // 舊版的 try/catch：同步與 await 段的錯誤一律落.health_check_error。
    match evaluate_channel_health_inner(page, target_url).await {
        Ok(result) => result,
        Err(error) => ChannelHealthResult::failed_with_error(
            ChannelHealthReason::HealthCheckError,
            safe_error_message(Some(&error.message)),
        ),
    }
}

/// 舊版 `evaluateChannelHealth()` 的 try 本體；可拋錯的部分只有 DOM snapshot。
async fn evaluate_channel_health_inner(
    page: &dyn HealthPage,
    target_url: &str,
) -> Result<ChannelHealthResult, DriverError> {
    if page.is_closed() {
        return Ok(ChannelHealthResult::failed(ChannelHealthReason::PageClosed));
    }

    if !is_expected_channel_url(&page.url(), target_url) {
        return Ok(ChannelHealthResult::failed(
            ChannelHealthReason::UrlMismatch,
        ));
    }

    let snapshot = read_channel_health_snapshot(page).await?;

    if snapshot.login_required {
        return Ok(ChannelHealthResult::failed(
            ChannelHealthReason::LoginRequired,
        ));
    }

    if snapshot.error || snapshot.unsupported_player_error {
        return Ok(ChannelHealthResult::failed(ChannelHealthReason::ErrorPage));
    }

    if snapshot.offline {
        return Ok(ChannelHealthResult::failed(ChannelHealthReason::Offline));
    }

    let content_warning_result = accept_content_warning(page).await?;
    if content_warning_result == ContentWarningAcceptResult::Accepted
        && is_visible_of(page.locator(CHANNEL_HEALTH_SELECTORS.live_content)).await?
    {
        return Ok(if snapshot.logged_out {
            ChannelHealthResult::failed(ChannelHealthReason::LoginMissing)
        } else {
            ChannelHealthResult::healthy()
        });
    }
    if content_warning_result != ContentWarningAcceptResult::NotPresent {
        return Ok(ChannelHealthResult::failed(
            ChannelHealthReason::ContentWarning,
        ));
    }

    if snapshot.logged_out {
        return Ok(ChannelHealthResult::failed(
            ChannelHealthReason::LoginMissing,
        ));
    }

    if snapshot.live_content {
        return Ok(ChannelHealthResult::healthy());
    }

    Ok(ChannelHealthResult::failed(
        ChannelHealthReason::LiveContentMissing,
    ))
}

/// 舊版 `acceptContentWarning(page)`：`.first()` 的 isVisible → 點擊 → 等 live
/// content（等待失敗吞掉）。`isVisible` 的 strict 錯誤照舊版向外拋（由
/// `evaluateChannelHealth` 的 catch 接住）。
pub(crate) async fn accept_content_warning(
    page: &dyn HealthPage,
) -> Result<ContentWarningAcceptResult, DriverError> {
    let button = page
        .locator(CHANNEL_HEALTH_SELECTORS.content_warning)
        .first();
    if !is_visible_of(Arc::clone(&button)).await? {
        return Ok(ContentWarningAcceptResult::NotPresent);
    }

    // 點擊失敗 → blocked；waitFor 失敗 → 舊版 `.catch(() => undefined)` 吞掉仍 accepted。
    if button
        .click(Some(CONTENT_WARNING_SETTLE_TIMEOUT_MS))
        .await
        .is_err()
    {
        return Ok(ContentWarningAcceptResult::Blocked);
    }
    let _ = page
        .locator(CHANNEL_HEALTH_SELECTORS.live_content)
        .first()
        .wait_visible(Some(CONTENT_WARNING_SETTLE_TIMEOUT_MS))
        .await;
    Ok(ContentWarningAcceptResult::Accepted)
}

/// 舊版 module-private `isVisible(locator)`：`locator.first().isVisible()`。
async fn is_visible_of(locator: Arc<dyn HealthLocator>) -> Result<bool, DriverError> {
    locator.first().is_visible().await
}

/// 舊版 module-private `readChannelHealthSnapshot(page)`：單一 `page.evaluate`
/// 以 `js/channel-health-snapshot.js` 讀全部判定點（照舊版 sort 的 selectors 參數）。
async fn read_channel_health_snapshot(
    page: &dyn HealthPage,
) -> Result<ChannelHealthSnapshot, DriverError> {
    let arg = json!({
        "loginRequired": CHANNEL_HEALTH_SELECTORS.login_required,
        "loggedOut": CHANNEL_HEALTH_SELECTORS.logged_out,
        "error": CHANNEL_HEALTH_SELECTORS.error,
        "offline": CHANNEL_HEALTH_SELECTORS.offline,
        "liveContent": CHANNEL_HEALTH_SELECTORS.live_content,
        "playerErrorText": concat!(
            "[data-a-target=\"player-error-message\"], ",
            "[data-test-selector=\"video-player\"], ",
            "[data-a-target=\"video-player\"]",
        ),
    });
    let value = page.evaluate(health_snapshot_script(), Some(arg)).await?;
    Ok(parse_health_snapshot(&value))
}

/// 照舊版語意把 evaluate 的回傳值轉成 snapshot；缺鍵（undefined）與非布林值
/// 以 JS 真值規則折為 `false`／`true`（與 `snapshot.loginRequired` 的取值同）。
fn parse_health_snapshot(value: &Value) -> ChannelHealthSnapshot {
    let field = |key: &str| -> bool { value.get(key).map(js_truthy).unwrap_or(false) };
    ChannelHealthSnapshot {
        login_required: field("loginRequired"),
        logged_out: field("loggedOut"),
        error: field("error"),
        offline: field("offline"),
        live_content: field("liveContent"),
        unsupported_player_error: field("unsupportedPlayerError"),
    }
}

/// JS truthiness：`null`／`undefined` 為 false，物件與陣列為 true，其餘照值。
fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => {
            matches!(number.as_f64(), Some(number) if number != 0.0 && !number.is_nan())
        }
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// `include_str!` 載入 `js/channel-health-snapshot.js`，剝除開頭連續 `//` 註解行
/// 後 trim（DEC-004）；送進瀏覽器的是 `page.evaluate` 的確切函式文字。
fn health_snapshot_script() -> &'static str {
    static SCRIPT: OnceLock<String> = OnceLock::new();
    SCRIPT
        .get_or_init(|| {
            let raw = include_str!("js/channel-health-snapshot.js");
            let header_lines = raw
                .split('\n')
                .take_while(|line| line.trim_start().starts_with("//"))
                .count();
            raw.split('\n')
                .skip(header_lines)
                .collect::<Vec<_>>()
                .join("\n")
                .trim()
                .to_string()
        })
        .as_str()
}

/// 舊版 `ChannelHealthEvaluator` 型別（`(page, targetUrl) => Promise<…>`）的
/// trait 形狀；可拋錯（Rust 以 `Err` 表達，呼叫端折為 health_check_error）。
#[async_trait]
pub(crate) trait ChannelHealthEvaluator: Send + Sync {
    async fn evaluate(
        &self,
        page: &dyn HealthPage,
        target_url: &str,
    ) -> Result<ChannelHealthResult, DriverError>;
}

/// 舊版預設的 `evaluateChannelHealth`（永遠不拋錯：內部照舊版 catch 全收）。
pub(crate) struct DefaultChannelHealthEvaluator;

#[async_trait]
impl ChannelHealthEvaluator for DefaultChannelHealthEvaluator {
    async fn evaluate(
        &self,
        page: &dyn HealthPage,
        target_url: &str,
    ) -> Result<ChannelHealthResult, DriverError> {
        Ok(evaluate_channel_health(page, target_url).await)
    }
}

#[cfg(test)]
mod tests;
