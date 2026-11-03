//! `StreamPlaybackOptimizer` 的單元測試：移植舊版
//! `test/unit/stream-playback-optimizer.test.ts` 的 6 個案例。

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{
    PlaybackOptimizationResult, StreamPlaybackLocator, StreamPlaybackOptimizerConfig,
    StreamPlaybackPage, choose_quality, optimize,
};
use crate::browser::cdp::context::DriverError;
use crate::browser::types::BrowserManagerLogger;
use crate::config::model::StreamQuality;
use crate::logging::logger::LogLevel;
use crate::logging::redact::SanitizedValue;

type SharedCounter = Arc<Mutex<usize>>;
type ClickedIndexes = Arc<Mutex<Vec<usize>>>;
type VideoEvaluate = Arc<dyn Fn(Option<Value>) -> Value + Send + Sync>;

#[derive(Clone, Copy, Debug, PartialEq)]
enum LocatorKind {
    Video,
    Settings,
    QualityMenu,
    QualityOptions,
}

#[derive(Clone)]
struct FakeLocator {
    kind: LocatorKind,
    video_count: usize,
    video_evaluate: VideoEvaluate,
    video_evaluate_calls: SharedCounter,
    settings_clicks: SharedCounter,
    quality_menu_clicks: SharedCounter,
    quality_option_clicks: SharedCounter,
    quality_option_indexes: ClickedIndexes,
    quality_labels: Arc<Vec<String>>,
}

#[async_trait]
impl StreamPlaybackLocator for FakeLocator {
    fn first(&self) -> Arc<dyn StreamPlaybackLocator> {
        Arc::new(self.clone())
    }

    fn nth(&self, index: usize) -> Arc<dyn StreamPlaybackLocator> {
        if self.kind == LocatorKind::QualityOptions {
            self.quality_option_indexes.lock().unwrap().push(index);
        }
        Arc::new(self.clone())
    }

    async fn count(&self) -> Result<usize, DriverError> {
        Ok(match self.kind {
            LocatorKind::Video => self.video_count,
            _ => 1,
        })
    }

    async fn wait_for(&self, _timeout: Option<u64>) -> Result<(), DriverError> {
        Ok(())
    }

    async fn click(&self, _timeout: Option<u64>) -> Result<(), DriverError> {
        match self.kind {
            LocatorKind::Settings => *self.settings_clicks.lock().unwrap() += 1,
            LocatorKind::QualityMenu => *self.quality_menu_clicks.lock().unwrap() += 1,
            LocatorKind::QualityOptions => *self.quality_option_clicks.lock().unwrap() += 1,
            LocatorKind::Video => {}
        }
        Ok(())
    }

    async fn hover(&self, _timeout: Option<u64>) -> Result<(), DriverError> {
        Ok(())
    }

    async fn evaluate(
        &self,
        _fn_declaration: &str,
        arg: Option<Value>,
    ) -> Result<Value, DriverError> {
        if self.kind != LocatorKind::Video {
            return Ok(Value::Null);
        }
        *self.video_evaluate_calls.lock().unwrap() += 1;
        Ok((self.video_evaluate)(arg))
    }

    async fn evaluate_all(&self, _fn_declaration: &str) -> Result<Value, DriverError> {
        if self.kind != LocatorKind::QualityOptions {
            return Ok(json!([]));
        }
        Ok(json!(
            self.quality_labels.iter().cloned().collect::<Vec<_>>()
        ))
    }
}

struct FakePage {
    video_evaluate: VideoEvaluate,
    video_evaluate_calls: SharedCounter,
    video_count: usize,
    settings_clicks: SharedCounter,
    quality_menu_clicks: SharedCounter,
    quality_option_clicks: SharedCounter,
    quality_option_indexes: ClickedIndexes,
    quality_labels: Arc<Vec<String>>,
}

#[async_trait]
impl StreamPlaybackPage for FakePage {
    fn locator(&self, selector: &str) -> Arc<dyn StreamPlaybackLocator> {
        let kind = if selector == "video" {
            LocatorKind::Video
        } else if selector.contains("player-settings-button") {
            LocatorKind::Settings
        } else if selector.contains("player-settings-menu-item-quality-option") {
            LocatorKind::QualityOptions
        } else {
            LocatorKind::QualityMenu
        };
        Arc::new(FakeLocator {
            kind,
            video_count: self.video_count,
            video_evaluate: Arc::clone(&self.video_evaluate),
            video_evaluate_calls: Arc::clone(&self.video_evaluate_calls),
            settings_clicks: Arc::clone(&self.settings_clicks),
            quality_menu_clicks: Arc::clone(&self.quality_menu_clicks),
            quality_option_clicks: Arc::clone(&self.quality_option_clicks),
            quality_option_indexes: Arc::clone(&self.quality_option_indexes),
            quality_labels: Arc::clone(&self.quality_labels),
        })
    }

    async fn mouse_move(&self, _x: f64, _y: f64) -> Result<(), DriverError> {
        Ok(())
    }
}

struct FakePageInput {
    video_evaluate: VideoEvaluate,
    quality_labels: Vec<&'static str>,
    quality_option_indexes_clicked: Option<ClickedIndexes>,
    video_count: usize,
}

fn counter() -> SharedCounter {
    Arc::new(Mutex::new(0))
}

fn create_page(input: FakePageInput) -> FakePage {
    FakePage {
        video_evaluate: input.video_evaluate,
        video_evaluate_calls: counter(),
        video_count: input.video_count,
        settings_clicks: counter(),
        quality_menu_clicks: counter(),
        quality_option_clicks: counter(),
        quality_option_indexes: input
            .quality_option_indexes_clicked
            .unwrap_or_else(|| Arc::new(Mutex::new(Vec::new()))),
        quality_labels: Arc::new(
            input
                .quality_labels
                .into_iter()
                .map(str::to_string)
                .collect(),
        ),
    }
}

fn config(mute_audio: bool, stream_quality: StreamQuality) -> StreamPlaybackOptimizerConfig {
    StreamPlaybackOptimizerConfig {
        mute_audio,
        stream_quality,
    }
}

struct FakeLogger {
    entries: Mutex<Vec<(LogLevel, String, serde_json::Value)>>,
}

fn to_json(value: &SanitizedValue) -> serde_json::Value {
    serde_json::from_str(&value.to_json_string()).unwrap_or(serde_json::Value::Null)
}

impl FakeLogger {
    fn fields_of(&self, level: LogLevel, event: &str) -> Vec<serde_json::Value> {
        self.entries
            .lock()
            .unwrap()
            .iter()
            .filter(|(entry_level, entry_event, _)| *entry_level == level && entry_event == event)
            .map(|(_, _, fields)| fields.clone())
            .collect()
    }
}

impl BrowserManagerLogger for FakeLogger {
    fn debug(&self, event: &str, fields: Option<&SanitizedValue>) {
        let fields = fields.map(to_json).unwrap_or(serde_json::Value::Null);
        self.entries
            .lock()
            .unwrap()
            .push((LogLevel::Debug, event.to_string(), fields));
    }

    fn info(&self, event: &str, fields: Option<&SanitizedValue>) {
        let fields = fields.map(to_json).unwrap_or(serde_json::Value::Null);
        self.entries
            .lock()
            .unwrap()
            .push((LogLevel::Info, event.to_string(), fields));
    }

    fn warn(&self, event: &str, fields: Option<&SanitizedValue>) {
        let _ = (event, fields);
    }

    fn error(&self, event: &str, fields: Option<&SanitizedValue>) {
        let _ = (event, fields);
    }
}

// 原測試：目標畫質不存在時選最低可用畫質
#[test]
fn choose_quality_selects_lowest_available_quality_when_requested_is_missing() {
    assert_eq!(
        choose_quality(
            &["Auto".to_string(), "720p".to_string(), "360p".to_string()],
            StreamQuality::P160
        ),
        Some(StreamQuality::P360)
    );
    assert_eq!(
        choose_quality(
            &["Auto".to_string(), "160p".to_string(), "360p".to_string()],
            StreamQuality::P360
        ),
        Some(StreamQuality::P360)
    );
    assert_eq!(
        choose_quality(
            &["Auto".to_string(), "Source".to_string()],
            StreamQuality::P160
        ),
        None
    );
}

// 原測試：靜音並選擇目標畫質
#[tokio::test]
async fn mutes_video_and_selects_target_quality() {
    let page = create_page(FakePageInput {
        video_evaluate: Arc::new(|_| json!({"muted": true})),
        quality_labels: vec!["Auto", "160p", "360p"],
        quality_option_indexes_clicked: None,
        video_count: 1,
    });
    let logger = fake_logger();

    let result = optimize(
        &page,
        "channel",
        config(true, StreamQuality::P160),
        logger.as_ref(),
    )
    .await;

    assert_eq!(
        result,
        PlaybackOptimizationResult {
            muted: true,
            selected_quality: Some(StreamQuality::P160),
        }
    );
    assert_eq!(*page.video_evaluate_calls.lock().unwrap(), 1);
    assert_eq!(*page.settings_clicks.lock().unwrap(), 1);
    assert_eq!(*page.quality_menu_clicks.lock().unwrap(), 1);
    assert_eq!(*page.quality_option_clicks.lock().unwrap(), 1);
    let fields = &logger.fields_of(LogLevel::Info, "stream_playback_optimized")[0];
    assert_eq!(fields["selectedQuality"], "160p");
}

// 原測試：選畫質時忽略 Picture-in-picture 選項並停用 video PiP
#[tokio::test]
async fn ignores_picture_in_picture_quality_and_disables_video_pip() {
    let video_state = Arc::new(Mutex::new(json!({
        "muted": false,
        "volume": 1.0,
    })));
    let state_for_evaluate = Arc::clone(&video_state);
    let clicked_indexes = Arc::new(Mutex::new(Vec::new()));
    let page = create_page(FakePageInput {
        video_evaluate: Arc::new(move |should_mute| {
            let should_mute = should_mute
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
            let mut state = state_for_evaluate.lock().unwrap();
            if should_mute {
                state["muted"] = json!(true);
                state["volume"] = json!(0.0);
                state["disablePictureInPicture"] = json!(true);
            }
            let muted =
                should_mute && state["muted"] == json!(true) && state["volume"] == json!(0.0);
            json!({"muted": muted})
        }),
        quality_labels: vec!["Picture-in-picture", "Auto", "160p", "360p"],
        quality_option_indexes_clicked: Some(Arc::clone(&clicked_indexes)),
        video_count: 1,
    });

    let result = optimize(
        &page,
        "channel",
        config(true, StreamQuality::P160),
        &no_logger(),
    )
    .await;

    assert_eq!(
        result,
        PlaybackOptimizationResult {
            muted: true,
            selected_quality: Some(StreamQuality::P160),
        }
    );
    assert_eq!(
        *video_state.lock().unwrap(),
        json!({
            "muted": true,
            "volume": 0.0,
            "disablePictureInPicture": true,
        })
    );
    assert_eq!(*clicked_indexes.lock().unwrap(), vec![2]);
}

// 原測試：auto 畫質只靜音，不開啟設定選單
#[tokio::test]
async fn auto_quality_only_mutes_and_does_not_open_settings_menu() {
    let page = create_page(FakePageInput {
        video_evaluate: Arc::new(|_| json!({"muted": true})),
        quality_labels: vec![],
        quality_option_indexes_clicked: None,
        video_count: 1,
    });

    let result = optimize(
        &page,
        "channel",
        config(true, StreamQuality::Auto),
        &no_logger(),
    )
    .await;

    assert_eq!(
        result,
        PlaybackOptimizationResult {
            muted: true,
            selected_quality: None,
        }
    );
    assert_eq!(*page.settings_clicks.lock().unwrap(), 0);
}

// 原測試：播放器元素不存在時立即略過，不觸發 Playwright 等待
#[tokio::test]
async fn skips_immediately_when_player_element_is_missing() {
    let page = create_page(FakePageInput {
        video_evaluate: Arc::new(|_| json!({"muted": true})),
        quality_labels: vec![],
        quality_option_indexes_clicked: None,
        video_count: 0,
    });

    let result = optimize(
        &page,
        "channel",
        config(true, StreamQuality::Auto),
        &no_logger(),
    )
    .await;

    assert_eq!(
        result,
        PlaybackOptimizationResult {
            muted: false,
            selected_quality: None,
        }
    );
    assert_eq!(*page.video_evaluate_calls.lock().unwrap(), 0);
    assert_eq!(*page.settings_clicks.lock().unwrap(), 0);
}

// 原測試：目前解析度符合目標時略過設定選單
#[tokio::test]
async fn skips_settings_menu_when_current_resolution_matches_target() {
    let page = create_page(FakePageInput {
        video_evaluate: Arc::new(|_| json!({"muted": true, "videoHeight": 360})),
        quality_labels: vec![],
        quality_option_indexes_clicked: None,
        video_count: 1,
    });
    let logger = fake_logger();

    let result = optimize(
        &page,
        "channel",
        config(true, StreamQuality::P360),
        logger.as_ref(),
    )
    .await;

    assert_eq!(
        result,
        PlaybackOptimizationResult {
            muted: true,
            selected_quality: Some(StreamQuality::P360),
        }
    );
    assert_eq!(*page.settings_clicks.lock().unwrap(), 0);
    let fields = &logger.fields_of(LogLevel::Debug, "stream_quality_already_selected")[0];
    assert_eq!(fields["videoHeight"], 360);
}

fn fake_logger() -> Arc<FakeLogger> {
    Arc::new(FakeLogger {
        entries: Mutex::new(Vec::new()),
    })
}

fn no_logger() -> FakeLogger {
    FakeLogger {
        entries: Mutex::new(Vec::new()),
    }
}
