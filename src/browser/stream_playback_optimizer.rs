//! `StreamPlaybackOptimizer.ts` 的移植：播放前靜音與畫質調整。
//!
//! `BrowserPage` 只暴露 `as_page_shared()`，回傳的是具體的 cdp `PageShared`，
//! 無法以假頁模擬舊測試的 Playwright `Page`。本模組因此定義僅涵蓋本檔用到的
//! 頁面與 locator API；正式端由 [`CdpStreamPlaybackPage`] 轉接，測試端注入
//! 舊測試同形的 fake page。所有畫質流程錯誤都只記 debug，不中斷觀看。
#![allow(dead_code)]

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use regex::Regex;
use serde_json::{Value, json};

use super::cdp::context::DriverError;
use super::cdp::locator::{Locator, WaitForState};
use super::cdp::page::PageShared;
use super::selectors::{
    QUALITY_MENU_SELECTOR, QUALITY_OPTION_SELECTOR, SETTINGS_BUTTON_SELECTOR,
    VIDEO_PLAYER_SELECTOR, VIDEO_SELECTOR,
};
use super::types::BrowserManagerLogger;
use crate::browser::safe_logging::safe_error_message;
use crate::config::model::{BrowserConfig, StreamQuality};
use crate::logging::redact::SanitizedValue;

/// 舊版 `CONTROL_WAIT_TIMEOUT_MS`。
const CONTROL_WAIT_TIMEOUT_MS: u64 = 2_000;

/// 舊版 `QUALITY_ORDER`。
const QUALITY_ORDER: [StreamQuality; 3] = [
    StreamQuality::P160,
    StreamQuality::P360,
    StreamQuality::P480,
];

/// 舊版 `PlaybackOptimizationResult`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PlaybackOptimizationResult {
    pub(crate) muted: bool,
    pub(crate) selected_quality: Option<StreamQuality>,
}

/// 舊版 constructor 的 `Pick<BrowserConfig, 'muteAudio' | 'streamQuality'>`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StreamPlaybackOptimizerConfig {
    pub(crate) mute_audio: bool,
    pub(crate) stream_quality: StreamQuality,
}

impl From<&BrowserConfig> for StreamPlaybackOptimizerConfig {
    fn from(config: &BrowserConfig) -> Self {
        StreamPlaybackOptimizerConfig {
            mute_audio: config.mute_audio,
            stream_quality: config.stream_quality,
        }
    }
}

/// 舊版 `Pick<Logger, 'debug' | 'info'>`；沿用 manager 的日誌抽象讓測試可收集。
pub(crate) type StreamPlaybackOptimizerLogger = dyn BrowserManagerLogger;

/// 本檔使用的頁面 API（對應舊版 `Page` 用到的子集）。
#[async_trait]
pub(crate) trait StreamPlaybackPage: Send + Sync {
    /// 舊版 `page.locator(selector)`；builder 不失敗，操作時才回錯。
    fn locator(&self, selector: &str) -> Arc<dyn StreamPlaybackLocator>;

    /// 舊版 `page.mouse.move(x, y)`。
    async fn mouse_move(&self, x: f64, y: f64) -> Result<(), DriverError>;
}

/// 本檔使用的 locator API（對應舊版 `Locator` 用到的子集）。
#[async_trait]
pub(crate) trait StreamPlaybackLocator: Send + Sync {
    /// 舊版 `locator.first()`。
    fn first(&self) -> Arc<dyn StreamPlaybackLocator>;

    /// 舊版 `locator.nth(index)`。
    fn nth(&self, index: usize) -> Arc<dyn StreamPlaybackLocator>;

    /// 舊版 `locator.count()`。
    async fn count(&self) -> Result<usize, DriverError>;

    /// 舊版 `locator.waitFor({state: 'visible', timeout})`。
    async fn wait_for(&self, timeout: Option<u64>) -> Result<(), DriverError>;

    /// 舊版 `locator.click({timeout})`。
    async fn click(&self, timeout: Option<u64>) -> Result<(), DriverError>;

    /// 舊版 `locator.hover({timeout})`。
    async fn hover(&self, timeout: Option<u64>) -> Result<(), DriverError>;

    /// 舊版 `locator.evaluate(fn, arg)`。
    async fn evaluate(
        &self,
        fn_declaration: &str,
        arg: Option<Value>,
    ) -> Result<Value, DriverError>;

    /// 舊版 `locator.allTextContents()`；cdp 層以 evaluateAll 呈現。
    async fn evaluate_all(&self, fn_declaration: &str) -> Result<Value, DriverError>;
}

/// 正式端的頁面轉接：包 [`PageShared`] 的 locator API。
struct CdpStreamPlaybackPage {
    shared: Arc<PageShared>,
}

impl CdpStreamPlaybackPage {
    pub(crate) fn new(shared: Arc<PageShared>) -> Self {
        Self { shared }
    }
}

#[async_trait]
impl StreamPlaybackPage for CdpStreamPlaybackPage {
    fn locator(&self, selector: &str) -> Arc<dyn StreamPlaybackLocator> {
        Arc::new(CdpStreamPlaybackLocator {
            inner: self.shared.locator(selector),
        })
    }

    async fn mouse_move(&self, x: f64, y: f64) -> Result<(), DriverError> {
        self.shared.mouse_move(x, y).await
    }
}

struct CdpStreamPlaybackLocator {
    inner: Locator,
}

#[async_trait]
impl StreamPlaybackLocator for CdpStreamPlaybackLocator {
    fn first(&self) -> Arc<dyn StreamPlaybackLocator> {
        Arc::new(Self {
            inner: self.inner.first(),
        })
    }

    fn nth(&self, index: usize) -> Arc<dyn StreamPlaybackLocator> {
        Arc::new(Self {
            inner: self.inner.nth(index as i64),
        })
    }

    async fn count(&self) -> Result<usize, DriverError> {
        self.inner.count().await
    }

    async fn wait_for(&self, timeout: Option<u64>) -> Result<(), DriverError> {
        self.inner.wait_for(WaitForState::Visible, timeout).await
    }

    async fn click(&self, timeout: Option<u64>) -> Result<(), DriverError> {
        self.inner.click(timeout).await
    }

    async fn hover(&self, timeout: Option<u64>) -> Result<(), DriverError> {
        self.inner.hover(timeout).await
    }

    async fn evaluate(
        &self,
        fn_declaration: &str,
        arg: Option<Value>,
    ) -> Result<Value, DriverError> {
        self.inner.evaluate(fn_declaration, arg, None).await
    }

    async fn evaluate_all(&self, fn_declaration: &str) -> Result<Value, DriverError> {
        self.inner.evaluate_all(fn_declaration).await
    }
}

/// 從 `BrowserPage::as_page_shared()` 進入的最佳化入口（正式呼叫端用）。
pub(crate) async fn optimize_page_shared(
    page_shared: &Arc<PageShared>,
    channel: &str,
    config: StreamPlaybackOptimizerConfig,
    logger: &StreamPlaybackOptimizerLogger,
) -> PlaybackOptimizationResult {
    optimize(
        &CdpStreamPlaybackPage::new(Arc::clone(page_shared)),
        channel,
        config,
        logger,
    )
    .await
}

/// 舊版 `DefaultStreamPlaybackOptimizer.optimize()`。
pub(crate) async fn optimize(
    page: &dyn StreamPlaybackPage,
    channel: &str,
    config: StreamPlaybackOptimizerConfig,
    logger: &StreamPlaybackOptimizerLogger,
) -> PlaybackOptimizationResult {
    let video_state = prepare_video(page, channel, config.mute_audio, logger).await;
    let muted = config.mute_audio && video_state.muted;

    if config.stream_quality == StreamQuality::Auto {
        return PlaybackOptimizationResult {
            muted,
            selected_quality: None,
        };
    }

    if is_requested_quality_active(video_state.video_height, config.stream_quality) {
        let fields = vec![
            ("channel".to_string(), string_value(channel)),
            (
                "requestedQuality".to_string(),
                string_value(config.stream_quality.as_str()),
            ),
            (
                "videoHeight".to_string(),
                SanitizedValue::Number(video_state.video_height.unwrap()),
            ),
        ];
        logger.debug(
            "stream_quality_already_selected",
            Some(&SanitizedValue::Object(fields)),
        );
        return PlaybackOptimizationResult {
            muted,
            selected_quality: Some(config.stream_quality),
        };
    }

    let selected_quality = match select_quality(page, channel, config.stream_quality, logger).await
    {
        Ok(selected_quality) => selected_quality,
        Err(error) => {
            log_quality_error(logger, channel, config.stream_quality, &error.to_string());
            None
        }
    };
    if let Some(selected_quality) = selected_quality {
        logger.info(
            "stream_playback_optimized",
            Some(&SanitizedValue::Object(vec![
                ("channel".to_string(), string_value(channel)),
                ("muted".to_string(), SanitizedValue::Bool(muted)),
                (
                    "selectedQuality".to_string(),
                    string_value(selected_quality.as_str()),
                ),
            ])),
        );
    }

    PlaybackOptimizationResult {
        muted,
        selected_quality,
    }
}

/// 舊版 `prepareVideo()`。
async fn prepare_video(
    page: &dyn StreamPlaybackPage,
    channel: &str,
    mute_audio: bool,
    logger: &StreamPlaybackOptimizerLogger,
) -> VideoPlaybackState {
    let result = async {
        let video = page.locator(VIDEO_SELECTOR).first();
        if video.count().await? == 0 {
            return Ok(VideoPlaybackState::default());
        }
        let state = video
            .evaluate(playback_state_script(), Some(json!(mute_audio)))
            .await?;
        parse_video_state(&state)
    }
    .await;

    match result {
        Ok(state) => state,
        Err(error) => {
            logger.debug(
                "stream_mute_skipped",
                Some(&SanitizedValue::Object(vec![
                    ("channel".to_string(), string_value(channel)),
                    (
                        "error".to_string(),
                        string_value(safe_error_message(Some(&error.to_string()))),
                    ),
                ])),
            );
            VideoPlaybackState::default()
        }
    }
}

/// 舊版 `selectQuality()`。
async fn select_quality(
    page: &dyn StreamPlaybackPage,
    channel: &str,
    requested_quality: StreamQuality,
    logger: &StreamPlaybackOptimizerLogger,
) -> Result<Option<StreamQuality>, DriverError> {
    reveal_player_controls(page).await?;

    let settings_button = page.locator(SETTINGS_BUTTON_SELECTOR).first();
    if settings_button
        .wait_for(Some(CONTROL_WAIT_TIMEOUT_MS))
        .await
        .is_err()
    {
        log_quality_skipped(logger, channel, requested_quality, "settings_missing");
        return Ok(None);
    }
    settings_button.click(Some(CONTROL_WAIT_TIMEOUT_MS)).await?;

    let quality_menu = page.locator(QUALITY_MENU_SELECTOR).first();
    if quality_menu
        .wait_for(Some(CONTROL_WAIT_TIMEOUT_MS))
        .await
        .is_err()
    {
        log_quality_skipped(logger, channel, requested_quality, "quality_menu_missing");
        return Ok(None);
    }
    // DECISION: 舊版對選單與選項使用 click({force: true})；Rust Locator 目前只有
    // regular click(Some(timeout))。regular click 會多做 actionability checks；
    // cdp 移植的 pointer.rs 已涵蓋 Playwright 的重試與 hit-target 邏輯。
    quality_menu.click(Some(CONTROL_WAIT_TIMEOUT_MS)).await?;

    let options = page.locator(QUALITY_OPTION_SELECTOR);
    if options
        .first()
        .wait_for(Some(CONTROL_WAIT_TIMEOUT_MS))
        .await
        .is_err()
    {
        log_quality_skipped(
            logger,
            channel,
            requested_quality,
            "quality_options_missing",
        );
        return Ok(None);
    }

    let labels = read_quality_labels(options.as_ref()).await?;
    let quality_candidates = quality_option_candidates(&labels);
    let candidate_labels = quality_candidates
        .iter()
        .map(|candidate| candidate.label.clone())
        .collect::<Vec<_>>();
    let Some(selected_quality) = choose_quality(&candidate_labels, requested_quality) else {
        log_quality_skipped(
            logger,
            channel,
            requested_quality,
            "supported_quality_missing",
        );
        return Ok(None);
    };

    let selected_lowercase = selected_quality.as_str().to_lowercase();
    let Some(option) = quality_candidates
        .into_iter()
        .find(|candidate| candidate.label.to_lowercase().contains(&selected_lowercase))
    else {
        log_quality_skipped(
            logger,
            channel,
            requested_quality,
            "quality_option_index_missing",
        );
        return Ok(None);
    };

    options
        .nth(option.index)
        .click(Some(CONTROL_WAIT_TIMEOUT_MS))
        .await?;
    Ok(Some(selected_quality))
}

/// 舊版 `revealPlayerControls()`。
async fn reveal_player_controls(page: &dyn StreamPlaybackPage) -> Result<(), DriverError> {
    let player = page.locator(VIDEO_PLAYER_SELECTOR).first();
    let player_hover: Result<bool, DriverError> = async {
        if player.count().await? == 0 {
            return Ok(false);
        }
        player.hover(Some(CONTROL_WAIT_TIMEOUT_MS)).await?;
        Ok(true)
    }
    .await;

    match player_hover {
        Ok(true) => Ok(()),
        // 舊版 count 為 0 或 hover 失敗時，都退回移動指標。
        Ok(false) | Err(_) => page.mouse_move(320.0, 320.0).await,
    }
}

/// 舊版 `options.allTextContents().map((label) => label.trim())`。
async fn read_quality_labels(
    options: &dyn StreamPlaybackLocator,
) -> Result<Vec<String>, DriverError> {
    let value = options
        .evaluate_all("(elements) => elements.map((element) => element.textContent)")
        .await?;
    let Value::Array(values) = value else {
        return Err(DriverError::new(
            "Error",
            "locator.allTextContents did not return an array",
        ));
    };
    let mut labels = Vec::with_capacity(values.len());
    for value in values {
        let Value::String(label) = value else {
            return Err(DriverError::new(
                "Error",
                "locator.allTextContents returned a non-string label",
            ));
        };
        labels.push(js_trim(&label).to_string());
    }
    Ok(labels)
}

/// 舊版 `qualityOptionCandidates()`。
fn quality_option_candidates(labels: &[String]) -> Vec<QualityOptionCandidate> {
    labels
        .iter()
        .enumerate()
        .filter(|(_, label)| is_quality_label(label))
        .map(|(index, label)| QualityOptionCandidate {
            index,
            label: label.clone(),
        })
        .collect()
}

/// 舊版 `isQualityLabel()`。
fn is_quality_label(label: &str) -> bool {
    let normalized = js_trim(label).to_lowercase();
    if normalized.is_empty()
        || normalized.contains("picture-in-picture")
        || normalized.contains("picture in picture")
    {
        return false;
    }
    normalized == "auto"
        || normalized.contains("source")
        || quality_height_pattern().is_match(&normalized)
}

/// 舊版 `chooseQuality()`。
pub(crate) fn choose_quality(
    labels: &[String],
    requested_quality: StreamQuality,
) -> Option<StreamQuality> {
    let normalized = labels
        .iter()
        .map(|label| label.to_lowercase())
        .collect::<Vec<_>>();
    let requested = requested_quality.as_str().to_lowercase();
    if normalized.iter().any(|label| label.contains(&requested)) {
        return Some(requested_quality);
    }
    QUALITY_ORDER.into_iter().find(|quality| {
        let quality = quality.as_str().to_lowercase();
        normalized.iter().any(|label| label.contains(&quality))
    })
}

/// 舊版 `isRequestedQualityActive()`。
fn is_requested_quality_active(
    video_height: Option<f64>,
    requested_quality: StreamQuality,
) -> bool {
    let Some(video_height) = video_height else {
        return false;
    };
    (video_height - requested_height(requested_quality)).abs() <= 16.0
}

/// 舊版 `Number.parseInt(requestedQuality, 10)` 的對應值（型別已封閉）。
fn requested_height(quality: StreamQuality) -> f64 {
    match quality {
        StreamQuality::P160 => 160.0,
        StreamQuality::P360 => 360.0,
        StreamQuality::P480 => 480.0,
        StreamQuality::Auto => 0.0,
    }
}

/// 舊版 `VideoPlaybackState`；undefined 的 `videoHeight` 以 `None` 表示。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct VideoPlaybackState {
    muted: bool,
    video_height: Option<f64>,
}

/// 解析 playback-state.js 的回傳值。
fn parse_video_state(value: &Value) -> Result<VideoPlaybackState, DriverError> {
    let Some(object) = value.as_object() else {
        return Err(DriverError::new(
            "Error",
            "playback-state script did not return an object",
        ));
    };
    Ok(VideoPlaybackState {
        muted: object
            .get("muted")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        video_height: object
            .get("videoHeight")
            .and_then(Value::as_f64)
            .filter(|height| *height > 0.0),
    })
}

/// 舊版 `logQualitySkipped()`。
fn log_quality_skipped(
    logger: &StreamPlaybackOptimizerLogger,
    channel: &str,
    requested_quality: StreamQuality,
    reason: &str,
) {
    logger.debug(
        "stream_quality_skipped",
        Some(&SanitizedValue::Object(vec![
            ("channel".to_string(), string_value(channel)),
            (
                "requestedQuality".to_string(),
                string_value(requested_quality.as_str()),
            ),
            ("reason".to_string(), string_value(reason)),
        ])),
    );
}

/// 舊版 catch 分支的 `stream_quality_skipped` 欄位。
fn log_quality_error(
    logger: &StreamPlaybackOptimizerLogger,
    channel: &str,
    requested_quality: StreamQuality,
    error: &str,
) {
    logger.debug(
        "stream_quality_skipped",
        Some(&SanitizedValue::Object(vec![
            ("channel".to_string(), string_value(channel)),
            (
                "requestedQuality".to_string(),
                string_value(requested_quality.as_str()),
            ),
            (
                "error".to_string(),
                string_value(safe_error_message(Some(error))),
            ),
        ])),
    );
}

fn string_value(value: impl Into<String>) -> SanitizedValue {
    SanitizedValue::String(value.into())
}

/// 舊版 `QualityOptionCandidate`。
struct QualityOptionCandidate {
    index: usize,
    label: String,
}

/// 舊版 `/\b\d{3,4}p\b/u`；JS 的 `\b` 只認 ASCII 單字字元。
fn quality_height_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| match Regex::new(r"(?-u:\b)\d{3,4}p(?-u:\b)") {
        Ok(regex) => regex,
        Err(error) => panic!("invalid built-in quality height pattern: {error}"),
    })
}

/// 舊版 `String.prototype.trim` 會移除的字元（U+0085 與 U+FEFF 的差異照舊版）。
fn js_trim(value: &str) -> &str {
    value.trim_matches(|character| {
        matches!(
            character,
            '\t' | '\n' | '\u{0b}' | '\u{0c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
                ..='\u{200a}'
                    | '\u{2028}'
                    | '\u{2029}'
                    | '\u{202f}'
                    | '\u{205f}'
                    | '\u{3000}'
                    | '\u{feff}'
        )
    })
}

/// `include_str!` 載入 playback-state.js 後剝除開頭連續的 `//` 註解行，再 trim
/// 前後空白（DEC-004）。
fn playback_state_script() -> &'static str {
    static SCRIPT: OnceLock<String> = OnceLock::new();
    SCRIPT.get_or_init(|| {
        let raw = include_str!("js/playback-state.js");
        let mut header_lines = 0usize;
        for line in raw.split('\n') {
            if line.trim_start().starts_with("//") {
                header_lines += 1;
                continue;
            }
            break;
        }
        let lines: Vec<&str> = raw.split('\n').collect();
        lines[header_lines..].join("\n").trim().to_string()
    })
}

#[cfg(test)]
mod tests;
