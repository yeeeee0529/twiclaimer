//! RewardClaimer：逐函式移植舊版 `src/browser/RewardClaimer.ts`。
//!
//! 主要與 fallback 的選擇器流程照舊版；批次 metadata 由
//! `src/browser/js/reward-candidates.js` 送到 `Locator::evaluateAll`。Rust 版
//! 以內部 page／locator trait 讓單元測試能表現舊測試的 `MockElement`／
//! `MockLocator` 行為；正式呼叫端只傳 [`BrowserPage`]，實際操作仍由
//! `PageShared` 的 CDP locator 完成。
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::StaticFormatDescription;
use time::macros::format_description;

use super::cdp::context::DriverError;
use super::cdp::locator::Locator;
use super::cdp::page::PageShared;
use super::safe_logging::safe_error_message;
use super::selectors::{
    COMMUNITY_POINTS_CLAIM_BUTTON_SELECTOR, COMMUNITY_POINTS_SUMMARY_SELECTOR,
    REWARD_BUTTON_LIKE_SELECTOR,
};
use super::types::BrowserPage;
use crate::logging::logger::{JsonLineLogger, LogEvents};
use crate::logging::redact::SanitizedValue;

pub(crate) const REWARD_CLAIM_COOLDOWN: Duration = Duration::from_secs(60);
pub(crate) const REWARD_CLAIM_CLICK_TIMEOUT_MS: u64 = 3_000;

const ISO_UTC_FORMAT: StaticFormatDescription =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z");

static REWARD_CANDIDATE_SCRIPT: OnceLock<String> = OnceLock::new();

/// 舊版 `RewardClaimResult`（`Clone` 供 session 模組的單一 flight 共用結果使用）。
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum RewardClaimResult {
    Claimed {
        channel: String,
        claimed_at: String,
    },
    NotFound {
        channel: String,
        checked_at: String,
    },
    ClickFailed {
        channel: String,
        checked_at: String,
        error: String,
    },
}

/// 舊版可拋錯的 logger 介面；正式 logger 的回傳值固定為 `Ok`。
pub(crate) trait RewardClaimerLogger: Send + Sync {
    fn info(&self, event: &str, fields: &SanitizedValue) -> Result<(), String>;
    fn warn(&self, event: &str, fields: &SanitizedValue) -> Result<(), String>;
}

pub(crate) trait RewardClaimerClock: Send + Sync {
    fn now_unix_millis(&self) -> u64;
}

pub(crate) type RewardClaimObserver =
    Box<dyn Fn(&RewardClaimResult) -> Result<(), String> + Send + Sync>;

pub(crate) struct RewardClaimerOptions {
    pub(crate) logger: Arc<dyn RewardClaimerLogger>,
    pub(crate) clock: Arc<dyn RewardClaimerClock>,
    pub(crate) on_result: Option<RewardClaimObserver>,
}

pub(crate) struct RewardClaimer {
    logger: Arc<dyn RewardClaimerLogger>,
    clock: Arc<dyn RewardClaimerClock>,
    on_result: Option<RewardClaimObserver>,
    last_claimed_at_by_channel: Mutex<HashMap<String, u64>>,
}

#[async_trait]
trait RewardPage: Send + Sync {
    type Locator: RewardLocator;

    async fn locator(&self, selector: &str) -> Result<Self::Locator, DriverError>;
}

#[async_trait]
trait RewardLocator: Send + Sync + Sized {
    async fn locator(&self, selector: &str) -> Result<Self, DriverError>;

    async fn nth(&self, index: usize) -> Result<Self, DriverError>;

    async fn evaluate_all(&self) -> Result<Value, DriverError>;

    async fn click(&self, timeout_ms: u64) -> Result<(), DriverError>;

    async fn dispatch_click(&self, timeout_ms: u64) -> Result<(), DriverError>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RewardCandidateMetadata {
    visible: bool,
    disabled: bool,
    aria_label: Option<String>,
    class_name: Option<String>,
}

struct SharedRewardPage {
    page: Arc<PageShared>,
}

#[async_trait]
impl RewardPage for SharedRewardPage {
    type Locator = Locator;

    async fn locator(&self, selector: &str) -> Result<Locator, DriverError> {
        Ok(self.page.locator(selector))
    }
}

#[async_trait]
impl RewardLocator for Locator {
    async fn locator(&self, selector: &str) -> Result<Locator, DriverError> {
        Ok(self.locator(selector))
    }

    async fn nth(&self, index: usize) -> Result<Locator, DriverError> {
        Ok(self.nth(index as i64))
    }

    async fn evaluate_all(&self) -> Result<Value, DriverError> {
        self.evaluate_all(reward_candidate_script()).await
    }

    async fn click(&self, timeout_ms: u64) -> Result<(), DriverError> {
        self.click(Some(timeout_ms)).await
    }

    async fn dispatch_click(&self, timeout_ms: u64) -> Result<(), DriverError> {
        self.dispatch_event("click", Value::Null, Some(timeout_ms))
            .await
    }
}

struct NoopLogger;

impl RewardClaimerLogger for NoopLogger {
    fn info(&self, _event: &str, _fields: &SanitizedValue) -> Result<(), String> {
        Ok(())
    }

    fn warn(&self, _event: &str, _fields: &SanitizedValue) -> Result<(), String> {
        Ok(())
    }
}

struct SystemRewardClock;

impl RewardClaimerClock for SystemRewardClock {
    fn now_unix_millis(&self) -> u64 {
        u64::try_from(OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000).unwrap_or(0)
    }
}

impl Default for RewardClaimerOptions {
    fn default() -> Self {
        Self {
            logger: Arc::new(NoopLogger),
            clock: Arc::new(SystemRewardClock),
            on_result: None,
        }
    }
}

impl RewardClaimerLogger for JsonLineLogger {
    fn info(&self, event: &str, fields: &SanitizedValue) -> Result<(), String> {
        JsonLineLogger::info(self, event, Some(fields));
        Ok(())
    }

    fn warn(&self, event: &str, fields: &SanitizedValue) -> Result<(), String> {
        JsonLineLogger::warn(self, event, Some(fields));
        Ok(())
    }
}

impl RewardClaimer {
    pub(crate) fn new(options: RewardClaimerOptions) -> Self {
        Self {
            logger: options.logger,
            clock: options.clock,
            on_result: options.on_result,
            last_claimed_at_by_channel: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) async fn has_claimable_bonus(&self, page: &dyn BrowserPage) -> bool {
        let Some(shared) = page.as_page_shared() else {
            return false;
        };
        self.has_claimable_bonus_on_page(&SharedRewardPage { page: shared })
            .await
    }

    pub(crate) async fn claim_if_available(
        &self,
        page: &dyn BrowserPage,
        channel: &str,
    ) -> RewardClaimResult {
        let checked_at = self.iso_time();
        if self.is_cooling_down(channel) {
            return self.not_found(channel, &checked_at);
        }

        let Some(shared) = page.as_page_shared() else {
            return self.failed_result(channel, &checked_at, None);
        };
        self.claim_on_page(&SharedRewardPage { page: shared }, channel, &checked_at)
            .await
    }

    async fn has_claimable_bonus_on_page<P: RewardPage>(&self, page: &P) -> bool {
        find_claim_button(page)
            .await
            .map(|result| result.is_some())
            .unwrap_or(false)
    }

    async fn claim_on_page<P: RewardPage>(
        &self,
        page: &P,
        channel: &str,
        checked_at: &str,
    ) -> RewardClaimResult {
        if self.is_cooling_down(channel) {
            return self.not_found(channel, checked_at);
        }

        let claim_button = match find_claim_button(page).await {
            Ok(claim_button) => claim_button,
            Err(error) => {
                return self.failed_result(channel, checked_at, Some(error.message.as_str()));
            }
        };
        let Some(claim_button) = claim_button else {
            return self.not_found(channel, checked_at);
        };

        if let Err(error) = click_claim_button(&claim_button).await {
            return self.failed_result(channel, checked_at, Some(error.message.as_str()));
        }

        let claimed_at = self.iso_time();
        self.set_last_claimed_at(channel);
        safe_log(
            self.logger.as_ref(),
            true,
            LogEvents::REWARD_CLAIMED,
            &object(vec![
                ("channel", channel.to_string()),
                ("claimedAt", claimed_at.clone()),
            ]),
        );

        let result = RewardClaimResult::Claimed {
            channel: channel.to_string(),
            claimed_at,
        };
        self.notify_result(&result);
        result
    }

    fn is_cooling_down(&self, channel: &str) -> bool {
        let map = self
            .last_claimed_at_by_channel
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        map.get(&normalize_channel(channel))
            .is_some_and(|&last_claimed_at| {
                self.clock.now_unix_millis().saturating_sub(last_claimed_at)
                    < REWARD_CLAIM_COOLDOWN.as_millis() as u64
            })
    }

    fn set_last_claimed_at(&self, channel: &str) {
        let now = self.clock.now_unix_millis();
        let mut map = self
            .last_claimed_at_by_channel
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        map.insert(normalize_channel(channel), now);
    }

    fn iso_time(&self) -> String {
        format_iso_millis(self.clock.now_unix_millis())
    }

    fn not_found(&self, channel: &str, checked_at: &str) -> RewardClaimResult {
        RewardClaimResult::NotFound {
            channel: channel.to_string(),
            checked_at: checked_at.to_string(),
        }
    }

    fn failed_result(
        &self,
        channel: &str,
        checked_at: &str,
        error: Option<&str>,
    ) -> RewardClaimResult {
        let safe_error = safe_error_message(error);
        safe_log(
            self.logger.as_ref(),
            false,
            LogEvents::REWARD_CLAIM_FAILED,
            &object(vec![
                ("channel", channel.to_string()),
                ("checkedAt", checked_at.to_string()),
                ("error", safe_error.clone()),
            ]),
        );

        let result = RewardClaimResult::ClickFailed {
            channel: channel.to_string(),
            checked_at: checked_at.to_string(),
            error: safe_error,
        };
        self.notify_result(&result);
        result
    }

    fn notify_result(&self, result: &RewardClaimResult) {
        if let Some(on_result) = &self.on_result
            && let Err(_error) = on_result(result)
        {
            safe_log(
                self.logger.as_ref(),
                false,
                "reward_notification_failed",
                &object(vec![("channel", result.channel().to_string())]),
            );
        }
    }
}

impl RewardClaimResult {
    fn channel(&self) -> &str {
        match self {
            Self::Claimed { channel, .. }
            | Self::NotFound { channel, .. }
            | Self::ClickFailed { channel, .. } => channel,
        }
    }
}

async fn find_claim_button<P: RewardPage>(page: &P) -> Result<Option<P::Locator>, DriverError> {
    let primary_candidates = page.locator(COMMUNITY_POINTS_CLAIM_BUTTON_SELECTOR).await?;
    let primary_metadata = read_candidate_metadata(&primary_candidates).await?;
    let primary_index = primary_metadata.iter().position(|candidate| {
        candidate.visible
            && !candidate.disabled
            && !is_destructive_button(candidate.class_name.as_deref())
    });

    if let Some(index) = primary_index {
        return primary_candidates.nth(index).await.map(Some);
    }
    if !primary_metadata.is_empty() {
        return Ok(None);
    }

    let summaries = page.locator(COMMUNITY_POINTS_SUMMARY_SELECTOR).await?;
    let fallback_candidates = summaries.locator(REWARD_BUTTON_LIKE_SELECTOR).await?;
    let fallback_metadata = read_candidate_metadata(&fallback_candidates).await?;
    let clickable_indexes = fallback_metadata
        .iter()
        .enumerate()
        .filter_map(|(index, candidate)| {
            (candidate.visible && !candidate.disabled && candidate.aria_label.is_none())
                .then_some(index)
        })
        .collect::<Vec<_>>();

    if clickable_indexes.len() == 1 {
        fallback_candidates
            .nth(clickable_indexes[0])
            .await
            .map(Some)
    } else {
        Ok(None)
    }
}

async fn read_candidate_metadata<P: RewardLocator>(
    candidates: &P,
) -> Result<Vec<RewardCandidateMetadata>, DriverError> {
    let reply = candidates.evaluate_all().await?;
    Ok(reply
        .as_array()
        .map(|elements| elements.iter().map(parse_candidate_metadata).collect())
        .unwrap_or_default())
}

fn parse_candidate_metadata(element: &Value) -> RewardCandidateMetadata {
    RewardCandidateMetadata {
        visible: element
            .get("visible")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        disabled: element
            .get("disabled")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        aria_label: string_option(element.get("ariaLabel")),
        class_name: string_option(element.get("className")),
    }
}

async fn click_claim_button<P: RewardLocator>(claim_button: &P) -> Result<(), DriverError> {
    if let Err(error) = claim_button.click(REWARD_CLAIM_CLICK_TIMEOUT_MS).await {
        return claim_button
            .dispatch_click(REWARD_CLAIM_CLICK_TIMEOUT_MS)
            .await
            .map_err(|_| error);
    }
    Ok(())
}

fn is_destructive_button(class_name: Option<&str>) -> bool {
    class_name
        .map(|class| class.contains("ScCoreButtonDestructive"))
        .unwrap_or(false)
}

fn normalize_channel(channel: &str) -> String {
    channel.trim().to_lowercase()
}

fn string_option(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(value)) => Some(value.clone()),
        _ => None,
    }
}

fn object(fields: Vec<(&'static str, String)>) -> SanitizedValue {
    SanitizedValue::Object(
        fields
            .into_iter()
            .map(|(key, value)| (key.to_string(), SanitizedValue::String(value)))
            .collect(),
    )
}

fn safe_log(logger: &dyn RewardClaimerLogger, info: bool, event: &str, fields: &SanitizedValue) {
    let result = if info {
        logger.info(event, fields)
    } else {
        logger.warn(event, fields)
    };
    if result.is_err() {
        // Logging must not turn a recoverable reward attempt into a session error.
    }
}

fn format_iso_millis(unix_millis: u64) -> String {
    let datetime = OffsetDateTime::from_unix_timestamp_nanos((unix_millis as i128) * 1_000_000)
        .expect("reward clock returned an invalid timestamp");
    datetime
        .format(&ISO_UTC_FORMAT)
        .expect("reward clock timestamp must be representable")
}

fn reward_candidate_script() -> &'static str {
    REWARD_CANDIDATE_SCRIPT
        .get_or_init(|| {
            let raw = include_str!("js/reward-candidates.js");
            strip_header_comments(raw)
        })
        .as_str()
}

fn strip_header_comments(raw: &str) -> String {
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
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use serde_json::json;

    use super::*;
    use crate::logging::redact::REDACTED_VALUE;

    const START_TIME_UNIX_MILLIS: u64 = 1_781_438_400_000;
    const START_TIME_ISO: &str = "2026-06-14T12:00:00.000Z";

    #[derive(Default)]
    struct ClickAction {
        click_count: usize,
        dispatch_count: usize,
        click_timeouts_ms: Vec<u64>,
        dispatch_timeouts_ms: Vec<u64>,
        click_error: Option<String>,
        dispatch_error: Option<String>,
    }

    impl ClickAction {
        fn with_click_error(mut self, message: &str) -> Self {
            self.click_error = Some(message.to_string());
            self
        }

        fn with_dispatch_error(mut self, message: &str) -> Self {
            self.dispatch_error = Some(message.to_string());
            self
        }
    }

    #[derive(Clone, Default)]
    struct MockElement {
        visible: bool,
        disabled: bool,
        aria_label: Option<String>,
        class_name: Option<String>,
        descendants: Vec<MockElement>,
        click: Option<Arc<Mutex<ClickAction>>>,
    }

    impl MockElement {
        fn visible() -> Self {
            Self {
                visible: true,
                ..Self::default()
            }
        }

        fn with_click(mut self) -> Self {
            self.click = Some(Arc::new(Mutex::new(ClickAction::default())));
            self
        }

        fn with_click_error(self, message: &str) -> Self {
            self.with_click_action(ClickAction::default().with_click_error(message))
        }

        fn with_failed_click_fallback(self, message: &str) -> Self {
            let action = ClickAction::default()
                .with_click_error(message)
                .with_dispatch_error("fallback click failed");
            self.with_click_action(action)
        }

        fn with_click_action(mut self, action: ClickAction) -> Self {
            self.click = Some(Arc::new(Mutex::new(action)));
            self
        }

        fn with_disabled(mut self) -> Self {
            self.disabled = true;
            self
        }

        fn with_aria_label(mut self, label: &str) -> Self {
            self.aria_label = Some(label.to_string());
            self
        }

        fn with_class(mut self, class: &str) -> Self {
            self.class_name = Some(class.to_string());
            self
        }
    }

    struct MockPageState {
        locator_error: Option<String>,
        primary: Vec<MockElement>,
        fallback: Vec<MockElement>,
        locator_calls: usize,
        evaluate_all_calls: usize,
    }

    #[derive(Clone)]
    struct MockPage {
        state: Arc<Mutex<MockPageState>>,
    }

    impl Default for MockPage {
        fn default() -> Self {
            Self {
                state: Arc::new(Mutex::new(MockPageState {
                    locator_error: None,
                    primary: Vec::new(),
                    fallback: Vec::new(),
                    locator_calls: 0,
                    evaluate_all_calls: 0,
                })),
            }
        }
    }

    impl MockPage {
        fn with_primary(self, elements: Vec<MockElement>) -> Self {
            self.state.lock().unwrap().primary = elements;
            self
        }

        fn with_fallback(self, elements: Vec<MockElement>) -> Self {
            self.state.lock().unwrap().fallback = elements;
            self
        }

        fn with_locator_error(self, message: &str) -> Self {
            self.state.lock().unwrap().locator_error = Some(message.to_string());
            self
        }

        fn evaluate_all_calls(&self) -> usize {
            self.state.lock().unwrap().evaluate_all_calls
        }
    }

    #[async_trait]
    impl RewardPage for MockPage {
        type Locator = MockLocator;

        async fn locator(&self, selector: &str) -> Result<MockLocator, DriverError> {
            let mut state = self.state.lock().unwrap();
            state.locator_calls += 1;
            if let Some(error) = &state.locator_error {
                return Err(DriverError::new("Error", error.clone()));
            }
            let elements = if selector == COMMUNITY_POINTS_CLAIM_BUTTON_SELECTOR {
                state.primary.clone()
            } else if selector == COMMUNITY_POINTS_SUMMARY_SELECTOR {
                if state.fallback.is_empty() {
                    Vec::new()
                } else {
                    vec![MockElement {
                        visible: true,
                        descendants: state.fallback.clone(),
                        ..MockElement::default()
                    }]
                }
            } else {
                Vec::new()
            };
            drop(state);
            Ok(MockLocator {
                page: Arc::clone(&self.state),
                elements,
            })
        }
    }

    #[derive(Clone)]
    struct MockLocator {
        page: Arc<Mutex<MockPageState>>,
        elements: Vec<MockElement>,
    }

    #[async_trait]
    impl RewardLocator for MockLocator {
        async fn locator(&self, selector: &str) -> Result<MockLocator, DriverError> {
            if selector != REWARD_BUTTON_LIKE_SELECTOR {
                return Ok(MockLocator {
                    page: Arc::clone(&self.page),
                    elements: Vec::new(),
                });
            }
            Ok(MockLocator {
                page: Arc::clone(&self.page),
                elements: self
                    .elements
                    .iter()
                    .flat_map(|element| element.descendants.clone())
                    .collect(),
            })
        }

        async fn nth(&self, index: usize) -> Result<MockLocator, DriverError> {
            Ok(MockLocator {
                page: Arc::clone(&self.page),
                elements: self.elements.get(index).cloned().into_iter().collect(),
            })
        }

        async fn evaluate_all(&self) -> Result<Value, DriverError> {
            self.page.lock().unwrap().evaluate_all_calls += 1;
            Ok(Value::Array(
                self.elements
                    .iter()
                    .map(|element| {
                        json!({
                            "visible": element.visible,
                            "disabled": element.disabled,
                            "ariaLabel": element.aria_label,
                            "className": element.class_name,
                        })
                    })
                    .collect(),
            ))
        }

        async fn click(&self, timeout_ms: u64) -> Result<(), DriverError> {
            let Some(action) = self
                .elements
                .first()
                .and_then(|element| element.click.clone())
            else {
                return Ok(());
            };
            let mut action = action.lock().unwrap();
            action.click_count += 1;
            action.click_timeouts_ms.push(timeout_ms);
            action
                .click_error
                .clone()
                .map_or(Ok(()), |error| Err(DriverError::new("TimeoutError", error)))
        }

        async fn dispatch_click(&self, timeout_ms: u64) -> Result<(), DriverError> {
            let Some(action) = self
                .elements
                .first()
                .and_then(|element| element.click.clone())
            else {
                return Ok(());
            };
            let mut action = action.lock().unwrap();
            action.dispatch_count += 1;
            action.dispatch_timeouts_ms.push(timeout_ms);
            action
                .dispatch_error
                .clone()
                .map_or(Ok(()), |error| Err(DriverError::new("Error", error)))
        }
    }

    struct FixedClock;

    impl RewardClaimerClock for FixedClock {
        fn now_unix_millis(&self) -> u64 {
            START_TIME_UNIX_MILLIS
        }
    }

    struct AdjustableClock(Arc<AtomicU64>);

    impl AdjustableClock {
        fn set(&self, unix_millis: u64) {
            self.0.store(unix_millis, Ordering::SeqCst);
        }
    }

    impl RewardClaimerClock for AdjustableClock {
        fn now_unix_millis(&self) -> u64 {
            self.0.load(Ordering::SeqCst)
        }
    }

    #[derive(Default)]
    struct CollectingLogger {
        events: Mutex<Vec<String>>,
    }

    impl RewardClaimerLogger for CollectingLogger {
        fn info(&self, event: &str, _fields: &SanitizedValue) -> Result<(), String> {
            self.events.lock().unwrap().push(event.to_string());
            Ok(())
        }

        fn warn(&self, event: &str, _fields: &SanitizedValue) -> Result<(), String> {
            self.events.lock().unwrap().push(event.to_string());
            Ok(())
        }
    }

    struct ThrowingLogger;

    impl RewardClaimerLogger for ThrowingLogger {
        fn info(&self, _event: &str, _fields: &SanitizedValue) -> Result<(), String> {
            Err("logger unavailable".to_string())
        }

        fn warn(&self, _event: &str, _fields: &SanitizedValue) -> Result<(), String> {
            Err("logger unavailable".to_string())
        }
    }

    fn start_time_clock() -> Arc<FixedClock> {
        Arc::new(FixedClock)
    }

    fn claimer(clock: Arc<dyn RewardClaimerClock>) -> RewardClaimer {
        RewardClaimer::new(RewardClaimerOptions {
            logger: Arc::new(CollectingLogger::default()),
            clock,
            on_result: None,
        })
    }

    async fn claim<P: RewardPage>(
        claimer: &RewardClaimer,
        page: &P,
        channel: &str,
    ) -> RewardClaimResult {
        let checked_at = claimer.iso_time();
        claimer.claim_on_page(page, channel, &checked_at).await
    }

    // 原測試：使用 primary selector 點擊可領取按鈕
    #[tokio::test]
    async fn claims_clickable_primary_button() {
        let page = MockPage::default().with_primary(vec![MockElement::visible().with_click()]);
        let logger = Arc::new(CollectingLogger::default());
        let observed = Arc::new(Mutex::new(Vec::new()));
        let observed_for_callback = Arc::clone(&observed);
        let claimer = RewardClaimer::new(RewardClaimerOptions {
            logger,
            clock: start_time_clock(),
            on_result: Some(Box::new(move |result| {
                observed_for_callback
                    .lock()
                    .unwrap()
                    .push(result.channel().to_string());
                Ok(())
            })),
        });

        let result = claim(&claimer, &page, "streamer_one").await;
        let primary = &page.state.lock().unwrap().primary[0];
        let action = primary.click.as_ref().unwrap().lock().unwrap();
        assert_eq!(action.click_count, 1);
        assert_eq!(
            result,
            RewardClaimResult::Claimed {
                channel: "streamer_one".to_string(),
                claimed_at: START_TIME_ISO.to_string(),
            }
        );
        assert_eq!(*observed.lock().unwrap(), vec!["streamer_one".to_string()]);
    }

    // 原測試：點擊領取按鈕時使用短 timeout
    #[tokio::test]
    async fn clicks_with_short_timeout() {
        let page = MockPage::default().with_primary(vec![MockElement::visible().with_click()]);
        let claimer = claimer(start_time_clock());

        let result = claim(&claimer, &page, "streamer_timeout").await;

        assert!(matches!(result, RewardClaimResult::Claimed { .. }));
        let state = page.state.lock().unwrap();
        let action = state.primary[0].click.as_ref().unwrap().lock().unwrap();
        assert_eq!(action.click_timeouts_ms, vec![3_000]);
    }

    // 原測試：一般 click 失敗時使用 DOM click fallback
    #[tokio::test]
    async fn uses_dom_click_fallback_after_click_failure() {
        let page = MockPage::default().with_primary(vec![
            MockElement::visible().with_click_error("click action timed out"),
        ]);
        let claimer = claimer(start_time_clock());

        let result = claim(&claimer, &page, "streamer_dom_fallback").await;

        assert!(matches!(result, RewardClaimResult::Claimed { .. }));
        let state = page.state.lock().unwrap();
        let action = state.primary[0].click.as_ref().unwrap().lock().unwrap();
        assert_eq!(action.dispatch_count, 1);
        assert_eq!(action.dispatch_timeouts_ms, vec![3_000]);
    }

    // 原測試：primary selector 不存在時使用 summary 結構 fallback
    #[tokio::test]
    async fn uses_summary_fallback_when_primary_is_empty() {
        let page = MockPage::default().with_fallback(vec![MockElement::visible().with_click()]);
        let claimer = claimer(start_time_clock());

        let result = claim(&claimer, &page, "streamer_two").await;

        assert!(matches!(result, RewardClaimResult::Claimed { .. }));
        assert_eq!(page.evaluate_all_calls(), 2);
        let state = page.state.lock().unwrap();
        let action = state.fallback[0].click.as_ref().unwrap().lock().unwrap();
        assert_eq!(action.click_count, 1);
    }

    // 原測試：批次讀取 primary metadata，避免逐一呼叫 locator probe
    #[tokio::test]
    async fn reads_primary_metadata_in_one_batch() {
        let page = MockPage::default().with_primary(vec![
            MockElement::default(),
            MockElement::visible().with_click(),
        ]);
        let claimer = claimer(start_time_clock());

        let result = claim(&claimer, &page, "streamer_batched").await;

        assert!(matches!(result, RewardClaimResult::Claimed { .. }));
        assert_eq!(page.evaluate_all_calls(), 1);
    }

    // 原測試：summary 有多個可點擊元素時不猜測要點擊哪一個
    #[tokio::test]
    async fn does_not_guess_when_summary_has_multiple_clickable_elements() {
        let page = MockPage::default().with_fallback(vec![
            MockElement::visible().with_click(),
            MockElement::visible().with_click(),
        ]);
        let claimer = claimer(start_time_clock());

        let result = claim(&claimer, &page, "streamer_ambiguous").await;

        assert!(matches!(result, RewardClaimResult::NotFound { .. }));
        let state = page.state.lock().unwrap();
        for element in &state.fallback {
            let action = element.click.as_ref().unwrap().lock().unwrap();
            assert_eq!(action.click_count, 0);
        }
    }

    // 原測試：summary 內有 aria-label 的餘額控制不視為領取按鈕
    #[tokio::test]
    async fn does_not_treat_aria_labeled_balance_control_as_claim_button() {
        let page = MockPage::default().with_fallback(vec![
            MockElement::visible()
                .with_aria_label("Bits and Points Balances")
                .with_click(),
        ]);
        let claimer = claimer(start_time_clock());

        let result = claim(&claimer, &page, "streamer_balance").await;

        assert!(matches!(result, RewardClaimResult::NotFound { .. }));
        let state = page.state.lock().unwrap();
        let action = state.fallback[0].click.as_ref().unwrap().lock().unwrap();
        assert_eq!(action.click_count, 0);
    }

    // 原測試：primary selector 已存在但 disabled 時不使用 fallback
    #[tokio::test]
    async fn does_not_use_fallback_when_primary_exists_but_is_disabled() {
        let primary = MockElement::visible().with_disabled().with_click();
        let fallback = MockElement::visible().with_click();
        let page = MockPage::default()
            .with_primary(vec![primary])
            .with_fallback(vec![fallback]);
        let claimer = claimer(start_time_clock());

        let result = claim(&claimer, &page, "streamer_three").await;

        assert!(matches!(result, RewardClaimResult::NotFound { .. }));
        let state = page.state.lock().unwrap();
        assert_eq!(
            state.primary[0]
                .click
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .click_count,
            0
        );
        assert_eq!(
            state.fallback[0]
                .click
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .click_count,
            0
        );
    }

    // 原測試：BetterTTV selector 若帶 destructive class 時不點擊
    #[tokio::test]
    async fn does_not_click_destructive_class_button() {
        let page = MockPage::default().with_primary(vec![
            MockElement::visible()
                .with_class("claimable-bonus__icon ScCoreButtonDestructive")
                .with_click(),
        ]);
        let claimer = claimer(start_time_clock());

        let result = claim(&claimer, &page, "streamer_destructive").await;

        assert!(matches!(result, RewardClaimResult::NotFound { .. }));
        let state = page.state.lock().unwrap();
        let action = state.primary[0].click.as_ref().unwrap().lock().unwrap();
        assert_eq!(action.click_count, 0);
    }

    // 原測試：按鈕不可見時回傳 not_found
    #[tokio::test]
    async fn returns_not_found_when_button_is_hidden() {
        let page = MockPage::default().with_primary(vec![MockElement::default().with_click()]);
        let claimer = claimer(start_time_clock());

        let result = claim(&claimer, &page, "streamer_hidden").await;

        assert_eq!(
            result,
            RewardClaimResult::NotFound {
                channel: "streamer_hidden".to_string(),
                checked_at: START_TIME_ISO.to_string(),
            }
        );
        let state = page.state.lock().unwrap();
        let action = state.primary[0].click.as_ref().unwrap().lock().unwrap();
        assert_eq!(action.click_count, 0);
    }

    // 原測試：按鈕不存在時回傳 not_found 且不寫 warn
    #[tokio::test]
    async fn returns_not_found_without_warn_when_button_is_missing() {
        let logger = Arc::new(CollectingLogger::default());
        let claimer = RewardClaimer::new(RewardClaimerOptions {
            logger: logger.clone(),
            clock: start_time_clock(),
            on_result: None,
        });

        let result = claim(&claimer, &MockPage::default(), "streamer_four").await;

        assert!(matches!(result, RewardClaimResult::NotFound { .. }));
        assert!(
            !logger
                .events
                .lock()
                .unwrap()
                .iter()
                .any(|event| event == "reward_claim_failed")
        );
    }

    // 原測試：點擊失敗時回傳安全錯誤並記錄 warn
    #[tokio::test]
    async fn returns_safe_error_and_logs_warn_when_click_fails() {
        let token_secret = "token-secret-value";
        let cookie_secret = "cookie-secret-value";
        let page = MockPage::default().with_primary(vec![
            MockElement::visible().with_failed_click_fallback(&format!(
                "token={token_secret} Cookie: session={cookie_secret}"
            )),
        ]);
        let logger = Arc::new(CollectingLogger::default());
        let observed = Arc::new(Mutex::new(Vec::new()));
        let observed_for_callback = Arc::clone(&observed);
        let claimer = RewardClaimer::new(RewardClaimerOptions {
            logger,
            clock: start_time_clock(),
            on_result: Some(Box::new(move |result| {
                observed_for_callback
                    .lock()
                    .unwrap()
                    .push(result.channel().to_string());
                Ok(())
            })),
        });

        let result = claim(&claimer, &page, "streamer_five").await;

        let RewardClaimResult::ClickFailed { error, .. } = result else {
            panic!("expected click_failed");
        };
        assert!(error.contains(REDACTED_VALUE));
        assert!(!error.contains(cookie_secret));
        assert!(!error.contains(token_secret));
        assert_eq!(*observed.lock().unwrap(), vec!["streamer_five".to_string()]);
    }

    // 原測試：selector API 拋錯時不向外拋出
    #[tokio::test]
    async fn does_not_propagate_selector_api_error() {
        let page = MockPage::default().with_locator_error("access_token=selector-token-secret");
        let claimer = claimer(start_time_clock());

        let result = claim(&claimer, &page, "streamer_six").await;

        let RewardClaimResult::ClickFailed { error, .. } = result else {
            panic!("expected click_failed");
        };
        assert!(!error.contains("selector-token-secret"));
    }

    // 原測試：成功後同一 channel 在 60 秒內不重複點擊
    #[tokio::test]
    async fn does_not_click_same_channel_again_within_cooldown() {
        let clock = Arc::new(AdjustableClock(Arc::new(AtomicU64::new(
            START_TIME_UNIX_MILLIS,
        ))));
        let page = MockPage::default().with_primary(vec![MockElement::visible().with_click()]);
        let claimer = claimer(clock.clone());

        assert!(matches!(
            claim(&claimer, &page, "Streamer_Seven").await,
            RewardClaimResult::Claimed { .. }
        ));
        clock
            .0
            .store(START_TIME_UNIX_MILLIS + 59_999, Ordering::SeqCst);
        assert!(matches!(
            claim(&claimer, &page, "streamer_seven").await,
            RewardClaimResult::NotFound { .. }
        ));
        clock
            .0
            .store(START_TIME_UNIX_MILLIS + 60_000, Ordering::SeqCst);
        assert!(matches!(
            claim(&claimer, &page, "streamer_seven").await,
            RewardClaimResult::Claimed { .. }
        ));
        let state = page.state.lock().unwrap();
        let action = state.primary[0].click.as_ref().unwrap().lock().unwrap();
        assert_eq!(action.click_count, 2);
    }

    // 原測試：logger 拋錯時仍回傳領取結果
    #[tokio::test]
    async fn returns_claim_result_even_when_logger_fails() {
        let page = MockPage::default().with_primary(vec![MockElement::visible().with_click()]);
        let claimer = RewardClaimer::new(RewardClaimerOptions {
            logger: Arc::new(ThrowingLogger),
            clock: start_time_clock(),
            on_result: None,
        });

        let result = claim(&claimer, &page, "streamer_eight").await;

        assert!(matches!(result, RewardClaimResult::Claimed { .. }));
    }

    // 原測試：hasClaimableBonus 在主要按鈕可見時回傳 true
    #[tokio::test]
    async fn has_claimable_bonus_is_true_for_visible_primary_button() {
        let page = MockPage::default().with_primary(vec![MockElement::visible()]);
        let claimer = claimer(start_time_clock());

        assert!(claimer.has_claimable_bonus_on_page(&page).await);
    }

    // 原測試：hasClaimableBonus 在沒有可見按鈕時回傳 false
    #[tokio::test]
    async fn has_claimable_bonus_is_false_without_visible_button() {
        let page = MockPage::default().with_primary(vec![MockElement::default()]);
        let claimer = claimer(start_time_clock());

        assert!(!claimer.has_claimable_bonus_on_page(&page).await);
    }

    // 原測試：hasClaimableBonus 在 fallback 剛好一個可點按鈕時回傳 true
    #[tokio::test]
    async fn has_claimable_bonus_is_true_for_single_fallback_button() {
        let page = MockPage::default().with_fallback(vec![MockElement::visible()]);
        let claimer = claimer(start_time_clock());

        assert!(claimer.has_claimable_bonus_on_page(&page).await);
    }

    // 原測試：hasClaimableBonus 在 DOM 檢查拋錯時回傳 false
    #[tokio::test]
    async fn has_claimable_bonus_is_false_when_dom_check_fails() {
        let page = MockPage::default()
            .with_primary(vec![MockElement::visible()])
            .with_locator_error("page crashed");
        let claimer = claimer(start_time_clock());

        assert!(!claimer.has_claimable_bonus_on_page(&page).await);
    }
}
