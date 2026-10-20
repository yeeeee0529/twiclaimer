//! e2e（§7.4 選擇器段落）：對 `selectors.rs` 的每個選擇器常數呼叫
//! `injected.parseSelector`，全部不得拋錯（G5 之外的選擇器關卡）。
//!
//! 需要真 headless shell（`#[ignore = "需要 CHROME_PATH"]`）。
use std::sync::Arc;

use serde_json::{Value, json};

use crate::browser::cdp::context::BrowserAdapter;
use crate::browser::cdp::injected::{InjectedScript, call_function_on};
use crate::browser::cdp::launcher::BrowserLauncher;
use crate::browser::cdp::page::PageShared;
use crate::browser::selectors::{
    CHANNEL_HEALTH_SELECTORS, COMMUNITY_POINTS_BALANCE_SELECTOR,
    COMMUNITY_POINTS_CLAIM_BUTTON_SELECTOR, COMMUNITY_POINTS_SUMMARY_SELECTOR,
    QUALITY_MENU_SELECTOR, QUALITY_OPTION_SELECTOR, REWARD_BUTTON_LIKE_SELECTOR,
    SETTINGS_BUTTON_SELECTOR, SIDE_NAV_SELECTOR, SIDE_NAV_TOGGLE_SELECTOR, VIDEO_PLAYER_SELECTOR,
    VIDEO_SELECTOR,
};

/// 在主框架 main world 解析選擇器（只驗證不拋錯，回傳 part 數）。
const PARSE_SELECTOR_SOURCE: &str =
    r#"(injected, selector) => injected.parseSelector(selector).parts.length"#;

/// 逐一列出所有集中管理的選擇器（含 ChannelHealth 的五個值）。
fn all_selectors() -> Vec<(&'static str, String)> {
    vec![
        (
            "CHANNEL_HEALTH_SELECTORS.login_required",
            CHANNEL_HEALTH_SELECTORS.login_required.to_string(),
        ),
        (
            "CHANNEL_HEALTH_SELECTORS.live_content",
            CHANNEL_HEALTH_SELECTORS.live_content.to_string(),
        ),
        (
            "CHANNEL_HEALTH_SELECTORS.error",
            CHANNEL_HEALTH_SELECTORS.error.to_string(),
        ),
        (
            "CHANNEL_HEALTH_SELECTORS.offline",
            CHANNEL_HEALTH_SELECTORS.offline.to_string(),
        ),
        (
            "CHANNEL_HEALTH_SELECTORS.content_warning",
            CHANNEL_HEALTH_SELECTORS.content_warning.to_string(),
        ),
        (
            "COMMUNITY_POINTS_BALANCE_SELECTOR",
            COMMUNITY_POINTS_BALANCE_SELECTOR.to_string(),
        ),
        (
            "COMMUNITY_POINTS_CLAIM_BUTTON_SELECTOR",
            COMMUNITY_POINTS_CLAIM_BUTTON_SELECTOR.to_string(),
        ),
        (
            "COMMUNITY_POINTS_SUMMARY_SELECTOR",
            COMMUNITY_POINTS_SUMMARY_SELECTOR.to_string(),
        ),
        (
            "REWARD_BUTTON_LIKE_SELECTOR",
            REWARD_BUTTON_LIKE_SELECTOR.to_string(),
        ),
        (
            "SIDE_NAV_TOGGLE_SELECTOR",
            SIDE_NAV_TOGGLE_SELECTOR.to_string(),
        ),
        ("SIDE_NAV_SELECTOR", SIDE_NAV_SELECTOR.to_string()),
        (
            "SETTINGS_BUTTON_SELECTOR",
            SETTINGS_BUTTON_SELECTOR.to_string(),
        ),
        ("QUALITY_MENU_SELECTOR", QUALITY_MENU_SELECTOR.to_string()),
        (
            "QUALITY_OPTION_SELECTOR",
            QUALITY_OPTION_SELECTOR.to_string(),
        ),
        ("VIDEO_SELECTOR", VIDEO_SELECTOR.to_string()),
        ("VIDEO_PLAYER_SELECTOR", VIDEO_PLAYER_SELECTOR.to_string()),
    ]
}

#[tokio::test]
#[ignore = "需要 CHROME_PATH"]
async fn selectors_parse() {
    let launched = BrowserLauncher::new().launch().await.unwrap();
    let browser = BrowserAdapter::new(launched);

    let context = browser.new_context(None, (1280, 720)).await.unwrap();
    let page = context.new_page().await.unwrap();

    let injected = InjectedScript::default();
    let shared: &Arc<PageShared> = &page.shared;
    for (name, selector) in all_selectors() {
        let reply = call_function_on(
            shared,
            &injected,
            PARSE_SELECTOR_SOURCE,
            json!([{"value": selector}]),
            true,
        )
        .await
        .unwrap_or_else(|error| panic!("parseSelector({name}) 應不拋錯：{error:?}"));
        let parts = reply
            .get("value")
            .and_then(Value::as_i64)
            .unwrap_or_else(|| panic!("parseSelector({name}) 應回傳數字：{reply:?}"));
        assert!(parts >= 1, "parseSelector({name}) 應至少一個 part");
    }

    let _ = page.shared.close().await;
    browser.close().await;
}
