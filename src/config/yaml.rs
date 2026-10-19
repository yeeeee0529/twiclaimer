//! YAML 載入：以 saphyr-parser 0.2.2 的事件重現舊版 `config/yaml.ts`
//! （yaml 套件 `parseDocument` + `toJS`）的行為（§7.1）。
//!
//! 行為重點：
//! - 標量採 YAML 1.2 core schema（test 順序照 `$REF/node_modules/yaml/dist/schema/core/schema.js`：
//!   null → bool → intOct → int → intHex → floatNaN → floatExp → float，都不符才是字串）；
//!   數字一律 f64（超過 2^53 的整數照 JS 失去精度）。
//! - 引號與 block 標量一律字串；saphyr 已在掃描階段處理跳脫與摺行。
//! - map 的鍵照 JS `String()` 轉字串（null 鍵為空字串）；`<<` 是普通鍵（YAML 1.2 無 merge）。
//! - 重複鍵在組合階段（pass 1）偵測，比較「解析後」的標量值
//!   （`uniqueKeys` 的 `a.value === b.value`；NaN 不等於 NaN、`-0` 等於 `0`）。
//! - alias 計數照 `$REF/node_modules/yaml/dist/nodes/Alias.js`：每個 anchor 的計數
//!   從 1 起算（anchor 節點自身轉換算 1）、每解析一次 +1，計數 × alias 倍數
//!   （`getAliasCount`，第一次需要時計算）> 100 就失敗；倍數為 0 的（如空集合）永遠不超限。
//! - 流程分兩段以忠實重現舊版：pass 1 = composer（事件 → 節點樹），pass 2 =
//!   `toJS`（節點樹 → [`JsValue`]，依文件順序轉換）。分兩段是因為 `getAliasCount`
//!   必須在「第一次解析 alias 當下」以巢狀 anchor 的當時計數計算倍數。
//! - 循環 alias（alias 指向仍在轉換中的祖先）在轉換完成後回報，對應舊版
//!   toJS 之後的 `assertAcyclic`；轉換期間的錯誤（alias 計數等）優先。
#![allow(dead_code)]

use std::collections::HashMap;

use saphyr_parser::{Event, Parser, ScalarStyle, Tag};

use crate::config::errors::ConfigParseError;
use crate::logging::redact::js_number_to_string;

/// 舊版 `ConfigParseError('YAML 語法無效')` 的訊息（不含「設定檔格式錯誤：」前綴）。
const YAML_INVALID: &str = "YAML 語法無效";
/// 舊版 `ConfigParseError('YAML 不可包含循環 alias')` 的訊息（不含前綴）。
const YAML_CYCLE: &str = "YAML 不可包含循環 alias";
/// 舊版 `Document#toJS()` 的 `maxAliasCount` 預設值。
const MAX_ALIAS_COUNT: u64 = 100;

/// JS 相容的值模型（對應 yaml 套件 `toJS` 的輸出；§7.1）。
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum JsValue {
    Null,
    Bool(bool),
    /// 數字一律 f64（照 JS Number）。
    Number(f64),
    String(String),
    Array(Vec<JsValue>),
    /// 物件：保留鍵的插入順序。標量鍵的重複在組合階段就報錯（`uniqueKeys`）；
    /// 字串化後才相同的鍵（alias 鍵、`1` 與 `"1"`）在這裡覆寫原位置的值，
    /// 與 yaml 套件 `addPairToJSMap` 的行為一致。
    Object(Vec<(String, JsValue)>),
}

/// 對應舊版 `parseYaml(source)`：載入單一 YAML 文件成 [`JsValue`]。
///
/// 所有失敗（語法錯誤、多份文件、重複鍵、alias 次數過多）都是 [`ConfigParseError`]，
/// 訊息 `設定檔格式錯誤：YAML 語法無效`；循環 alias 則是
/// `設定檔格式錯誤：YAML 不可包含循環 alias`。
pub(crate) fn parse_yaml(source: &str) -> Result<JsValue, ConfigParseError> {
    // UTF-8 BOM 先去掉（yaml 套件的 parser 會丟棄 byte-order-mark token；golden l11）。
    let source = source.strip_prefix('\u{feff}').unwrap_or(source);
    let composed = compose(source)?;
    let mut converter = Converter::new(&composed);
    let value = match composed.root {
        Some(root) => converter.convert(root)?,
        // 空文件（空檔、只有註解）對應 yaml 套件的 contents = null。
        None => JsValue::Null,
    };
    if converter.cycle {
        return Err(ConfigParseError::new(YAML_CYCLE));
    }
    Ok(value)
}

// ---------------------------------------------------------------------------
// pass 1：組合（composer）
// ---------------------------------------------------------------------------

/// 標量在組合階段的解析結果（core schema；yaml 套件在 compose 時就套用 tag）。
#[derive(Debug, Clone, PartialEq)]
enum ResolvedScalar {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
}

/// 組合階段的節點內容（對應 yaml 套件的 Scalar／YAMLSeq／YAMLMap／Alias 節點）。
enum NodeKind {
    /// 標量；解析結果在此定案。
    Scalar(ResolvedScalar),
    /// 序列；元素為節點索引。
    Seq(Vec<usize>),
    /// 映射：鍵值對（節點索引）＋尚未收到 value 的鍵。
    Map(MapNode),
    /// alias；`anchor_id` 由 saphyr 解析（指向定義在它之前的 anchor）。
    Alias(usize),
}

/// 映射節點的內容。
struct MapNode {
    entries: Vec<(usize, usize)>,
    /// 尚未收到 value 的鍵節點（事件依序出現鍵與值）。
    pending_key: Option<usize>,
}

/// 組合階段的節點。
struct Node {
    /// 非 0 表示此節點帶 `&anchor`（saphyr 的 anchor_id，每次定義都是新 id）。
    anchor_id: usize,
    kind: NodeKind,
}

/// pass 1 的結果：節點樹與 anchor 對照。
struct Composed {
    nodes: Vec<Node>,
    /// anchor_id → 帶該 anchor 的節點索引。
    anchor_targets: HashMap<usize, usize>,
    /// 文件根節點；`None` 表示空文件（yaml 套件組出 contents = null 的文件）。
    root: Option<usize>,
}

/// pass 1 的累積狀態。
#[derive(Default)]
struct ComposeState {
    nodes: Vec<Node>,
    anchor_targets: HashMap<usize, usize>,
    /// 進行中的集合節點（SequenceStart／MappingStart 後、End 前）。
    stack: Vec<usize>,
    root: Option<usize>,
    /// 已開始的文件數；超過一份即為錯誤（`parseDocument` 的 MULTIPLE_DOCS）。
    doc_count: usize,
}

/// pass 1：把 saphyr 的事件組合成節點樹（對應 yaml 套件的 composer）。
fn compose(source: &str) -> Result<Composed, ConfigParseError> {
    let mut parser = Parser::new_from_str(source);
    let mut state = ComposeState::default();
    while let Some(result) = parser.next_event() {
        let (event, _) = result.map_err(|_| ConfigParseError::new(YAML_INVALID))?;
        match event {
            Event::StreamStart | Event::StreamEnd | Event::Nothing => {}
            Event::DocumentStart(_) => {
                state.doc_count += 1;
                if state.doc_count > 1 {
                    return Err(ConfigParseError::new(YAML_INVALID));
                }
            }
            Event::DocumentEnd => {}
            Event::Alias(anchor_id) => {
                let node = state.push_node(0, NodeKind::Alias(anchor_id));
                state.attach(node)?;
            }
            Event::Scalar(value, style, anchor_id, tag) => {
                let resolved = resolve_scalar(&value, style, tag.as_deref());
                let node = state.push_node(anchor_id, NodeKind::Scalar(resolved));
                state.attach(node)?;
            }
            Event::SequenceStart(anchor_id, _) => {
                // 集合節點的 tag 不影響輸出（yaml 套件仍組成 seq，未知 tag 落回預設）。
                let node = state.push_node(anchor_id, NodeKind::Seq(Vec::new()));
                state.stack.push(node);
            }
            Event::MappingStart(anchor_id, _) => {
                let node = state.push_node(
                    anchor_id,
                    NodeKind::Map(MapNode {
                        entries: Vec::new(),
                        pending_key: None,
                    }),
                );
                state.stack.push(node);
            }
            Event::SequenceEnd | Event::MappingEnd => {
                let node = match state.stack.pop() {
                    Some(node) => node,
                    None => return Err(ConfigParseError::new(YAML_INVALID)),
                };
                state.attach(node)?;
            }
        }
    }
    Ok(Composed {
        nodes: state.nodes,
        anchor_targets: state.anchor_targets,
        root: state.root,
    })
}

impl ComposeState {
    /// 新增節點；有 anchor 時記錄 anchor_id → 節點的對照。
    fn push_node(&mut self, anchor_id: usize, kind: NodeKind) -> usize {
        let id = self.nodes.len();
        self.nodes.push(Node { anchor_id, kind });
        if anchor_id > 0 {
            self.anchor_targets.insert(anchor_id, id);
        }
        id
    }

    /// 把子節點接到父節點（或成為根節點）；對 map 鍵做 `uniqueKeys` 檢查
    ///（比較解析後的標量值，見 `resolve-block-map.js` 的 `mapIncludes`）。
    fn attach(&mut self, child: usize) -> Result<(), ConfigParseError> {
        let Some(parent) = self.stack.last().copied() else {
            // 沒有父節點：這是根節點。一個文件只會有一個根節點（saphyr 保證）。
            if self.root.is_some() {
                return Err(ConfigParseError::new(YAML_INVALID));
            }
            self.root = Some(child);
            return Ok(());
        };
        let is_map = matches!(self.nodes[parent].kind, NodeKind::Map(_));
        if !is_map {
            match &mut self.nodes[parent].kind {
                NodeKind::Seq(items) => items.push(child),
                // saphyr 的狀態機不會把標量／alias 留在棧上；防禦性錯誤。
                _ => return Err(ConfigParseError::new(YAML_INVALID)),
            }
            return Ok(());
        }
        // 鍵或值：尚未收到值時，此節點是鍵。
        let is_key =
            matches!(&self.nodes[parent].kind, NodeKind::Map(map) if map.pending_key.is_none());
        if is_key {
            // 標量鍵的重複檢查：比較解析後的值（NaN 不算相等，JS `===`）。
            if let NodeKind::Scalar(resolved) = &self.nodes[child].kind {
                let duplicate = match &self.nodes[parent].kind {
                    NodeKind::Map(map) => map.entries.iter().any(|(key, _)| {
                        matches!(&self.nodes[*key].kind, NodeKind::Scalar(existing) if existing == resolved)
                    }),
                    _ => false,
                };
                if duplicate {
                    return Err(ConfigParseError::new(YAML_INVALID));
                }
            }
        }
        match &mut self.nodes[parent].kind {
            NodeKind::Map(map) => {
                if is_key {
                    map.pending_key = Some(child);
                } else if let Some(key) = map.pending_key.take() {
                    map.entries.push((key, child));
                }
            }
            // 上面已排除非 map 的情況。
            _ => return Err(ConfigParseError::new(YAML_INVALID)),
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 標量解析（core schema）
// ---------------------------------------------------------------------------

/// 標量 tag 的種類；其餘 tag 一律當字串。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CoreTag {
    Int,
    Float,
    Bool,
    Null,
}

/// 把 saphyr 的 tag 正規化成 core schema 的標量 tag。
///
/// `!!x` 會被 saphyr 解析成 handle `tag:yaml.org,2002:`；`!<tag:yaml.org,2002:x>`
/// 是 handle 空字串、suffix 為完整 URI；`!x`、`!`、未宣告的 handle 都是 local tag。
/// `!!seq`、`!!map` 等非標量的 core tag套用在標量上時，yaml 套件找不到可用的
/// 標量 tag，落回字串，這裡也回 [`None`]。
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

/// 解析標量（對應 `compose-scalar.js`）：
/// 沒有 tag 時只有 plain 標量依 core schema 的 test 解析，引號與 block 一律字串；
/// 有 tag 時用該型別的 test 解析（不符則落回字串，照 yaml 套件行為）。
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
        // `!!str` 與其他 tag（`!custom`、`!`、`!!seq`…）一律字串。
        None => None,
    };
    resolved.unwrap_or_else(|| ResolvedScalar::Str(source.to_string()))
}

/// plain 標量的解析：依 core schema 的 test 順序（都沒有 test 符合才是字串）。
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

/// core schema 的 null test：`~`、`null`、`Null`、`NULL`、空值。
fn is_null_scalar(source: &str) -> bool {
    matches!(source, "~" | "null" | "Null" | "NULL") || source.is_empty()
}

/// core schema 的 bool test：`true`／`false` 的大小寫組合；`yes`、`on` 是字串。
fn bool_scalar(source: &str) -> Option<bool> {
    match source {
        "true" | "True" | "TRUE" => Some(true),
        "false" | "False" | "FALSE" => Some(false),
        _ => None,
    }
}

/// int 系列的 test（依 schema 順序：8 進位 → 10 進位 → 16 進位）。
fn int_scalar(source: &str) -> Option<f64> {
    octal_int(source)
        .or_else(|| decimal_int(source))
        .or_else(|| hex_int(source))
}

/// `0o[0-7]+`（yaml 套件 intOct）。
fn octal_int(source: &str) -> Option<f64> {
    let digits = source.strip_prefix("0o")?;
    digits_to_f64(digits, 8)
}

/// `[-+]?[0-9]+`（yaml 套件 int）：`010` → 10、`+12` → 12、`-0` → -0。
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

/// `0x[0-9a-fA-F]+`（yaml 套件 intHex）；不帶正負號（`+0x1F` 是字串）。
fn hex_int(source: &str) -> Option<f64> {
    let digits = source.strip_prefix("0x")?;
    digits_to_f64(digits, 16)
}

/// 把指定位基的數字字串轉成 f64（照 JS `parseInt` 的字串轉換，正確捨入）。
/// 超過 u128 可表示範圍的極端輸入改用累積乘法（僅可能差最後一位的捨入）。
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

/// float 系列的 test（依 schema 順序：NaN／±Inf → 指數 → 一般小數）。
fn float_scalar(source: &str) -> Option<f64> {
    special_float(source)
        .or_else(|| exponent_float(source))
        .or_else(|| plain_float(source))
}

/// `[-+]?\.(?:inf|Inf|INF)` 與 `\.nan|\.NaN|\.NAN`（yaml 套件 floatNaN）。
fn special_float(source: &str) -> Option<f64> {
    match source {
        ".inf" | ".Inf" | ".INF" => Some(f64::INFINITY),
        "+.inf" | "+.Inf" | "+.INF" => Some(f64::INFINITY),
        "-.inf" | "-.Inf" | "-.INF" => Some(f64::NEG_INFINITY),
        ".nan" | ".NaN" | ".NAN" => Some(f64::NAN),
        _ => None,
    }
}

/// `[-+]?(?:\.[0-9]+|[0-9]+(?:\.[0-9]*)?)[eE][-+]?[0-9]+`（yaml 套件 floatExp）。
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

/// `[-+]?(?:\.[0-9]+|[0-9]+\.[0-9]*)`（yaml 套件 float）：必須有小數點、無指數。
fn plain_float(source: &str) -> Option<f64> {
    let mantissa = source.strip_prefix(['+', '-']).unwrap_or(source);
    if !mantissa.contains('.') || !valid_float_mantissa(mantissa) {
        return None;
    }
    source.parse::<f64>().ok()
}

/// 檢查去掉正負號後的尾數形狀：`.digits` 或 `digits(.digits?)?`（含 `1.`、`.5`）。
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

// ---------------------------------------------------------------------------
// pass 2：toJS（節點樹 → JsValue）
// ---------------------------------------------------------------------------

/// 對應 yaml 套件 toJS 的 `ctx.anchors` 資料（`nodes/Alias.js` 的 `data`）。
struct AnchorData {
    /// 解析計數：anchor 節點自身轉換算 1，之後每次解析 alias +1。
    count: u64,
    /// alias 倍數（`data.aliasCount`）；0 表示尚未計算，或計算結果為 0（此時每次重算）。
    alias_count: u64,
    /// 轉換結果；`None` 表示節點還在轉換中（是目前路徑的祖先）。
    res: Option<JsValue>,
}

/// pass 2：把節點樹依文件順序轉成 [`JsValue`]（對應 `Document#toJS`）。
struct Converter<'a> {
    nodes: &'a [Node],
    /// anchor_id → 帶該 anchor 的節點索引（pass 1 產物）。
    anchor_targets: &'a HashMap<usize, usize>,
    /// 已開始轉換的 anchor 節點資料，以節點索引為鍵（照 JS 以節點物件為鍵）。
    data: HashMap<usize, AnchorData>,
    /// 轉換期間發現 alias 指向仍在轉換中的祖先節點（循環）。
    cycle: bool,
}

impl<'a> Converter<'a> {
    fn new(composed: &'a Composed) -> Self {
        Self {
            nodes: &composed.nodes,
            anchor_targets: &composed.anchor_targets,
            data: HashMap::new(),
            cycle: false,
        }
    }

    /// 轉換單一節點；順序照 yaml 套件：集合先建立 data（count = 1），
    /// map 逐對先鍵後值，alias 解析時更新計數。
    fn convert(&mut self, node_id: usize) -> Result<JsValue, ConfigParseError> {
        match &self.nodes[node_id].kind {
            NodeKind::Scalar(resolved) => {
                let value = scalar_to_js(resolved);
                if self.nodes[node_id].anchor_id > 0 {
                    self.data.insert(
                        node_id,
                        AnchorData {
                            count: 1,
                            alias_count: 0,
                            res: Some(value.clone()),
                        },
                    );
                }
                Ok(value)
            }
            NodeKind::Alias(anchor_id) => self.convert_alias(*anchor_id),
            NodeKind::Seq(_) => {
                let items = match &self.nodes[node_id].kind {
                    NodeKind::Seq(items) => items.clone(),
                    _ => return Err(ConfigParseError::new(YAML_INVALID)),
                };
                let anchored = self.nodes[node_id].anchor_id > 0;
                if anchored {
                    self.data.insert(
                        node_id,
                        AnchorData {
                            count: 1,
                            alias_count: 0,
                            res: None,
                        },
                    );
                }
                let mut values = Vec::with_capacity(items.len());
                for item in items {
                    values.push(self.convert(item)?);
                }
                if anchored && let Some(data) = self.data.get_mut(&node_id) {
                    data.res = Some(JsValue::Array(values.clone()));
                }
                Ok(JsValue::Array(values))
            }
            NodeKind::Map(_) => {
                let entries = match &self.nodes[node_id].kind {
                    NodeKind::Map(map) => map.entries.clone(),
                    _ => return Err(ConfigParseError::new(YAML_INVALID)),
                };
                let anchored = self.nodes[node_id].anchor_id > 0;
                if anchored {
                    self.data.insert(
                        node_id,
                        AnchorData {
                            count: 1,
                            alias_count: 0,
                            res: None,
                        },
                    );
                }
                let mut entries_out: Vec<(String, JsValue)> = Vec::with_capacity(entries.len());
                for (key_node, value_node) in entries {
                    // 順序照 addPairToJSMap：先鍵後值。
                    let key_value = self.convert(key_node)?;
                    let value = self.convert(value_node)?;
                    let key = js_key_string(&key_value);
                    upsert_object_entry(&mut entries_out, key, value);
                }
                if anchored && let Some(data) = self.data.get_mut(&node_id) {
                    data.res = Some(JsValue::Object(entries_out.clone()));
                }
                Ok(JsValue::Object(entries_out))
            }
        }
    }

    /// 解析 alias（對應 `Alias.js` 的 `toJSON`）。
    fn convert_alias(&mut self, anchor_id: usize) -> Result<JsValue, ConfigParseError> {
        // saphyr 在掃描階段就解析 alias；找不到對應節點視為語法錯誤（不會發生）。
        let Some(target) = self.anchor_targets.get(&anchor_id).copied() else {
            return Err(ConfigParseError::new(YAML_INVALID));
        };
        let Some(data) = self.data.get_mut(&target) else {
            // 目標尚未開始轉換：alias 一定在 anchor 之後，不會發生；防禦性錯誤。
            return Err(ConfigParseError::new(YAML_INVALID));
        };
        data.count += 1;
        let count = data.count;
        let cached_alias_count = data.alias_count;
        let in_progress = data.res.is_none();
        let resolved = data.res.clone();

        if in_progress {
            // 目標是仍在轉換中的祖先：JS 會共用同一個物件，最後由 assertAcyclic
            // 擋下；這裡以 Null 佔位並記下循環，等整份轉換完成後回報。
            self.cycle = true;
            return Ok(JsValue::Null);
        }
        let Some(res) = resolved else {
            return Err(ConfigParseError::new(YAML_INVALID));
        };
        let alias_count = if cached_alias_count == 0 {
            // 第一次需要時計算倍數（之後若仍是 0，每次重算，照 JS 的 `=== 0`）。
            let computed = self.get_alias_count(target);
            if let Some(data) = self.data.get_mut(&target) {
                data.alias_count = computed;
            }
            computed
        } else {
            cached_alias_count
        };
        if count.saturating_mul(alias_count) > MAX_ALIAS_COUNT {
            return Err(ConfigParseError::new(YAML_INVALID));
        }
        Ok(res)
    }

    /// 對應 `Alias.js` 的 `getAliasCount`：節點內的 alias 倍數。
    /// 標量為 1、集合取元素的最大值（空集合為 0）、alias 取其目標的
    /// `count × alias_count`（以當下的計數計算）。
    fn get_alias_count(&self, node_id: usize) -> u64 {
        match &self.nodes[node_id].kind {
            NodeKind::Alias(anchor_id) => self
                .anchor_targets
                .get(anchor_id)
                .copied()
                .and_then(|target| self.data.get(&target))
                .map(|data| data.count.saturating_mul(data.alias_count))
                .unwrap_or(0),
            NodeKind::Seq(items) => items
                .iter()
                .map(|&item| self.get_alias_count(item))
                .max()
                .unwrap_or(0),
            NodeKind::Map(map) => map
                .entries
                .iter()
                .map(|(key, value)| self.get_alias_count(*key).max(self.get_alias_count(*value)))
                .max()
                .unwrap_or(0),
            NodeKind::Scalar(_) => 1,
        }
    }
}

/// 把組合階段的標量結果轉成 [`JsValue`]。
fn scalar_to_js(resolved: &ResolvedScalar) -> JsValue {
    match resolved {
        ResolvedScalar::Null => JsValue::Null,
        ResolvedScalar::Bool(boolean) => JsValue::Bool(*boolean),
        ResolvedScalar::Num(number) => JsValue::Number(*number),
        ResolvedScalar::Str(text) => JsValue::String(text.clone()),
    }
}

/// map 的鍵：依 JS `String()` 轉字串（對應 `stringifyKey`；null 鍵為空字串）。
fn js_key_string(value: &JsValue) -> String {
    match value {
        JsValue::Null => String::new(),
        JsValue::Bool(true) => "true".to_string(),
        JsValue::Bool(false) => "false".to_string(),
        JsValue::Number(number) => js_number_to_string(*number),
        JsValue::String(text) => text.clone(),
        // 集合鍵：yaml 套件會把節點字串化成 YAML（如 `[ 1, 2 ]`）；設定檔不會
        // 出現，以近似的 flow 輸出處理（見 doc/decisions.md）。
        JsValue::Array(_) | JsValue::Object(_) => collection_key_string(value),
    }
}

/// 集合鍵的近似字串化（對應 yaml 套件 `stringifyCollection` 的 flow 輸出）。
fn collection_key_string(value: &JsValue) -> String {
    match value {
        JsValue::Array(items) if items.is_empty() => "[]".to_string(),
        JsValue::Array(items) => format!(
            "[ {} ]",
            items
                .iter()
                .map(stringify_flow_scalar)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        JsValue::Object(entries) if entries.is_empty() => "{}".to_string(),
        JsValue::Object(entries) => format!(
            "{{ {} }}",
            entries
                .iter()
                .map(|(key, value)| format!("{}: {}", key, stringify_flow_scalar(value)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        // 非集合不會走到這裡（外層已分派）；照標量輸出以防禦。
        scalar => stringify_flow_scalar(scalar),
    }
}

/// flow 輸出的標量文字（集合遞迴；字串照 plain 輸出，僅供鍵字串化近似使用）。
fn stringify_flow_scalar(value: &JsValue) -> String {
    match value {
        JsValue::Null => "null".to_string(),
        JsValue::Bool(true) => "true".to_string(),
        JsValue::Bool(false) => "false".to_string(),
        JsValue::Number(number) => js_number_to_string(*number),
        JsValue::String(text) => text.clone(),
        JsValue::Array(_) | JsValue::Object(_) => collection_key_string(value),
    }
}

/// 把鍵值放進物件：同鍵時覆寫原位置的值（保留插入順序，照 `addPairToJSMap`）。
fn upsert_object_entry(entries: &mut Vec<(String, JsValue)>, key: String, value: JsValue) {
    if let Some(existing) = entries
        .iter_mut()
        .find(|(existing_key, _)| *existing_key == key)
    {
        existing.1 = value;
    } else {
        entries.push((key, value));
    }
}

#[cfg(test)]
mod tests;
