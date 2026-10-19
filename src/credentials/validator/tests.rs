//! 憑證驗證器的測試：移植舊版 `test/unit/credential.test.ts`（9 個 `it` 加
//! 1 個 `it.each` 的 3 個案例；標題與 `doc/spec/ts-test-titles.tsv` 逐字相同）。
//!
//! 讀檔一律以記憶體 buffer 的 [`CredentialReadFile`] 注入（fixture 內容以
//! `include_str!` 於編譯期嵌入），測試不碰真實檔案系統；未知路徑由 fake 回傳
//! `ErrorKind::NotFound`，對應舊版預設讀檔的 `ENOENT`。

use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex};

use super::{
    CredentialLogger, CredentialReadFile, CredentialValidationResult, FileCredentialValidator,
    FileCredentialValidatorOptions,
};
use crate::config::model::{
    AppConfig, BrowserConfig, BrowserEngine, BrowserRecoveryConfig, BrowserSessionStartConfig,
    DiscordConfig, ResourceGuardConfig, StreamQuality, TelegramConfig, TwitchApiConfig,
};
use crate::config::thresholds::{
    ResourceGuardAnchorThresholds, compute_effective_resource_guard_thresholds,
};
use crate::credentials::errors::CredentialValidationError;
use crate::logging::logger::{LogEvents, LogLevel};
use crate::logging::redact::SanitizedValue;

/// 舊測試的 `ACCESS_TOKEN_SECRET`。
const ACCESS_TOKEN_SECRET: &str = "fixture-access-token-must-not-leak";
/// 舊測試的 `COOKIE_SECRET`。
const COOKIE_SECRET: &str = "fixture-cookie-value-must-not-leak";

/// fixture 內容以 `include_str!` 編譯期嵌入（`testdata/fixtures/credentials/`
/// 照舊版 `test/fixtures/credentials/` 的內容）。
const EMPTY_COOKIES_STORAGE_STATE: &str =
    include_str!("../../../testdata/fixtures/credentials/empty-cookies-storage-state.json");
const INVALID_COOKIES_STORAGE_STATE: &str =
    include_str!("../../../testdata/fixtures/credentials/invalid-cookies-storage-state.json");
const INVALID_JSON_STORAGE_STATE: &str =
    include_str!("../../../testdata/fixtures/credentials/invalid-json-storage-state.json");
const INVALID_ORIGINS_STORAGE_STATE: &str =
    include_str!("../../../testdata/fixtures/credentials/invalid-origins-storage-state.json");
const INVALID_TOP_LEVEL_STORAGE_STATE: &str =
    include_str!("../../../testdata/fixtures/credentials/invalid-top-level-storage-state.json");
const VALID_STORAGE_STATE: &str =
    include_str!("../../../testdata/fixtures/credentials/valid-storage-state.json");

/// 舊測試的 `fixturePath`（路徑只是 fake 讀檔的鍵，不碰真實檔案系統）。
fn fixture_path(name: &str) -> String {
    format!(
        "{}/testdata/fixtures/credentials/{name}",
        env!("CARGO_MANIFEST_DIR")
    )
}

/// 依 fixture 名稱回傳編譯期嵌入的內容（對應舊測試讀取 fixture 檔）。
fn fixture_content(name: &str) -> &'static str {
    match name {
        "empty-cookies-storage-state.json" => EMPTY_COOKIES_STORAGE_STATE,
        "invalid-cookies-storage-state.json" => INVALID_COOKIES_STORAGE_STATE,
        "invalid-json-storage-state.json" => INVALID_JSON_STORAGE_STATE,
        "invalid-origins-storage-state.json" => INVALID_ORIGINS_STORAGE_STATE,
        "invalid-top-level-storage-state.json" => INVALID_TOP_LEVEL_STORAGE_STATE,
        "valid-storage-state.json" => VALID_STORAGE_STATE,
        other => panic!("unknown fixture {other}"),
    }
}

/// 建立以 `make_result` 決定回傳值的讀檔器與其呼叫記錄
/// （對應舊測試的 `vi.fn<CredentialReadFile>()` mock；encoding 固定 UTF-8）。
fn mock_read_file<F>(make_result: F) -> (CredentialReadFile, Arc<Mutex<Vec<String>>>)
where
    F: Fn(&str) -> io::Result<String> + Send + Sync + 'static,
{
    let calls = Arc::new(Mutex::new(Vec::new()));
    let calls_writer = Arc::clone(&calls);
    let read_file: CredentialReadFile = Box::new(move |path: &str| {
        calls_writer
            .lock()
            .expect("calls lock")
            .push(path.to_string());
        let result = make_result(path);
        Box::pin(async move { result })
    });
    (read_file, calls)
}

/// 建立掛載 fixture 內容的記憶體 buffer 讀檔器；未知路徑回傳
/// `ErrorKind::NotFound`（對應真實檔案系統的 `ENOENT`）。
fn fixture_read_file(files: &[(&str, &str)]) -> (CredentialReadFile, Arc<Mutex<Vec<String>>>) {
    let files: HashMap<String, String> = files
        .iter()
        .map(|(path, content)| ((*path).to_string(), (*content).to_string()))
        .collect();
    mock_read_file(move |path| match files.get(path) {
        Some(content) => Ok(content.clone()),
        None => Err(io::Error::from(io::ErrorKind::NotFound)),
    })
}

/// 舊測試的 `createLogger()`：收集 info／warn 呼叫，欄位照消毒前的
/// [`SanitizedValue`] 序列化成 JSON（與 config 測試的記法相同）。
struct RecordingLogger {
    info_calls: Arc<Mutex<Vec<serde_json::Value>>>,
    warn_calls: Arc<Mutex<Vec<serde_json::Value>>>,
}

impl RecordingLogger {
    fn push(calls: &Mutex<Vec<serde_json::Value>>, event: &str, fields: Option<&SanitizedValue>) {
        let fields_json = fields.map_or_else(String::new, SanitizedValue::to_json_string);
        let fields =
            serde_json::from_str::<serde_json::Value>(&fields_json).expect("欄位 JSON 應可解析");
        calls
            .lock()
            .expect("calls lock")
            .push(serde_json::json!({ "event": event, "fields": fields }));
    }
}

impl CredentialLogger for RecordingLogger {
    fn info(&self, event: &str, fields: Option<&SanitizedValue>) {
        Self::push(&self.info_calls, event, fields);
    }

    fn warn(&self, event: &str, fields: Option<&SanitizedValue>) {
        Self::push(&self.warn_calls, event, fields);
    }
}

/// 舊測試 logger 的呼叫記錄緩衝區（`{"event", "fields"}` 陣列）。
type RecordedCalls = Arc<Mutex<Vec<serde_json::Value>>>;

/// 建立收集用的 logger 與其 info／warn 記錄緩衝區。
fn recording_logger() -> (Arc<dyn CredentialLogger>, RecordedCalls, RecordedCalls) {
    let info_calls = Arc::new(Mutex::new(Vec::new()));
    let warn_calls = Arc::new(Mutex::new(Vec::new()));
    let logger = Arc::new(RecordingLogger {
        info_calls: Arc::clone(&info_calls),
        warn_calls: Arc::clone(&warn_calls),
    });
    (logger, info_calls, warn_calls)
}

/// 建立驗證器（對應舊測試的 `new FileCredentialValidator({ logger, readFile })`）。
fn validator(
    logger: Option<Arc<dyn CredentialLogger>>,
    read_file: Option<CredentialReadFile>,
) -> FileCredentialValidator {
    FileCredentialValidator::new(FileCredentialValidatorOptions { logger, read_file })
}

/// 舊測試的 `createConfig`：以 loader 預設值填滿 [`AppConfig`]（Rust 版是完整
/// 形狀，驗證器只讀 `storage_state_path` 與 `twitch_api`，其餘欄位不影響行為），
/// 再套用舊測試明確指定的欄位。
fn create_config(storage_state_path: &str, access_token: &str) -> AppConfig {
    // loader 預設的 resource guard 錨點（`max_concurrent_streams` 縮放為 1）。
    let effective = compute_effective_resource_guard_thresholds(
        &ResourceGuardAnchorThresholds {
            scale_with_streams: true,
            baseline_streams: 3.0,
            base_memory_mib: 512.0,
            warning_memory_mib: 4_096.0,
            warning_reset_memory_mib: 3_840.0,
            browser_recycle_memory_mib: 4_608.0,
            emergency_memory_mib: 5_376.0,
            post_recycle_target_memory_mib: 4_096.0,
        },
        1.0,
    );
    AppConfig {
        channels: vec!["streamer_one".to_string()],
        check_interval_seconds: 60.0,
        max_concurrent_streams: 1.0,
        headless: true,
        storage_state_path: storage_state_path.to_string(),
        log_level: LogLevel::Info,
        twitch_api: TwitchApiConfig {
            client_id: "fixture-client-id".to_string(),
            access_token: access_token.to_string(),
            client_secret: String::new(),
        },
        browser: BrowserConfig {
            engine: BrowserEngine::Chromium,
            navigation_timeout_ms: 30_000.0,
            page_health_check_interval_seconds: 30.0,
            reward_check_interval_seconds: 15.0,
            page_refresh_interval_seconds: 0.0,
            restart_on_crash: true,
            stream_quality: StreamQuality::P160,
            enforce_stream_quality_seconds: 120.0,
            viewport_width: 1_280.0,
            viewport_height: 720.0,
            mute_audio: true,
            block_images: false,
            block_fonts: false,
            block_known_tracking: false,
            disable_chat: true,
            resource_telemetry_interval_seconds: 60.0,
            recovery: BrowserRecoveryConfig {
                page_crash_backoff_seconds: vec![30.0, 60.0, 120.0],
                channel_crash_window_seconds: 600.0,
                channel_quarantine_threshold: 4.0,
                channel_quarantine_seconds: 900.0,
                stable_reset_seconds: 1_800.0,
                multi_channel_crash_window_seconds: 15.0,
                multi_channel_crash_threshold: 2.0,
                browser_failure_window_seconds: 600.0,
                browser_failure_container_threshold: 3.0,
            },
            session_start: BrowserSessionStartConfig {
                failure_backoff_seconds: vec![30.0, 60.0, 120.0],
                maximum_cooldown_seconds: 900.0,
            },
            resource_guard: ResourceGuardConfig {
                enabled: true,
                sample_interval_seconds: 2.0,
                startup_rate_grace_seconds: 120.0,
                scale_with_streams: true,
                baseline_streams: 3.0,
                base_memory_mib: 512.0,
                warning_memory_mib: 4_096.0,
                warning_reset_memory_mib: 3_840.0,
                browser_recycle_memory_mib: 4_608.0,
                browser_recycle_consecutive_samples: 2.0,
                emergency_memory_mib: 5_376.0,
                emergency_swap_mib: 768.0,
                fast_growth_mib: 1_024.0,
                fast_growth_window_seconds: 30.0,
                post_browser_restart_rate_grace_seconds: 120.0,
                post_recycle_observation_seconds: 20.0,
                post_recycle_target_memory_mib: 4_096.0,
                post_recycle_minimum_drop_mib: 512.0,
                effective,
            },
        },
        telegram: TelegramConfig {
            enabled: false,
            bot_token: String::new(),
            allowed_chat_ids: Vec::new(),
            polling_timeout_seconds: 25.0,
        },
        discord: DiscordConfig {
            enabled: false,
            bot_token: String::new(),
            application_id: String::new(),
            guild_id: String::new(),
            allowed_channel_ids: Vec::new(),
            allow_direct_messages: false,
            allowed_user_ids: Vec::new(),
        },
    }
}

/// 舊測試的 `captureError`：斷言 validate 失敗並回傳錯誤。
fn capture_error(
    result: Result<CredentialValidationResult, CredentialValidationError>,
) -> CredentialValidationError {
    result.expect_err("預期 validate 拋出錯誤")
}

/// 舊測試 `JSON.stringify(result)` 的對應（欄位名照舊版物件）。
fn result_to_json(result: &CredentialValidationResult) -> serde_json::Value {
    serde_json::json!({
        "storageStatePath": result.storage_state_path,
        "hasCookies": result.has_cookies,
        "twitchApiConfigured": result.twitch_api_configured,
    })
}

// 原測試：找不到 storageState 時拋出可行動的專用錯誤
#[tokio::test]
async fn storage_state_not_found_reports_actionable_error() {
    let path = fixture_path("does-not-exist.json");
    // 舊測試用預設讀檔（真實檔案系統上不存在）；Rust 版以空的記憶體 buffer
    // 對應（fake 對未知路徑回傳 `ErrorKind::NotFound`）。
    let (read_file, _) = fixture_read_file(&[]);
    let validator = validator(None, Some(read_file));

    let error = capture_error(
        validator
            .validate(&create_config(&path, ACCESS_TOKEN_SECRET))
            .await,
    );

    assert!(matches!(error, CredentialValidationError::NotFound(_)));
    assert_eq!(error.name(), "StorageStateNotFoundError");
    assert_eq!(error.storage_state_path(), path);
    assert!(error.message().contains("找不到登入狀態檔案"));
    assert!(
        error
            .message()
            .contains("請重新匯出 Playwright storageState 並掛載到容器")
    );
}

// 原測試：readFile 失敗時穩定區分不可讀，不依賴檔案權限或 root 身分
#[tokio::test]
async fn unreadable_error_is_stable_without_root_or_permissions() {
    let path = "/mounted/storage-state.json".to_string();
    let (read_file, calls) = mock_read_file(move |_| {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "permission denied",
        ))
    });
    let validator = validator(None, Some(read_file));

    let error = capture_error(
        validator
            .validate(&create_config(&path, ACCESS_TOKEN_SECRET))
            .await,
    );

    // 舊測試斷言 readFile 以 `(path, 'utf8')` 呼叫；Rust 版 encoding 固定 UTF-8。
    assert_eq!(*calls.lock().expect("calls lock"), vec![path.clone()]);
    assert!(matches!(error, CredentialValidationError::Unreadable(_)));
    assert_eq!(error.name(), "StorageStateUnreadableError");
    assert_eq!(error.storage_state_path(), path);
    assert!(error.message().contains("無法讀取登入狀態檔案"));
    assert!(error.message().contains("具有讀取權限"));
    assert!(!error.message().contains("permission denied"));
}

// 原測試：JSON 語法錯誤時拋出安全且可行動的專用錯誤
#[tokio::test]
async fn invalid_json_reports_safe_actionable_error() {
    let path = fixture_path("invalid-json-storage-state.json");
    let (read_file, _) = fixture_read_file(&[(&path, INVALID_JSON_STORAGE_STATE)]);
    let validator = validator(None, Some(read_file));

    let error = capture_error(
        validator
            .validate(&create_config(&path, ACCESS_TOKEN_SECRET))
            .await,
    );

    assert!(matches!(error, CredentialValidationError::Parse(_)));
    assert_eq!(error.name(), "StorageStateParseError");
    assert_eq!(error.storage_state_path(), path);
    assert!(error.message().contains("登入狀態檔案格式錯誤"));
    assert!(
        error
            .message()
            .contains("請重新匯出有效的 Playwright storageState JSON")
    );
    assert!(!error.message().contains(COOKIE_SECRET));
    assert!(!error.message().contains(ACCESS_TOKEN_SECRET));
}

// 原測試：%s 時拋出格式錯誤
#[tokio::test]
async fn invalid_shape_reports_format_error() {
    // `it.each` 的案例表照抄（`%s` 對應案例名稱）。
    for (name, fixture, message) in [
        (
            "頂層不是物件",
            "invalid-top-level-storage-state.json",
            "JSON 頂層必須是物件",
        ),
        (
            "cookies 不是陣列",
            "invalid-cookies-storage-state.json",
            "cookies 若存在必須是陣列",
        ),
        (
            "origins 不是陣列",
            "invalid-origins-storage-state.json",
            "origins 若存在必須是陣列",
        ),
    ] {
        let path = fixture_path(fixture);
        let (read_file, _) = fixture_read_file(&[(&path, fixture_content(fixture))]);
        let validator = validator(None, Some(read_file));

        let error = capture_error(
            validator
                .validate(&create_config(&path, ACCESS_TOKEN_SECRET))
                .await,
        );

        assert!(
            matches!(error, CredentialValidationError::Format(_)),
            "案例「{name}」應拋出 StorageStateFormatError"
        );
        assert!(
            error.message().contains(message),
            "案例「{name}」的訊息應含「{message}」"
        );
        assert!(
            error
                .message()
                .contains("請重新匯出有效的 Playwright storageState JSON")
        );
        assert!(
            !error.message().contains(COOKIE_SECRET),
            "案例「{name}」的訊息不得含 cookie value"
        );
        assert!(
            !error.message().contains(ACCESS_TOKEN_SECRET),
            "案例「{name}」的訊息不得含 access token"
        );
    }
}

// 原測試：合法 storageState 回傳安全摘要並記錄 credential_checked
#[tokio::test]
async fn valid_storage_state_returns_safe_summary_and_logs_credential_checked() {
    let (logger, info_calls, warn_calls) = recording_logger();
    let path = fixture_path("valid-storage-state.json");
    let (read_file, _) = fixture_read_file(&[(&path, VALID_STORAGE_STATE)]);
    let validator = validator(Some(logger), Some(read_file));

    let result = validator
        .validate(&create_config(&path, ACCESS_TOKEN_SECRET))
        .await
        .expect("合法 storageState 應驗證成功");

    assert_eq!(
        result,
        CredentialValidationResult {
            storage_state_path: path.clone(),
            has_cookies: true,
            twitch_api_configured: true,
        }
    );
    // 舊測試的 `Object.isFrozen(result)`：Rust 的所有權模型保證不可變，無對應斷言。
    assert!(warn_calls.lock().expect("warn lock").is_empty());
    assert_eq!(
        *info_calls.lock().expect("info lock"),
        vec![serde_json::json!({
            "event": LogEvents::CREDENTIAL_CHECKED,
            "fields": {
                "storageStatePath": path,
                "hasCookies": true,
                "twitchApiConfigured": true,
            },
        })]
    );
}

// 原測試：cookies 為空時警告但允許啟動
#[tokio::test]
async fn empty_cookies_warns_but_allows_startup() {
    let (logger, info_calls, warn_calls) = recording_logger();
    let path = fixture_path("empty-cookies-storage-state.json");
    let (read_file, _) = fixture_read_file(&[(&path, EMPTY_COOKIES_STORAGE_STATE)]);
    let validator = validator(Some(logger), Some(read_file));

    let result = validator
        .validate(&create_config(&path, ACCESS_TOKEN_SECRET))
        .await
        .expect("cookies 為空仍應驗證成功");

    assert_eq!(
        result,
        CredentialValidationResult {
            storage_state_path: path.clone(),
            has_cookies: false,
            twitch_api_configured: true,
        }
    );
    assert_eq!(
        *warn_calls.lock().expect("warn lock"),
        vec![serde_json::json!({
            "event": "credential_storage_state_empty_cookies",
            "fields": {
                "storageStatePath": path,
                "hasCookies": false,
                "twitchApiConfigured": true,
            },
        })]
    );
    assert_eq!(
        *info_calls.lock().expect("info lock"),
        vec![serde_json::json!({
            "event": LogEvents::CREDENTIAL_CHECKED,
            "fields": {
                "storageStatePath": path,
                "hasCookies": false,
                "twitchApiConfigured": true,
            },
        })]
    );
}

// 原測試：cookies 與 origins 可缺省，並將缺少 cookies 視為空值警告
#[tokio::test]
async fn missing_cookies_and_origins_warn_as_empty() {
    let (logger, info_calls, warn_calls) = recording_logger();
    let (read_file, _) = mock_read_file(|_| Ok("{}".to_string()));
    let validator = validator(Some(logger), Some(read_file));

    let result = validator
        .validate(&create_config(
            "/mounted/storage-state.json",
            ACCESS_TOKEN_SECRET,
        ))
        .await
        .expect("缺少 cookies 與 origins 仍應驗證成功");

    assert!(!result.has_cookies);
    assert_eq!(warn_calls.lock().expect("warn lock").len(), 1);
    assert_eq!(info_calls.lock().expect("info lock").len(), 1);
}

// 原測試：API 設定摘要只回報是否已設定，不驗證 token 有效性
#[tokio::test]
async fn twitch_api_summary_reports_configuration_only() {
    let (read_file, _) = mock_read_file(|_| Ok(r#"{"cookies":[],"origins":[]}"#.to_string()));
    let validator = validator(None, Some(read_file));

    let configured = validator
        .validate(&create_config("/state.json", "unknown-but-present-token"))
        .await
        .expect("應驗證成功");
    let mut missing = create_config("/state.json", ACCESS_TOKEN_SECRET);
    missing.twitch_api = TwitchApiConfig {
        client_id: "fixture-client-id".to_string(),
        access_token: String::new(),
        client_secret: String::new(),
    };
    let missing = validator.validate(&missing).await.expect("應驗證成功");
    let mut secret_only = create_config("/state.json", ACCESS_TOKEN_SECRET);
    secret_only.twitch_api = TwitchApiConfig {
        client_id: "fixture-client-id".to_string(),
        access_token: String::new(),
        client_secret: "fixture-client-secret".to_string(),
    };
    let secret_only = validator.validate(&secret_only).await.expect("應驗證成功");

    assert!(configured.twitch_api_configured);
    assert!(secret_only.twitch_api_configured);
    assert!(!missing.twitch_api_configured);
}

// 原測試：錯誤、結果與所有日誌皆不包含 cookie value 或 access token
#[tokio::test]
async fn results_and_logs_exclude_cookie_values_and_access_tokens() {
    let (logger, info_calls, warn_calls) = recording_logger();
    let content = serde_json::json!({
        "cookies": [
            {
                "name": "auth-token",
                "value": COOKIE_SECRET,
            },
        ],
        "origins": [],
    })
    .to_string();
    let (read_file, _) = mock_read_file(move |_| Ok(content.clone()));
    let validator = validator(Some(logger), Some(read_file));

    let result = validator
        .validate(&create_config(
            "/mounted/storage-state.json",
            ACCESS_TOKEN_SECRET,
        ))
        .await
        .expect("應驗證成功");

    let serialized_output = serde_json::json!({
        "result": result_to_json(&result),
        "info": *info_calls.lock().expect("info lock"),
        "warn": *warn_calls.lock().expect("warn lock"),
    })
    .to_string();

    assert!(!serialized_output.contains(COOKIE_SECRET));
    assert!(!serialized_output.contains(ACCESS_TOKEN_SECRET));
    assert!(!serialized_output.contains("auth-token"));
}

// 原測試：底層 readFile 錯誤即使含敏感資料也不會進入公開錯誤
#[tokio::test]
async fn underlying_read_file_error_secrets_stay_private() {
    let message = format!("cookie={COOKIE_SECRET} access_token={ACCESS_TOKEN_SECRET}");
    let (read_file, _) = mock_read_file(move |_| {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            message.clone(),
        ))
    });
    let validator = validator(None, Some(read_file));

    let error = capture_error(
        validator
            .validate(&create_config(
                "/mounted/storage-state.json",
                ACCESS_TOKEN_SECRET,
            ))
            .await,
    );

    // 舊測試的 `JSON.stringify(error)` 只序列化 own enumerable 屬性
    // （`name`、`storageStatePath`）；這裡把 message 一併序列化（更嚴格）。
    let serialized_error = serde_json::json!({
        "name": error.name(),
        "storageStatePath": error.storage_state_path(),
        "message": error.message(),
    })
    .to_string();

    assert!(!error.message().contains(COOKIE_SECRET));
    assert!(!error.message().contains(ACCESS_TOKEN_SECRET));
    assert!(!serialized_error.contains(COOKIE_SECRET));
    assert!(!serialized_error.contains(ACCESS_TOKEN_SECRET));
}
