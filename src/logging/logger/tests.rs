//! 日誌本體的單元測試：移植舊版 `test/unit/logging.test.ts` 與 Logger 直接相關的 5 個案例
//! （層級過濾、輸出格式、保留欄位、LOG_EVENTS、flush；§7.7）。消毒相關的 6 個已在
//! `redact/tests.rs` 移植。時間以注入的固定時鐘提供，不等真實時間。
use std::collections::HashSet;
use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use regex::Regex;
use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time};

use super::{JsonLineLogger, JsonLineLoggerOptions, LOG_EVENTS, LogClock, LogLevel, LogWriter};
use crate::logging::redact::{LogError, SanitizedValue};

/// 舊測試的 `FIXED_TIME`：`2026-06-14T12:00:00.000Z`（`Date.toISOString()` 的輸出）。
const FIXED_TIME_JSON: &str = "2026-06-14T12:00:00.000Z";

/// 測試輔助：字串欄位值。
fn s(value: &str) -> SanitizedValue {
    SanitizedValue::String(value.to_string())
}

/// 測試輔助：物件欄位值（插入順序保留）。
fn obj(entries: Vec<(&str, SanitizedValue)>) -> SanitizedValue {
    SanitizedValue::Object(
        entries
            .into_iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect(),
    )
}

/// 對應舊測試的 `now: () => FIXED_TIME`：回傳固定時間的時鐘。
struct FixedClock(OffsetDateTime);

impl LogClock for FixedClock {
    fn now(&self) -> OffsetDateTime {
        self.0
    }
}

/// 舊測試的 `FIXED_TIME`（2026-06-14T12:00:00.000Z）以 time crate 建構。
fn fixed_time() -> OffsetDateTime {
    let date = Date::from_calendar_date(2026, Month::June, 14).expect("固定的測試日期應有效");
    let time = Time::from_hms_milli(12, 0, 0, 0).expect("固定的測試時間應有效");
    PrimitiveDateTime::new(date, time).assume_utc()
}

/// 模擬舊測試的 `lines` 陣列：把每一行收集到共用的緩衝區。
struct BufferWriter {
    lines: Arc<Mutex<Vec<String>>>,
}

impl BufferWriter {
    fn new() -> Self {
        Self {
            lines: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn shared(&self) -> Arc<Mutex<Vec<String>>> {
        Arc::clone(&self.lines)
    }
}

impl LogWriter for BufferWriter {
    fn write_line(&self, line: &str) {
        self.lines
            .lock()
            .expect("buffer lock")
            .push(line.to_string());
    }

    fn flush(&self) -> io::Result<()> {
        Ok(())
    }
}

/// 對應舊測試的 `createCapture`：收集輸出的 logger；層級與時鐘照參數注入。
fn capture(level: LogLevel) -> (JsonLineLogger, Arc<Mutex<Vec<String>>>) {
    let writer = BufferWriter::new();
    let lines = writer.shared();
    let logger = JsonLineLogger::new(JsonLineLoggerOptions {
        level: Some(level),
        output: Some(Box::new(writer)),
        clock: Some(Box::new(FixedClock(fixed_time()))),
    });
    (logger, lines)
}

/// 取出緩衝區內容（測試輔助）。
fn lines_of(buffer: &Arc<Mutex<Vec<String>>>) -> std::sync::MutexGuard<'_, Vec<String>> {
    buffer.lock().expect("buffer lock")
}

/// 預期的一行：record 的欄位順序固定為 level、event、time，再接呼叫端的欄位
/// （`fields_json` 為該段的 JSON 片段；空字串代表沒有欄位）。
fn expected_line(level: LogLevel, event: &str, fields_json: &str) -> String {
    let mut line = format!(
        "{{\"level\":\"{}\",\"event\":\"{event}\",\"time\":\"{FIXED_TIME_JSON}\"",
        level.as_str()
    );
    if !fields_json.is_empty() {
        line.push(',');
        line.push_str(fields_json);
    }
    line.push_str("}\n");
    line
}

// 原測試：輸出 debug、info、warn 與 error 訊息
// 舊版用 JSON.parse 後逐一比對欄位；這裡直接比對整行（額外涵蓋 §7.7 要求的插入順序）。
#[test]
fn outputs_debug_info_warn_and_error_messages() {
    let (logger, buffer) = capture(LogLevel::Debug);

    logger.debug(
        "debug_event",
        Some(&obj(vec![("value", SanitizedValue::Number(1.0))])),
    );
    logger.info(
        "info_event",
        Some(&obj(vec![("value", SanitizedValue::Number(2.0))])),
    );
    logger.warn(
        "warn_event",
        Some(&obj(vec![("value", SanitizedValue::Number(3.0))])),
    );
    logger.error(
        "error_event",
        Some(&obj(vec![("value", SanitizedValue::Number(4.0))])),
    );

    let lines = lines_of(&buffer);
    assert_eq!(lines.len(), 4);
    assert_eq!(
        lines[0],
        expected_line(LogLevel::Debug, "debug_event", "\"value\":1")
    );
    assert_eq!(
        lines[1],
        expected_line(LogLevel::Info, "info_event", "\"value\":2")
    );
    assert_eq!(
        lines[2],
        expected_line(LogLevel::Warn, "warn_event", "\"value\":3")
    );
    assert_eq!(
        lines[3],
        expected_line(LogLevel::Error, "error_event", "\"value\":4")
    );
}

// 原測試：依最低層級過濾訊息
#[test]
fn filters_messages_by_minimum_level() {
    let (logger, buffer) = capture(LogLevel::Warn);

    logger.debug("debug_event", None);
    logger.info("info_event", None);
    logger.warn("warn_event", None);
    logger.error("error_event", None);

    let lines = lines_of(&buffer);
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0], expected_line(LogLevel::Warn, "warn_event", ""));
    assert_eq!(lines[1], expected_line(LogLevel::Error, "error_event", ""));
}

// 原測試：每次輸出恰好一行可解析 JSON 並保護保留欄位
// 舊版用 JSON.parse 驗證可解析；這裡以 serde_json 解析一次，再比對整行。
#[test]
fn writes_exactly_one_parseable_json_line_and_protects_reserved_fields() {
    let (logger, buffer) = capture(LogLevel::Debug);

    logger.info(
        "format_event",
        Some(&obj(vec![
            ("level", s("forged")),
            ("event", s("forged")),
            ("time", s("forged")),
            ("message", s("first line\nsecond line")),
        ])),
    );

    let lines = lines_of(&buffer);
    assert_eq!(lines.len(), 1);
    let line = &lines[0];
    // 恰好一行：結尾一個換行，其餘位置沒有換行（message 內的換行被 JSON 跳脫）。
    assert!(line.ends_with('\n'));
    assert!(!line[..line.len() - 1].contains('\n'));
    // 可解析的 JSON。
    serde_json::from_str::<serde_json::Value>(line).expect("應為可解析的 JSON");
    // 呼叫端欄位裡的 level、event、time 被丟棄（整行不含 forged）。
    assert_eq!(
        line,
        "{\"level\":\"info\",\"event\":\"format_event\",\"time\":\"2026-06-14T12:00:00.000Z\",\"message\":\"first line\\nsecond line\"}\n"
    );
}

// 原測試：提供完整且不重複的必要事件名稱，且每個事件都有標準欄位
// LOG_EVENTS 的值與順序照舊版陣列（舊版以 Object.values 取出）。
#[test]
fn provides_complete_and_unique_required_event_names_with_standard_fields() {
    assert_eq!(
        LOG_EVENTS,
        [
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
        ]
    );
    // 不重複。
    assert_eq!(
        LOG_EVENTS.iter().collect::<HashSet<_>>().len(),
        LOG_EVENTS.len()
    );

    let (logger, buffer) = capture(LogLevel::Debug);
    for event in LOG_EVENTS {
        logger.info(event, Some(&obj(vec![("channel", s("streamer_one"))])));
    }

    let lines = lines_of(&buffer);
    assert_eq!(lines.len(), LOG_EVENTS.len());
    for (index, event) in LOG_EVENTS.iter().enumerate() {
        assert_eq!(
            lines[index],
            expected_line(LogLevel::Info, event, "\"channel\":\"streamer_one\"")
        );
    }
}

// 補充測試：事件名稱只能用 doc/spec/log-events.txt 裡的（149 個）。舊版的驗證機制是
// extract-spec.sh 從 LOG_EVENTS 抽出清單、A/B 比對（tools/bench/log-schema.py）時檢查
// 輸出的事件都在清單內；這裡在編譯載入時以 include_str! 驗證 LOG_EVENTS 的每個名稱
// 都在清單內。
#[test]
fn log_events_names_are_within_log_events_spec() {
    let known_events = include_str!("../../../doc/spec/log-events.txt");
    for event in LOG_EVENTS {
        assert!(
            known_events.lines().any(|known| known == event),
            "事件 {event} 不在 doc/spec/log-events.txt"
        );
    }
}

// 原測試：flush 委派給注入的輸出目的地
// 舊版以 vi.fn() 記錄 flush 呼叫；這裡用計數的輸出端（LogWriter 實作在 Arc 上，
// 讓 logger 持有輸出端的同時，測試仍能讀取計數）。
#[tokio::test]
async fn flush_delegates_to_injected_output() {
    struct FlushSpy {
        flush_count: AtomicUsize,
    }

    impl LogWriter for Arc<FlushSpy> {
        fn write_line(&self, _line: &str) {}
        fn flush(&self) -> io::Result<()> {
            self.flush_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    let spy = Arc::new(FlushSpy {
        flush_count: AtomicUsize::new(0),
    });
    let logger = JsonLineLogger::new(JsonLineLoggerOptions {
        output: Some(Box::new(Arc::clone(&spy))),
        ..Default::default()
    });

    logger.flush().await.expect("flush 應成功");

    assert_eq!(spy.flush_count.load(Ordering::SeqCst), 1);
}

// 補充測試：未指定 level 時預設 info（照舊版 `options.level ?? 'info'`）；
// 輸出端仍注入 Buffer，避免測試寫到真實 stdout。
#[test]
fn default_level_is_info() {
    let writer = BufferWriter::new();
    let buffer = writer.shared();
    let logger = JsonLineLogger::new(JsonLineLoggerOptions {
        output: Some(Box::new(writer)),
        clock: Some(Box::new(FixedClock(fixed_time()))),
        ..Default::default()
    });

    logger.debug("debug_event", None);
    logger.info("info_event", None);

    let lines = lines_of(&buffer);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0], expected_line(LogLevel::Info, "info_event", ""));
}

// 補充測試：毫秒以下照 `Date.toISOString()` 截斷（不進位）。
#[test]
fn time_truncates_subsecond_to_milliseconds() {
    let writer = BufferWriter::new();
    let buffer = writer.shared();
    let time = Time::from_hms_nano(12, 0, 0, 123_456_789).expect("固定的測試時間應有效");
    let datetime = PrimitiveDateTime::new(
        Date::from_calendar_date(2026, Month::June, 14).expect("固定的測試日期應有效"),
        time,
    )
    .assume_utc();
    let logger = JsonLineLogger::new(JsonLineLoggerOptions {
        output: Some(Box::new(writer)),
        clock: Some(Box::new(FixedClock(datetime))),
        ..Default::default()
    });

    logger.info("info_event", None);

    let lines = lines_of(&buffer);
    assert_eq!(
        lines[0],
        "{\"level\":\"info\",\"event\":\"info_event\",\"time\":\"2026-06-14T12:00:00.123Z\"}\n"
    );
}

// 補充測試：SystemClock 的輸出符合 UTC `YYYY-MM-DDTHH:MM:SS.sssZ` 的形狀（不驗證特定時刻）。
#[test]
fn system_clock_formats_utc_timestamp_shape() {
    let writer = BufferWriter::new();
    let buffer = writer.shared();
    let logger = JsonLineLogger::new(JsonLineLoggerOptions {
        output: Some(Box::new(writer)),
        ..Default::default()
    });

    logger.info("info_event", None);

    let lines = lines_of(&buffer);
    let record: serde_json::Value = serde_json::from_str(&lines[0]).expect("應為可解析的 JSON");
    let time_value = record["time"].as_str().expect("time 欄位應存在");
    let shape = Regex::new(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$").expect("樣式應有效");
    assert!(shape.is_match(time_value), "時間形狀不符：{time_value}");
}

// 補充測試：欄位值不是物件時（照舊版 `redactLogFields` 的 null／陣列／非物件分支）
// 整包放進 `fields` 欄位。
#[test]
fn wraps_non_object_fields_under_fields_key() {
    let (logger, buffer) = capture(LogLevel::Debug);

    logger.info(
        "info_event",
        Some(&SanitizedValue::Array(vec![SanitizedValue::Number(1.0)])),
    );
    logger.info("second_event", Some(&SanitizedValue::Null));

    let lines = lines_of(&buffer);
    assert_eq!(
        lines[0],
        expected_line(LogLevel::Info, "info_event", "\"fields\":[1]")
    );
    assert_eq!(
        lines[1],
        expected_line(LogLevel::Info, "second_event", "\"fields\":null")
    );
}

// 補充測試：Error 輸出 {name, message, …其他欄位}，沒有 stack 欄位
// （X5：Rust 沒有對應的堆疊字串；也不會出現 processHeapUsedBytes 等 Node 專屬欄位，
//  舊版那些欄位由 RuntimeResourceMonitor 呼叫端加入，Logger 本體本來就沒有）。
#[test]
fn error_field_output_has_no_stack() {
    let (logger, buffer) = capture(LogLevel::Debug);

    logger.error(
        "uncaught_exception",
        Some(&obj(vec![(
            "error",
            SanitizedValue::Error(LogError {
                name: "Error".to_string(),
                message: "request failed".to_string(),
                cause: None,
                extra: vec![],
            }),
        )])),
    );

    let lines = lines_of(&buffer);
    assert_eq!(lines.len(), 1);
    assert_eq!(
        lines[0],
        "{\"level\":\"error\",\"event\":\"uncaught_exception\",\"time\":\"2026-06-14T12:00:00.000Z\",\"error\":{\"name\":\"Error\",\"message\":\"request failed\"}}\n"
    );
    assert!(!lines[0].contains("stack"));
}
