//! 日誌本體：`JsonLineLogger` 逐函式移植（§7.7，行為依據舊版 `src/logging/Logger.ts`）。
//!
//! - 每筆一行 JSON：`{"level","event","time",…欄位}`；欄位依插入順序，欄位中的
//!   `level`、`event`、`time`（保留欄位）丟棄。
//! - `time` 為 UTC 的 `YYYY-MM-DDTHH:MM:SS.sssZ`（time crate 的 formatting + macros，
//!   `[subsecond digits:3]` 截斷到毫秒，與 `Date.toISOString()` 相同）。
//! - 最低層級與 `LEVEL_PRIORITY` 照舊版，預設 `info`；低於最低層級的訊息直接丟棄。
//! - 輸出端與時鐘以 trait 注入（對應舊版 options 的 `output`／`now`）：
//!   正式程式用 [`StdoutWriter`]＋[`SystemClock`]，測試注入收集用的輸出與固定時鐘，
//!   單元測試不等真實時間。
//! - 允許差異（X5）：Rust 的 [`SanitizedValue::Error`](super::redact::SanitizedValue::Error)
//!   沒有 `stack` 欄位（沒有對應的堆疊字串）；`processHeapUsedBytes` 等 Node 專屬欄位
//!   由舊版呼叫端（`RuntimeResourceMonitor`）加入，Logger 本體本來就沒有，Rust 端的
//!   對應元件（S3.2）也不會有。
#![allow(dead_code)]

use std::io::{self, Write};

use time::OffsetDateTime;
use time::UtcOffset;
use time::format_description::StaticFormatDescription;
use time::macros::format_description;

use super::redact::{SanitizedValue, redact_sensitive_data};

/// 舊版 `LOG_LEVELS`：層級名稱（照舊版陣列保留以利對照；層級本身用 [`LogLevel`]）。
pub(crate) const LOG_LEVELS: [&str; 4] = ["debug", "info", "warn", "error"];

/// 舊版 `LOG_EVENTS` 的 17 個事件名稱，順序照 `Logger.ts` 的物件（port 的測試依此驗證
/// 完整與不重複）。每個名稱都必須在 `doc/spec/log-events.txt` 的 158 個清單內
/// （測試驗證；清單由 `tools/ref/extract-spec.sh` 重抽，DEC-023 補抓逾時事件）。
pub(crate) const LOG_EVENTS: [&str; 17] = [
    "service_started",
    "config_loaded",
    "config_error",
    "credential_checked",
    "twitch_api_auth_failed",
    "stream_online",
    "stream_offline",
    "watch_started",
    "watch_stopped",
    "reward_claimed",
    "reward_claim_failed",
    "reward_claim_failure_threshold",
    "reward_claim_failure_recovery_refresh",
    "container_restart_requested",
    "browser_restarted",
    "page_health_failed",
    "service_stopped",
];

/// 對應舊版 `LOG_EVENTS.SERVICE_STARTED` 等具名常數（以 [`LOG_EVENTS`] 陣列為單一來源，
/// 越界索引在編譯期失敗）。
pub(crate) struct LogEvents;

impl LogEvents {
    pub const SERVICE_STARTED: &'static str = LOG_EVENTS[0];
    pub const CONFIG_LOADED: &'static str = LOG_EVENTS[1];
    pub const CONFIG_ERROR: &'static str = LOG_EVENTS[2];
    pub const CREDENTIAL_CHECKED: &'static str = LOG_EVENTS[3];
    pub const TWITCH_API_AUTH_FAILED: &'static str = LOG_EVENTS[4];
    pub const STREAM_ONLINE: &'static str = LOG_EVENTS[5];
    pub const STREAM_OFFLINE: &'static str = LOG_EVENTS[6];
    pub const WATCH_STARTED: &'static str = LOG_EVENTS[7];
    pub const WATCH_STOPPED: &'static str = LOG_EVENTS[8];
    pub const REWARD_CLAIMED: &'static str = LOG_EVENTS[9];
    pub const REWARD_CLAIM_FAILED: &'static str = LOG_EVENTS[10];
    pub const REWARD_CLAIM_FAILURE_THRESHOLD: &'static str = LOG_EVENTS[11];
    pub const REWARD_CLAIM_FAILURE_RECOVERY_REFRESH: &'static str = LOG_EVENTS[12];
    pub const CONTAINER_RESTART_REQUESTED: &'static str = LOG_EVENTS[13];
    pub const BROWSER_RESTARTED: &'static str = LOG_EVENTS[14];
    pub const PAGE_HEALTH_FAILED: &'static str = LOG_EVENTS[15];
    pub const SERVICE_STOPPED: &'static str = LOG_EVENTS[16];
}

/// 日誌層級，對應舊版 `LogLevel`（`LOG_LEVELS` 的 debug、info、warn、error）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    /// JSON `level` 欄位的字串（與 `LOG_LEVELS` 一致）。
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            LogLevel::Debug => "debug",
            LogLevel::Info => "info",
            LogLevel::Warn => "warn",
            LogLevel::Error => "error",
        }
    }

    /// 對應 `LEVEL_PRIORITY`：debug 10、info 20、warn 30、error 40。
    fn priority(self) -> u8 {
        match self {
            LogLevel::Debug => 10,
            LogLevel::Info => 20,
            LogLevel::Warn => 30,
            LogLevel::Error => 40,
        }
    }
}

/// 舊版 `RESERVED_FIELDS`：欄位中的 `level`、`event`、`time` 由 record 本身提供，丟棄。
const RESERVED_FIELDS: [&str; 3] = ["level", "event", "time"];

/// 舊版 `toISOString()` 的格式：UTC 的 `YYYY-MM-DDTHH:MM:SS.sssZ`。
/// `[subsecond digits:3]` 輸出固定 3 位毫秒（截斷），與 `Date.toISOString()` 相同。
const ISO_UTC_FORMAT: StaticFormatDescription =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z");

/// 對應舊版 options 的 `now`：回傳記錄當下的時間；offset 必須是 UTC
/// （`toISOString()` 一律輸出 UTC，[`to_iso_string`] 也會先正規化到 UTC）。
pub(crate) trait LogClock: Send + Sync {
    fn now(&self) -> OffsetDateTime;
}

/// 正式時鐘（舊版 `() => new Date()`）：系統時間的 UTC。
pub(crate) struct SystemClock;

impl LogClock for SystemClock {
    fn now(&self) -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }
}

/// 對應舊版 `LoggerOutput`：日誌輸出端抽象。正式程式用 stdout，測試注入收集用的輸出。
pub(crate) trait LogWriter: Send + Sync {
    /// 對應 `write(line)`：寫出一筆；呼叫端保證含結尾換行，輸出端不再加工。
    fn write_line(&self, line: &str);

    /// 對應 `flush?()`：等到輸出寫完。
    fn flush(&self) -> io::Result<()>;
}

/// 正式輸出端（舊版 `defaultOutput`）：寫到 stdout。
pub(crate) struct StdoutWriter;

impl LogWriter for StdoutWriter {
    fn write_line(&self, line: &str) {
        // 舊版 `process.stdout.write(line)` 不帶 callback、不處理寫入錯誤；
        // 這裡同樣忽略寫入錯誤（fire-and-forget），flush 時才回報。
        let _ = io::stdout().lock().write_all(line.as_bytes());
    }

    fn flush(&self) -> io::Result<()> {
        // 舊版 flush 等待 stdout 的寫入 callback；這裡把 LineWriter 的緩衝區
        // 推到底層並等到寫完（每行本來就以換行結尾，實際幾乎沒有殘留）。
        io::stdout().lock().flush()
    }
}

/// 對應舊版 `JsonLineLoggerOptions`：最低層級、輸出端與時鐘皆可注入（`None` 用預設）。
#[derive(Default)]
pub(crate) struct JsonLineLoggerOptions {
    /// 最低輸出層級；預設 `info`。
    pub(crate) level: Option<LogLevel>,
    /// 輸出端；預設 [`StdoutWriter`]。
    pub(crate) output: Option<Box<dyn LogWriter>>,
    /// 時鐘；預設 [`SystemClock`]。
    pub(crate) clock: Option<Box<dyn LogClock>>,
}

/// 對應舊版 `JsonLineLogger`：每筆一行 JSON 的日誌器。
pub(crate) struct JsonLineLogger {
    /// 舊版 `minimumPriority`。
    minimum_priority: u8,
    output: Box<dyn LogWriter>,
    clock: Box<dyn LogClock>,
}

impl JsonLineLogger {
    pub(crate) fn new(options: JsonLineLoggerOptions) -> Self {
        let level = options.level.unwrap_or(LogLevel::Info);
        Self {
            minimum_priority: level.priority(),
            output: options.output.unwrap_or_else(|| Box::new(StdoutWriter)),
            clock: options.clock.unwrap_or_else(|| Box::new(SystemClock)),
        }
    }

    pub(crate) fn debug(&self, event: &str, fields: Option<&SanitizedValue>) {
        self.log(LogLevel::Debug, event, fields);
    }

    pub(crate) fn info(&self, event: &str, fields: Option<&SanitizedValue>) {
        self.log(LogLevel::Info, event, fields);
    }

    pub(crate) fn warn(&self, event: &str, fields: Option<&SanitizedValue>) {
        self.log(LogLevel::Warn, event, fields);
    }

    pub(crate) fn error(&self, event: &str, fields: Option<&SanitizedValue>) {
        self.log(LogLevel::Error, event, fields);
    }

    /// 對應舊版 `flush()`：等到輸出端寫完；失敗交給呼叫端處理（§7.9 的 `logger_flush_failed`）。
    pub(crate) async fn flush(&self) -> io::Result<()> {
        self.output.flush()
    }

    /// 對應舊版私有 `log`：先依層級過濾，再組出 `{"level","event","time",…欄位}` 的一行。
    fn log(&self, level: LogLevel, event: &str, fields: Option<&SanitizedValue>) {
        // 舊版：低於最低層級的訊息直接丟棄（連時間都不計算）。
        if level.priority() < self.minimum_priority {
            return;
        }

        // 舊版 `fields = {}` 的預設參數。
        let empty = SanitizedValue::Object(vec![]);
        let fields = fields.unwrap_or(&empty);

        let mut entries = vec![
            (
                "level".to_string(),
                SanitizedValue::String(level.as_str().to_string()),
            ),
            (
                "event".to_string(),
                SanitizedValue::String(event.to_string()),
            ),
            (
                "time".to_string(),
                SanitizedValue::String(to_iso_string(self.clock.now())),
            ),
        ];
        // 舊版 `...redactLogFields(fields)`：保留欄位丟棄，其餘照插入順序接在後面。
        entries.extend(redact_log_fields(fields));
        let record = SanitizedValue::Object(entries);

        let mut line = record.to_json_string();
        line.push('\n');
        self.output.write_line(&line);
    }
}

/// 對應舊版 `createLogger`。
pub(crate) fn create_logger(options: JsonLineLoggerOptions) -> JsonLineLogger {
    JsonLineLogger::new(options)
}

/// 對應舊版 `redactLogFields`：消毒整個欄位值；是物件時丟棄保留欄位、其餘照插入順序，
/// 不是物件（null、陣列、純值）時整包放進 `fields` 欄位。
///
/// 消毒的輸出不會是 `Map`／`Error`／`Shared`（分別照舊版輸出成物件或被展開）；
/// `match` 的其餘分支只是防衛性處理，實際不會走到。
fn redact_log_fields(fields: &SanitizedValue) -> Vec<(String, SanitizedValue)> {
    let sanitized = redact_sensitive_data(fields);
    match sanitized {
        SanitizedValue::Object(entries) => entries
            .into_iter()
            .filter(|(key, _)| !RESERVED_FIELDS.contains(&key.as_str()))
            .collect(),
        // 舊版：`sanitized === null || Array.isArray(sanitized) || typeof !== 'object'`。
        other => vec![("fields".to_string(), other)],
    }
}

/// 把時鐘的時間轉成舊版 `toISOString()` 的字串：先正規化到 UTC（時鐘契約是 UTC，
/// 此處照 `toISOString` 的行為一律轉換），再以固定格式輸出 `YYYY-MM-DDTHH:MM:SS.sssZ`。
fn to_iso_string(datetime: OffsetDateTime) -> String {
    // 年份轉換超出 time 支援範圍才會失敗；時鐘提供的 datetime 不可能，照 redact 的
    // `compile_pattern` 前例以明確訊息終止。
    let utc = match datetime.checked_to_offset(UtcOffset::UTC) {
        Some(utc) => utc,
        None => panic!("log clock returned a datetime outside the supported range"),
    };
    match utc.format(&ISO_UTC_FORMAT) {
        Ok(formatted) => formatted,
        Err(error) => panic!("failed to format log timestamp: {error}"),
    }
}

#[cfg(test)]
mod tests;
