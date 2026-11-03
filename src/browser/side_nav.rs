//! `SideNavCollapser.ts` 的移植：視需要把 Twitch 左側欄收合。
//!
//! 頁面依賴以 [`super::types::BrowserPage`] 注入（D12）；為了讓單元測試以 fake
//! 模擬舊測試的 Playwright fake page，內部改透過僅涵蓋本檔用到的
//! [`SideNavPage`]／[`SideNavLocator`] trait 操作（同 stream_playback_optimizer
//! 的做法）。判定「側欄是否展開」的 evaluate 函式照 §4.2／DEC-004 從
//! `src/browser/js/side-nav-state.js` 以 `include_str!` 載入。逾時值照舊版
//! 傳給 cdp `Locator`；本檔不直接使用時鐘、不持任何鎖 await 瀏覽器操作。
#![allow(dead_code)]

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use serde_json::{Value, json};

use super::cdp::context::DriverError;
use super::cdp::locator::{Locator, WaitForState};
use super::cdp::page::PageShared;
use super::selectors::{SIDE_NAV_SELECTOR, SIDE_NAV_TOGGLE_SELECTOR};
use super::types::BrowserPage;

/// 舊版 `SIDE_NAV_WAIT_TIMEOUT_MS`。
const SIDE_NAV_WAIT_TIMEOUT_MS: u64 = 2_000;

/// 舊版 `EXPANDED_SIDE_NAV_MIN_WIDTH_PX`。
const EXPANDED_SIDE_NAV_MIN_WIDTH_PX: i64 = 100;

/// 舊版 `SideNavCollapseResult`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SideNavCollapseResult {
    Collapsed,
    AlreadyCollapsed,
    NotFound,
    Failed,
}

// ChannelSession（S3.4 的 session 模組）以同形 fake 走 internal 入口，故 trait
// 與本體提升為 pub(crate)（不改行為）。頁面依賴僅涵蓋本檔用到的 API。
#[async_trait]
pub(crate) trait SideNavPage: Send + Sync {
    /// 舊版 `page.locator(selector)`；builder 不失敗，操作時才回錯。
    fn locator(&self, selector: &str) -> Arc<dyn SideNavLocator>;
}

/// 本檔使用的 locator API（對應舊版 `Locator` 用到的子集）。
#[async_trait]
pub(crate) trait SideNavLocator: Send + Sync {
    /// 舊版 `locator.first()`。
    fn first(&self) -> Arc<dyn SideNavLocator>;

    /// 舊版 `locator.waitFor({state: 'visible', timeout})`。
    async fn wait_for(&self, timeout: Option<u64>) -> Result<(), DriverError>;

    /// 舊版 `locator.evaluate(fn, arg)`。
    async fn evaluate(
        &self,
        fn_declaration: &str,
        arg: Option<Value>,
    ) -> Result<Value, DriverError>;

    /// 舊版 `locator.click({timeout})`。
    async fn click(&self, timeout: Option<u64>) -> Result<(), DriverError>;
}

/// 正式端的頁面轉接：包 [`PageShared`] 的 locator API。
struct CdpSideNavPage {
    shared: Arc<PageShared>,
}

impl CdpSideNavPage {
    fn new(shared: Arc<PageShared>) -> Self {
        Self { shared }
    }
}

#[async_trait]
impl SideNavPage for CdpSideNavPage {
    fn locator(&self, selector: &str) -> Arc<dyn SideNavLocator> {
        Arc::new(CdpSideNavLocator {
            inner: self.shared.locator(selector),
        })
    }
}

struct CdpSideNavLocator {
    inner: Locator,
}

#[async_trait]
impl SideNavLocator for CdpSideNavLocator {
    fn first(&self) -> Arc<dyn SideNavLocator> {
        Arc::new(Self {
            inner: self.inner.first(),
        })
    }

    async fn wait_for(&self, timeout: Option<u64>) -> Result<(), DriverError> {
        self.inner.wait_for(WaitForState::Visible, timeout).await
    }

    async fn evaluate(
        &self,
        fn_declaration: &str,
        arg: Option<Value>,
    ) -> Result<Value, DriverError> {
        // 舊版 `toggle.evaluate(…)` 沒有 timeout，交由 cdp 層的預設逾時。
        self.inner.evaluate(fn_declaration, arg, None).await
    }

    async fn click(&self, timeout: Option<u64>) -> Result<(), DriverError> {
        self.inner.click(timeout).await
    }
}

/// 舊版 `collapseSideNav(page)`：視需要收合側欄，永不向外拋錯。
pub(crate) async fn collapse_side_nav(page: &dyn BrowserPage) -> SideNavCollapseResult {
    let Some(shared) = page.as_page_shared() else {
        // 舊版直接收 Playwright Page；fake 沒有 cdp 實體時視同瀏覽器操作失敗
        //（同 channel_points_reader 的兜底）。
        return SideNavCollapseResult::Failed;
    };
    collapse_side_nav_on_page(&CdpSideNavPage::new(shared)).await
}

/// 舊版 `collapseSideNav()` 本體（session 模組以 fake Page 直接呼叫，故 pub(crate)）。
pub(crate) async fn collapse_side_nav_on_page(page: &dyn SideNavPage) -> SideNavCollapseResult {
    let toggle = page.locator(SIDE_NAV_TOGGLE_SELECTOR).first();
    if toggle
        .wait_for(Some(SIDE_NAV_WAIT_TIMEOUT_MS))
        .await
        .is_err()
    {
        return SideNavCollapseResult::NotFound;
    }

    let expanded = match toggle
        .evaluate(side_nav_state_script(), Some(side_nav_state_options()))
        .await
    {
        Ok(value) => js_truthy(value),
        Err(_) => return SideNavCollapseResult::Failed,
    };
    if !expanded {
        return SideNavCollapseResult::AlreadyCollapsed;
    }

    if toggle.click(Some(SIDE_NAV_WAIT_TIMEOUT_MS)).await.is_err() {
        return SideNavCollapseResult::Failed;
    }
    SideNavCollapseResult::Collapsed
}

/// 舊版 evaluate 的 options 參數（`sideNavSelector`／`expandedMinWidthPx`，
/// 鍵名照 `js/side-nav-state.js` 的合約）。
fn side_nav_state_options() -> Value {
    json!({
        "sideNavSelector": SIDE_NAV_SELECTOR,
        "expandedMinWidthPx": EXPANDED_SIDE_NAV_MIN_WIDTH_PX,
    })
}

/// 舊版 `!expanded` 照 JS 真值規則判定。`js/side-nav-state.js` 只會回布林；
/// 此函式確保萬一拿到其他型別時，落地方式仍與舊版 `!expanded` 相同。
fn js_truthy(value: Value) -> bool {
    match value {
        Value::Bool(expanded) => expanded,
        Value::Null => false,
        Value::Number(number) => matches!(
            number.as_f64(),
            Some(width) if width != 0.0 && !width.is_nan(),
        ),
        Value::String(label) => !label.is_empty(),
        // JS 的物件與陣列一律 truthy。
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// `include_str!` 載入 `js/side-nav-state.js`，剝除開頭連續的 `//` 註解行後
/// trim 前後空白（DEC-004）；送進瀏覽器的確切文字不含檔頭說明。
fn side_nav_state_script() -> &'static str {
    static SCRIPT: OnceLock<String> = OnceLock::new();
    SCRIPT
        .get_or_init(|| {
            let raw = include_str!("js/side-nav-state.js");
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

#[cfg(test)]
mod tests;
