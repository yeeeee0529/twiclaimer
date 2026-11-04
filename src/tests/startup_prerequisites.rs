//! `test/integration/startup-prerequisites.test.ts` 的 2 個舊測試移植（S3.6）：
//! `YamlConfigLoader`＋`FileCredentialValidator` 以暫存檔案串接（真實 fs，
//! 不需 Chromium）。
//!
//! 舊測試的 helper（`createFixedClock`／`createRecordingLogger`）以
//! `JsonLineLogger` 的注入接縫對應；暫存目錄以 [`tempfile`] 建立。
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time};

use crate::config::loader::{ConfigLogger, ProcessEnv, YamlConfigLoader};
use crate::credentials::errors::CredentialValidationError;
use crate::credentials::validator::{
    CredentialLogger, CredentialValidationResult, FileCredentialValidator,
    FileCredentialValidatorOptions,
};
use crate::logging::logger::{
    JsonLineLogger, JsonLineLoggerOptions, LogClock, LogLevel, LogWriter,
};

/// 舊測試常數。
const ACCESS_TOKEN: &str = "startup-token-must-remain-secret";
const COOKIE_VALUE: &str = "startup-cookie-must-remain-secret";

// ---------------------------------------------------------------------------
// 舊測試 helper 的對應（createFixedClock／createRecordingLogger）
// ---------------------------------------------------------------------------

/// 對應舊 `createFixedClock`：回傳固定時間的時鐘。
struct FixedClock;

impl LogClock for FixedClock {
    fn now(&self) -> OffsetDateTime {
        let date = Date::from_calendar_date(2026, Month::June, 14).expect("固定的測試日期應有效");
        let time = Time::from_hms_milli(12, 0, 0, 0).expect("固定的測試時間應有效");
        PrimitiveDateTime::new(date, time).assume_utc()
    }
}

/// 舊 `RecordingLogger` 的收集輸出（`lines` 陣列）。
struct BufferWriter {
    lines: Arc<Mutex<Vec<String>>>,
    flushed: AtomicBool,
}

impl LogWriter for BufferWriter {
    fn write_line(&self, line: &str) {
        self.lines
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(line.to_string());
    }

    fn flush(&self) -> std::io::Result<()> {
        self.flushed.store(true, Ordering::SeqCst);
        Ok(())
    }
}

/// 對應舊測試的 `RecordingLogger`：logger 本體（含 clock）＋ `records()`／
/// `serialized()` 的觀測面。
struct RecordingLogger {
    logger: Arc<JsonLineLogger>,
    lines: Arc<Mutex<Vec<String>>>,
}

impl RecordingLogger {
    fn new() -> Self {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let logger = Arc::new(JsonLineLogger::new(JsonLineLoggerOptions {
            level: Some(LogLevel::Debug),
            output: Some(Box::new(BufferWriter {
                lines: Arc::clone(&lines),
                flushed: AtomicBool::new(false),
            })),
            clock: Some(Box::new(FixedClock)),
        }));
        Self { logger, lines }
    }

    fn as_config_logger(&self) -> Arc<dyn ConfigLogger> {
        Arc::clone(&self.logger) as Arc<dyn ConfigLogger>
    }

    fn as_credential_logger(&self) -> Arc<dyn CredentialLogger> {
        Arc::clone(&self.logger) as Arc<dyn CredentialLogger>
    }

    /// 舊 `records()`：每行解析的 JSON 物件。
    fn records(&self) -> Vec<serde_json::Value> {
        self.lines
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .map(|line| serde_json::from_str(line).unwrap_or(serde_json::Value::Null))
            .collect()
    }

    /// 舊 `serialized()`：全部行的串接。
    fn serialized(&self) -> String {
        self.lines
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .join("")
    }
}

/// 舊測試的 `startupConfigSource()`（`${TEST_STORAGE_STATE_PATH}` 為 loader 的
/// 環境代換記法）。
fn startup_config_source() -> String {
    r#"channels:
  - first_channel
  - second_channel
check_interval_seconds: 60
max_concurrent_streams: 2
storage_state_path: ${TEST_STORAGE_STATE_PATH}
twitch_api:
  client_id: file-client-id
  access_token: file-access-token
"#
    .to_string()
}

/// 舊測試的暫存目錄（ afterEach 清理；tempfile 的 Drop 等價）。
struct TemporaryDirectory(tempfile::TempDir);

impl TemporaryDirectory {
    fn new() -> Self {
        Self(
            tempfile::Builder::new()
                .prefix("watchdog-startup-test-")
                .tempdir()
                .expect("暫存目錄應可建立"),
        )
    }

    fn path(&self) -> &std::path::Path {
        self.0.path()
    }

    fn write_file(&self, name: &str, content: &str) -> String {
        let path = self.path().join(name);
        std::fs::write(&path, content).expect("暫存檔案應可寫入");
        path.to_string_lossy().into_owned()
    }
}

fn env_with(storage_state_path: &str) -> ProcessEnv {
    ProcessEnv::from([
        (
            "TEST_STORAGE_STATE_PATH".to_string(),
            storage_state_path.to_string(),
        ),
        (
            "TWITCH_CLIENT_ID".to_string(),
            "startup-client-id".to_string(),
        ),
        ("TWITCH_ACCESS_TOKEN".to_string(), ACCESS_TOKEN.to_string()),
    ])
}

// ---------------------------------------------------------------------------
// 測試本體（順序照舊測試檔）
// ---------------------------------------------------------------------------

// 原測試：從暫存設定載入 env 憑證與 storageState，僅記錄安全摘要
#[tokio::test]
async fn loads_env_credentials_and_storage_state_from_temporary_config() {
    let directory = TemporaryDirectory::new();
    let storage_state_path = directory.write_file(
        "storage-state.json",
        r#"{"cookies":[{"name":"auth-token","value":"startup-cookie-must-remain-secret","domain":".twitch.tv","path":"/"}],"origins":[]}"#,
    );
    let config_path = directory.write_file("config.yml", &startup_config_source());
    let recording_logger = RecordingLogger::new();
    let config_loader = YamlConfigLoader::new(recording_logger.as_config_logger());
    let credential_validator = FileCredentialValidator::new(FileCredentialValidatorOptions {
        logger: Some(recording_logger.as_credential_logger()),
        read_file: None,
    });

    let config = config_loader
        .load(&config_path, &env_with(&storage_state_path))
        .await
        .expect("config 載入應成功");
    let credentials = credential_validator
        .validate(&config)
        .await
        .expect("credential 驗證應成功");

    assert_eq!(
        config.channels,
        vec!["first_channel".to_string(), "second_channel".to_string()]
    );
    assert_eq!(config.storage_state_path, storage_state_path);
    assert_eq!(
        credentials,
        CredentialValidationResult {
            storage_state_path: storage_state_path.clone(),
            has_cookies: true,
            twitch_api_configured: true,
        }
    );
    let credential_checked = recording_logger
        .records()
        .into_iter()
        .find(|record| record.get("event") == Some(&serde_json::json!("credential_checked")))
        .expect("需記錄 credential_checked");
    assert_eq!(
        credential_checked.get("storageStatePath"),
        Some(&serde_json::json!("[REDACTED]")),
        "已消毒記錄欄位"
    );
    assert_eq!(
        credential_checked.get("hasCookies"),
        Some(&serde_json::json!("[REDACTED]"))
    );
    assert_eq!(
        credential_checked.get("twitchApiConfigured"),
        Some(&serde_json::json!(true))
    );
    let serialized = recording_logger.serialized();
    assert!(
        !serialized.contains(ACCESS_TOKEN),
        "記錄不得帶 access token"
    );
    assert!(!serialized.contains(COOKIE_VALUE), "記錄不得帶 cookie 值");
}

// 原測試：設定可載入但 storageState 不存在時阻止啟動前置驗證
#[tokio::test]
async fn blocks_startup_prerequisites_when_storage_state_file_is_missing() {
    let directory = TemporaryDirectory::new();
    let storage_state_path = directory
        .path()
        .join("missing-storage-state.json")
        .to_string_lossy()
        .into_owned();
    let config_path = directory.write_file("config.yml", &startup_config_source());
    let recording_logger = RecordingLogger::new();
    let config = YamlConfigLoader::new(recording_logger.as_config_logger())
        .load(&config_path, &env_with(&storage_state_path))
        .await
        .expect("config 載入應成功");

    let outcome = FileCredentialValidator::new(FileCredentialValidatorOptions {
        logger: Some(recording_logger.as_credential_logger()),
        read_file: None,
    })
    .validate(&config)
    .await;

    assert!(
        matches!(&outcome, Err(CredentialValidationError::NotFound(error)) if error.name() == "StorageStateNotFoundError"),
        "需為 StorageStateNotFoundError：{outcome:?}"
    );
    assert!(
        !recording_logger.serialized().contains(ACCESS_TOKEN),
        "記錄不得帶 access token"
    );
}
