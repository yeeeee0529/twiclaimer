//! Runtime 啟動診斷與安全設定 fingerprint：`app/RuntimeDiagnostics.ts` 逐函式移植
//! （§4.2）。fingerprint 的雜湊輸入與編碼照舊版重現（golden 於 S2.6 驗證）：
//!
//! - 輸入是「安全設定」物件（`schemaVersion`、去空白並轉小寫的 `channels`、
//!   頂層欄位、整個 `browser` 設定、`integrations` 三個布林），**不含**
//!   credentials、`storage_state_path` 與 bot 的 private IDs；
//!   序列化與 JS `JSON.stringify` 逐字相同：物件鍵照建立順序、數字用
//!   `js_number_to_string`（ryu-js，§7.7；`60.0` 輸出 `60`）、字串跳脫只處理
//!   `"`、`\` 與控制字元。
//! - 編碼是 SHA-256 的小寫十六進位（`digest('hex')`），輸出 64 字元。
//!
//! 取捨：
//! - `readPackageVersion('../../package.json')`：應用程式版本改用編譯期常數
//!   [`APPLICATION_VERSION`]（`CARGO_PKG_VERSION`；舊版 package.json 同為 0.1.0）。
//! - `readPackageVersion('playwright/package.json')`：Rust 版沒有 playwright 套件
//!   （X1 只用 CDP chromium），[`PLAYWRIGHT_DEFAULT_VERSION`] 為 `None`；
//!   `PLAYWRIGHT_VERSION` 環境變數仍可覆寫。
//! - `nodeVersion` 的預設值是 Node 的 `process.version`；Rust 版沒有對應的執行期
//!   字串，未提供 `node_version` 時直接得到 `"unknown"`（與舊版值不安全時相同）。
//! - `StartupDiagnosticsProvider`（非同步 provider 型別）屬呼叫端使用，等 S3.6 的
//!   `AppRunner` 移植時再定義。
//! - `channels` 的 `toLocaleLowerCase('en-US')` 以 [`str::to_lowercase`] 對應
//!   （Unicode 預設轉換即 en-US 的行為；頻道名稱另受 `CHANNEL_PATTERN` 限制，
//!   實務上只含小寫字母與數字底線）。`trim` 的差異（不含 `U+FEFF`）見 DEC-013。

#![allow(dead_code)]

use std::sync::OnceLock;

use regex::Regex;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::config::loader::ProcessEnv;
use crate::config::model::AppConfig;
use crate::logging::redact::js_number_to_string;

/// 舊版 `DiagnosticLimitValue`：`number | string | null`。
///
/// 資源上限值；超過 `Number.MAX_SAFE_INTEGER` 的位元組數照舊版以字串表示。
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum DiagnosticLimitValue {
    Number(f64),
    Text(String),
    Null,
}

/// 舊版 `RuntimeResourceLimitsSnapshot`（`RuntimeResourceMonitor` 的快照，
/// monitor 本體在 S3.2 移植）。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RuntimeResourceLimitsSnapshot {
    pub(crate) memory_max_bytes: DiagnosticLimitValue,
    pub(crate) pids_max: DiagnosticLimitValue,
}

/// 舊版 `RuntimeDiagnosticsBrowser`：診斷需要的瀏覽器管理器介面。
pub(crate) trait RuntimeDiagnosticsBrowser {
    fn get_browser_generation(&self) -> f64;
    fn get_browser_version(&self) -> Option<String>;
}

/// 舊版 `resourceLimits` 選項的介面：`{ getLatestResourceLimits(): snapshot }`。
pub(crate) trait RuntimeResourceLimitsProvider {
    fn get_latest_resource_limits(&self) -> RuntimeResourceLimitsSnapshot;
}

/// 舊版 `RuntimeStartupDiagnostics`（欄位順序照舊版物件建立的順序）。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RuntimeStartupDiagnostics {
    pub(crate) run_id: String,
    pub(crate) application_version: Option<String>,
    pub(crate) git_commit: Option<String>,
    pub(crate) node_version: String,
    pub(crate) playwright_version: Option<String>,
    pub(crate) browser_engine: String,
    pub(crate) browser_version: Option<String>,
    pub(crate) browser_generation: f64,
    pub(crate) memory_max_bytes: DiagnosticLimitValue,
    pub(crate) pids_max: DiagnosticLimitValue,
    pub(crate) safe_config_fingerprint: String,
}

/// 舊版 `RuntimeDiagnosticsOptions`：`undefined` 的選項以 `None` 對應。
pub(crate) struct RuntimeDiagnosticsOptions<'a> {
    pub(crate) config: &'a AppConfig,
    pub(crate) browser_manager: &'a dyn RuntimeDiagnosticsBrowser,
    pub(crate) resource_limits: Option<&'a dyn RuntimeResourceLimitsProvider>,
    /// 舊版 `env?: NodeJS.ProcessEnv`；`None` 時讀真實程序的環境。
    pub(crate) env: Option<&'a ProcessEnv>,
    pub(crate) browser_engine: Option<&'a str>,
    pub(crate) run_id: Option<&'a str>,
    pub(crate) node_version: Option<&'a str>,
}

/// 舊版 `DEFAULT_APPLICATION_VERSION`：應用程式版本（舊版由 package.json 在模組
/// 載入時讀入，這裡以編譯期常數對應；舊版同為 0.1.0）。
const APPLICATION_VERSION: &str = env!("CARGO_PKG_VERSION");

/// 舊版 `DEFAULT_PLAYWRIGHT_VERSION`：Rust 版沒有 playwright 套件（X1），
/// 預設沒有版本可讀；`PLAYWRIGHT_VERSION` 環境變數仍可覆寫。
const PLAYWRIGHT_DEFAULT_VERSION: Option<&str> = None;

/// 舊版 `PROCESS_RUN_ID = randomUUID()`：程序層級的隨機 run ID（首次使用時產生，
/// Node 的 `randomUUID` 與 `uuid::Uuid::new_v4` 同為連字號小寫的 v4 格式）。
fn process_run_id() -> &'static str {
    static RUN_ID: OnceLock<String> = OnceLock::new();
    RUN_ID.get_or_init(|| Uuid::new_v4().to_string())
}

/// 對應 `createRuntimeStartupDiagnostics`。
pub(crate) fn create_runtime_startup_diagnostics(
    options: &RuntimeDiagnosticsOptions<'_>,
) -> RuntimeStartupDiagnostics {
    let fallback_env;
    let env: &ProcessEnv = match options.env {
        Some(env) => env,
        // 舊版的 `options.env ?? process.env`。
        None => {
            fallback_env = collect_process_env();
            &fallback_env
        }
    };
    // 舊版的 `options.resourceLimits?.getLatestResourceLimits() ?? { null, null }`。
    let limits = match options.resource_limits {
        Some(provider) => provider.get_latest_resource_limits(),
        None => RuntimeResourceLimitsSnapshot {
            memory_max_bytes: DiagnosticLimitValue::Null,
            pids_max: DiagnosticLimitValue::Null,
        },
    };

    RuntimeStartupDiagnostics {
        run_id: safe_run_id(options.run_id)
            .map_or_else(|| process_run_id().to_string(), str::to_string),
        application_version: safe_version(
            env_value(env, "APPLICATION_VERSION")
                .or_else(|| env_value(env, "npm_package_version"))
                .unwrap_or(APPLICATION_VERSION),
        ),
        git_commit: safe_git_commit(env_value(env, "GIT_COMMIT")),
        node_version: options
            .node_version
            .and_then(safe_version)
            .unwrap_or_else(|| "unknown".to_string()),
        playwright_version: env_value(env, "PLAYWRIGHT_VERSION")
            .or(PLAYWRIGHT_DEFAULT_VERSION)
            .and_then(safe_version),
        browser_engine: safe_engine(options.browser_engine),
        browser_version: options
            .browser_manager
            .get_browser_version()
            .as_deref()
            .and_then(safe_version),
        browser_generation: max_zero(options.browser_manager.get_browser_generation().trunc()),
        memory_max_bytes: limits.memory_max_bytes,
        pids_max: limits.pids_max,
        safe_config_fingerprint: create_safe_config_fingerprint(options.config),
    }
}

/// 對應 `createSafeConfigFingerprint`：SHA-256 的小寫十六進位（64 字元）。
pub(crate) fn create_safe_config_fingerprint(config: &AppConfig) -> String {
    // 鍵與值照舊版 safeConfig 物件建立的順序；`browser` 直接重用設定模型的
    // `to_json`（欄位順序與舊版 loader 的物件建立順序一致，golden 已驗證）。
    let safe_config = serde_json::json!({
        "schemaVersion": 1,
        "channels": config
            .channels
            .iter()
            .map(|channel| channel.trim().to_lowercase())
            .collect::<Vec<String>>(),
        "checkIntervalSeconds": config.check_interval_seconds,
        "maxConcurrentStreams": config.max_concurrent_streams,
        "headless": config.headless,
        "logLevel": config.log_level.as_str(),
        "browser": config.browser.to_json(),
        "integrations": {
            "telegramEnabled": config.telegram.enabled,
            "discordEnabled": config.discord.enabled,
            "discordDirectMessages": config.discord.allow_direct_messages,
        },
    });
    let mut payload = String::new();
    write_js_json(&safe_config, &mut payload);

    // 舊版 `createHash('sha256').update(payload).digest('hex')`。
    to_hex(&Sha256::digest(payload.as_bytes()))
}

/// 舊版 `safeVersion`：`/^[A-Za-z0-9][A-Za-z0-9._+()-]{0,127}$/u`。
fn safe_version(value: &str) -> Option<String> {
    version_pattern().is_match(value).then(|| value.to_string())
}

/// 舊版 `safeGitCommit`：`/^[a-f0-9]{7,64}$/iu`；輸出轉小寫（符合的值只有
/// ASCII 十六進位字元，`to_ascii_lowercase` 與 `toLocaleLowerCase('en-US')` 相同）。
fn safe_git_commit(value: Option<&str>) -> Option<String> {
    let value = value?;
    commit_pattern()
        .is_match(value)
        .then(|| value.to_ascii_lowercase())
}

/// 舊版 `safeEngine`：`/^(?:firefox|chromium)$/u`（區分大小寫）；其餘為 `unknown`。
fn safe_engine(value: Option<&str>) -> String {
    match value {
        Some(value) if engine_pattern().is_match(value) => value.to_string(),
        _ => "unknown".to_string(),
    }
}

/// 舊版 `safeRunId`：`/^[A-Za-z0-9-]{8,64}$/u`。
fn safe_run_id(value: Option<&str>) -> Option<&str> {
    let value = value?;
    run_id_pattern().is_match(value).then_some(value)
}

/// 舊版的 `env.KEY`：不存在為 `None`（空字串「存在」，會參與 `??` 鏈）。
fn env_value<'a>(env: &'a ProcessEnv, key: &str) -> Option<&'a str> {
    env.get(key).map(String::as_str)
}

/// 舊版的 `process.env` 預設值：真實程序的環境變數（非 UTF-8 的鍵或值略過，
/// `ProcessEnv` 只放得下字串）。
fn collect_process_env() -> ProcessEnv {
    std::env::vars_os()
        .filter_map(|(key, value)| {
            let (key, value) = (key.into_string().ok()?, value.into_string().ok()?);
            Some((key, value))
        })
        .collect()
}

/// 對應 `Math.max(0, value)`：任一參數是 NaN 時 JS 回傳 NaN（[`f64::max`] 回 0）。
fn max_zero(value: f64) -> f64 {
    if value.is_nan() {
        f64::NAN
    } else {
        value.max(0.0)
    }
}

/// 把 [`serde_json::Value`] 以 JS `JSON.stringify` 的語意序列化：物件鍵照建立
/// 順序（`preserve_order`）、數字用 `js_number_to_string`（ryu-js）、字串跳脫
/// 見 [`write_json_string`]。serde_json 的 `Number` 必為有限數（非有限 f64 在
/// `Value::from` 時已轉成 `Null`，與 `JSON.stringify` 的 `null` 一致）。
fn write_js_json(value: &serde_json::Value, out: &mut String) {
    match value {
        serde_json::Value::Null => out.push_str("null"),
        serde_json::Value::Bool(inner) => out.push_str(if *inner { "true" } else { "false" }),
        serde_json::Value::Number(inner) => {
            let number = inner.as_f64().unwrap_or(0.0);
            out.push_str(&js_number_to_string(number));
        }
        serde_json::Value::String(inner) => write_json_string(inner, out),
        serde_json::Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_js_json(item, out);
            }
            out.push(']');
        }
        serde_json::Value::Object(entries) => {
            out.push('{');
            for (index, (key, value)) in entries.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_json_string(key, out);
                out.push(':');
                write_js_json(value, out);
            }
            out.push('}');
        }
    }
}

/// JSON 字串跳脫，與 `JSON.stringify` 相同（照 `logging/redact.rs` 的前例）：
/// 僅 `"`、`\` 與控制字元會被跳脫（`\b`、`\t`、`\n`、`\f`、`\r` 用短寫，
/// 其餘用 `\u00xx` 小寫十六進位）。
fn write_json_string(value: &str, out: &mut String) {
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{09}' => out.push_str("\\t"),
            '\u{0a}' => out.push_str("\\n"),
            '\u{0c}' => out.push_str("\\f"),
            '\u{0d}' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// 舊版 `digest('hex')`：每個位元組輸出兩位小寫十六進位。
fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

// ---------------------------------------------------------------------------
// 固定樣式（編譯失敗代表實作錯誤，依 loader.rs 的前例以明確訊息終止）
// ---------------------------------------------------------------------------

/// 編譯固定樣式。
fn compile_pattern(source: &str) -> Regex {
    match Regex::new(source) {
        Ok(regex) => regex,
        Err(error) => panic!("invalid built-in diagnostics pattern {source:?}: {error}"),
    }
}

/// 舊版 `safeVersion` 的樣式：`/^[A-Za-z0-9][A-Za-z0-9._+()-]{0,127}$/u`。
fn version_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| compile_pattern("^[A-Za-z0-9][A-Za-z0-9._+()-]{0,127}$"))
}

/// 舊版 `safeGitCommit` 的樣式：`/^[a-f0-9]{7,64}$/iu`（`i` 以大小寫不分寫出）。
fn commit_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| compile_pattern("(?i)^[a-f0-9]{7,64}$"))
}

/// 舊版 `safeEngine` 的樣式：`/^(?:firefox|chromium)$/u`。
fn engine_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| compile_pattern("^(?:firefox|chromium)$"))
}

/// 舊版 `safeRunId` 的樣式：`/^[A-Za-z0-9-]{8,64}$/u`。
fn run_id_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| compile_pattern("^[A-Za-z0-9-]{8,64}$"))
}

// ---------------------------------------------------------------------------
// 移植的舊測試（標題逐字見 `doc/spec/ts-test-titles.tsv`）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::model::{
        BrowserConfig, BrowserEngine, BrowserRecoveryConfig, BrowserSessionStartConfig,
        DiscordConfig, ResourceGuardConfig, StreamQuality, TelegramConfig, TwitchApiConfig,
    };
    use crate::config::thresholds::{
        ResourceGuardAnchorThresholds, compute_effective_resource_guard_thresholds,
    };
    use crate::logging::logger::LogLevel;

    /// 舊測試的 `createTestConfig()`（`test/helpers/test-config.ts` 的預設值，
    /// 只參數化 `maxConcurrentStreams`；`effective` 門檻照 `createDefaultResourceGuard`
    /// 重新計算）。
    fn test_config(max_concurrent_streams: f64) -> AppConfig {
        let anchors = ResourceGuardAnchorThresholds {
            scale_with_streams: true,
            baseline_streams: 3.0,
            base_memory_mib: 512.0,
            warning_memory_mib: 4096.0,
            warning_reset_memory_mib: 3840.0,
            browser_recycle_memory_mib: 4608.0,
            emergency_memory_mib: 5376.0,
            post_recycle_target_memory_mib: 4096.0,
        };
        AppConfig {
            channels: vec!["first_channel".to_string(), "second_channel".to_string()],
            check_interval_seconds: 60.0,
            max_concurrent_streams,
            headless: true,
            storage_state_path: "/tmp/test-storage-state.json".to_string(),
            log_level: LogLevel::Debug,
            twitch_api: TwitchApiConfig {
                client_id: "integration-client-id".to_string(),
                access_token: "integration-access-token".to_string(),
                client_secret: String::new(),
            },
            browser: BrowserConfig {
                engine: BrowserEngine::Firefox,
                navigation_timeout_ms: 30000.0,
                page_health_check_interval_seconds: 60.0,
                reward_check_interval_seconds: 30.0,
                page_refresh_interval_seconds: 300.0,
                restart_on_crash: true,
                stream_quality: StreamQuality::P160,
                enforce_stream_quality_seconds: 120.0,
                viewport_width: 1280.0,
                viewport_height: 720.0,
                mute_audio: true,
                block_images: false,
                block_fonts: false,
                block_known_tracking: false,
                disable_chat: true,
                resource_telemetry_interval_seconds: 300.0,
                recovery: BrowserRecoveryConfig {
                    page_crash_backoff_seconds: vec![30.0, 60.0, 120.0],
                    channel_crash_window_seconds: 600.0,
                    channel_quarantine_threshold: 4.0,
                    channel_quarantine_seconds: 900.0,
                    stable_reset_seconds: 1800.0,
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
                    warning_memory_mib: 4096.0,
                    warning_reset_memory_mib: 3840.0,
                    browser_recycle_memory_mib: 4608.0,
                    browser_recycle_consecutive_samples: 2.0,
                    emergency_memory_mib: 5376.0,
                    emergency_swap_mib: 768.0,
                    fast_growth_mib: 1024.0,
                    fast_growth_window_seconds: 30.0,
                    post_browser_restart_rate_grace_seconds: 120.0,
                    post_recycle_observation_seconds: 20.0,
                    post_recycle_target_memory_mib: 4096.0,
                    post_recycle_minimum_drop_mib: 512.0,
                    effective: compute_effective_resource_guard_thresholds(
                        &anchors,
                        max_concurrent_streams,
                    ),
                },
            },
            telegram: TelegramConfig {
                enabled: false,
                bot_token: String::new(),
                allowed_chat_ids: vec![],
                polling_timeout_seconds: 25.0,
            },
            discord: DiscordConfig {
                enabled: false,
                bot_token: String::new(),
                application_id: String::new(),
                guild_id: String::new(),
                allowed_channel_ids: vec![],
                allow_direct_messages: false,
                allowed_user_ids: vec![],
            },
        }
    }

    /// 固定回傳的假瀏覽器管理器（對應舊測試的 inline 物件）。
    struct FixedBrowser {
        generation: f64,
        version: Option<&'static str>,
    }

    impl RuntimeDiagnosticsBrowser for FixedBrowser {
        fn get_browser_generation(&self) -> f64 {
            self.generation
        }

        fn get_browser_version(&self) -> Option<String> {
            self.version.map(str::to_string)
        }
    }

    /// 固定回傳的假資源上限 provider（對應舊測試的 inline 物件）。
    struct FixedLimits {
        snapshot: RuntimeResourceLimitsSnapshot,
    }

    impl RuntimeResourceLimitsProvider for FixedLimits {
        fn get_latest_resource_limits(&self) -> RuntimeResourceLimitsSnapshot {
            self.snapshot.clone()
        }
    }

    /// 舊測試的 `env` 對照表。
    fn env_map(pairs: &[(&str, &str)]) -> ProcessEnv {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    // 原測試：產生完整、安全且可關聯的 startup diagnostics
    #[test]
    fn generates_complete_safe_and_correlatable_startup_diagnostics() {
        let config = test_config(2.0);
        let browser_manager = FixedBrowser {
            generation: 7.0,
            version: Some("142.0.1"),
        };
        let limits = FixedLimits {
            snapshot: RuntimeResourceLimitsSnapshot {
                memory_max_bytes: DiagnosticLimitValue::Number(6_442_450_944.0),
                pids_max: DiagnosticLimitValue::Text("9007199254740992".to_string()),
            },
        };
        let env = env_map(&[
            ("APPLICATION_VERSION", "0.2.0-canary"),
            ("GIT_COMMIT", "ABCDEF1234567"),
            ("PLAYWRIGHT_VERSION", "1.62.1"),
        ]);
        let diagnostics = create_runtime_startup_diagnostics(&RuntimeDiagnosticsOptions {
            config: &config,
            browser_manager: &browser_manager,
            resource_limits: Some(&limits),
            env: Some(&env),
            browser_engine: Some("firefox"),
            run_id: Some("test-run-1234"),
            node_version: Some("v24.7.0"),
        });

        assert_eq!(diagnostics.run_id, "test-run-1234");
        assert_eq!(
            diagnostics.application_version.as_deref(),
            Some("0.2.0-canary")
        );
        assert_eq!(diagnostics.git_commit.as_deref(), Some("abcdef1234567"));
        assert_eq!(diagnostics.node_version, "v24.7.0");
        assert_eq!(diagnostics.playwright_version.as_deref(), Some("1.62.1"));
        assert_eq!(diagnostics.browser_engine, "firefox");
        assert_eq!(diagnostics.browser_version.as_deref(), Some("142.0.1"));
        assert_eq!(diagnostics.browser_generation, 7.0);
        assert_eq!(
            diagnostics.memory_max_bytes,
            DiagnosticLimitValue::Number(6_442_450_944.0)
        );
        assert_eq!(
            diagnostics.pids_max,
            DiagnosticLimitValue::Text("9007199254740992".to_string())
        );
        let fingerprint = &diagnostics.safe_config_fingerprint;
        assert_eq!(
            fingerprint.len(),
            64,
            "fingerprint 應為 64 字元：{fingerprint}"
        );
        assert!(
            fingerprint
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
            "fingerprint 應為小寫 hex：{fingerprint}"
        );
        // 補充（S2.6）：`createTestConfig()` 的 fingerprint 與舊版 TS 實作
        // （node dist/app/RuntimeDiagnostics.js）逐字相同。
        assert_eq!(
            fingerprint,
            "6b8b5a91dc37fcc3e348d05cdc3cd42df5f17a9c81a70d22bce7cde17b658fe0"
        );
    }

    // 原測試：fingerprint 排除 credentials、storage path 與 bot private IDs
    #[test]
    fn fingerprint_excludes_credentials_storage_path_and_bot_private_ids() {
        // 舊測試以 createTestConfig 的覆寫參數注入不同的 secrets；Rust 的設定
        // 欄位是 pub(crate)，直接改克隆的欄位（同組差異：first／second 兩份，
        // chat ID 是 6 位數的 `111111`／`222222`）。
        let make_config = |prefix: &str, digit: char| {
            let mut config = test_config(2.0);
            config.storage_state_path = format!("/secret/{prefix}-storage-state.json");
            config.twitch_api = TwitchApiConfig {
                client_id: format!("{prefix}-client"),
                access_token: format!("{prefix}-access-token"),
                client_secret: format!("{prefix}-client-secret"),
            };
            config.telegram.bot_token = format!("{prefix}-telegram-token");
            config.telegram.allowed_chat_ids = vec![digit.to_string().repeat(6)];
            config.discord.bot_token = format!("{prefix}-discord-token");
            config.discord.application_id = format!("{prefix}-application-id");
            config.discord.guild_id = format!("{prefix}-guild-id");
            config.discord.allowed_channel_ids = vec![format!("{prefix}-channel-id")];
            config.discord.allowed_user_ids = vec![format!("{prefix}-user-id")];
            config
        };
        let first = make_config("first", '1');
        let second = make_config("second", '2');

        assert_eq!(
            create_safe_config_fingerprint(&first),
            create_safe_config_fingerprint(&second)
        );
        assert_ne!(
            create_safe_config_fingerprint(&test_config(1.0)),
            create_safe_config_fingerprint(&first)
        );
    }

    // 原測試：缺值或不安全版本欄位使用明確 null/unknown
    #[test]
    fn missing_or_unsafe_version_fields_use_explicit_null_or_unknown() {
        let config = test_config(2.0);
        let browser_manager = FixedBrowser {
            generation: 0.0,
            version: None,
        };
        let env = env_map(&[
            ("APPLICATION_VERSION", "token=secret"),
            ("GIT_COMMIT", "not-a-commit"),
            ("PLAYWRIGHT_VERSION", "Bearer secret"),
        ]);
        let first = create_runtime_startup_diagnostics(&RuntimeDiagnosticsOptions {
            config: &config,
            browser_manager: &browser_manager,
            resource_limits: None,
            env: Some(&env),
            browser_engine: Some("webkit"),
            run_id: None,
            node_version: None,
        });
        let empty_env = ProcessEnv::new();
        let second = create_runtime_startup_diagnostics(&RuntimeDiagnosticsOptions {
            config: &config,
            browser_manager: &browser_manager,
            resource_limits: None,
            env: Some(&empty_env),
            browser_engine: None,
            run_id: None,
            node_version: None,
        });

        assert_eq!(first.application_version, None);
        assert_eq!(first.git_commit, None);
        assert_eq!(first.playwright_version, None);
        assert_eq!(first.browser_engine, "unknown");
        assert_eq!(first.browser_version, None);
        assert_eq!(first.memory_max_bytes, DiagnosticLimitValue::Null);
        assert_eq!(first.pids_max, DiagnosticLimitValue::Null);
        // 兩次都退回程序層級的隨機 run ID，因此相同。
        assert_eq!(first.run_id, second.run_id);
    }
}
