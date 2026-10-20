//! 舊版 `src/browser/*.ts` 的選擇器常數，**逐字**集中（計畫 §7.4 的選擇器段落）。
//!
//! 字串內容一律從 `twitch-watchdog-docker/src/browser/` 逐字複製，不要改寫；
//! 需要新選擇器時同樣以舊版原文為準。
//!
//! 呼叫端（ChannelHealthChecker／RewardClaimer 等的 Rust 版）在 S1.7 接上，
//! 屆時移除 allow。
#![allow(dead_code)]

/// `ChannelHealthChecker.ts` 的 `CHANNEL_HEALTH_SELECTORS`（鍵名轉 snake_case，
/// 字串值逐字相同）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ChannelHealthSelectors {
    pub(crate) login_required: &'static str,
    pub(crate) live_content: &'static str,
    pub(crate) error: &'static str,
    pub(crate) offline: &'static str,
    pub(crate) content_warning: &'static str,
}

/// `ChannelHealthChecker.ts` 的 `CHANNEL_HEALTH_SELECTORS`。
pub(crate) const CHANNEL_HEALTH_SELECTORS: ChannelHealthSelectors = ChannelHealthSelectors {
    login_required: concat!(
        "[data-test-selector=\"login-required\"], ",
        "[data-a-target=\"login-button\"], ",
        "[data-channel-health=\"login-required\"]",
    ),
    live_content: concat!(
        "[data-a-target=\"video-player\"], ",
        "[data-test-selector=\"video-player\"], ",
        "[data-channel-health=\"live\"]",
    ),
    error: concat!(
        "[data-test-selector=\"error-page\"], ",
        "[data-a-target=\"player-error-message\"], ",
        "[data-channel-health=\"error\"]",
    ),
    offline: concat!(
        "[data-test-selector=\"offline-page\"], ",
        "[data-a-target=\"offline-channel-main-content\"], ",
        "[data-channel-health=\"offline\"]",
    ),
    content_warning: concat!(
        "[data-a-target=\"content-classification-gate-overlay-start-watching-button\"], ",
        "[data-a-target=\"content-warning-start-watching-button\"], ",
        "[data-channel-health=\"content-warning\"], ",
        "button:has-text(\"Start Watching\"), ",
        "button:has-text(\"開始觀看\")",
    ),
};

/// `ChannelPointsReader.ts` 的 `COMMUNITY_POINTS_BALANCE_SELECTOR`。
pub(crate) const COMMUNITY_POINTS_BALANCE_SELECTOR: &str =
    "[data-test-selector=\"community-points-summary\"]";

/// `RewardClaimer.ts` 的 `COMMUNITY_POINTS_CLAIM_BUTTON_SELECTOR`。
pub(crate) const COMMUNITY_POINTS_CLAIM_BUTTON_SELECTOR: &str = concat!(
    "[data-test-selector=\"community-points-claim-button\"], ",
    ".claimable-bonus__icon",
);

/// `RewardClaimer.ts` 的 `COMMUNITY_POINTS_SUMMARY_SELECTOR`。
pub(crate) const COMMUNITY_POINTS_SUMMARY_SELECTOR: &str =
    "[data-test-selector=\"community-points-summary\"]";

/// `RewardClaimer.ts` 的 `REWARD_BUTTON_LIKE_SELECTOR`。
pub(crate) const REWARD_BUTTON_LIKE_SELECTOR: &str =
    "button:not([type=\"submit\"]), [role=\"button\"], input[type=\"button\"]";

/// `SideNavCollapser.ts` 的 `SIDE_NAV_TOGGLE_SELECTOR`。
pub(crate) const SIDE_NAV_TOGGLE_SELECTOR: &str = "button[data-a-target=\"side-nav-arrow\"]";

/// `SideNavCollapser.ts` 的 `SIDE_NAV_SELECTOR`。
pub(crate) const SIDE_NAV_SELECTOR: &str = "[data-a-target=\"side-nav-bar\"]";

/// `StreamPlaybackOptimizer.ts` 的 `SETTINGS_BUTTON_SELECTOR`。
pub(crate) const SETTINGS_BUTTON_SELECTOR: &str =
    "button[data-a-target=\"player-settings-button\"]";

/// `StreamPlaybackOptimizer.ts` 的 `QUALITY_MENU_SELECTOR`。
pub(crate) const QUALITY_MENU_SELECTOR: &str =
    "[data-a-target=\"player-settings-menu-item-quality\"]";

/// `StreamPlaybackOptimizer.ts` 的 `QUALITY_OPTION_SELECTOR`。
pub(crate) const QUALITY_OPTION_SELECTOR: &str = concat!(
    "[data-a-target=\"player-settings-menu-item-quality-option\"], ",
    "[role=\"menuitemradio\"]",
);

/// 直接寫在 `StreamPlaybackOptimizer.ts` 裡的 `page.locator('video')` 選擇器。
pub(crate) const VIDEO_SELECTOR: &str = "video";

/// 直接寫在 `StreamPlaybackOptimizer.ts` 裡的 `revealPlayerControls` 選擇器。
pub(crate) const VIDEO_PLAYER_SELECTOR: &str = "[data-a-target=\"video-player\"], video";

#[cfg(test)]
mod tests;
