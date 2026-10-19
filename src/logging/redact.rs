//! 日誌消毒：`sanitizeValue`、`sanitizeError`、`isSensitiveKey`、`redactSensitiveString`
//! 逐函式移植（§7.7，行為依據舊版 `src/logging/Logger.ts`）。
//!
//! 與 JS 的型別對應（§7.7）：
//! - 值模型是 [`SanitizedValue`]，分支對照舊版 `sanitizeValue` 處理的 JS 值。
//! - 字串一律經過 [`redact_sensitive_string`]；敏感鍵的值換成 `[REDACTED]`。
//! - 非有限數字（NaN、±Infinity）在消毒時轉成 JS 同款字串；數字輸出用 ryu-js，
//!   與 `JSON.stringify` 相同（`1e21` → `1e+21`、`0.1`、`-0` → `0`）。
//! - `[Circular]` 由 [`SanitizedValue::Shared`]（`Rc` 共用節點）重現循環參照後照舊版
//!   WeakSet 的路徑祖先偵測觸發；`[Undefined]` 對應 `Undefined`。
//! - Rust 型別不可能出現的情況不模擬：getter（`[Accessor]`）、symbol、throw 的
//!   getter（`[Unserializable]`）、Date 物件（呼叫端先轉成 ISO 字串，`[Invalid Date]`
//!   不會出現）——常數仍照舊保留以利對照。
#![allow(dead_code)]

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::OnceLock;

use regex::Regex;

/// 舊版 `REDACTED_VALUE`：敏感鍵的值一律換成這個字串。
pub(crate) const REDACTED_VALUE: &str = "[REDACTED]";
/// 舊版 `CIRCULAR_VALUE`：值成為自己的路徑祖先（循環參照）時的輸出。
pub(crate) const CIRCULAR_VALUE: &str = "[Circular]";
/// 舊版 `OMITTED_VALUE`：JS `undefined` 的輸出。
const OMITTED_VALUE: &str = "[Undefined]";
/// 舊版 `ACCESSOR_VALUE`：Rust 值模型沒有 getter，常數照舊保留（§7.7）。
const ACCESSOR_VALUE: &str = "[Accessor]";
/// 舊版 `UNSERIALIZABLE_VALUE`：舊版由 try/catch 觸發；Rust 的消毒走訪不會丟例外，
/// 常數照舊保留（§7.7）。
const UNSERIALIZABLE_VALUE: &str = "[Unserializable]";
/// 舊版 `[Invalid Date]`：Rust 值模型沒有 Date（呼叫端先轉成 ISO 字串），常數照舊保留（§7.7）。
const INVALID_DATE_VALUE: &str = "[Invalid Date]";

/// 日誌值模型，對應舊版 `sanitizeValue` 處理的 JS 值（§7.7）。
///
/// - `Undefined` 對應 JS `undefined`，消毒後輸出 `[Undefined]`。
/// - `Number` 用 f64；非有限值在消毒時轉成字串（照舊版 `String(value)`）。
/// - `Object`／`Map` 的鍵值對保留插入順序（R10）；`Map` 的鍵在建模時先 `String()` 化。
/// - `Shared` 讓物件可以共用與構成循環，對應舊版 WeakSet 的 `[Circular]` 偵測；
///   一般的擁有型巢狀直接用 `Object`／`Array`。
/// - JS 的 bigint、symbol、function、Date、Set 在 Rust 值模型沒有對應：
///   bigint 由呼叫端以字串傳入（避免 f64 失真）、Set 用 `Array` 表示，
///   其餘見模組文件。
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum SanitizedValue {
    Null,
    Undefined,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<SanitizedValue>),
    /// 插入順序保留（R10）。
    Object(Vec<(String, SanitizedValue)>),
    /// 對應 JS `Map`：鍵已在建模時 `String()` 化，消毒後輸出成 `Object`（照舊版 `sanitizeMap`）。
    Map(Vec<(String, SanitizedValue)>),
    /// 對應 JS `Error`，消毒後輸出 `{name, message, cause?, …其他欄位}`（照舊版 `sanitizeError`）。
    Error(LogError),
    /// 可共用的節點（`Rc`），可能構成循環參照；消毒時照舊版 WeakSet 偵測 `[Circular]`。
    Shared(Rc<RefCell<SanitizedValue>>),
}

/// 記錄用的錯誤值：對應舊版 `sanitizeError` 的 Error 輸入。
///
/// `stack` 依允許差異 X5 省略（Rust 沒有對應的堆疊字串）。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LogError {
    pub name: String,
    pub message: String,
    /// 對應 `error.cause`；`None` 表示沒有這個屬性（`Some(Undefined)` 表示屬性存在但值是 undefined）。
    pub cause: Option<Box<SanitizedValue>>,
    /// 其他 enumerable 欄位，插入順序照呼叫端（對應 `Object.assign` 上去的屬性）。
    pub extra: Vec<(String, SanitizedValue)>,
}

/// 對應舊版 `redactSensitiveData`：以全新的祖先集合消毒整棵值樹。
pub(crate) fn redact_sensitive_data(value: &SanitizedValue) -> SanitizedValue {
    sanitize_value(value, &mut HashSet::new())
}

/// 對應舊版 `sanitizeValue`。`ancestors` 記錄當前路徑上 `Shared` 節點的位址
/// （對應舊版的 WeakSet：進入前加入、離開時移除，兄弟節點的重複不算循環）。
fn sanitize_value(value: &SanitizedValue, ancestors: &mut HashSet<usize>) -> SanitizedValue {
    match value {
        SanitizedValue::Null => SanitizedValue::Null,
        SanitizedValue::Undefined => SanitizedValue::String(OMITTED_VALUE.to_string()),
        SanitizedValue::Bool(inner) => SanitizedValue::Bool(*inner),
        // 非有限數字照舊版 `String(value)` 轉成字串。
        SanitizedValue::Number(inner) => {
            if inner.is_finite() {
                SanitizedValue::Number(*inner)
            } else {
                SanitizedValue::String(js_number_to_string(*inner))
            }
        }
        SanitizedValue::String(inner) => SanitizedValue::String(redact_sensitive_string(inner)),
        SanitizedValue::Array(items) => SanitizedValue::Array(
            items
                .iter()
                .map(|item| sanitize_value(item, ancestors))
                .collect(),
        ),
        SanitizedValue::Object(entries) => {
            SanitizedValue::Object(sanitize_entries(entries, ancestors))
        }
        // 舊版 `sanitizeMap` 也輸出成普通 object。
        SanitizedValue::Map(entries) => {
            SanitizedValue::Object(sanitize_entries(entries, ancestors))
        }
        SanitizedValue::Error(error) => sanitize_error(error, ancestors),
        SanitizedValue::Shared(node) => {
            let address = Rc::as_ptr(node) as *const u8 as usize;
            if !ancestors.insert(address) {
                return SanitizedValue::String(CIRCULAR_VALUE.to_string());
            }
            // 借用衝突（記錄期間持有可變借用）是呼叫端程式錯誤，等同舊版走訪丟例外的情形。
            let borrowed = node.borrow();
            let sanitized = sanitize_value(&borrowed, ancestors);
            ancestors.remove(&address);
            sanitized
        }
    }
}

/// 對應舊版 `sanitizeError`：輸出 `{name, message, cause?, …其他欄位}`。
///
/// 舊版在 `message` 之後可能輸出 `stack`；Rust 沒有堆疊字串，依允許差異 X5 省略。
/// 舊版對 accessor 形式的屬性輸出 `[Accessor]`，Rust 值模型沒有 getter，不會發生。
fn sanitize_error(error: &LogError, ancestors: &mut HashSet<usize>) -> SanitizedValue {
    let mut entries = vec![
        (
            "name".to_string(),
            SanitizedValue::String(redact_sensitive_string(&error.name)),
        ),
        (
            "message".to_string(),
            SanitizedValue::String(redact_sensitive_string(&error.message)),
        ),
    ];

    if let Some(cause) = &error.cause {
        entries.push(("cause".to_string(), sanitize_value(cause, ancestors)));
    }

    for (key, value) in &error.extra {
        let sanitized = if is_sensitive_key(key) {
            SanitizedValue::String(REDACTED_VALUE.to_string())
        } else {
            sanitize_value(value, ancestors)
        };
        entries.push((key.clone(), sanitized));
    }

    SanitizedValue::Object(entries)
}

/// 物件／Map 共用的欄位消毒：敏感鍵整個換成 `[REDACTED]`，其餘遞迴消毒。
fn sanitize_entries(
    entries: &[(String, SanitizedValue)],
    ancestors: &mut HashSet<usize>,
) -> Vec<(String, SanitizedValue)> {
    entries
        .iter()
        .map(|(key, value)| {
            let sanitized = if is_sensitive_key(key) {
                SanitizedValue::String(REDACTED_VALUE.to_string())
            } else {
                sanitize_value(value, ancestors)
            };
            (key.clone(), sanitized)
        })
        .collect()
}

/// 對應舊版 `isSensitiveKey`：鍵轉小寫、去掉非 `[a-z0-9]` 後比對子字串與私人 identifier 字尾。
///
/// 舊版的 `toLocaleLowerCase('en-US')` 以 Rust 的全 Unicode `to_lowercase()` 對應：
/// 會產生 ASCII 字母的輸入（如 `İ` → `i` + 結合符號）在兩邊都化約到相同的 `[a-z0-9]` 內容。
fn is_sensitive_key(key: &str) -> bool {
    let normalized: String = key
        .to_lowercase()
        .chars()
        .filter(|c| matches!(c, 'a'..='z' | '0'..='9'))
        .collect();

    normalized.contains("cookie")
        || normalized.contains("token")
        || normalized.contains("oauth")
        || normalized.contains("authorization")
        || normalized.contains("storagestate")
        || private_identifier_suffix_pattern().is_match(&normalized)
}

/// 對應舊版 `redactSensitiveString`：依序套用 Bearer、廣泛指派、token 指派與
/// 私人 identifier 指派四組遮罩樣式。
pub(crate) fn redact_sensitive_string(value: &str) -> String {
    let mut result = value.to_string();
    result = bearer_token_pattern()
        .replace_all(&result, format!("Bearer {REDACTED_VALUE}"))
        .into_owned();
    result = broad_assignment_pattern()
        .replace_all(&result, format!("${{1}}{REDACTED_VALUE}"))
        .into_owned();
    result = token_assignment_pattern()
        .replace_all(&result, format!("${{1}}{REDACTED_VALUE}"))
        .into_owned();
    result = private_identifier_assignment_pattern()
        .replace_all(&result, format!("${{1}}{REDACTED_VALUE}"))
        .into_owned();
    result
}

/// JS 的 `\s`（White_Space 加行終止符）展開成明確字元類別（§7.7）：
/// Rust 內建的 `\s` 多了 `\u{85}`、少了 `\u{feff}`。
const JS_WHITESPACE_CLASS: &str = r"[\t\n\x0B\x0C\r \u{a0}\u{1680}\u{2000}-\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}\u{feff}]";

/// 編譯固定樣式。樣式是程式內常數且每條都有測試覆蓋，編譯失敗代表實作錯誤，
/// 直接以明確訊息終止（依專案規範不用 `unwrap()`／`expect()`）。
fn compile_pattern(source: &str) -> Regex {
    match Regex::new(source) {
        Ok(regex) => regex,
        Err(error) => panic!("invalid built-in log redaction pattern {source:?}: {error}"),
    }
}

fn bearer_token_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        // 舊版：/\bBearer\s+[^\s"',;]+/giu
        // `\b` 在 JS 只認 ASCII 單字字元 → `(?-u:\b)`；`i` → `(?i)`；`\s` → 展開字元類別。
        compile_pattern(&format!(
            r#"(?i)(?-u:\b)Bearer{ws}+[^{ws}"',;]+"#,
            ws = JS_WHITESPACE_CLASS
        ))
    })
}

fn broad_assignment_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        // 舊版：/(["']?\b(?:cookies?|authorization|storage[\s_-]?state)\b["']?\s*[:=]\s*)(?:"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|[^\r\n]+)/giu
        compile_pattern(&format!(
            r#"(?i)(["']?(?-u:\b)(?:cookies?|authorization|storage[{ws}_-]?state)(?-u:\b)["']?[{ws}]*[:=][{ws}]*)(?:"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|[^\r\n]+)"#,
            ws = JS_WHITESPACE_CLASS
        ))
    })
}

fn token_assignment_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        // 舊版：/(["']?\b(?:token|access[\s_-]?token|oauth(?:[\s_-]?token)?)\b["']?\s*[:=]\s*)(?:"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|[^\s,;]+)/giu
        compile_pattern(&format!(
            r#"(?i)(["']?(?-u:\b)(?:token|access[{ws}_-]?token|oauth(?:[{ws}_-]?token)?)(?-u:\b)["']?[{ws}]*[:=][{ws}]*)(?:"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|[^{ws},;]+)"#,
            ws = JS_WHITESPACE_CLASS
        ))
    })
}

fn private_identifier_assignment_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        // 舊版：/(["']?\b(?:chat|channel|user|guild)[\s_-]?id\b["']?\s*[:=]\s*)(?:"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|[^\s,;]+)/giu
        compile_pattern(&format!(
            r#"(?i)(["']?(?-u:\b)(?:chat|channel|user|guild)[{ws}_-]?id(?-u:\b)["']?[{ws}]*[:=][{ws}]*)(?:"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|[^{ws},;]+)"#,
            ws = JS_WHITESPACE_CLASS
        ))
    })
}

/// 舊版：`/(?:chat|channel|user|guild)ids?$/u`（輸入已先小寫化與正規化，無需 `i` 旗標）。
const PRIVATE_IDENTIFIER_SUFFIX_PATTERN: &str = r"(?:chat|channel|user|guild)ids?$";

fn private_identifier_suffix_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| compile_pattern(PRIVATE_IDENTIFIER_SUFFIX_PATTERN))
}

/// 把 f64 轉成 JS `String(number)`／`JSON.stringify` 同款字串（§7.7，ryu-js）：
/// `0.1` → `0.1`、`1e21` → `1e+21`、`-0` → `0`、`NaN` → `NaN`、`Infinity` → `Infinity`。
pub(crate) fn js_number_to_string(value: f64) -> String {
    let mut buffer = ryu_js::Buffer::new();
    buffer.format(value).to_string()
}

impl SanitizedValue {
    /// 對應 `JSON.stringify(value)`（不含換行）：數字用 ryu-js（§7.7）；
    /// 物件欄位中的 `Undefined` 省略、陣列中的 `Undefined` 輸出 `null`，與 JS 相同。
    /// 供消毒後的值（logger 輸出）使用；`Shared` 循環以防衛性的 `[Circular]` 輸出。
    pub(crate) fn to_json_string(&self) -> String {
        let mut out = String::new();
        let mut shared = HashSet::new();
        self.write_json(&mut out, &mut shared);
        out
    }

    fn write_json(&self, out: &mut String, shared: &mut HashSet<usize>) {
        match self {
            SanitizedValue::Null => out.push_str("null"),
            // 陣列中的 undefined 在 JSON.stringify 是 null；物件欄位見 write_json_object。
            SanitizedValue::Undefined => out.push_str("null"),
            SanitizedValue::Bool(inner) => out.push_str(if *inner { "true" } else { "false" }),
            SanitizedValue::Number(inner) => out.push_str(&js_number_to_string(*inner)),
            SanitizedValue::String(inner) => write_json_string(inner, out),
            SanitizedValue::Array(items) => {
                out.push('[');
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    item.write_json(out, shared);
                }
                out.push(']');
            }
            SanitizedValue::Object(entries) | SanitizedValue::Map(entries) => {
                write_json_object(
                    entries.iter().map(|(key, value)| (key.as_str(), value)),
                    out,
                    shared,
                );
            }
            SanitizedValue::Error(error) => {
                // 消毒後不會殘留 Error；此處供未消毒的值防衛性輸出，欄位順序照 sanitizeError。
                let mut entries: Vec<(String, SanitizedValue)> = vec![
                    (
                        "name".to_string(),
                        SanitizedValue::String(error.name.clone()),
                    ),
                    (
                        "message".to_string(),
                        SanitizedValue::String(error.message.clone()),
                    ),
                ];
                if let Some(cause) = &error.cause {
                    entries.push(("cause".to_string(), cause.as_ref().clone()));
                }
                entries.extend(error.extra.iter().cloned());
                write_json_object(
                    entries.iter().map(|(key, value)| (key.as_str(), value)),
                    out,
                    shared,
                );
            }
            SanitizedValue::Shared(node) => {
                let address = Rc::as_ptr(node) as *const u8 as usize;
                if !shared.insert(address) {
                    write_json_string(CIRCULAR_VALUE, out);
                } else {
                    node.borrow().write_json(out, shared);
                    shared.remove(&address);
                }
            }
        }
    }
}

/// 物件序列化：`Undefined` 欄位照 `JSON.stringify({a: undefined})` 整個省略。
fn write_json_object<'a>(
    entries: impl Iterator<Item = (&'a str, &'a SanitizedValue)>,
    out: &mut String,
    shared: &mut HashSet<usize>,
) {
    out.push('{');
    let mut first = true;
    for (key, value) in entries {
        if matches!(value, SanitizedValue::Undefined) {
            continue;
        }
        if !first {
            out.push(',');
        }
        first = false;
        write_json_string(key, out);
        out.push(':');
        value.write_json(out, shared);
    }
    out.push('}');
}

/// JSON 字串跳脫，與 `JSON.stringify` 相同：僅 `"`、`\` 與控制字元會被跳脫
/// （`\b`、`\t`、`\n`、`\f`、`\r` 用短寫，其餘用 `\u00xx` 小寫十六進位）。
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

#[cfg(test)]
mod tests;
