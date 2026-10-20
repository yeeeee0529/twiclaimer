//! 選擇器常數的逐字測試：與舊版 `.ts` 原文比對（防止搬移時被改寫）。
use super::*;

/// 舊版 `ChannelHealthChecker.ts` 的字串值（供逐字比對）。
const LOGIN_REQUIRED_TS: &str = "[data-test-selector=\"login-required\"], [data-a-target=\"login-button\"], [data-channel-health=\"login-required\"]";
const LIVE_CONTENT_TS: &str = "[data-a-target=\"video-player\"], [data-test-selector=\"video-player\"], [data-channel-health=\"live\"]";
const ERROR_TS: &str = "[data-test-selector=\"error-page\"], [data-a-target=\"player-error-message\"], [data-channel-health=\"error\"]";
const OFFLINE_TS: &str = "[data-test-selector=\"offline-page\"], [data-a-target=\"offline-channel-main-content\"], [data-channel-health=\"offline\"]";
const CONTENT_WARNING_TS: &str = "[data-a-target=\"content-classification-gate-overlay-start-watching-button\"], [data-a-target=\"content-warning-start-watching-button\"], [data-channel-health=\"content-warning\"], button:has-text(\"Start Watching\"), button:has-text(\"開始觀看\")";

#[test]
fn channel_health_selectors_are_verbatim() {
    assert_eq!(CHANNEL_HEALTH_SELECTORS.login_required, LOGIN_REQUIRED_TS);
    assert_eq!(CHANNEL_HEALTH_SELECTORS.live_content, LIVE_CONTENT_TS);
    assert_eq!(CHANNEL_HEALTH_SELECTORS.error, ERROR_TS);
    assert_eq!(CHANNEL_HEALTH_SELECTORS.offline, OFFLINE_TS);
    assert_eq!(CHANNEL_HEALTH_SELECTORS.content_warning, CONTENT_WARNING_TS);
}

#[test]
fn scalar_selectors_are_verbatim() {
    assert_eq!(
        COMMUNITY_POINTS_BALANCE_SELECTOR,
        "[data-test-selector=\"community-points-summary\"]"
    );
    assert_eq!(
        COMMUNITY_POINTS_CLAIM_BUTTON_SELECTOR,
        "[data-test-selector=\"community-points-claim-button\"], .claimable-bonus__icon"
    );
    assert_eq!(
        COMMUNITY_POINTS_SUMMARY_SELECTOR,
        "[data-test-selector=\"community-points-summary\"]"
    );
    assert_eq!(
        REWARD_BUTTON_LIKE_SELECTOR,
        "button:not([type=\"submit\"]), [role=\"button\"], input[type=\"button\"]"
    );
    assert_eq!(
        SIDE_NAV_TOGGLE_SELECTOR,
        "button[data-a-target=\"side-nav-arrow\"]"
    );
    assert_eq!(SIDE_NAV_SELECTOR, "[data-a-target=\"side-nav-bar\"]");
    assert_eq!(
        SETTINGS_BUTTON_SELECTOR,
        "button[data-a-target=\"player-settings-button\"]"
    );
    assert_eq!(
        QUALITY_MENU_SELECTOR,
        "[data-a-target=\"player-settings-menu-item-quality\"]"
    );
    assert_eq!(
        QUALITY_OPTION_SELECTOR,
        "[data-a-target=\"player-settings-menu-item-quality-option\"], [role=\"menuitemradio\"]"
    );
    assert_eq!(VIDEO_SELECTOR, "video");
    assert_eq!(
        VIDEO_PLAYER_SELECTOR,
        "[data-a-target=\"video-player\"], video"
    );
}
