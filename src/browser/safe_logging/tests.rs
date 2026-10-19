//! safe_logging 的單元測試：舊版 `safe-logging.ts` 沒有專屬測試檔（舊測試清單亦無
//! 對應標題），此處為補測——行為對照舊版使用方式，消毒結果以 `redact`／`logger`
//! 的既有測試風格驗證。
use std::io;
use std::sync::{Arc, Mutex};

use crate::logging::logger::{JsonLineLogger, JsonLineLoggerOptions, LogLevel, LogWriter};
use crate::logging::redact::{REDACTED_VALUE, SanitizedValue};

use super::{UNKNOWN_CHANNEL_SESSION_FAILURE, safe_error_message, safe_log};

/// 測試輔助：把每一行收集到共用緩衝區的輸出端（比照 `logger/tests.rs`）。
struct BufferWriter {
    lines: Arc<Mutex<Vec<String>>>,
}

impl BufferWriter {
    fn new() -> (Self, Arc<Mutex<Vec<String>>>) {
        let lines = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                lines: Arc::clone(&lines),
            },
            lines,
        )
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

/// 測試輔助：收集輸出的 logger（層級全開；時間不驗證，用預設時鐘）。
fn capture() -> (JsonLineLogger, Arc<Mutex<Vec<String>>>) {
    let (writer, lines) = BufferWriter::new();
    let logger = JsonLineLogger::new(JsonLineLoggerOptions {
        level: Some(LogLevel::Debug),
        output: Some(Box::new(writer)),
        ..Default::default()
    });
    (logger, lines)
}

// 補測：error 為 undefined／null（`None`）時回傳預設訊息。
#[test]
fn safe_error_message_defaults_for_missing_error() {
    assert_eq!(safe_error_message(None), UNKNOWN_CHANNEL_SESSION_FAILURE);
    assert_eq!(
        safe_error_message(None),
        "Unknown channel session failure".to_string()
    );
}

// 補測：訊息一律經過敏感字串消毒（對應舊版回傳前套 redactSensitiveString）。
#[test]
fn safe_error_message_redacts_sensitive_strings() {
    assert_eq!(
        safe_error_message(Some("request failed with Bearer abc123")),
        format!("request failed with Bearer {REDACTED_VALUE}")
    );
}

// 補測：safeLog 依層級轉發，且欄位照 Logger 既有路徑消毒。
#[test]
fn safe_log_forwards_level_event_and_redacts_fields() {
    let levels = [
        (LogLevel::Debug, "debug"),
        (LogLevel::Info, "info"),
        (LogLevel::Warn, "warn"),
        (LogLevel::Error, "error"),
    ];
    for (level, level_str) in levels {
        let (logger, lines) = capture();
        let fields = SanitizedValue::Object(vec![(
            "token".to_string(),
            SanitizedValue::String("secret-token".to_string()),
        )]);

        safe_log(&logger, level, "session_event", &fields);

        let lines = lines.lock().expect("buffer lock");
        assert_eq!(lines.len(), 1, "level {level_str} 應輸出一筆");
        assert!(
            lines[0].starts_with(&format!(
                "{{\"level\":\"{level_str}\",\"event\":\"session_event\""
            )),
            "level {level_str} 轉發錯誤：{}",
            lines[0]
        );
        assert!(
            lines[0].contains(&format!("\"token\":\"{REDACTED_VALUE}\"")),
            "敏感鍵應被消毒：{}",
            lines[0]
        );
    }
}
