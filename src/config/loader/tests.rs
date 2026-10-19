//! 設定載入器的測試：移植舊版 `test/unit/config.test.ts`（24 個 `it` 加 11 個
//! `it.each`），並外加 golden 設定案例（`testdata/golden/config/*.config.yml`，
//! 預期值由舊版實作產生，D11）。三個測試依 X1／X2 調整，處置記在 `doc/test-port.md`。
//!
//! 暫存檔用 `tempfile`（對應舊測試的 `mkdtemp` ＋ afterEach 清理），環境一律以
//! 參數傳入（不碰真實 process env，測試可平行）。深層的預期值以 JSON 字串解析
//! （`json!` 巨集的展開深度超過預設 recursion limit，且 crate 屬性不歸本模組）。

use std::sync::{Arc, Mutex};

use crate::config::errors::ConfigLoadError;
use crate::config::golden::{GOLDEN_DIR, json_eq};
use crate::config::loader::{ConfigLogger, ProcessEnv, YamlConfigLoader};
use crate::config::model::{AppConfig, BrowserEngine};
use crate::logging::logger::LogLevel;
use crate::logging::redact::SanitizedValue;

/// 舊測試的 `fixtureDirectory`（fixture 已複製到 `testdata/fixtures/config/`）。
fn fixture_path(name: &str) -> String {
    format!(
        "{}/testdata/fixtures/config/{name}",
        env!("CARGO_MANIFEST_DIR")
    )
}

/// 把 (鍵, 值) 對照表轉成 [`ProcessEnv`]。
fn env_map(pairs: &[(&str, &str)]) -> ProcessEnv {
    pairs
        .iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

/// 舊測試的 `vi.fn<ConfigLogger['debug']>()`：把每次 debug 呼叫收集成
/// `{"event", "fields"}`（照舊版 golden 產生器的記法，未消毒的原始欄位）。
struct RecordingLogger {
    logs: Arc<Mutex<Vec<serde_json::Value>>>,
}

impl ConfigLogger for RecordingLogger {
    fn debug(&self, event: &str, fields: Option<&SanitizedValue>) {
        let empty = SanitizedValue::Object(vec![]);
        let fields_json = fields.unwrap_or(&empty).to_json_string();
        let fields =
            serde_json::from_str::<serde_json::Value>(&fields_json).expect("欄位 JSON 應可解析");
        self.logs
            .lock()
            .expect("logs lock")
            .push(serde_json::json!({ "event": event, "fields": fields }));
    }
}

/// 建立收集用的 logger 與其記錄緩衝區。
fn recording_logger() -> (Arc<dyn ConfigLogger>, Arc<Mutex<Vec<serde_json::Value>>>) {
    let logs = Arc::new(Mutex::new(Vec::new()));
    let logger = Arc::new(RecordingLogger {
        logs: Arc::clone(&logs),
    });
    (logger, logs)
}

/// 舊測試的 `loadFixture`。
async fn load_fixture(
    name: &str,
    env: &[(&str, &str)],
    logger: Option<Arc<dyn ConfigLogger>>,
) -> Result<AppConfig, ConfigLoadError> {
    load_path(&fixture_path(name), env, logger).await
}

/// 舊測試直接以 `new YamlConfigLoader(logger).load(path, env)` 呼叫的場合。
async fn load_path(
    path: &str,
    env: &[(&str, &str)],
    logger: Option<Arc<dyn ConfigLogger>>,
) -> Result<AppConfig, ConfigLoadError> {
    let env = env_map(env);
    match logger {
        Some(logger) => YamlConfigLoader::new(logger).load(path, &env).await,
        None => YamlConfigLoader::default().load(path, &env).await,
    }
}

/// 舊測試的 `loadSource`：寫入暫存檔（`mkdtemp`）後載入；目錄在 drop 時清理。
async fn load_source(
    source: &str,
    env: &[(&str, &str)],
    logger: Option<Arc<dyn ConfigLogger>>,
) -> Result<AppConfig, ConfigLoadError> {
    let directory = tempfile::Builder::new()
        .prefix("twitch-config-test-")
        .tempdir()
        .expect("應能建立暫存目錄");
    let path = directory.path().join("config.yml");
    std::fs::write(&path, source).expect("應能寫入暫存設定檔");
    load_path(path.to_str().expect("暫存路徑應為 UTF-8"), env, logger).await
}

/// 斷言載入結果是錯誤並回傳（對應舊測試的 `await expect(...).rejects…`）。
fn expect_err(result: Result<AppConfig, ConfigLoadError>) -> ConfigLoadError {
    match result {
        Ok(_) => panic!("應載入失敗"),
        Err(error) => error,
    }
}

/// 斷言錯誤是 `ConfigValidationError`（對應 `rejects.toBeInstanceOf`）。
fn assert_validation_error(error: &ConfigLoadError) {
    assert_eq!(error.name(), "ConfigValidationError", "錯誤：{error}");
}

/// 斷言驗證錯誤的欄位（對應 `rejects.toMatchObject({ name, field })`）。
fn assert_validation_field(error: &ConfigLoadError, field: &str) {
    assert_validation_error(error);
    assert_eq!(error.field(), Some(field), "錯誤：{error}");
}

// ---------------------------------------------------------------------------
// golden 設定案例（一案例一測試；fingerprint 於 S2.6 加入後才比）
// ---------------------------------------------------------------------------

/// 讀 golden 設定案例並比對 `value`、`error`、`logs`；`known-differences.json`
/// 列出的案例（X1／X2）改比對該檔的 `expected`。
async fn run_golden_config_case(name: &str) {
    let base = format!("{GOLDEN_DIR}/{name}");
    let expected: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(format!("{base}.config.expected.json"))
            .unwrap_or_else(|error| panic!("讀不到 {base}.config.expected.json：{error}")),
    )
    .unwrap();
    // known-differences（X1／X2）的案例改比對該檔的 expected。
    let known: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(format!("{GOLDEN_DIR}/known-differences.json"))
            .unwrap_or_else(|error| panic!("讀不到 known-differences.json：{error}")),
    )
    .unwrap();
    let expected = known
        .get(name)
        .and_then(|entry| entry.get("expected"))
        .unwrap_or(&expected);

    let env = match std::fs::read_to_string(format!("{base}.env.json")) {
        Ok(text) => env_from_json(&text),
        Err(_) => ProcessEnv::new(),
    };

    let (logger, logs) = recording_logger();
    let loader = YamlConfigLoader::new(logger);
    let actual = match loader.load(&format!("{base}.config.yml"), &env).await {
        Ok(value) => serde_json::json!({ "value": value.to_json() }),
        Err(error) => serde_json::json!({
            "error": { "name": error.name(), "message": error.message() },
        }),
    };
    let mut actual = actual;
    if let serde_json::Value::Object(entries) = &mut actual {
        entries.insert(
            "logs".to_string(),
            serde_json::Value::Array(logs.lock().expect("logs lock").clone()),
        );
    }

    // 比對 expected 有的每個鍵（fingerprint 於 S2.6 加入後才比）。
    for key in ["value", "error", "logs"] {
        if let Some(expected_part) = expected.get(key) {
            let actual_part = actual.get(key).unwrap_or(&serde_json::Value::Null);
            assert!(
                json_eq(actual_part, expected_part),
                "golden 設定案例不符：{name}（{key}）\n實際：{actual_part}\n預期：{expected_part}"
            );
        }
    }
}

/// 把 `<base>.env.json` 轉成 [`ProcessEnv`]（值一律為字串）。
fn env_from_json(text: &str) -> ProcessEnv {
    let value: serde_json::Value = serde_json::from_str(text).expect("env JSON 應可解析");
    value
        .as_object()
        .expect("env JSON 應是物件")
        .iter()
        .map(|(key, value)| {
            (
                key.clone(),
                value.as_str().expect("env 值應為字串").to_string(),
            )
        })
        .collect()
}

/// golden c01：完整欄位（headless: true、N=2 縮放）。
#[tokio::test]
async fn golden_c01_valid() {
    run_golden_config_case("c01-valid").await;
}

/// golden c02：全預設值，併發數降為頻道數並輸出 debug 事件。
#[tokio::test]
async fn golden_c02_defaults() {
    run_golden_config_case("c02-defaults").await;
}

/// golden c03：`${VAR}` 代換與直接環境覆寫（X2：HEADLESS=TRUE → headless true）。
#[tokio::test]
async fn golden_c03_env() {
    run_golden_config_case("c03-env").await;
}

/// golden c04：YAML 語法錯誤。
#[tokio::test]
async fn golden_c04_parse_error() {
    run_golden_config_case("c04-parse-error").await;
}

/// golden c05：`config.example.yml` 的內容（access_token 空字串、client_secret）。
#[tokio::test]
async fn golden_c05_example() {
    run_golden_config_case("c05-example").await;
}

/// golden c06：環境變數啟用 Telegram 與 Discord（chat／channel ID 去重）。
#[tokio::test]
async fn golden_c06_bots() {
    run_golden_config_case("c06-bots").await;
}

/// golden c07：HEADLESS 不是 true 或 false。
#[tokio::test]
async fn golden_c07_bad_headless() {
    run_golden_config_case("c07-bad-headless").await;
}

/// golden c08：X2 —— 檔案 headless: false 被拒絕（比對 known-differences 的 expected）。
#[tokio::test]
async fn golden_c08_headless_false() {
    run_golden_config_case("c08-headless-false").await;
}

/// golden c09：X1 —— browser.engine: firefox 被拒絕（比對 known-differences 的 expected）。
#[tokio::test]
async fn golden_c09_firefox() {
    run_golden_config_case("c09-firefox").await;
}

// ---------------------------------------------------------------------------
// 移植的舊測試（標題逐字見 `doc/spec/ts-test-titles.tsv`）
// ---------------------------------------------------------------------------

// X1／X2 調整（test-port.md）：fixture valid.yml 改成 headless: true，預期值跟著改。
// 原測試：載入合法 YAML 並將 snake_case 轉為 camelCase
#[tokio::test]
async fn loads_valid_yaml_and_converts_snake_case_to_camel_case() {
    let config = load_fixture("valid.yml", &[], None)
        .await
        .expect("應成功載入 valid.yml");

    assert!(json_eq(&config.to_json(), &valid_yml_expected()));
}

/// `載入合法 YAML 並將 snake_case 轉為 camelCase` 的完整預期值
/// （headless 為 true，依 X2 調整）。
fn valid_yml_expected() -> serde_json::Value {
    serde_json::from_str(
        r#"{
        "channels": ["streamer_one", "streamer_two", "streamer_three"],
        "checkIntervalSeconds": 90,
        "maxConcurrentStreams": 2,
        "headless": true,
        "storageStatePath": "/tmp/test-storage-state.json",
        "logLevel": "debug",
        "twitchApi": {
            "clientId": "fixture-client-id",
            "accessToken": "fixture-access-token",
            "clientSecret": ""
        },
        "browser": {
            "engine": "chromium",
            "navigationTimeoutMs": 45000,
            "pageHealthCheckIntervalSeconds": 40,
            "rewardCheckIntervalSeconds": 20,
            "pageRefreshIntervalSeconds": 900,
            "restartOnCrash": false,
            "streamQuality": "360p",
            "enforceStreamQualitySeconds": 180,
            "viewportWidth": 800,
            "viewportHeight": 450,
            "muteAudio": false,
            "blockImages": false,
            "blockFonts": false,
            "blockKnownTracking": true,
            "disableChat": false,
            "resourceTelemetryIntervalSeconds": 240,
            "recovery": {
                "pageCrashBackoffSeconds": [30, 60, 120],
                "channelCrashWindowSeconds": 600,
                "channelQuarantineThreshold": 4,
                "channelQuarantineSeconds": 900,
                "stableResetSeconds": 1800,
                "multiChannelCrashWindowSeconds": 15,
                "multiChannelCrashThreshold": 2,
                "browserFailureWindowSeconds": 600,
                "browserFailureContainerThreshold": 3
            },
            "sessionStart": {
                "failureBackoffSeconds": [30, 60, 120],
                "maximumCooldownSeconds": 900
            },
            "resourceGuard": {
                "enabled": true,
                "sampleIntervalSeconds": 2,
                "startupRateGraceSeconds": 120,
                "scaleWithStreams": true,
                "baselineStreams": 3,
                "baseMemoryMib": 512,
                "warningMemoryMib": 4096,
                "warningResetMemoryMib": 3840,
                "browserRecycleMemoryMib": 4608,
                "browserRecycleConsecutiveSamples": 2,
                "emergencyMemoryMib": 5376,
                "emergencySwapMib": 768,
                "fastGrowthMib": 1024,
                "fastGrowthWindowSeconds": 30,
                "postBrowserRestartRateGraceSeconds": 120,
                "postRecycleObservationSeconds": 20,
                "postRecycleTargetMemoryMib": 4096,
                "postRecycleMinimumDropMib": 512,
                "effective": {
                    "maxConcurrentStreams": 2,
                    "warningMemoryMib": 2901,
                    "warningResetMemoryMib": 2731,
                    "browserRecycleMemoryMib": 3243,
                    "emergencyMemoryMib": 3755,
                    "postRecycleTargetMemoryMib": 2901
                }
            }
        },
        "telegram": {
            "enabled": false,
            "botToken": "",
            "allowedChatIds": [],
            "pollingTimeoutSeconds": 25
        },
        "discord": {
            "enabled": false,
            "botToken": "",
            "applicationId": "",
            "guildId": "",
            "allowedChannelIds": [],
            "allowDirectMessages": false,
            "allowedUserIds": []
        }    }"#,
    )
    .expect("預期值 JSON 應可解析")
}

// 原測試：config.example.yml 是可直接複製使用的單檔設定範例
#[tokio::test]
async fn config_example_yml_is_a_ready_to_use_single_file_example() {
    let path = format!("{}/config.example.yml", env!("CARGO_MANIFEST_DIR"));
    let config = YamlConfigLoader::default()
        .load(&path, &ProcessEnv::new())
        .await
        .expect("應成功載入 config.example.yml");

    assert_eq!(config.twitch_api.client_id, "your_twitch_client_id");
    assert_eq!(config.twitch_api.client_secret, "your_twitch_client_secret");
    assert_eq!(config.browser.page_refresh_interval_seconds, 0.0);
    assert_eq!(config.browser.engine, BrowserEngine::Chromium);
    assert_eq!(config.browser.resource_telemetry_interval_seconds, 60.0);
    assert_eq!(
        config.browser.recovery.page_crash_backoff_seconds,
        [30.0, 60.0, 120.0]
    );
    assert_eq!(config.browser.session_start.maximum_cooldown_seconds, 900.0);
    assert!(config.browser.resource_guard.enabled);
    assert_eq!(
        config
            .browser
            .resource_guard
            .effective
            .max_concurrent_streams,
        2.0
    );
    assert!(!config.telegram.enabled);
    assert!(!config.discord.enabled);
}

// 原測試：載入自訂的頻道復原與 session 啟動冷卻設定
#[tokio::test]
async fn loads_custom_recovery_and_session_start_settings() {
    let config = load_source(
        "channels: [streamer]\n\
         twitch_api:\n\
         \x20 client_id: fixture-client-id\n\
         \x20 access_token: fixture-access-token\n\
         browser:\n\
         \x20 recovery:\n\
         \x20   page_crash_backoff_seconds: [10, 20, 40]\n\
         \x20   channel_crash_window_seconds: 300\n\
         \x20   channel_quarantine_threshold: 5\n\
         \x20   channel_quarantine_seconds: 1200\n\
         \x20   stable_reset_seconds: 2400\n\
         \x20   multi_channel_crash_window_seconds: 20\n\
         \x20   multi_channel_crash_threshold: 3\n\
         \x20   browser_failure_window_seconds: 900\n\
         \x20   browser_failure_container_threshold: 4\n\
         \x20 session_start:\n\
         \x20   failure_backoff_seconds: [15, 45]\n\
         \x20   maximum_cooldown_seconds: 600\n",
        &[],
        None,
    )
    .await
    .expect("應成功載入");

    assert!(
        json_eq(
            &config.browser.recovery.to_json(),
            &serde_json::json!({
                "pageCrashBackoffSeconds": [10, 20, 40],
                "channelCrashWindowSeconds": 300,
                "channelQuarantineThreshold": 5,
                "channelQuarantineSeconds": 1200,
                "stableResetSeconds": 2400,
                "multiChannelCrashWindowSeconds": 20,
                "multiChannelCrashThreshold": 3,
                "browserFailureWindowSeconds": 900,
                "browserFailureContainerThreshold": 4,
            })
        ),
        "recovery 不符：{}",
        config.browser.recovery.to_json()
    );
    assert!(
        json_eq(
            &config.browser.session_start.to_json(),
            &serde_json::json!({
                "failureBackoffSeconds": [15, 45],
                "maximumCooldownSeconds": 600,
            })
        ),
        "sessionStart 不符：{}",
        config.browser.session_start.to_json()
    );
}

// 與 `doc/spec/ts-test-titles.tsv` 的 config.test.ts:661 同標題（兩個 it.each）；
// 此組為 recovery 與 session_start 的案例表（test-port.md，DEC-010）。
// 原測試：%s 時拒絕設定
#[tokio::test]
async fn rejects_invalid_recovery_and_session_start_settings() {
    let cases: Vec<(&str, &str)> = vec![
        ("recovery 不是物件", "browser:\n  recovery: invalid"),
        (
            "page crash backoff 為空",
            "browser:\n  recovery:\n    page_crash_backoff_seconds: []",
        ),
        (
            "page crash backoff 含非正整數",
            "browser:\n  recovery:\n    page_crash_backoff_seconds: [30, 0]",
        ),
        (
            "quarantine 小於十分鐘",
            "browser:\n  recovery:\n    channel_quarantine_seconds: 599",
        ),
        (
            "quarantine 大於三十分鐘",
            "browser:\n  recovery:\n    channel_quarantine_seconds: 1801",
        ),
        (
            "session_start 不是物件",
            "browser:\n  session_start: invalid",
        ),
        (
            "session start backoff 為空",
            "browser:\n  session_start:\n    failure_backoff_seconds: []",
        ),
        (
            "session start cooldown 不是正整數",
            "browser:\n  session_start:\n    maximum_cooldown_seconds: 0",
        ),
    ];
    for (name, browser_source) in cases {
        let source = format!(
            "{browser_source}\nchannels: [streamer]\n\
             twitch_api:\n\
             \x20 client_id: fixture-client-id\n\
             \x20 access_token: fixture-access-token\n"
        );
        let Err(error) = load_source(&source, &[], None).await else {
            panic!("{name} 應驗證失敗");
        };
        assert_validation_error(&error);
    }
}

// 原測試：resource_guard 門檻依 max_concurrent_streams 縮放（N=3 錨點）
#[tokio::test]
async fn scales_resource_guard_thresholds_by_max_concurrent_streams() {
    let source = format!(
        "{}max_concurrent_streams: 5\n",
        config_with_channels(&["one", "two", "three", "four", "five"])
    );
    let config = load_source(&source, &[], None).await.expect("應成功載入");

    assert!(
        json_eq(
            &config.browser.resource_guard.effective.to_json(),
            &serde_json::json!({
                "maxConcurrentStreams": 5,
                "warningMemoryMib": 6485,
                "warningResetMemoryMib": 6059,
                "browserRecycleMemoryMib": 7339,
                "emergencyMemoryMib": 8619,
                "postRecycleTargetMemoryMib": 6485,
            })
        ),
        "effective 不符：{}",
        config.browser.resource_guard.effective.to_json()
    );
}

// 原測試：resource_guard.scale_with_streams 為 false 時使用絕對門檻
#[tokio::test]
async fn uses_absolute_thresholds_when_scale_with_streams_is_false() {
    let config = load_source(
        "channels: [streamer]\n\
         twitch_api:\n\
         \x20 client_id: fixture-client-id\n\
         \x20 access_token: fixture-access-token\n\
         browser:\n\
         \x20 resource_guard:\n\
         \x20   scale_with_streams: false\n\
         \x20   warning_memory_mib: 5000\n\
         \x20   warning_reset_memory_mib: 4500\n\
         \x20   browser_recycle_memory_mib: 5500\n\
         \x20   emergency_memory_mib: 6000\n\
         \x20   post_recycle_target_memory_mib: 4800\n",
        &[],
        None,
    )
    .await
    .expect("應成功載入");

    assert_eq!(
        config.browser.resource_guard.effective.warning_memory_mib,
        5000.0
    );
    assert_eq!(
        config.browser.resource_guard.effective.emergency_memory_mib,
        6000.0
    );
}

// 原測試：resource_guard 門檻順序錯誤時拒絕設定
#[tokio::test]
async fn rejects_resource_guard_threshold_order_error() {
    let error = expect_err(
        load_source(
            "channels: [streamer]\n\
         twitch_api:\n\
         \x20 client_id: fixture-client-id\n\
         \x20 access_token: fixture-access-token\n\
         browser:\n\
         \x20 resource_guard:\n\
         \x20   warning_memory_mib: 5000\n\
         \x20   browser_recycle_memory_mib: 4000\n\
         \x20   emergency_memory_mib: 6000\n",
            &[],
            None,
        )
        .await,
    );
    assert_validation_error(&error);
}

// 原測試：支援正式 YAML 的引號、註解與 flow sequence
#[tokio::test]
async fn supports_production_yaml_quoting_comments_and_flow_sequences() {
    let config = load_source(
        "\n\
         # flow sequence 與引號內的特殊字元應由正式 YAML parser 處理\n\
         channels: [\"quoted_channel\", 'second_channel'] # 行尾註解\n\
         storage_state_path: \"/tmp/state # 1.json\"\n\
         twitch_api:\n\
         \x20 client_id: 'client: # not a comment'\n\
         \x20 access_token: \"fixture-token\"\n",
        &[],
        None,
    )
    .await
    .expect("應成功載入");

    assert_eq!(config.channels, ["quoted_channel", "second_channel"]);
    assert_eq!(config.storage_state_path, "/tmp/state # 1.json");
    assert_eq!(config.twitch_api.client_id, "client: # not a comment");
}

// 原測試：套用所有預設值並將併發數降為頻道數
#[tokio::test]
async fn applies_all_defaults_and_clamps_concurrency_to_channel_count() {
    let (logger, logs) = recording_logger();
    let config = load_fixture("defaults.yml", &[], Some(logger))
        .await
        .expect("應成功載入 defaults.yml");

    assert!(
        json_eq(&config.to_json(), &defaults_yml_expected()),
        "defaults.yml 的設定不符：\n實際：{}\n預期：{}",
        config.to_json(),
        defaults_yml_expected()
    );
    let logs = logs.lock().expect("logs lock");
    assert_eq!(logs.len(), 1, "恰好一次 debug 事件：{logs:?}");
    assert!(
        json_eq(
            &logs[0],
            &serde_json::json!({
                "event": "config_concurrency_clamped",
                "fields": {
                    "requestedMaxConcurrentStreams": 3,
                    "effectiveMaxConcurrentStreams": 2,
                    "channelCount": 2,
                },
            })
        ),
        "debug 事件不符：{logs:?}"
    );
}

/// `套用所有預設值並將併發數降為頻道數` 的完整預期值（全預設、併發數降為 2）。
fn defaults_yml_expected() -> serde_json::Value {
    serde_json::from_str(
        r#"{
        "channels": ["streamer_one", "streamer_two"],
        "checkIntervalSeconds": 60,
        "maxConcurrentStreams": 2,
        "headless": true,
        "storageStatePath": "/data/browser-state/storage-state.json",
        "logLevel": "info",
        "twitchApi": {
            "clientId": "fixture-client-id",
            "accessToken": "fixture-access-token",
            "clientSecret": ""
        },
        "browser": {
            "engine": "chromium",
            "navigationTimeoutMs": 30000,
            "pageHealthCheckIntervalSeconds": 60,
            "rewardCheckIntervalSeconds": 30,
            "pageRefreshIntervalSeconds": 0,
            "restartOnCrash": true,
            "streamQuality": "160p",
            "enforceStreamQualitySeconds": 120,
            "viewportWidth": 1280,
            "viewportHeight": 720,
            "muteAudio": true,
            "blockImages": false,
            "blockFonts": false,
            "blockKnownTracking": false,
            "disableChat": true,
            "resourceTelemetryIntervalSeconds": 60,
            "recovery": {
                "pageCrashBackoffSeconds": [30, 60, 120],
                "channelCrashWindowSeconds": 600,
                "channelQuarantineThreshold": 4,
                "channelQuarantineSeconds": 900,
                "stableResetSeconds": 1800,
                "multiChannelCrashWindowSeconds": 15,
                "multiChannelCrashThreshold": 2,
                "browserFailureWindowSeconds": 600,
                "browserFailureContainerThreshold": 3
            },
            "sessionStart": {
                "failureBackoffSeconds": [30, 60, 120],
                "maximumCooldownSeconds": 900
            },
            "resourceGuard": {
                "enabled": true,
                "sampleIntervalSeconds": 2,
                "startupRateGraceSeconds": 120,
                "scaleWithStreams": true,
                "baselineStreams": 3,
                "baseMemoryMib": 512,
                "warningMemoryMib": 4096,
                "warningResetMemoryMib": 3840,
                "browserRecycleMemoryMib": 4608,
                "browserRecycleConsecutiveSamples": 2,
                "emergencyMemoryMib": 5376,
                "emergencySwapMib": 768,
                "fastGrowthMib": 1024,
                "fastGrowthWindowSeconds": 30,
                "postBrowserRestartRateGraceSeconds": 120,
                "postRecycleObservationSeconds": 20,
                "postRecycleTargetMemoryMib": 4096,
                "postRecycleMinimumDropMib": 512,
                "effective": {
                    "maxConcurrentStreams": 2,
                    "warningMemoryMib": 2901,
                    "warningResetMemoryMib": 2731,
                    "browserRecycleMemoryMib": 3243,
                    "emergencyMemoryMib": 3755,
                    "postRecycleTargetMemoryMib": 2901
                }
            }
        },
        "telegram": {
            "enabled": false,
            "botToken": "",
            "allowedChatIds": [],
            "pollingTimeoutSeconds": 25
        },
        "discord": {
            "enabled": false,
            "botToken": "",
            "applicationId": "",
            "guildId": "",
            "allowedChannelIds": [],
            "allowDirectMessages": false,
            "allowedUserIds": []
        }    }"#,
    )
    .expect("預期值 JSON 應可解析")
}

// 原測試：頻道數足夠時 max_concurrent_streams 預設為 3
#[tokio::test]
async fn defaults_max_concurrent_streams_to_3_when_channels_are_sufficient() {
    let (logger, logs) = recording_logger();
    let source = config_with_channels(&[
        "streamer_one",
        "streamer_two",
        "streamer_three",
        "streamer_four",
    ]);
    let config = load_source(&source, &[], Some(logger))
        .await
        .expect("應成功載入");

    assert_eq!(config.max_concurrent_streams, 3.0);
    assert!(logs.lock().expect("logs lock").is_empty());
}

// 原測試：未指定 browser.engine 時預設為 chromium
#[tokio::test]
async fn defaults_browser_engine_to_chromium_when_unspecified() {
    let config = load_source(
        "channels: [streamer]\n\
         twitch_api:\n\
         \x20 client_id: fixture-client-id\n\
         \x20 access_token: fixture-access-token\n",
        &[],
        None,
    )
    .await
    .expect("應成功載入");

    assert_eq!(config.browser.engine, BrowserEngine::Chromium);
}

// X1 調整（test-port.md）：Rust 版只支援 chromium，firefox 改為斷言 X1 的錯誤。
// 原測試：明確指定 firefox 仍受支援
#[tokio::test]
async fn explicitly_specified_firefox_is_rejected_by_x1() {
    let error = expect_err(
        load_source(
            "channels: [streamer]\n\
         twitch_api:\n\
         \x20 client_id: fixture-client-id\n\
         \x20 access_token: fixture-access-token\n\
         browser:\n\
         \x20 engine: firefox\n",
            &[],
            None,
        )
        .await,
    );
    assert_validation_field(&error, "browser.engine");
    assert_eq!(error.message(), "設定欄位 browser.engine 必須是 chromium");
}

// 原測試：明確指定 chromium 仍受支援
#[tokio::test]
async fn explicitly_specified_chromium_is_still_supported() {
    let config = load_source(
        "channels: [streamer]\n\
         twitch_api:\n\
         \x20 client_id: fixture-client-id\n\
         \x20 access_token: fixture-access-token\n\
         browser:\n\
         \x20 engine: chromium\n",
        &[],
        None,
    )
    .await
    .expect("應成功載入");

    assert_eq!(config.browser.engine, BrowserEngine::Chromium);
}

// 原測試：顯式併發數大於頻道數時降級並輸出 debug 事件
#[tokio::test]
async fn clamps_concurrency_above_channel_count_and_emits_debug_event() {
    let (logger, logs) = recording_logger();
    let source = format!(
        "{}max_concurrent_streams: 8\n",
        config_with_channels(&["one", "two"])
    );
    let config = load_source(&source, &[], Some(logger))
        .await
        .expect("應成功載入");

    assert_eq!(config.max_concurrent_streams, 2.0);
    let logs = logs.lock().expect("logs lock");
    assert_eq!(logs.len(), 1, "恰好一次 debug 事件：{logs:?}");
    assert!(
        json_eq(
            &logs[0],
            &serde_json::json!({
                "event": "config_concurrency_clamped",
                "fields": {
                    "requestedMaxConcurrentStreams": 8,
                    "effectiveMaxConcurrentStreams": 2,
                    "channelCount": 2,
                },
            })
        ),
        "debug 事件不符：{logs:?}"
    );
}

// X2 調整（test-port.md）：fixture env.yml 改成 headless: false；HEADLESS 由
// 'FALSE' 改用 'TRUE'，預期 headless 為 true。
// 原測試：替換 ${ENV_VAR} 且讓直接環境覆寫優先
#[tokio::test]
async fn replaces_env_vars_and_direct_env_overrides_win() {
    let config_path = fixture_path("env.yml");
    let env = env_map(&[
        ("CONFIG_PATH", config_path.as_str()),
        ("PRIMARY_CHANNEL", "environment_channel"),
        ("FILE_CLIENT_ID", "file-client-id"),
        ("FILE_ACCESS_TOKEN", "file-access-token"),
        ("TWITCH_CLIENT_ID", "override-client-id"),
        ("TWITCH_ACCESS_TOKEN", "override-access-token"),
        ("LOG_LEVEL", "warn"),
        ("HEADLESS", "TRUE"),
    ]);
    let config = YamlConfigLoader::default()
        .load("/missing/config.yml", &env)
        .await
        .expect("應成功載入 env.yml");

    assert_eq!(config.channels, ["environment_channel", "static_channel"]);
    assert!(
        json_eq(
            &config.twitch_api.to_json(),
            &serde_json::json!({
                "clientId": "override-client-id",
                "accessToken": "override-access-token",
                "clientSecret": "",
            })
        ),
        "twitchApi 不符：{}",
        config.twitch_api.to_json()
    );
    assert_eq!(config.log_level, LogLevel::Warn);
    assert!(config.headless);
}

// 原測試：缺少的環境替換值會成為空字串並由欄位驗證拒絕
#[tokio::test]
async fn missing_env_substitution_becomes_empty_string_and_fails_validation() {
    let error = expect_err(
        load_fixture(
            "env.yml",
            &[
                ("PRIMARY_CHANNEL", "environment_channel"),
                ("FILE_CLIENT_ID", "file-client-id"),
            ],
            None,
        )
        .await,
    );
    assert_validation_field(&error, "twitch_api");
}

// 原測試：只有 Client ID 與 Client Secret 時允許啟動設定
#[tokio::test]
async fn allows_startup_with_client_id_and_client_secret_only() {
    let config = load_source(
        "\n\
         channels: [streamer]\n\
         twitch_api:\n\
         \x20 client_id: fixture-client-id\n\
         \x20 client_secret: fixture-client-secret\n",
        &[],
        None,
    )
    .await
    .expect("應成功載入");

    assert!(
        json_eq(
            &config.twitch_api.to_json(),
            &serde_json::json!({
                "clientId": "fixture-client-id",
                "accessToken": "",
                "clientSecret": "fixture-client-secret",
            })
        ),
        "twitchApi 不符：{}",
        config.twitch_api.to_json()
    );
}

// 舊版以 /headless.*true.*false/u 的寬鬆樣式比對；這裡改比對完整訊息（內容一致）。
// 原測試：拒絕不是 true 或 false 的 HEADLESS
#[tokio::test]
async fn rejects_headless_env_that_is_not_true_or_false() {
    let error = expect_err(load_fixture("valid.yml", &[("HEADLESS", "yes")], None).await);
    assert_validation_field(&error, "headless");
    assert_eq!(
        error.message(),
        "設定欄位 headless 環境變數必須是 true 或 false"
    );
}

// 原測試：可由環境變數啟用 Telegram 並解析多個 chat ID
#[tokio::test]
async fn enables_telegram_via_env_and_parses_multiple_chat_ids() {
    let config = load_fixture(
        "defaults.yml",
        &[
            ("TELEGRAM_ENABLED", "true"),
            ("TELEGRAM_BOT_TOKEN", "123456:test-token"),
            ("TELEGRAM_ALLOWED_CHAT_IDS", "42, -100123,42"),
        ],
        None,
    )
    .await
    .expect("應成功載入");

    assert!(
        json_eq(
            &config.telegram.to_json(),
            &serde_json::json!({
                "enabled": true,
                "botToken": "123456:test-token",
                "allowedChatIds": ["42", "-100123"],
                "pollingTimeoutSeconds": 25,
            })
        ),
        "telegram 不符：{}",
        config.telegram.to_json()
    );
}

// 原測試：Telegram %s 時拒絕設定
#[tokio::test]
async fn rejects_invalid_telegram_env_settings() {
    let cases: Vec<(&str, Vec<(&str, &str)>)> = vec![
        (
            "缺少 bot token",
            vec![
                ("TELEGRAM_ENABLED", "true"),
                ("TELEGRAM_ALLOWED_CHAT_IDS", "42"),
            ],
        ),
        (
            "缺少 chat ID",
            vec![
                ("TELEGRAM_ENABLED", "true"),
                ("TELEGRAM_BOT_TOKEN", "token"),
            ],
        ),
        (
            "chat ID 格式錯誤",
            vec![
                ("TELEGRAM_ENABLED", "true"),
                ("TELEGRAM_BOT_TOKEN", "token"),
                ("TELEGRAM_ALLOWED_CHAT_IDS", "not-a-chat"),
            ],
        ),
    ];
    for (name, env) in &cases {
        let result = load_fixture("defaults.yml", env, None).await;
        let Err(error) = result else {
            panic!("Telegram {name} 應驗證失敗");
        };
        assert_validation_error(&error);
    }
}

// 原測試：可由環境變數啟用 Discord 並解析多個 channel ID
#[tokio::test]
async fn enables_discord_via_env_and_parses_multiple_channel_ids() {
    let config = load_fixture(
        "defaults.yml",
        &[
            ("DISCORD_ENABLED", "true"),
            ("DISCORD_BOT_TOKEN", "discord-token"),
            ("DISCORD_APPLICATION_ID", "123456789012345678"),
            ("DISCORD_GUILD_ID", "987654321098765432"),
            (
                "DISCORD_ALLOWED_CHANNEL_IDS",
                "111111111111111111, 222222222222222222,111111111111111111",
            ),
            ("DISCORD_ALLOW_DIRECT_MESSAGES", "true"),
            (
                "DISCORD_ALLOWED_USER_IDS",
                "333333333333333333,333333333333333333",
            ),
        ],
        None,
    )
    .await
    .expect("應成功載入");

    assert!(
        json_eq(
            &config.discord.to_json(),
            &serde_json::json!({
                "enabled": true,
                "botToken": "discord-token",
                "applicationId": "123456789012345678",
                "guildId": "987654321098765432",
                "allowedChannelIds": [
                    "111111111111111111",
                    "222222222222222222",
                ],
                "allowDirectMessages": true,
                "allowedUserIds": ["333333333333333333"],
            })
        ),
        "discord 不符：{}",
        config.discord.to_json()
    );
}

// 原測試：Discord %s 時拒絕設定
#[tokio::test]
async fn rejects_invalid_discord_env_settings() {
    let cases: Vec<(&str, Vec<(&str, &str)>)> = vec![
        (
            "缺少 bot token",
            vec![
                ("DISCORD_ENABLED", "true"),
                ("DISCORD_APPLICATION_ID", "123456789012345678"),
                ("DISCORD_ALLOWED_CHANNEL_IDS", "111111111111111111"),
            ],
        ),
        (
            "缺少 application ID",
            vec![
                ("DISCORD_ENABLED", "true"),
                ("DISCORD_BOT_TOKEN", "token"),
                ("DISCORD_ALLOWED_CHANNEL_IDS", "111111111111111111"),
            ],
        ),
        (
            "application ID 格式錯誤",
            vec![
                ("DISCORD_ENABLED", "true"),
                ("DISCORD_BOT_TOKEN", "token"),
                ("DISCORD_APPLICATION_ID", "not-a-snowflake"),
                ("DISCORD_ALLOWED_CHANNEL_IDS", "111111111111111111"),
            ],
        ),
        (
            "缺少 channel ID",
            vec![
                ("DISCORD_ENABLED", "true"),
                ("DISCORD_BOT_TOKEN", "token"),
                ("DISCORD_APPLICATION_ID", "123456789012345678"),
            ],
        ),
        (
            "啟用私訊但缺少 user ID",
            vec![
                ("DISCORD_ENABLED", "true"),
                ("DISCORD_BOT_TOKEN", "token"),
                ("DISCORD_APPLICATION_ID", "123456789012345678"),
                ("DISCORD_ALLOW_DIRECT_MESSAGES", "true"),
            ],
        ),
        (
            "channel ID 格式錯誤",
            vec![
                ("DISCORD_ENABLED", "true"),
                ("DISCORD_BOT_TOKEN", "token"),
                ("DISCORD_APPLICATION_ID", "123456789012345678"),
                ("DISCORD_ALLOWED_CHANNEL_IDS", "not-a-channel"),
            ],
        ),
        (
            "user ID 格式錯誤",
            vec![
                ("DISCORD_ENABLED", "true"),
                ("DISCORD_BOT_TOKEN", "token"),
                ("DISCORD_APPLICATION_ID", "123456789012345678"),
                ("DISCORD_ALLOW_DIRECT_MESSAGES", "true"),
                ("DISCORD_ALLOWED_USER_IDS", "not-a-user"),
            ],
        ),
    ];
    for (name, env) in &cases {
        let result = load_fixture("defaults.yml", env, None).await;
        let Err(error) = result else {
            panic!("Discord {name} 應驗證失敗");
        };
        assert_validation_error(&error);
    }
}

// 原測試：%s 時拋出 ConfigValidationError
#[tokio::test]
async fn invalid_channels_throw_config_validation_error() {
    let cases: Vec<(&str, String)> = vec![
        ("缺少 channels", without_channels()),
        ("channels 不是陣列", valid_source("channels: streamer")),
        ("channels 是空陣列", valid_source("channels: []")),
        (
            "頻道名稱包含非法字元",
            valid_source("channels: [good, bad-name]"),
        ),
        (
            "頻道名稱超過 25 字元",
            valid_source(&format!("channels: [{}]", "a".repeat(26))),
        ),
        ("頻道不是字串", valid_source("channels: [123]")),
    ];
    for (name, source) in &cases {
        let Err(error) = load_source(source, &[], None).await else {
            panic!("{name} 應拋出 ConfigValidationError");
        };
        assert_validation_error(&error);
    }
}

// 與 config.test.ts:223 同標題（兩個 it.each）；此組為頂層欄位的型別案例表
// （test-port.md，DEC-010）。
// 原測試：%s 時拒絕設定
#[tokio::test]
async fn rejects_invalid_top_level_field_types() {
    let cases: Vec<(&str, &str)> = vec![
        (
            "check_interval_seconds 型別錯誤",
            "check_interval_seconds: \"60\"",
        ),
        (
            "check_interval_seconds 小於 30",
            "check_interval_seconds: 29",
        ),
        (
            "check_interval_seconds 不是整數",
            "check_interval_seconds: 30.5",
        ),
        (
            "max_concurrent_streams 型別錯誤",
            "max_concurrent_streams: \"2\"",
        ),
        ("max_concurrent_streams 小於 1", "max_concurrent_streams: 0"),
        (
            "max_concurrent_streams 不是整數",
            "max_concurrent_streams: 1.5",
        ),
        ("headless 型別錯誤", "headless: \"true\""),
        ("storage_state_path 型別錯誤", "storage_state_path: 123"),
        ("storage_state_path 為空", "storage_state_path: \"\""),
        ("log_level 型別錯誤", "log_level: 1"),
        ("log_level 不在允許清單", "log_level: verbose"),
    ];
    for (name, replacement) in cases {
        let Err(error) = load_source(&valid_source(replacement), &[], None).await else {
            panic!("{name} 應驗證失敗");
        };
        assert_validation_error(&error);
    }
}

// 原測試：%s 不是物件時拒絕設定
#[tokio::test]
async fn rejects_non_object_sections() {
    let cases: Vec<(&str, String)> = vec![
        (
            "twitch_api",
            "channels: [streamer]\ntwitch_api: invalid".to_string(),
        ),
        (
            "browser",
            format!("{}browser: invalid", valid_source("channels: [streamer]")),
        ),
    ];
    for (field, source) in &cases {
        let error = expect_err(load_source(source, &[], None).await);
        assert_validation_field(&error, field);
    }
}

// 原測試：twitch_api.%s 時拒絕設定
#[tokio::test]
async fn rejects_invalid_twitch_api_fields() {
    let cases: Vec<(&str, &str)> = vec![
        ("client_id 缺少", "access_token: fixture-access-token"),
        (
            "client_id 型別錯誤",
            "client_id: 123\n  access_token: fixture-token",
        ),
        (
            "client_id 為空",
            "client_id: \"\"\n  access_token: fixture-token",
        ),
        ("access_token 缺少", "client_id: fixture-client-id"),
        (
            "access_token 型別錯誤",
            "client_id: fixture-client-id\n  access_token: 123",
        ),
        (
            "access_token 為空",
            "client_id: fixture-client-id\n  access_token: \"\"",
        ),
    ];
    for (name, body) in cases {
        let Err(error) = load_source(&config_with_twitch_api(body), &[], None).await else {
            panic!("{name} 應驗證失敗");
        };
        assert_validation_error(&error);
    }
}

// 原測試：browser.%s 無效時拒絕設定
#[tokio::test]
async fn rejects_invalid_browser_fields() {
    let cases: Vec<(&str, &str)> = vec![
        ("engine", "engine: webkit"),
        ("navigation_timeout_ms", "navigation_timeout_ms: 0"),
        (
            "navigation_timeout_ms 型別",
            "navigation_timeout_ms: \"30000\"",
        ),
        (
            "page_health_check_interval_seconds",
            "page_health_check_interval_seconds: 0",
        ),
        (
            "page_health_check_interval_seconds 型別",
            "page_health_check_interval_seconds: 1.5",
        ),
        (
            "reward_check_interval_seconds",
            "reward_check_interval_seconds: -1",
        ),
        (
            "reward_check_interval_seconds 型別",
            "reward_check_interval_seconds: false",
        ),
        (
            "page_refresh_interval_seconds 型別",
            "page_refresh_interval_seconds: false",
        ),
        ("restart_on_crash", "restart_on_crash: \"true\""),
        ("stream_quality", "stream_quality: 720p"),
        (
            "enforce_stream_quality_seconds",
            "enforce_stream_quality_seconds: 0",
        ),
        ("viewport_width", "viewport_width: 319"),
        ("viewport_height", "viewport_height: 179"),
        ("mute_audio", "mute_audio: \"true\""),
        ("block_images", "block_images: \"true\""),
        ("block_fonts", "block_fonts: \"true\""),
        ("block_known_tracking", "block_known_tracking: \"true\""),
        ("disable_chat", "disable_chat: \"true\""),
        (
            "resource_telemetry_interval_seconds",
            "resource_telemetry_interval_seconds: 0",
        ),
    ];
    for (name, body) in cases {
        let Err(error) = load_source(&config_with_browser(body), &[], None).await else {
            panic!("{name} 應驗證失敗");
        };
        assert_validation_error(&error);
    }
}

// 原測試：設定檔不存在時拋出 ConfigFileNotFoundError
#[tokio::test]
async fn throws_config_file_not_found_error_when_missing() {
    let path = std::env::temp_dir().join("config-does-not-exist.yml");
    let path_text = path.to_str().expect("路徑應為 UTF-8");

    let error = expect_err(
        YamlConfigLoader::default()
            .load(path_text, &ProcessEnv::new())
            .await,
    );
    assert_eq!(error.name(), "ConfigFileNotFoundError");
    assert_eq!(
        error.message(),
        format!("找不到設定檔：{path_text}"),
        "錯誤：{error}"
    );
    // 對應 `rejects.toMatchObject({ configPath: path })`。
    let ConfigLoadError::FileNotFound(file_error) = &error else {
        panic!("應是 ConfigFileNotFoundError：{error:?}");
    };
    assert_eq!(file_error.config_path(), path_text);
}

// 原測試：YAML 語法錯誤時拋出 ConfigParseError
#[tokio::test]
async fn throws_config_parse_error_on_yaml_syntax_error() {
    let error = expect_err(load_fixture("parse-error.yml", &[], None).await);
    assert_eq!(error.name(), "ConfigParseError");
}

// 舊版接受 ConfigParseError 或 ConfigValidationError（前兩例是解析錯誤，
// 重複鍵在 Rust 版的 parse_yaml 階段回報為 ConfigParseError）。
// 原測試：拒絕格式錯誤的 YAML
#[tokio::test]
async fn rejects_malformed_yaml() {
    let cases: Vec<&str> = vec![
        "channels:\n\t- channel",
        "channels: [channel,,other]",
        "channels:\n  - channel\nchannels:\n  - other",
    ];
    for source in cases {
        let error = expect_err(load_source(source, &[], None).await);
        assert!(
            matches!(error.name(), "ConfigParseError" | "ConfigValidationError"),
            "錯誤類別不符：{error}"
        );
    }
}

// 原測試：拒絕形成循環物件的 YAML alias
#[tokio::test]
async fn rejects_yaml_alias_forming_cyclic_object() {
    let error = expect_err(
        load_source(
            "\n\
         root: &root\n\
         \x20 self: *root\n\
         channels: [streamer]\n\
         twitch_api:\n\
         \x20 client_id: fixture-client-id\n\
         \x20 access_token: fixture-access-token\n",
            &[],
            None,
        )
        .await,
    );
    assert_eq!(error.name(), "ConfigParseError");
}

// 原測試：%s 超過 Node timer 上限時拒絕設定
#[tokio::test]
async fn rejects_values_above_node_timer_limit() {
    let cases: Vec<(&str, &str)> = vec![
        ("check_interval_seconds", "check_interval_seconds: 2147484"),
        (
            "browser.navigation_timeout_ms",
            "browser:\n  navigation_timeout_ms: 2147483648",
        ),
        (
            "browser.page_health_check_interval_seconds",
            "browser:\n  page_health_check_interval_seconds: 2147484",
        ),
        (
            "browser.reward_check_interval_seconds",
            "browser:\n  reward_check_interval_seconds: 2147484",
        ),
        (
            "browser.page_refresh_interval_seconds",
            "browser:\n  page_refresh_interval_seconds: 2147484",
        ),
    ];
    for (field, replacement) in cases {
        let Err(error) = load_source(&valid_source(replacement), &[], None).await else {
            panic!("{field} 超過上限應驗證失敗");
        };
        assert_validation_error(&error);
    }
}

// 原測試：%s 根節點走 ConfigValidationError
#[tokio::test]
async fn non_object_root_throws_config_validation_error() {
    let cases: Vec<(&str, &str)> = vec![
        ("sequence", "- root_sequence"),
        ("scalar", "root_scalar"),
        ("null", "null"),
    ];
    for (kind, source) in cases {
        let Err(error) = load_source(source, &[], None).await else {
            panic!("{kind} 根節點應驗證失敗");
        };
        assert_validation_field(&error, "root");
    }
}

// 舊版以 Promise.all 平行觸發兩種錯誤；Rust 版依序執行（斷言內容相同）。
// 原測試：任何 parse 或 validation error 都不包含 access token 原文
#[tokio::test]
async fn parse_and_validation_errors_do_not_contain_access_token_text() {
    let secret = "never-print-this-access-token";
    let validation_error = expect_err(
        load_source(
            &format!(
                "channels: [channel]\n\
             check_interval_seconds: 1\n\
             twitch_api:\n\
             \x20 client_id: fixture-client-id\n\
             \x20 access_token: {secret}\n"
            ),
            &[("TWITCH_ACCESS_TOKEN", secret)],
            None,
        )
        .await,
    );
    let parse_error = expect_err(
        load_source(
            &format!(
                "channels: [channel]\n\
             twitch_api:\n\
             \x20 client_id: fixture-client-id\n\
             \x20 access_token: \"{secret}\n"
            ),
            &[],
            None,
        )
        .await,
    );

    assert!(
        !validation_error.to_string().contains(secret),
        "驗證錯誤含原文：{validation_error}"
    );
    assert!(
        !parse_error.to_string().contains(secret),
        "解析錯誤含原文：{parse_error}"
    );
}

// 舊版以 Object.freeze 驗證深層不可變，並在修改時拋 TypeError；Rust 的所有權
// 模型在編譯期保證（設定值是擁有型資料，沒有可透過參照取得的 &mut）。這裡移植
// 對應的執行期性質：兩次載入的結果相等，且修改克隆不影響原值（無共享可變狀態）。
// 原測試：回傳物件、陣列與巢狀設定皆不可變
#[tokio::test]
async fn loaded_config_is_deeply_immutable_and_not_shared() {
    let config = load_fixture("valid.yml", &[], None)
        .await
        .expect("應成功載入");

    let mut cloned = config.clone();
    cloned.channels.push("another_channel".to_string());
    assert_eq!(config.channels.len(), 3, "原設定不受克隆的修改影響");
    assert_eq!(
        config.channels.first().map(String::as_str),
        Some("streamer_one")
    );

    let again = load_fixture("valid.yml", &[], None)
        .await
        .expect("應成功載入");
    assert!(
        json_eq(&config.to_json(), &again.to_json()),
        "兩次載入的結果應相等"
    );
}

// ---------------------------------------------------------------------------
// 舊測試的來源組合輔助（與 config.test.ts 的同名函式逐字對應）
// ---------------------------------------------------------------------------

/// 舊測試的 `validSource`：替代的欄位在前，其後接合法的 twitch_api。
fn valid_source(replacement: &str) -> String {
    format!(
        "{replacement}\ntwitch_api:\n\
         \x20 client_id: fixture-client-id\n\
         \x20 access_token: fixture-access-token\n"
    )
}

/// 舊測試的 `withoutChannels`。
fn without_channels() -> String {
    "twitch_api:\n\
     \x20 client_id: fixture-client-id\n\
     \x20 access_token: fixture-access-token\n"
        .to_string()
}

/// 舊測試的 `configWithTwitchApi`。
fn config_with_twitch_api(body: &str) -> String {
    format!("channels: [streamer]\ntwitch_api:\n  {body}\n")
}

/// 舊測試的 `configWithBrowser`。
fn config_with_browser(body: &str) -> String {
    format!(
        "channels: [streamer]\n\
         twitch_api:\n\
         \x20 client_id: fixture-client-id\n\
         \x20 access_token: fixture-access-token\n\
         browser:\n\
         \x20 {body}\n"
    )
}

/// 舊測試的 `configWithChannels`。
fn config_with_channels(channels: &[&str]) -> String {
    format!(
        "channels: [{}]\n\
         twitch_api:\n\
         \x20 client_id: fixture-client-id\n\
         \x20 access_token: fixture-access-token\n",
        channels.join(", ")
    )
}
