//! 設定寫回：移植舊版 `src/config/RuntimeConfigManager.ts`（§7.2，允許差異 X3）。
//!
//! 舊版以 yaml 套件的 `parseDocument` → `set` → `String(document)` 整份重新序列化；
//! 這裡照計畫改為「位元組範圍取代」：用 saphyr-parser 解析並記下每個節點的位元組
//! 範圍，只取代根層 `channels` 與 `max_concurrent_streams` 兩個值的文字，其餘位元組
//! （註解、縮排、換行風格）不變。兩種做法產生的檔案在「解析後的值」上相同，golden
//! 以此驗證（D11：預期值由舊版實作產生，`tools/ref/gen-config-golden.mjs`）。
//!
//! 行為重點（與舊版逐行對應）：
//! - `set_channels`／`set_max_concurrent_streams` 以 [`tokio::sync::Mutex`] 串行（對應
//!   舊版 `runExclusive`）；順序為驗證 → `persist` → 更新記憶體 → `target.update_config`，
//!   `persist` 失敗時記憶體不變。
//! - 每次 `persist` 都重新讀檔（外部改過檔案也一樣），以截斷覆寫的方式寫回同一檔，
//!   不用暫存檔再 rename（`config.yml` 是單檔 bind mount）。
//! - 取代規則（§7.2）：`channels` 是 block sequence 時取代第一個 `-` 到最後一個項目
//!   內容結尾（沿用 `-` 的縮排）；是 flow sequence 時取代 `[` 到 `]` 輸出 `[a, b]`；
//!   其他值（標量、alias、映射、空值）換掉整個節點。`max_concurrent_streams` 原本是
//!   標量時只換標量本身的文字（anchor 與 tag 在範圍外自然保留）；其他情況換掉整個
//!   節點。鍵不存在時加在檔尾，順序先 `channels` 後 `max_concurrent_streams`。
//! - 頻道名稱用 plain scalar，除非會被 core schema 解析成非字串，這時改用雙引號。
//! - 被取代節點（含其子樹）內的 anchor 若被範圍外 alias 引用 → 錯誤
//!   `Unresolved alias (the anchor must be set before the alias): <anchor>`（對應 yaml
//!   套件 stringify 時 `ctx.anchors` 的檢查）。
//! - 換行沿用原檔（有 `\r\n` 就用 `\r\n`）；檔尾沒有換行時補上一個。UTF-8 BOM 保留。
#![allow(dead_code)]

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Mutex;

use saphyr_parser::{Event, Parser, ScalarStyle, Tag};

use crate::config::errors::ConfigParseError;
use crate::logging::redact::js_number_to_string;

/// 舊版 `persist` 的語法錯誤訊息（不含「設定檔格式錯誤：」前綴）。
const SYNTAX_ERROR: &str = "無法更新含有語法錯誤的設定檔";
/// 舊版 `persist` 的根節點錯誤訊息（不含前綴）。
const ROOT_NOT_MAP: &str = "設定檔根節點必須是物件";

/// 舊版 `RuntimeWatchConfig`（`freezeConfig` 後的值）。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RuntimeWatchConfig {
    /// 驗證過、去重後的頻道清單（保留第一個出現的大小寫）。
    pub(crate) channels: Vec<String>,
    /// 最大同時觀看數；照舊版是 JS Number，以 f64 表示。
    pub(crate) max_concurrent_streams: f64,
}

/// 寫回時被取代的位元組範圍（以「寫入前」的檔案內容為座標，BOM 含在內）。
///
/// golden 測試用它判斷哪些輸入行不在取代範圍內（子序列檢查）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ReplacedRange {
    /// 範圍起點（含）。
    pub(crate) start: usize,
    /// 範圍終點（不含）。
    pub(crate) end: usize,
}

/// 欄位驗證錯誤（對應舊版 `ConfigValidationError`；`errors.rs` 尚未提供，先在此定義）。
///
/// 訊息格式照舊版建構子：`設定欄位 <field> <message>`。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ConfigValidationError {
    /// 驗證失敗的欄位（如 `channels[0]`）。
    pub(crate) field: String,
    /// 欄位訊息（不含前綴）。
    pub(crate) message: String,
}

impl ConfigValidationError {
    /// 建立錯誤（照舊版建構子）。
    fn new(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            message: message.into(),
        }
    }

    /// 完整訊息（對應舊版 `Error.message`）。
    fn message(&self) -> String {
        format!("設定欄位 {} {}", self.field, self.message)
    }
}

impl core::fmt::Display for ConfigValidationError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.message())
    }
}

/// 寫回操作的錯誤（對應舊版 `setChannels`／`setMaxConcurrentStreams` reject 的 Error）。
#[derive(Debug)]
pub(crate) enum ConfigWriteError {
    /// 欄位驗證失敗（對應 `ConfigValidationError`）。
    Validation(ConfigValidationError),
    /// 設定檔解析失敗（對應 `ConfigParseError`；訊息含「設定檔格式錯誤：」前綴）。
    Parse(ConfigParseError),
    /// 被取代的節點內有 anchor 被範圍外 alias 引用（對應 yaml 套件 stringify 時丟出的
    /// `Error`，`name` 為 `Error`）。
    UnresolvedAlias(String),
    /// 讀寫設定檔的 IO 錯誤（對應注入 `readFile`／`writeFile` 的 reject）。
    Io(String),
}

impl ConfigWriteError {
    /// 錯誤類別名稱（對應舊版 `Error.name`）。
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Self::Validation(_) => "ConfigValidationError",
            Self::Parse(_) => "ConfigParseError",
            Self::UnresolvedAlias(_) | Self::Io(_) => "Error",
        }
    }

    /// 完整訊息（對應舊版 `Error.message`）。
    pub(crate) fn message(&self) -> String {
        match self {
            Self::Validation(error) => error.message(),
            Self::Parse(error) => error.message().to_string(),
            Self::UnresolvedAlias(name) => {
                format!("Unresolved alias (the anchor must be set before the alias): {name}")
            }
            Self::Io(message) => message.clone(),
        }
    }
}

impl core::fmt::Display for ConfigWriteError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for ConfigWriteError {}

impl From<ConfigParseError> for ConfigWriteError {
    fn from(error: ConfigParseError) -> Self {
        Self::Parse(error)
    }
}

/// 注入的檔案讀寫（對應舊版 `readFile`／`writeFile` 參數；正式環境用
/// [`TokioConfigFileIo`]，測試用記憶體 buffer）。
pub(crate) trait ConfigFileIo {
    /// 讀取設定檔（對應 `readFile(path, 'utf8')`）。
    async fn read(&self, path: &str) -> Result<String, ConfigWriteError>;
    /// 以截斷覆寫的方式寫回設定檔（對應 `writeFile(path, data, 'utf8')`）。
    async fn write(&self, path: &str, data: String) -> Result<(), ConfigWriteError>;
}

/// 正式環境的檔案讀寫：`tokio::fs`（`write` 即截斷覆寫，同 Node `writeFile` 的預設旗標）。
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct TokioConfigFileIo;

impl ConfigFileIo for TokioConfigFileIo {
    async fn read(&self, path: &str) -> Result<String, ConfigWriteError> {
        tokio::fs::read_to_string(path)
            .await
            .map_err(|error| ConfigWriteError::Io(error.to_string()))
    }

    async fn write(&self, path: &str, data: String) -> Result<(), ConfigWriteError> {
        tokio::fs::write(path, data)
            .await
            .map_err(|error| ConfigWriteError::Io(error.to_string()))
    }
}

/// 寫回目標（對應舊版 `RuntimeWatchConfigTarget`；寫回成功後呼叫）。
pub(crate) trait RuntimeWatchConfigTarget {
    /// 套用新設定（對應 `updateConfig(config)`）。
    async fn update_config(&self, config: RuntimeWatchConfig) -> Result<(), ConfigWriteError>;
}

/// YAML 設定檔的執行期寫回管理器（逐行移植舊版 `YamlRuntimeConfigManager`）。
pub(crate) struct YamlRuntimeConfigManager<T, F>
where
    T: RuntimeWatchConfigTarget,
    F: ConfigFileIo,
{
    /// 設定檔路徑（舊版 `options.configPath`）。
    config_path: String,
    /// 目前的設定（舊版 `current`；凍結物件在這裡就是擁有的值。操作已在
    /// `operation_lock` 內串行，這裡的 `std::sync::Mutex` 供 `get_config` 同步讀，
    /// 持有期間不會跨 await）。
    current: Mutex<RuntimeWatchConfig>,
    /// 寫回成功後要通知的目標。
    target: T,
    /// 注入的檔案讀寫。
    io: F,
    /// 串行化寫回操作（對應舊版 `runExclusive` 的 promise 尾巴）。
    operation_lock: tokio::sync::Mutex<()>,
    /// 最近一次成功 persist 所取代的位元組範圍（golden 子序列檢查用；
    /// 只在操作鎖內寫入，讀取方在操作完成後讀）。
    last_replaced: Mutex<Vec<ReplacedRange>>,
}

impl<T, F> YamlRuntimeConfigManager<T, F>
where
    T: RuntimeWatchConfigTarget,
    F: ConfigFileIo,
{
    /// 建立管理器（對應舊版建構子）。
    pub(crate) fn new(
        config_path: impl Into<String>,
        initial_config: RuntimeWatchConfig,
        target: T,
        io: F,
    ) -> Self {
        Self {
            config_path: config_path.into(),
            current: Mutex::new(initial_config),
            target,
            io,
            operation_lock: tokio::sync::Mutex::new(()),
            last_replaced: Mutex::new(Vec::new()),
        }
    }

    /// 目前的設定（對應 `getConfig()`；回傳副本）。
    pub(crate) fn get_config(&self) -> RuntimeWatchConfig {
        match self.current.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// 更新頻道清單（對應 `setChannels`）。
    pub(crate) async fn set_channels(
        &self,
        channels: &[String],
    ) -> Result<RuntimeWatchConfig, ConfigWriteError> {
        let _guard = self.operation_lock.lock().await;
        let validated = validate_channels(channels)?;
        let next = RuntimeWatchConfig {
            max_concurrent_streams: js_min(
                self.get_config().max_concurrent_streams,
                validated.len() as f64,
            ),
            channels: validated,
        };
        self.persist(&next).await?;
        *self
            .current
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = next.clone();
        self.target.update_config(next.clone()).await?;
        Ok(next)
    }

    /// 更新最大同時觀看數（對應 `setMaxConcurrentStreams`）。
    pub(crate) async fn set_max_concurrent_streams(
        &self,
        value: f64,
    ) -> Result<RuntimeWatchConfig, ConfigWriteError> {
        let _guard = self.operation_lock.lock().await;
        let current = self.get_config();
        if !is_safe_integer(value) || value < 1.0 {
            return Err(ConfigWriteError::Validation(ConfigValidationError::new(
                "max_concurrent_streams",
                "必須是大於或等於 1 的整數",
            )));
        }
        if value > current.channels.len() as f64 {
            return Err(ConfigWriteError::Validation(ConfigValidationError::new(
                "max_concurrent_streams",
                format!("不可大於目前頻道數 {}", current.channels.len()),
            )));
        }

        let next = RuntimeWatchConfig {
            channels: current.channels.clone(),
            max_concurrent_streams: value,
        };
        self.persist(&next).await?;
        *self
            .current
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = next.clone();
        self.target.update_config(next.clone()).await?;
        Ok(next)
    }

    /// 寫回設定檔（對應舊版 `persist`）：每次都重新讀檔，只取代兩個值的文字，
    /// 以截斷覆寫寫回同一檔。回傳取代的位元組範圍（golden 檢查用）。
    async fn persist(
        &self,
        config: &RuntimeWatchConfig,
    ) -> Result<Vec<ReplacedRange>, ConfigWriteError> {
        let source = self.io.read(&self.config_path).await?;
        let (content, ranges) = build_persisted_content(&source, config)?;
        self.io.write(&self.config_path, content).await?;
        if let Ok(mut guard) = self.last_replaced.lock() {
            *guard = ranges.clone();
        }
        Ok(ranges)
    }

    /// 最近一次成功 persist 所取代的位元組範圍（測試觀測用）。
    pub(crate) fn last_replaced_ranges(&self) -> Vec<ReplacedRange> {
        match self.last_replaced.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }
}

/// JS `Math.min`（任一為 NaN 結果為 NaN，與 Rust `f64::min` 不同）。
fn js_min(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else {
        a.min(b)
    }
}

/// `Number.isSafeInteger`。
fn is_safe_integer(value: f64) -> bool {
    value.is_finite() && value.fract() == 0.0 && value.abs() <= 9_007_199_254_740_991.0
}

// ---------------------------------------------------------------------------
// 欄位驗證（照舊版 `validateChannels`）
// ---------------------------------------------------------------------------

/// JS `String.prototype.trim()` 的字元集合（ECMA-262 WhiteSpace ＋ LineTerminator；
/// 與 Rust `char::is_whitespace` 不同：不含 U+0085，含 U+FEFF）。
fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\u{9}'
            | '\u{a}'
            | '\u{b}'
            | '\u{c}'
            | '\u{d}'
            | '\u{20}'
            | '\u{a0}'
            | '\u{1680}'
            | '\u{2000}'
            ..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
    )
}

/// 對應 JS `trim()`（字元集合見 [`is_js_whitespace`]）。
fn js_trim(value: &str) -> &str {
    let start = value
        .char_indices()
        .find(|(_, c)| !is_js_whitespace(*c))
        .map_or(value.len(), |(index, _)| index);
    let end = value[start..]
        .char_indices()
        .rev()
        .find(|(_, c)| !is_js_whitespace(*c))
        .map_or(start, |(index, c)| start + index + c.len_utf8());
    &value[start..end]
}

/// 舊版 `CHANNEL_PATTERN`：`/^[A-Za-z0-9_]{1,25}$/u`。
fn channel_pattern_matches(value: &str) -> bool {
    (1..=25).contains(&value.chars().count())
        && value.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// 對應舊版 `validateChannels`：非空、每個頻道 trim 後符合樣式、
/// 以 `toLocaleLowerCase('en-US')` 去重（保留第一個）。
fn validate_channels(channels: &[String]) -> Result<Vec<String>, ConfigWriteError> {
    if channels.is_empty() {
        return Err(ConfigWriteError::Validation(ConfigValidationError::new(
            "channels",
            "至少需要一個頻道",
        )));
    }

    let mut result = Vec::with_capacity(channels.len());
    let mut seen = HashSet::new();
    for (index, channel) in channels.iter().enumerate() {
        let normalized = js_trim(channel);
        if !channel_pattern_matches(normalized) {
            return Err(ConfigWriteError::Validation(ConfigValidationError::new(
                format!("channels[{index}]"),
                "必須是 1 到 25 字元的英數字或底線",
            )));
        }
        // 樣式通過後只剩 ASCII，`toLocaleLowerCase('en-US')` 即 ASCII 小寫。
        let key = normalized.to_ascii_lowercase();
        if seen.insert(key) {
            result.push(normalized.to_string());
        }
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
// persist 的核心：解析（記錄位元組範圍）→ 找鍵 → 取代 → alias 檢查 → 組合
// ---------------------------------------------------------------------------

/// 由原始檔產生寫回內容與取代範圍（§7.2 的四步）。
///
/// 回傳 (寫回的完整內容, 以 `source` 座標表示的取代範圍)。
pub(crate) fn build_persisted_content(
    source: &str,
    config: &RuntimeWatchConfig,
) -> Result<(String, Vec<ReplacedRange>), ConfigWriteError> {
    // BOM 保留在輸出（X3：其餘位元組不變）；解析在去掉 BOM 之後做，座標以
    // 去掉後的文字為基準，回傳範圍時再加回偏移。
    let (bom, text) = match source.strip_prefix('\u{feff}') {
        Some(rest) => (true, rest),
        None => (false, source),
    };
    let bom_offset = if bom { '\u{feff}'.len_utf8() } else { 0 };

    let composed = compose_with_ranges(text)
        .map_err(|()| ConfigWriteError::Parse(ConfigParseError::new(SYNTAX_ERROR)))?;
    let plan = plan_replacements(text, &composed, config)?;
    check_unresolved_aliases(text, &composed, &plan.dropped)?;

    // 取代範圍（以去掉 BOM 後的文字為座標）加回 BOM 偏移，換回原檔座標。
    let ranges = plan
        .replacements
        .iter()
        .map(|&(start, end, _)| ReplacedRange {
            start: start + bom_offset,
            end: end + bom_offset,
        })
        .collect();
    let content = assemble_content(text, plan.replacements);
    // 檔尾沒有換行時補上一個（換行風格沿用原檔，照舊版 String(document) 恆以
    // 換行結尾的行為）。
    let content = if content.ends_with('\n') {
        content
    } else if text.contains("\r\n") {
        format!("{content}\r\n")
    } else {
        format!("{content}\n")
    };
    let content = if bom {
        format!("\u{feff}{content}")
    } else {
        content
    };

    Ok((content, ranges))
}

/// 組合取代後的內容：`replacements` 以起點排序、互不重疊。
fn assemble_content(source: &str, replacements: Vec<(usize, usize, String)>) -> String {
    let mut sorted = replacements;
    sorted.sort_by_key(|&(start, end, _)| (start, end));
    let mut out = String::with_capacity(source.len() + 64);
    let mut cursor = 0;
    for (start, end, text) in sorted {
        out.push_str(&source[cursor..start]);
        out.push_str(&text);
        cursor = end;
    }
    out.push_str(&source[cursor..]);
    out
}

// ---------------------------------------------------------------------------
// 位元組範圍追蹤的解析 pass（saphyr 事件 → 帶範圍的節點樹）
// ---------------------------------------------------------------------------

/// 標量在組合階段的解析結果（core schema；與 `yaml.rs` 同一套規則，因模組邊界
/// 無法共用私有函式，於此複製）。
#[derive(Debug, Clone, PartialEq)]
enum ResolvedScalar {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
}

/// 帶位元組範圍的節點種類。
enum RangeNodeKind {
    /// 標量：`raw` 為 token 的字串值（引號已跳脫還原）、`resolved` 為 core schema
    /// 解析結果（重複鍵檢查用）。範圍為 scalar token 本身（不含 anchor／tag token；
    /// 隱式空標量為零寬 span）。
    Scalar {
        raw: String,
        resolved: ResolvedScalar,
    },
    /// 序列：`flow` 依 SequenceStart span 是否非空判斷（block 的 BlockSequenceStart
    /// token 是空 span、flow 的 `[` token 寬 1）。
    Seq { flow: bool, items: Vec<usize> },
    /// 映射：鍵值對（節點索引）＋尚未收到 value 的鍵。
    Map {
        flow: bool,
        entries: Vec<(usize, usize)>,
        pending_key: Option<usize>,
    },
    /// alias；`anchor_id` 指向定義在它之前的 anchor（saphyr 的 id）。
    Alias { anchor_id: usize },
}

/// 帶位元組範圍的節點。
struct RangeNode {
    /// saphyr 的 anchor_id；0 表示沒有 anchor。
    anchor_id: usize,
    /// 節點範圍 (start, end)：
    /// - 標量／alias：token 範圍。
    /// - flow 集合：`[`／`{` 起至 `]`／`}` 之後。
    /// - block 集合：第一個項目指示詞（第一個 `-`／第一個鍵）起至最後一個子節點
    ///   的內容結尾（不含其後的換行／縮排）。
    span: (usize, usize),
    kind: RangeNodeKind,
}

/// 進行中集合的追蹤（block 集合的範圍終點取子節點內容結尾的最大值）。
struct OpenCollection {
    node: usize,
    content_end: usize,
}

/// 解析 pass 的結果。
struct Composed {
    nodes: Vec<RangeNode>,
    /// anchor_id → 帶該 anchor 的節點索引。
    anchor_targets: HashMap<usize, usize>,
    /// 所有 scalar token 的範圍（掃 anchor token 時要跳過，避免誤判）。
    scalar_spans: Vec<(usize, usize)>,
    /// 文件根節點；`None` 表示空文件（空檔、只有註解）。
    root: Option<usize>,
}

#[derive(Default)]
struct ComposeState {
    nodes: Vec<RangeNode>,
    anchor_targets: HashMap<usize, usize>,
    stack: Vec<OpenCollection>,
    scalar_spans: Vec<(usize, usize)>,
    root: Option<usize>,
    /// 已開始的文件數；超過一份即為錯誤（對應 `parseDocument` 的多文件錯誤）。
    doc_count: usize,
}

/// 解析並記錄節點範圍；任何失敗（語法錯誤、多份文件、重複的鍵）都回 [`Err(())`]，
/// 由呼叫端統一轉成「無法更新含有語法錯誤的設定檔」。
///
/// 注意：saphyr 的 `Marker.index()` 雖然文件寫「in bytes」，實作上每跳過一個字元
/// 只加一（`skip_non_blank` 等），所以是**字元索引**；這裡轉換成位元組索引再存。
fn compose_with_ranges(source: &str) -> Result<Composed, ()> {
    // 字元索引 → 位元組索引的對照表。
    let mut char_to_byte: Vec<usize> = Vec::with_capacity(source.chars().count() + 1);
    for (byte_index, _) in source.char_indices() {
        char_to_byte.push(byte_index);
    }
    char_to_byte.push(source.len());

    let mut parser = Parser::new_from_str(source);
    let mut state = ComposeState::default();
    while let Some(result) = parser.next_event() {
        let (event, span) = result.map_err(|_| ())?;
        let range = (
            char_to_byte[span.start.index()],
            char_to_byte[span.end.index()],
        );
        match event {
            Event::StreamStart | Event::StreamEnd | Event::Nothing => {}
            Event::DocumentStart(_) => {
                state.doc_count += 1;
                if state.doc_count > 1 {
                    return Err(());
                }
            }
            Event::DocumentEnd => {}
            Event::Alias(anchor_id) => {
                let node = state.push_node(anchor_id, range, RangeNodeKind::Alias { anchor_id });
                state.attach(node)?;
            }
            Event::Scalar(value, style, anchor_id, tag) => {
                state.scalar_spans.push(range);
                let resolved = resolve_scalar(&value, style, tag.as_deref());
                let node = state.push_node(
                    anchor_id,
                    range,
                    RangeNodeKind::Scalar {
                        raw: value.into_owned(),
                        resolved,
                    },
                );
                state.attach(node)?;
            }
            Event::SequenceStart(anchor_id, _) => {
                let flow = !span.is_empty();
                let node = state.push_node(
                    anchor_id,
                    range,
                    RangeNodeKind::Seq {
                        flow,
                        items: Vec::new(),
                    },
                );
                state.stack.push(OpenCollection {
                    node,
                    content_end: 0,
                });
            }
            Event::MappingStart(anchor_id, _) => {
                let flow = !span.is_empty();
                let node = state.push_node(
                    anchor_id,
                    range,
                    RangeNodeKind::Map {
                        flow,
                        entries: Vec::new(),
                        pending_key: None,
                    },
                );
                state.stack.push(OpenCollection {
                    node,
                    content_end: 0,
                });
            }
            Event::SequenceEnd | Event::MappingEnd => {
                let open = state.stack.pop().ok_or(())?;
                // flow 集合以 `]`／`}` token 為準：saphyr 的 FlowSequenceEnd／
                // FlowMappingEnd token span 會延續到行尾（含其後空白與註解，
                // `skip_ws_to_eol`），所以終點用 token 的起點 + 1；block 集合取
                // 子節點內容結尾（BlockEnd token 的 span 在去縮排位置，不採用）。
                let end = if is_flow_node(&state.nodes, open.node) {
                    char_to_byte[span.start.index() + 1]
                } else {
                    open.content_end
                };
                state.nodes[open.node].span.1 = end;
                state.attach(open.node)?;
            }
        }
    }
    Ok(Composed {
        nodes: state.nodes,
        anchor_targets: state.anchor_targets,
        scalar_spans: state.scalar_spans,
        root: state.root,
    })
}

/// 節點是否為 flow 集合。
fn is_flow_node(nodes: &[RangeNode], index: usize) -> bool {
    match &nodes[index].kind {
        RangeNodeKind::Seq { flow, .. } | RangeNodeKind::Map { flow, .. } => *flow,
        _ => false,
    }
}

impl ComposeState {
    /// 新增節點；有 anchor 時記錄 anchor_id → 節點的對照。
    fn push_node(&mut self, anchor_id: usize, span: (usize, usize), kind: RangeNodeKind) -> usize {
        let id = self.nodes.len();
        self.nodes.push(RangeNode {
            anchor_id,
            span,
            kind,
        });
        if anchor_id > 0 {
            self.anchor_targets.insert(anchor_id, id);
        }
        id
    }

    /// 把子節點接到父節點（或成為根節點），並把子節點的內容結尾併入父集合的
    /// 追蹤值；map 鍵做重複檢查（比較解析後的標量值，照 yaml 套件 `uniqueKeys`）。
    fn attach(&mut self, child: usize) -> Result<(), ()> {
        if let Some(parent) = self.stack.last_mut() {
            parent.content_end = parent.content_end.max(self.nodes[child].span.1);
        }
        let Some(open) = self.stack.last() else {
            // 沒有父節點：這是根節點（一個文件只會有一個，saphyr 保證）。
            if self.root.is_some() {
                return Err(());
            }
            self.root = Some(child);
            return Ok(());
        };
        let parent = open.node;
        let is_map = matches!(self.nodes[parent].kind, RangeNodeKind::Map { .. });
        if !is_map {
            match &mut self.nodes[parent].kind {
                RangeNodeKind::Seq { items, .. } => items.push(child),
                // saphyr 的狀態機不會把標量／alias 留在棧上；防禦性錯誤。
                _ => return Err(()),
            }
            return Ok(());
        }
        // 鍵或值：尚未收到值時，此節點是鍵。
        let is_key = matches!(
            &self.nodes[parent].kind,
            RangeNodeKind::Map {
                pending_key: None,
                ..
            }
        );
        if is_key && let RangeNodeKind::Scalar { resolved, .. } = &self.nodes[child].kind {
            let duplicate = match &self.nodes[parent].kind {
                RangeNodeKind::Map { entries, .. } => entries.iter().any(|(key, _)| {
                    matches!(&self.nodes[*key].kind, RangeNodeKind::Scalar { resolved: existing, .. } if existing == resolved)
                }),
                _ => false,
            };
            if duplicate {
                return Err(());
            }
        }
        match &mut self.nodes[parent].kind {
            RangeNodeKind::Map {
                entries,
                pending_key,
                ..
            } => {
                if is_key {
                    *pending_key = Some(child);
                } else if let Some(key) = pending_key.take() {
                    entries.push((key, child));
                }
            }
            // 上面已排除非 map 的情況。
            _ => return Err(()),
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// anchor token 掃描（saphyr 的事件不帶 anchor 名稱，自行從原始檔掃出）
// ---------------------------------------------------------------------------

/// 從原始檔掃出 `&anchor` token 的 (位置, 名稱) 清單（原始檔順序）。
///
/// saphyr 的 anchor／alias token 只在掃描器內部可見、事件只帶 anchor_id，因此
/// 藉此取得名稱。為避免誤判，掃描時跳過：
/// - 所有 scalar token 的範圍（plain／引號／block 標量內的 `&`／`!` 都不是指示詞）；
/// - 註解（`#` 在行首或空白之後起至行尾）；
/// - `%` 開頭的 directive 行。
///
/// 在合法文件中，掃出的 token 與「帶 anchor 的節點」（建立順序）一一對應。
fn scan_anchor_tokens(source: &str, scalar_spans: &[(usize, usize)]) -> Vec<(usize, String)> {
    let mut tokens = Vec::new();
    let bytes = source.as_bytes();
    let mut pos = 0;
    let mut span_index = 0;
    while pos < source.len() {
        // 跳過 scalar token 範圍（清單依事件順序＝原始檔順序，已排序）。
        while let Some(&(start, end)) = scalar_spans.get(span_index) {
            if pos >= end {
                span_index += 1;
            } else if pos >= start {
                pos = end;
                span_index += 1;
                continue;
            } else {
                break;
            }
        }
        if pos >= source.len() {
            break;
        }
        let Some(current) = source[pos..].chars().next() else {
            break;
        };
        match current {
            // 註解：`#` 在行首或空白之後；跳到行尾。
            '#' if pos == 0
                || bytes[pos - 1] == b' '
                || bytes[pos - 1] == b'\t'
                || bytes[pos - 1] == b'\n'
                || bytes[pos - 1] == b'\r' =>
            {
                pos = match source[pos..].find('\n') {
                    Some(offset) => pos + offset + 1,
                    None => source.len(),
                };
            }
            // directive 行：跳到行尾。
            '%' if bytes[..pos].iter().all(|byte| *byte != b'\n') => {
                pos = match source[pos..].find('\n') {
                    Some(offset) => pos + offset + 1,
                    None => source.len(),
                };
            }
            // `&` 在 scalar 範圍與註解之外即為 anchor 指示詞。
            '&' => {
                let (name, end) = read_anchor_name(source, pos + 1);
                tokens.push((pos, name));
                pos = end;
            }
            // verbatim tag `!<...>`：跳過（tag 內可能含 `&`）。
            '!' if source[pos..].starts_with("!<") => {
                pos = match source[pos..].find('>') {
                    Some(offset) => pos + offset + 1,
                    None => source.len(),
                };
            }
            // 普通 tag token（如 `!x`、`!!int`）：跳過整個 token（tag suffix 可能含 `&`）。
            '!' => {
                let mut end = pos + 1;
                while let Some(c) = source[end..].chars().next() {
                    if c.is_whitespace() || matches!(c, ',' | '[' | ']' | '{' | '}') || c == '#' {
                        break;
                    }
                    end += c.len_utf8();
                }
                pos = end;
            }
            _ => pos += current.len_utf8(),
        }
    }
    tokens
}

/// anchor 名稱字元（照 saphyr `is_anchor_char`：非 blankz 且非 flow 指示字元）。
fn is_anchor_char_byte(byte: u8) -> bool {
    !matches!(
        byte,
        b' ' | b'\t' | b'\r' | b'\n' | 0x00 | b',' | b'[' | b']' | b'{' | b'}'
    )
}

/// 讀取 anchor 名稱；回傳 (名稱, 結束位置)。
///
/// 以位元組推進：名稱字元的集合（非 blankz、非 flow）涵蓋多位元組字元的所有
/// 續位元組，所以結束位置一定落在字元邊界上。
fn read_anchor_name(source: &str, start: usize) -> (String, usize) {
    let bytes = source.as_bytes();
    let mut end = start;
    while end < bytes.len() && is_anchor_char_byte(bytes[end]) {
        end += 1;
    }
    (source[start..end].to_string(), end)
}

/// 把掃出的 anchor token 依建立順序對應到帶 anchor 的節點，得到每個節點的
/// anchor 名稱（節點建立順序＝文件前序走訪＝原始檔順序，兩邊一一對應）。
fn anchor_names_per_node(source: &str, composed: &Composed) -> Vec<String> {
    let tokens = scan_anchor_tokens(source, &composed.scalar_spans);
    let mut names = vec![String::new(); composed.nodes.len()];
    let mut cursor = 0;
    for (index, node) in composed.nodes.iter().enumerate() {
        if node.anchor_id != 0 {
            names[index] = tokens
                .get(cursor)
                .map_or_else(String::new, |(_, name)| name.clone());
            cursor += 1;
        }
    }
    names
}

// ---------------------------------------------------------------------------
// 取代規劃（§7.2 步驟 2–3）
// ---------------------------------------------------------------------------

/// 取代規劃的結果：`(範圍, 新文字)` 清單與每個節點是否被取代（從輸出消失）。
struct PersistPlan {
    replacements: Vec<(usize, usize, String)>,
    dropped: Vec<bool>,
}

/// 找根層指定鍵的 (鍵節點, 值節點)；只比對標量鍵的字串值
/// （照舊版 `findPair`：`isScalar(it.key) && it.key.value === key`）。
fn find_root_entry(
    nodes: &[RangeNode],
    entries: &[(usize, usize)],
    key: &str,
) -> Option<(usize, usize)> {
    entries
        .iter()
        .copied()
        .find_map(|(key_node, value_node)| match &nodes[key_node].kind {
            RangeNodeKind::Scalar { raw, .. } if raw == key => Some((key_node, value_node)),
            _ => None,
        })
}

/// 規劃 `channels` 與 `max_concurrent_streams` 的取代。
fn plan_replacements(
    source: &str,
    composed: &Composed,
    config: &RuntimeWatchConfig,
) -> Result<PersistPlan, ConfigWriteError> {
    let root = composed
        .root
        .ok_or_else(|| ConfigWriteError::Parse(ConfigParseError::new(ROOT_NOT_MAP)))?;
    let entries = match &composed.nodes[root].kind {
        RangeNodeKind::Map { entries, .. } => entries,
        _ => {
            return Err(ConfigWriteError::Parse(ConfigParseError::new(ROOT_NOT_MAP)));
        }
    };

    // 換行風格沿用原檔。
    let newline = if source.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let mut plan = PersistPlan {
        replacements: Vec::new(),
        dropped: vec![false; composed.nodes.len()],
    };
    let mut appends: Vec<String> = Vec::new();

    // 順序照舊版：先 set channels、再 set max_concurrent_streams；鍵不存在時
    // 附加在檔尾的順序也因此是先 channels 後 max。
    if let Some((key_index, value_index)) = find_root_entry(&composed.nodes, entries, "channels") {
        plan_channels_replacement(
            source,
            composed,
            config,
            key_index,
            value_index,
            newline,
            &mut plan,
        );
    } else {
        appends.push(missing_channels_text(config, newline));
    }

    if let Some((key_index, value_index)) =
        find_root_entry(&composed.nodes, entries, "max_concurrent_streams")
    {
        plan_max_replacement(
            source,
            composed,
            config,
            key_index,
            value_index,
            newline,
            &mut plan,
        );
    } else {
        appends.push(format!(
            "max_concurrent_streams: {}{newline}",
            js_number_to_string(config.max_concurrent_streams)
        ));
    }

    // 附加的鍵放在檔尾（同一個零寬取代範圍）；檔尾沒有換行時先補一個。
    if !appends.is_empty() {
        let text = appends.concat();
        let text = if source.ends_with('\n') {
            text
        } else {
            format!("{newline}{text}")
        };
        let start = source.len();
        plan.replacements.push((start, start, text));
    }
    Ok(plan)
}

/// `channels` 鍵不存在時的附加文字：block sequence，縮排為鍵的欄位 + 2
/// （檔尾附加時鍵的欄位為 0）。
fn missing_channels_text(config: &RuntimeWatchConfig, newline: &str) -> String {
    format!(
        "channels{}{newline}",
        block_sequence_after_key_text(&config.channels, 0, newline)
    )
}

/// 把節點子樹標記為被取代（從輸出消失；其 anchor 也一併消失）。
fn mark_subtree_dropped(nodes: &[RangeNode], index: usize, dropped: &mut [bool]) {
    dropped[index] = true;
    match &nodes[index].kind {
        RangeNodeKind::Seq { items, .. } => {
            for &item in items {
                mark_subtree_dropped(nodes, item, dropped);
            }
        }
        RangeNodeKind::Map { entries, .. } => {
            for (key, value) in entries {
                mark_subtree_dropped(nodes, *key, dropped);
                mark_subtree_dropped(nodes, *value, dropped);
            }
        }
        RangeNodeKind::Scalar { .. } | RangeNodeKind::Alias { .. } => {}
    }
}

/// block sequence 節點的第一個 `-` 位置。
///
/// 一般 block sequence 的 SequenceStart span 是在第一個 `-` 位置產生的空 span；
/// indentless sequence（`key:\n- item`）只有 BlockEntry token，且其空 span 在
/// 跳過 `-` 後的空白之後（即第一個項目的內容起點），所以往回跳過空白找 `-`。
fn block_seq_dash_start(source: &str, node: &RangeNode) -> Option<usize> {
    let bytes = source.as_bytes();
    let start = node.span.0;
    if bytes.get(start) == Some(&b'-') {
        // 一般 block sequence：span 起點就是第一個 `-`。
        return Some(start);
    }
    // indentless sequence：往回跳過空白，`-` 就在第一個非空白字元之前。
    let mut pos = start;
    while pos > 0 && (bytes[pos - 1] == b' ' || bytes[pos - 1] == b'\t') {
        pos -= 1;
    }
    if pos > 0 && bytes[pos - 1] == b'-' {
        return Some(pos - 1);
    }
    None
}

/// 位置所在的欄（行首到位置之間的位元組數；縮排為空白時即欄位數）。
fn column_of(source: &str, position: usize) -> usize {
    let line_start = source[..position]
        .rfind('\n')
        .map_or(0, |offset| offset + 1);
    position - line_start
}

/// 頻道名稱的 scalar 文字：plain，除非會被 core schema 解析成非字串
/// （`null`、`true`、`123`、`0x1F`、`1e3` 等），這時改用雙引號。
fn channel_scalar_text(name: &str) -> String {
    if matches!(resolve_plain_scalar(name), ResolvedScalar::Str(_)) {
        name.to_string()
    } else {
        format!("\"{name}\"")
    }
}

/// block sequence 的新文字（取代範圍從第一個 `-` 開始，首行不含縮排——
/// 範圍前的原始縮排保留；後續行補上 `-` 的縮排）。
fn block_sequence_items_text(channels: &[String], indent: usize, newline: &str) -> String {
    let indent_text = " ".repeat(indent);
    channels
        .iter()
        .map(|name| format!("- {}", channel_scalar_text(name)))
        .collect::<Vec<_>>()
        .join(&format!("{newline}{indent_text}"))
}

/// 以「鍵的欄位 ＋ 2」為項目縮排的 block sequence 文字（鍵行會以 `:` 結尾）。
fn block_sequence_after_key_text(channels: &[String], key_indent: usize, newline: &str) -> String {
    let indent_text = " ".repeat(key_indent + 2);
    let items = channels
        .iter()
        .map(|name| format!("{indent_text}- {}", channel_scalar_text(name)))
        .collect::<Vec<_>>()
        .join(newline);
    format!(":{newline}{items}")
}

/// `channels` 的取代規劃。
fn plan_channels_replacement(
    source: &str,
    composed: &Composed,
    config: &RuntimeWatchConfig,
    key_index: usize,
    value_index: usize,
    newline: &str,
    plan: &mut PersistPlan,
) {
    let node = &composed.nodes[value_index];
    let (span_start, span_end) = node.span;
    let key_end = composed.nodes[key_index].span.1;
    match &node.kind {
        RangeNodeKind::Seq { flow: false, .. } => {
            // block sequence：取代第一個 `-` 到最後一個項目內容結尾，沿用 `-` 的縮排。
            let dash = block_seq_dash_start(source, node).unwrap_or(span_start);
            let text =
                block_sequence_items_text(&config.channels, column_of(source, dash), newline);
            plan.replacements.push((dash, span_end, text));
        }
        RangeNodeKind::Seq { flow: true, .. } => {
            // flow sequence：取代 `[` 到 `]`，輸出 `[a, b]`。
            let text = format!(
                "[{}]",
                config
                    .channels
                    .iter()
                    .map(|name| channel_scalar_text(name))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            plan.replacements.push((span_start, span_end, text));
        }
        _ => {
            // 其他值（標量、alias、映射、空值）：換掉整個節點，輸出 block sequence，
            // 縮排為鍵的欄位 + 2。範圍從鍵的結尾開始（吃掉 `:` 之後、節點之前的
            // anchor／tag）；若範圍吞掉了換行，新文字補回一個。
            //
            // 隱式空值（`channels:`，節點 span 為 `:` 位置上的零寬 span）要連
            // `:` 一起取代（新文字會補回 `:`）；帶 anchor 的空值 span 在下一個
            // token 上，`:` 已在範圍內，不調整。
            let key_indent = column_of(source, composed.nodes[key_index].span.0);
            let range_end =
                if span_start == span_end && source.as_bytes().get(span_end) == Some(&b':') {
                    span_end + 1
                } else {
                    span_end
                };
            let mut text = block_sequence_after_key_text(&config.channels, key_indent, newline);
            if source[key_end..range_end].ends_with('\n') {
                text.push_str(newline);
            }
            plan.replacements.push((key_end, range_end, text));
        }
    }
    mark_subtree_dropped(&composed.nodes, value_index, &mut plan.dropped);
}

/// `max_concurrent_streams` 的取代規劃。
fn plan_max_replacement(
    source: &str,
    composed: &Composed,
    config: &RuntimeWatchConfig,
    key_index: usize,
    value_index: usize,
    newline: &str,
    plan: &mut PersistPlan,
) {
    let node = &composed.nodes[value_index];
    let (span_start, span_end) = node.span;
    let number = js_number_to_string(config.max_concurrent_streams);
    let key_end = composed.nodes[key_index].span.1;
    match &node.kind {
        RangeNodeKind::Scalar { .. } if span_start == span_end => {
            // 隱式空標量（`max_concurrent_streams:`）：節點沒有文字，在鍵行 `:`
            // 之後的插入點（其後的 anchor／tag 之後）插入數字文字。節點保留
            // （anchor 與 tag 在範圍外自然保留）。
            let (position, text) = empty_value_insertion(source, key_end, &number);
            plan.replacements.push((position, position, text));
        }
        RangeNodeKind::Scalar { .. } => {
            // 標量：只換標量本身的文字，保留 anchor 與 tag（案例 15）。
            plan.replacements
                .push((span_start, span_end, number.clone()));
        }
        _ => {
            // 其他情況（集合、alias）：換掉整個節點。範圍從鍵的結尾開始；
            // 若範圍吞掉了換行，新文字補回一個。
            let mut text = format!(": {number}");
            if source[key_end..span_end].ends_with('\n') {
                text.push_str(newline);
            }
            plan.replacements.push((key_end, span_end, text));
            mark_subtree_dropped(&composed.nodes, value_index, &mut plan.dropped);
        }
    }
}

/// 隱式空值的插入點與插入文字：從鍵之後找到 `:`，掃過其後（同一行）的
/// anchor／tag token，插入點為最後一個 token 的結尾（沒有 token 時為 `:` 之後
/// 第一個非空白位置，如行尾或註解起點）。文字補上必要的前置／後置空白，
/// 讓插入結果保持 `key: value` 的形式且不吞掉註解。
fn empty_value_insertion(source: &str, key_end: usize, number: &str) -> (usize, String) {
    let bytes = source.as_bytes();
    // 從鍵的結尾跳過空白找到 `:`（值分隔符一定緊跟在鍵之後）。
    let mut pos = key_end;
    while pos < bytes.len() && (bytes[pos] == b' ' || bytes[pos] == b'\t') {
        pos += 1;
    }
    if bytes.get(pos) == Some(&b':') {
        pos += 1;
    }
    // 掃過 `:` 之後的 anchor／tag token（值尚未出現，所以只有這兩種指示詞）。
    let mut last_token_end: Option<usize> = None;
    loop {
        while pos < bytes.len() && (bytes[pos] == b' ' || bytes[pos] == b'\t') {
            pos += 1;
        }
        if pos >= bytes.len() {
            break;
        }
        let Some(current) = source[pos..].chars().next() else {
            break;
        };
        if current == '&' || current == '!' {
            // anchor：`&` ＋ 名稱字元；tag：`!<...>` 或 `!` ＋ 非 blankz 字元。
            let end = if current == '&' {
                read_anchor_name(source, pos + 1).1
            } else if source[pos..].starts_with("!<") {
                source[pos..]
                    .find('>')
                    .map_or(source.len(), |offset| pos + offset + 1)
            } else {
                let mut end = pos + 1;
                while end < bytes.len() && is_anchor_char_byte(bytes[end]) {
                    end += 1;
                }
                end
            };
            last_token_end = Some(end);
            pos = end;
            continue;
        }
        break;
    }
    let insertion = last_token_end.unwrap_or(pos);
    // 前置空白：插入點前一個位元組不是空白（如 `key:` 的 `:`、anchor 的尾字元）
    // 時補一個空格。
    let preceded_by_space =
        insertion > 0 && (bytes[insertion - 1] == b' ' || bytes[insertion - 1] == b'\t');
    let followed_by_comment = bytes.get(insertion) == Some(&b'#');
    let mut text = String::new();
    if !preceded_by_space {
        text.push(' ');
    }
    text.push_str(number);
    if followed_by_comment {
        // 插在註解前要留一個空白，讓 `#` 仍是註解（如 `key: 2 # c`）。
        text.push(' ');
    }
    (insertion, text)
}

// ---------------------------------------------------------------------------
// alias 檢查（對應 yaml 套件 stringify 的 `Unresolved alias`）
// ---------------------------------------------------------------------------

/// 檢查被取代節點內的 anchor 是否被範圍外 alias 引用。
///
/// 照 yaml 套件 stringify 的語意：依文件順序走訪輸出中仍存在的節點，
/// anchor 名稱註冊進集合；alias 必須能在集合中找到（anchor 需在 alias 之前）。
/// 被取代的節點不會出現在輸出裡：其 anchor 不註冊、其 alias 不檢查。
fn check_unresolved_aliases(
    source: &str,
    composed: &Composed,
    dropped: &[bool],
) -> Result<(), ConfigWriteError> {
    let names = anchor_names_per_node(source, composed);
    let mut defined: HashSet<String> = HashSet::new();
    for (index, node) in composed.nodes.iter().enumerate() {
        if dropped[index] {
            continue;
        }
        if node.anchor_id != 0 {
            defined.insert(names[index].clone());
        }
        if let RangeNodeKind::Alias { .. } = &node.kind {
            // alias token 的文字即 `*<名稱>`；去掉 `*` 取得名稱。
            let name = &source[node.span.0 + 1..node.span.1];
            if !defined.contains(name) {
                return Err(ConfigWriteError::UnresolvedAlias(name.to_string()));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// core schema 標量解析（與 `yaml.rs` 同一套；模組邊界限制無法共用，於此複製）
// ---------------------------------------------------------------------------

/// 標量 tag 的種類；其餘 tag 一律當字串。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CoreTag {
    Int,
    Float,
    Bool,
    Null,
}

/// 把 saphyr 的 tag 正規化成 core schema 的標量 tag（照 `yaml.rs` 的 `core_tag`）。
fn core_tag(tag: &Tag) -> Option<CoreTag> {
    let suffix = if tag.handle == "tag:yaml.org,2002:" {
        Some(tag.suffix.as_str())
    } else if tag.handle.is_empty() && tag.suffix.starts_with("tag:yaml.org,2002:") {
        Some(&tag.suffix["tag:yaml.org,2002:".len()..])
    } else {
        None
    }?;
    match suffix {
        "int" => Some(CoreTag::Int),
        "float" => Some(CoreTag::Float),
        "bool" => Some(CoreTag::Bool),
        "null" => Some(CoreTag::Null),
        _ => None,
    }
}

/// 解析標量（照 `yaml.rs` 的 `resolve_scalar`）。
fn resolve_scalar(source: &str, style: ScalarStyle, tag: Option<&Tag>) -> ResolvedScalar {
    let Some(tag) = tag else {
        return if matches!(style, ScalarStyle::Plain) {
            resolve_plain_scalar(source)
        } else {
            ResolvedScalar::Str(source.to_string())
        };
    };
    let resolved = match core_tag(tag) {
        Some(CoreTag::Int) => int_scalar(source).map(ResolvedScalar::Num),
        Some(CoreTag::Float) => float_scalar(source).map(ResolvedScalar::Num),
        Some(CoreTag::Bool) => bool_scalar(source).map(ResolvedScalar::Bool),
        Some(CoreTag::Null) => is_null_scalar(source).then_some(ResolvedScalar::Null),
        // `!!str` 與其他 tag 一律字串。
        None => None,
    };
    resolved.unwrap_or_else(|| ResolvedScalar::Str(source.to_string()))
}

/// plain 標量的解析（照 `yaml.rs` 的 `resolve_plain_scalar`）。
fn resolve_plain_scalar(source: &str) -> ResolvedScalar {
    if is_null_scalar(source) {
        return ResolvedScalar::Null;
    }
    if let Some(boolean) = bool_scalar(source) {
        return ResolvedScalar::Bool(boolean);
    }
    if let Some(number) = int_scalar(source) {
        return ResolvedScalar::Num(number);
    }
    if let Some(number) = float_scalar(source) {
        return ResolvedScalar::Num(number);
    }
    ResolvedScalar::Str(source.to_string())
}

/// core schema 的 null test。
fn is_null_scalar(source: &str) -> bool {
    matches!(source, "~" | "null" | "Null" | "NULL") || source.is_empty()
}

/// core schema 的 bool test。
fn bool_scalar(source: &str) -> Option<bool> {
    match source {
        "true" | "True" | "TRUE" => Some(true),
        "false" | "False" | "FALSE" => Some(false),
        _ => None,
    }
}

/// int 系列的 test（8 進位 → 10 進位 → 16 進位）。
fn int_scalar(source: &str) -> Option<f64> {
    octal_int(source)
        .or_else(|| decimal_int(source))
        .or_else(|| hex_int(source))
}

/// `0o[0-7]+`。
fn octal_int(source: &str) -> Option<f64> {
    let digits = source.strip_prefix("0o")?;
    digits_to_f64(digits, 8)
}

/// `[-+]?[0-9]+`。
fn decimal_int(source: &str) -> Option<f64> {
    let digits = source.strip_prefix(['+', '-']).unwrap_or(source);
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let value = digits.parse::<f64>().ok()?;
    Some(if source.starts_with('-') {
        -value
    } else {
        value
    })
}

/// `0x[0-9a-fA-F]+`。
fn hex_int(source: &str) -> Option<f64> {
    let digits = source.strip_prefix("0x")?;
    digits_to_f64(digits, 16)
}

/// 把指定位基的數字字串轉成 f64（照 JS `parseInt` 的字串轉換，正確捨入）。
fn digits_to_f64(digits: &str, radix: u32) -> Option<f64> {
    if digits.is_empty() {
        return None;
    }
    if let Ok(value) = u128::from_str_radix(digits, radix) {
        return Some(value as f64);
    }
    let mut value = 0.0f64;
    let radix_f64 = f64::from(radix);
    for byte in digits.bytes() {
        let digit = (byte as char).to_digit(radix)?;
        value = value.mul_add(radix_f64, f64::from(digit));
    }
    Some(value)
}

/// float 系列的 test。
fn float_scalar(source: &str) -> Option<f64> {
    special_float(source)
        .or_else(|| exponent_float(source))
        .or_else(|| plain_float(source))
}

/// `[-+]?\.(?:inf|Inf|INF)` 與 `\.nan|\.NaN|\.NAN`。
fn special_float(source: &str) -> Option<f64> {
    match source {
        ".inf" | ".Inf" | ".INF" => Some(f64::INFINITY),
        "+.inf" | "+.Inf" | "+.INF" => Some(f64::INFINITY),
        "-.inf" | "-.Inf" | "-.INF" => Some(f64::NEG_INFINITY),
        ".nan" | ".NaN" | ".NAN" => Some(f64::NAN),
        _ => None,
    }
}

/// 指數 float。
fn exponent_float(source: &str) -> Option<f64> {
    let (mantissa, exponent) = source.split_once(['e', 'E'])?;
    let exponent_digits = exponent.strip_prefix(['+', '-']).unwrap_or(exponent);
    if exponent_digits.is_empty() || !exponent_digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let mantissa = mantissa.strip_prefix(['+', '-']).unwrap_or(mantissa);
    if !valid_float_mantissa(mantissa) {
        return None;
    }
    source.parse::<f64>().ok()
}

/// 一般 float（必須有小數點、無指數）。
fn plain_float(source: &str) -> Option<f64> {
    let mantissa = source.strip_prefix(['+', '-']).unwrap_or(source);
    if !mantissa.contains('.') || !valid_float_mantissa(mantissa) {
        return None;
    }
    source.parse::<f64>().ok()
}

/// 檢查去掉正負號後的尾數形狀。
fn valid_float_mantissa(mantissa: &str) -> bool {
    if let Some(fraction) = mantissa.strip_prefix('.') {
        return !fraction.is_empty() && fraction.bytes().all(|byte| byte.is_ascii_digit());
    }
    match mantissa.split_once('.') {
        Some((int_part, fraction)) => {
            !int_part.is_empty()
                && int_part.bytes().all(|byte| byte.is_ascii_digit())
                && fraction.bytes().all(|byte| byte.is_ascii_digit())
        }
        None => !mantissa.is_empty() && mantissa.bytes().all(|byte| byte.is_ascii_digit()),
    }
}

#[cfg(test)]
mod tests;
