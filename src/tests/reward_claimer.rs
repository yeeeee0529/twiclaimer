//! e2e（S1.7）：RewardClaimer 接真實 headless shell 的 mock 頁面整合測試。
//!
//! 舊版 e2e（`test/e2e/reward-claimer.spec.ts`）以 Playwright page 開 file://
//! mock 頁並驅動 `src/browser/RewardClaimer.ts`；Rust 版照 §7.4 的開頁法
//! （launch → new_context → new_page → goto），同樣的 mock 頁
//! （`testdata/mock-pages/reward-*.html`）以 file:// 開在 headless shell，
//! 經正式碼的 [`BrowserPage`] 把手呼叫 [`RewardClaimer`]，驗證模組移植與
//! locator／click 真實行為的整合。屬性斷言以輪詢對應
//! `expect().toHaveAttribute`。
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::browser::cdp::context::BrowserAdapter;
use crate::browser::cdp::launcher::BrowserLauncher;
use crate::browser::cdp::locator::Locator;
use crate::browser::cdp::page::{PageShared, WaitUntil};
use crate::browser::reward_claimer::{
    RewardClaimResult, RewardClaimer, RewardClaimerClock, RewardClaimerOptions,
};
use crate::browser::selectors::{
    COMMUNITY_POINTS_CLAIM_BUTTON_SELECTOR, COMMUNITY_POINTS_SUMMARY_SELECTOR,
};
use crate::browser::types::{BrowserPage, BrowserPageAdapter, CdpPageAdapter};

/// 舊 e2e 冷卻測試的起始時間：`new Date('2026-06-14T12:00:00.000Z')` 的
/// unix 毫秒。
const COOLDOWN_CLOCK_START_UNIX_MILLIS: u64 = 1_781_438_400_000;

/// mock 頁面的 file:// URL（與其他 e2e 相同的 helper 慣例）。
fn mock_page(name: &str) -> String {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("testdata/mock-pages");
    path.push(name);
    format!("file://{}", path.display())
}

/// 啟動 headless shell（每個測試一個瀏覽器；對應 Playwright 每個測試的
/// 瀏覽器程序）。
async fn launch_browser() -> Arc<BrowserAdapter> {
    let launched = BrowserLauncher::new().launch().await.unwrap();
    BrowserAdapter::new(launched)
}

/// 開一個全新 context 的頁面並導覽到 mock 頁（對應舊 e2e 的 page fixture +
/// `page.goto`）；回傳（正式碼頁面把手、cdp 把手）。context 把手不像舊版
/// fixture 需要保留：頁面屬於該 context，由關閉頁面與瀏覽器收尾。
async fn open_page(
    browser: &BrowserAdapter,
    file: &str,
) -> (Arc<dyn BrowserPage>, Arc<PageShared>) {
    let context = browser.new_context(None, (1280, 720)).await.unwrap();
    let page = context.new_page().await.unwrap();
    page.shared
        .goto(&mock_page(file), WaitUntil::DomContentLoaded)
        .await
        .unwrap();
    let handle = CdpPageAdapter::new(Arc::clone(&page.shared)).page();
    (handle, Arc::clone(&page.shared))
}

/// 輪詢屬性到期望值（`expect().toHaveAttribute` 的語意：5 秒內持續重查，
/// 點擊訊號落地的時序不必緊貼 claim 回傳）。
async fn expect_attribute_eventually(locator: &Locator, name: &str, expected: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let value = locator.get_attribute(name, Some(1_000)).await.unwrap();
        if value.as_deref() == Some(expected) {
            return;
        }
        let last = value.unwrap_or_else(|| "(無屬性)".to_string());
        assert!(
            Instant::now() < deadline,
            "屬性 {name} 期限內未變成 {expected:?}（最後值：{last:?}，locator：{}）",
            locator.selector()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// 舊版 `expect(result.status).toBe('claimed')`。
fn assert_claimed(result: &RewardClaimResult) {
    assert!(
        matches!(result, RewardClaimResult::Claimed { .. }),
        "應回傳 claimed，實際：{result:?}"
    );
}

/// 舊版 `expect(result.status).toBe('not_found')`。
fn assert_not_found(result: &RewardClaimResult) {
    assert!(
        matches!(result, RewardClaimResult::NotFound { .. }),
        "應回傳 not_found，實際：{result:?}"
    );
}

/// 舊 e2e 的注入時鐘（`{clock: () => new Date(now)}`）：以原子值表現可推進的
/// `now`。
struct AdjustableClock(Arc<AtomicU64>);

impl RewardClaimerClock for AdjustableClock {
    fn now_unix_millis(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

// ===== 選擇器行為 =====

// 原測試：primary selector 不依賴按鈕文字
#[tokio::test]
#[ignore = "需要 CHROME_PATH"]
async fn primary_selector_does_not_depend_on_button_text() {
    let browser = launch_browser().await;
    let (page, shared) = open_page(&browser, "reward-available.html").await;
    let claim_button = shared.locator(COMMUNITY_POINTS_CLAIM_BUTTON_SELECTOR);
    // `expect(button).toHaveText('Reclamar bonificacion')`（toHaveText 會正規化
    // 空白，textContent 取回後以 trim 對照）。
    let text = claim_button
        .text_content(Some(1_000))
        .await
        .unwrap()
        .unwrap_or_default();
    assert_eq!(text.trim(), "Reclamar bonificacion");

    let result = RewardClaimer::new(RewardClaimerOptions::default())
        .claim_if_available(page.as_ref(), "localized_channel")
        .await;
    assert_claimed(&result);
    expect_attribute_eventually(&claim_button, "data-click-count", "1").await;

    let _ = shared.close().await;
    browser.close().await;
}

// 原測試：primary selector 缺少時使用 summary 結構 fallback
#[tokio::test]
#[ignore = "需要 CHROME_PATH"]
async fn uses_summary_structure_fallback_when_primary_selector_is_missing() {
    let browser = launch_browser().await;
    let (page, shared) = open_page(&browser, "reward-available.html").await;
    let button = shared
        .locator(COMMUNITY_POINTS_SUMMARY_SELECTOR)
        .locator("button");
    // 移除 primary 的 data-test-selector，讓按鈕只能走 summary fallback。
    button
        .evaluate(
            "(element) => { element.removeAttribute('data-test-selector'); element.textContent = 'Obtenir le bonus'; }",
            None,
            Some(1_000),
        )
        .await
        .unwrap();

    let claimer = RewardClaimer::new(RewardClaimerOptions::default());
    let result = claimer
        .claim_if_available(page.as_ref(), "fallback_channel")
        .await;
    assert_claimed(&result);
    expect_attribute_eventually(&button, "data-click-count", "1").await;

    let _ = shared.close().await;
    browser.close().await;
}

// 原測試：支援 BetterTTV 使用的 claimable bonus selector
#[tokio::test]
#[ignore = "需要 CHROME_PATH"]
async fn supports_betterttv_claimable_bonus_selector() {
    let browser = launch_browser().await;
    let (page, shared) = open_page(&browser, "reward-available.html").await;
    let button = shared
        .locator(COMMUNITY_POINTS_SUMMARY_SELECTOR)
        .locator("button");
    // 移除 data-test-selector 後改掛 BetterTTV 的 class 與 aria-label。
    button
        .evaluate(
            "(element) => { element.removeAttribute('data-test-selector'); element.className = 'claimable-bonus__icon'; element.setAttribute('aria-label', 'Bonus Channel Points'); }",
            None,
            Some(1_000),
        )
        .await
        .unwrap();

    let claimer = RewardClaimer::new(RewardClaimerOptions::default());
    let result = claimer
        .claim_if_available(page.as_ref(), "betterttv_selector_channel")
        .await;
    assert_claimed(&result);
    expect_attribute_eventually(&button, "data-click-count", "1").await;

    let _ = shared.close().await;
    browser.close().await;
}

// ===== not_found 的情境 =====

// 原測試：disabled 按鈕不會被點擊
#[tokio::test]
#[ignore = "需要 CHROME_PATH"]
async fn does_not_click_disabled_button() {
    let browser = launch_browser().await;
    let (page, shared) = open_page(&browser, "reward-disabled.html").await;
    let claim_button = shared.locator(COMMUNITY_POINTS_CLAIM_BUTTON_SELECTOR);

    let result = RewardClaimer::new(RewardClaimerOptions::default())
        .claim_if_available(page.as_ref(), "disabled_channel")
        .await;
    assert_not_found(&result);
    expect_attribute_eventually(&claim_button, "data-click-count", "0").await;

    let _ = shared.close().await;
    browser.close().await;
}

// 原測試：按鈕不存在時回傳 not_found
#[tokio::test]
#[ignore = "需要 CHROME_PATH"]
async fn returns_not_found_when_button_is_missing() {
    let browser = launch_browser().await;
    let (page, shared) = open_page(&browser, "reward-unavailable.html").await;

    let result = RewardClaimer::new(RewardClaimerOptions::default())
        .claim_if_available(page.as_ref(), "unavailable_channel")
        .await;
    assert_not_found(&result);

    let _ = shared.close().await;
    browser.close().await;
}

// 原測試：餘額選單按鈕不會被誤判為領取按鈕
#[tokio::test]
#[ignore = "需要 CHROME_PATH"]
async fn does_not_mistake_balance_menu_button_for_claim_button() {
    let browser = launch_browser().await;
    let (page, shared) = open_page(&browser, "reward-unavailable.html").await;
    let summary = shared.locator(COMMUNITY_POINTS_SUMMARY_SELECTOR);
    // summary 內動態加一個 aria-label 的餘額控制（點擊會把 dataset 改成 1）。
    summary
        .evaluate(
            "(summary) => {
                const balanceButton = document.createElement('button');
                balanceButton.type = 'button';
                balanceButton.setAttribute('aria-label', 'Bits and Points Balances');
                balanceButton.dataset.clickCount = '0';
                balanceButton.addEventListener('click', () => {
                    balanceButton.dataset.clickCount = '1';
                });
                summary.append(balanceButton);
            }",
            None,
            Some(1_000),
        )
        .await
        .unwrap();
    let balance_button = summary.locator("button");

    let result = RewardClaimer::new(RewardClaimerOptions::default())
        .claim_if_available(page.as_ref(), "balance_control_channel")
        .await;
    assert_not_found(&result);
    expect_attribute_eventually(&balance_button, "data-click-count", "0").await;

    let _ = shared.close().await;
    browser.close().await;
}

// ===== 點擊 fallback =====

// 原測試：真實 locator 一般點擊失敗時使用 DOM click fallback
#[tokio::test]
#[ignore = "需要 CHROME_PATH"]
async fn uses_dom_click_fallback_when_regular_click_fails_on_real_locator() {
    let browser = launch_browser().await;
    let (page, shared) = open_page(&browser, "reward-available.html").await;
    let claim_button = shared.locator(COMMUNITY_POINTS_CLAIM_BUTTON_SELECTOR);
    // pointer-events = none：真實點擊交不進按鈕（hit target 檢查吃了整個
    // timeout），必須走 dispatchEvent 的 DOM click fallback。
    claim_button
        .evaluate(
            "(element) => { element.style.pointerEvents = 'none'; }",
            None,
            Some(1_000),
        )
        .await
        .unwrap();
    // 舊版在這裡 `page.setDefaultTimeout(150)`；§7.4 的 Page API 沒有
    // setDefaultTimeout（僅 setDefaultNavigationTimeout），且 claim 的
    // click／dispatchEvent 都傳顯式 timeout（模組常數 3 000 ms），本測試
    // 流程的行為不受該設定影響，故省略。

    let result = RewardClaimer::new(RewardClaimerOptions::default())
        .claim_if_available(page.as_ref(), "click_failure_channel")
        .await;
    assert_claimed(&result);
    expect_attribute_eventually(&claim_button, "data-click-count", "1").await;

    let _ = shared.close().await;
    browser.close().await;
}

// ===== 冷卻 =====

// 原測試：成功後同一 channel 套用 60 秒冷卻
#[tokio::test]
#[ignore = "需要 CHROME_PATH"]
async fn applies_60s_cooldown_to_same_channel_after_success() {
    let browser = launch_browser().await;
    let (page, shared) = open_page(&browser, "reward-available.html").await;
    let claim_button = shared.locator(COMMUNITY_POINTS_CLAIM_BUTTON_SELECTOR);

    let now_ms = Arc::new(AtomicU64::new(COOLDOWN_CLOCK_START_UNIX_MILLIS));
    let claimer = RewardClaimer::new(RewardClaimerOptions {
        clock: Arc::new(AdjustableClock(Arc::clone(&now_ms))),
        ..RewardClaimerOptions::default()
    });

    let result = claimer
        .claim_if_available(page.as_ref(), "cooldown_channel")
        .await;
    assert_claimed(&result);

    // now += 59_999：還在 60 秒冷卻內，不應再點。
    now_ms.fetch_add(59_999, Ordering::SeqCst);
    let result = claimer
        .claim_if_available(page.as_ref(), "cooldown_channel")
        .await;
    assert_not_found(&result);
    expect_attribute_eventually(&claim_button, "data-click-count", "1").await;

    // now += 1：冷卻結束，可以再點一次。
    now_ms.fetch_add(1, Ordering::SeqCst);
    let result = claimer
        .claim_if_available(page.as_ref(), "cooldown_channel")
        .await;
    assert_claimed(&result);
    expect_attribute_eventually(&claim_button, "data-click-count", "2").await;

    let _ = shared.close().await;
    browser.close().await;
}

// ===== hasClaimableBonus =====

// 原測試：hasClaimableBonus 在領取按鈕存在時回傳 true
#[tokio::test]
#[ignore = "需要 CHROME_PATH"]
async fn has_claimable_bonus_is_true_when_claim_button_exists() {
    let browser = launch_browser().await;
    let (page, shared) = open_page(&browser, "reward-available.html").await;

    let claimer = RewardClaimer::new(RewardClaimerOptions::default());
    assert!(claimer.has_claimable_bonus(page.as_ref()).await);

    let _ = shared.close().await;
    browser.close().await;
}

// 原測試：hasClaimableBonus 在沒有領取按鈕時回傳 false
#[tokio::test]
#[ignore = "需要 CHROME_PATH"]
async fn has_claimable_bonus_is_false_when_no_claim_button() {
    let browser = launch_browser().await;
    let (page, shared) = open_page(&browser, "reward-unavailable.html").await;

    let claimer = RewardClaimer::new(RewardClaimerOptions::default());
    assert!(!claimer.has_claimable_bonus(page.as_ref()).await);

    let _ = shared.close().await;
    browser.close().await;
}
