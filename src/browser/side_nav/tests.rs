//! `SideNavCollapser` 的單元測試：移植舊版
//! `test/unit/side-nav-collapser.test.ts` 的 4 個案例；fake page／locator
//! 照舊測試的 `createPage()` 逐一對應。

use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use regex::Regex;
use serde_json::{Value, json};

use super::{SideNavCollapseResult, SideNavLocator, SideNavPage, collapse_side_nav_on_page};
use crate::browser::cdp::context::DriverError;

type ClickFn = Arc<dyn Fn() -> Result<(), DriverError> + Send + Sync>;
type SharedCounter = Arc<Mutex<usize>>;

/// 舊測試 fake element：`getAttribute` 只認 `aria-expanded`／`aria-label`，
/// `closest()` 恆回 null（`title` 也照 fake 行為回 null）。
#[derive(Clone, Default)]
struct FakeElement {
    aria_expanded: Option<String>,
    aria_label: Option<String>,
}

#[derive(Clone)]
struct FakeLocator {
    element: FakeElement,
    visible: Option<bool>,
    click: Option<ClickFn>,
    click_calls: SharedCounter,
}

#[async_trait]
impl SideNavLocator for FakeLocator {
    fn first(&self) -> Arc<dyn SideNavLocator> {
        // 舊測試 fake：`first: () => locator`（同一個 locator）。
        Arc::new(self.clone())
    }

    async fn wait_for(&self, _timeout: Option<u64>) -> Result<(), DriverError> {
        // 舊測試 fake：`input.visible === false` 時擲錯，其餘成功。
        if matches!(self.visible, Some(false)) {
            return Err(DriverError::new("Error", "not visible"));
        }
        Ok(())
    }

    async fn evaluate(
        &self,
        _fn_declaration: &str,
        _arg: Option<Value>,
    ) -> Result<Value, DriverError> {
        // 舊測試 fake 是 `evaluate(callback, options) => callback(element,
        // options)`：用真的 callback 在 fake element 上求值；Rust 端送進瀏覽器的
        // 是 `js/side-nav-state.js`，fake 以同一段規則在 fake element 上模擬其
        // 結果（`mirrored_expanded`）。
        let expanded = mirrored_expanded(&self.element)?;
        Ok(json!(expanded))
    }

    async fn click(&self, _timeout: Option<u64>) -> Result<(), DriverError> {
        *self.click_calls.lock().unwrap() += 1;
        match self.click.as_deref() {
            Some(click) => click(),
            None => Ok(()),
        }
    }
}

struct FakePage {
    element: FakeElement,
    visible: Option<bool>,
    click: Option<ClickFn>,
    click_calls: SharedCounter,
}

#[async_trait]
impl SideNavPage for FakePage {
    fn locator(&self, _selector: &str) -> Arc<dyn SideNavLocator> {
        Arc::new(FakeLocator {
            element: self.element.clone(),
            visible: self.visible,
            click: self.click.clone(),
            click_calls: Arc::clone(&self.click_calls),
        })
    }
}

/// 舊測試的 `createPage()` 輸入（欄位照舊）。
#[derive(Default)]
struct FakePageInput {
    visible: Option<bool>,
    aria_expanded: Option<&'static str>,
    aria_label: Option<&'static str>,
    click: Option<ClickFn>,
}

fn create_page(input: FakePageInput) -> FakePage {
    FakePage {
        element: FakeElement {
            aria_expanded: input.aria_expanded.map(str::to_string),
            aria_label: input.aria_label.map(str::to_string),
        },
        visible: input.visible,
        click: input.click,
        click_calls: Arc::new(Mutex::new(0)),
    }
}

/// 舊測試 `click: vi.fn(async () => { throw new Error('detached'); })`。
fn failing_click() -> ClickFn {
    Arc::new(|| Err(DriverError::new("Error", "detached")))
}

// 原測試：按鈕表示側欄展開時自動收合
#[tokio::test]
async fn collapses_side_nav_when_toggle_indicates_expanded() {
    let page = create_page(FakePageInput {
        aria_label: Some("Collapse Side Nav"),
        ..FakePageInput::default()
    });

    assert_eq!(
        collapse_side_nav_on_page(&page).await,
        SideNavCollapseResult::Collapsed
    );
    assert_eq!(*page.click_calls.lock().unwrap(), 1);
}

// 原測試：aria-expanded 為 false 時不重複點擊
#[tokio::test]
async fn does_not_click_when_aria_expanded_is_false() {
    let page = create_page(FakePageInput {
        aria_expanded: Some("false"),
        ..FakePageInput::default()
    });

    assert_eq!(
        collapse_side_nav_on_page(&page).await,
        SideNavCollapseResult::AlreadyCollapsed
    );
    assert_eq!(*page.click_calls.lock().unwrap(), 0);
}

// 原測試：缺少按鈕時安全略過
#[tokio::test]
async fn skips_safely_when_toggle_is_missing() {
    let page = create_page(FakePageInput {
        visible: Some(false),
        ..FakePageInput::default()
    });

    assert_eq!(
        collapse_side_nav_on_page(&page).await,
        SideNavCollapseResult::NotFound
    );
}

// 原測試：按鈕點擊失敗時不向外拋錯
#[tokio::test]
async fn catches_failed_click() {
    let page = create_page(FakePageInput {
        aria_label: Some("Collapse Side Nav"),
        click: Some(failing_click()),
        ..FakePageInput::default()
    });

    assert_eq!(
        collapse_side_nav_on_page(&page).await,
        SideNavCollapseResult::Failed
    );
}

/// `side-nav-state.js` 在舊測試 fake element 上的等價行為：先看
/// `aria-expanded`，再看 `aria-label`（fake 的 `title` 恆為 null）；最後
/// `closest('nav')` 在 fake 恆回 null，原有的 JS 這時會在
/// `document.querySelector` 上擲 TypeError（fake 環境沒有 document）。
fn mirrored_expanded(element: &FakeElement) -> Result<bool, DriverError> {
    if let Some(aria_expanded) = &element.aria_expanded {
        return Ok(aria_expanded == "true");
    }

    // `toLocaleLowerCase('en-US')` 對本測試的 ASCII 與漢字輸入等同 Unicode
    // 小寫。
    // `toLocaleLowerCase('en-US')` 對本測試的 ASCII 與漢字輸入等同 Unicode
    // 小寫；regex 比對的是小寫後的標籤（照舊版）。
    let accessible_label = element
        .aria_label
        .clone()
        .unwrap_or_default()
        .to_lowercase();
    if collapse_pattern().is_match(&accessible_label) {
        return Ok(true);
    }
    if expand_pattern().is_match(&accessible_label) {
        return Ok(false);
    }

    Err(DriverError::new(
        "Error",
        "Cannot read properties of undefined (reading 'querySelector')",
    ))
}

/// 舊版 `/collapse|收合|折疊|折叠|縮小|缩小/u`。
fn collapse_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| Regex::new(r"collapse|收合|折疊|折叠|縮小|缩小").unwrap())
}

/// 舊版 `/expand|展開|展开/u`。
fn expand_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| Regex::new(r"expand|展開|展开").unwrap())
}
