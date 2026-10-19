//! 設定寫回的測試：golden 案例（`testdata/golden/config/*.input.yml`，預期值由
//! 舊版實作產生，D11）、舊版單元測試移植（`test/unit/runtime-config-manager.test.ts`）
//! 與取代規則的單元測試。
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::{
    ConfigFileIo, ConfigWriteError, RangeNodeKind, ReplacedRange, RuntimeWatchConfig,
    RuntimeWatchConfigTarget, TokioConfigFileIo, YamlRuntimeConfigManager, block_seq_dash_start,
    build_persisted_content, compose_with_ranges,
};
use crate::config::yaml::parse_yaml;

/// golden 案例目錄。
const GOLDEN_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/golden/config");

// ---------------------------------------------------------------------------
// 測試用注入實作（記憶體 buffer，對應舊版注入 readFile／writeFile 的介面）
// ---------------------------------------------------------------------------

/// 記憶體 buffer 的檔案讀寫；buffer 由測試端共享（`replaceFile` 直接改內容）。
#[derive(Clone)]
struct MemIo {
    content: Rc<RefCell<String>>,
    writes: Rc<RefCell<usize>>,
}

impl MemIo {
    fn new(source: &str) -> Self {
        Self {
            content: Rc::new(RefCell::new(source.to_string())),
            writes: Rc::new(RefCell::new(0)),
        }
    }

    fn read(&self) -> String {
        self.content.borrow().clone()
    }
}

impl ConfigFileIo for MemIo {
    async fn read(&self, _path: &str) -> Result<String, ConfigWriteError> {
        Ok(self.read())
    }

    async fn write(&self, _path: &str, data: String) -> Result<(), ConfigWriteError> {
        *self.content.borrow_mut() = data;
        *self.writes.borrow_mut() += 1;
        Ok(())
    }
}

/// 記錄 `update_config` 呼叫的目標（對應舊版測試的 `vi.fn` spy）。
#[derive(Clone)]
struct RecordingTarget {
    updates: Arc<Mutex<Vec<RuntimeWatchConfig>>>,
}

impl RuntimeWatchConfigTarget for RecordingTarget {
    async fn update_config(&self, config: RuntimeWatchConfig) -> Result<(), ConfigWriteError> {
        match self.updates.lock() {
            Ok(mut guard) => {
                guard.push(config);
                Ok(())
            }
            Err(_) => Err(ConfigWriteError::Io("poisoned".to_string())),
        }
    }
}

/// 一次寫檔的觀測資料（子序列檢查用）。
struct WriteRecord {
    input: String,
    output: String,
    ranges: Vec<ReplacedRange>,
}

/// 把設定轉成 golden 的 JSON 形狀（`{"channels", "maxConcurrentStreams"}`）。
fn config_to_json(config: &RuntimeWatchConfig) -> Value {
    json!({
        "channels": config.channels,
        "maxConcurrentStreams": config.max_concurrent_streams,
    })
}

/// 比較 golden 的 JSON：數字先轉 f64（NaN 視為相等）。
fn json_eq(actual: &Value, expected: &Value) -> bool {
    match (actual, expected) {
        (Value::Number(x), Value::Number(y)) => {
            let (Some(x), Some(y)) = (x.as_f64(), y.as_f64()) else {
                return false;
            };
            x == y || (x.is_nan() && y.is_nan())
        }
        (Value::String(x), Value::String(y)) => x == y,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Null, Value::Null) => true,
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y.iter()).all(|(a, b)| json_eq(a, b))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .zip(y.iter())
                    .all(|((key_x, value_x), (key_y, value_y))| {
                        key_x == key_y && json_eq(value_x, value_y)
                    })
        }
        _ => false,
    }
}

/// 把 ops 陣列轉成 Vec<String>。
fn json_strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// 把 op 的結果轉成 golden 的 JSON 形狀。
fn result_to_json(result: &Result<RuntimeWatchConfig, ConfigWriteError>) -> Value {
    match result {
        Ok(config) => json!({ "value": config_to_json(config) }),
        Err(error) => json!({ "error": { "name": error.name(), "message": error.message() } }),
    }
}

/// 把輸入切成行：回傳 (行文字, 行起點, 行終點)（終點不含換行符）；
/// 檔尾以換行結尾時不產生空的尾行。
fn input_lines(input: &str) -> Vec<(&str, usize, usize)> {
    let mut lines = Vec::new();
    let mut start = 0;
    for (offset, c) in input.char_indices() {
        if c == '\n' {
            lines.push((&input[start..offset], start, offset));
            start = offset + 1;
        }
    }
    if start < input.len() {
        lines.push((&input[start..], start, input.len()));
    }
    lines
}

/// 檢查：輸入中與取代範圍沒有交集的行，必須依原順序逐字出現在輸出裡。
fn surviving_lines_appear_in_order(input: &str, output: &str, replaced: &[ReplacedRange]) -> bool {
    let output_lines: Vec<&str> = output.split('\n').collect();
    let mut cursor = 0usize;
    for (text, start, end) in input_lines(input) {
        if replaced
            .iter()
            .any(|range| range.start < end && range.end > start)
        {
            continue;
        }
        let Some(offset) = output_lines[cursor..].iter().position(|line| *line == text) else {
            return false;
        };
        cursor += offset + 1;
    }
    true
}

// ---------------------------------------------------------------------------
// golden 案例（一案例一測試）
// ---------------------------------------------------------------------------

/// 逐 op 重播 golden 案例，比對 `results`、`updates`、`writes`、最終內容
/// （解析後的值）與每筆寫檔的子序列檢查。
async fn run_golden_writeback_case(name: &str) {
    let base = format!("{GOLDEN_DIR}/{name}");
    let input = std::fs::read_to_string(format!("{base}.input.yml"))
        .unwrap_or_else(|error| panic!("讀不到 {base}.input.yml：{error}"));
    let ops: Value = serde_json::from_str(
        &std::fs::read_to_string(format!("{base}.ops.json"))
            .unwrap_or_else(|error| panic!("讀不到 {base}.ops.json：{error}")),
    )
    .unwrap();
    let expected: Value = serde_json::from_str(
        &std::fs::read_to_string(format!("{base}.expected.json"))
            .unwrap_or_else(|error| panic!("讀不到 {base}.expected.json：{error}")),
    )
    .unwrap();

    let io = MemIo::new(&input);
    let initial = &ops["initial"];
    let manager = YamlRuntimeConfigManager::new(
        "config.yml",
        RuntimeWatchConfig {
            channels: json_strings(&initial["channels"]),
            max_concurrent_streams: initial["maxConcurrentStreams"].as_f64().unwrap(),
        },
        RecordingTarget {
            updates: Arc::new(Mutex::new(Vec::new())),
        },
        io.clone(),
    );

    let mut results = Vec::new();
    let mut records: Vec<WriteRecord> = Vec::new();
    for op in ops["ops"].as_array().unwrap() {
        if let Some(text) = op.get("replaceFile").and_then(Value::as_str) {
            *io.content.borrow_mut() = text.to_string();
            results.push(json!({ "replaced": true }));
            continue;
        }
        let before = io.read();
        let writes_before = *io.writes.borrow();
        let result = if let Some(channels) = op.get("setChannels") {
            manager.set_channels(&json_strings(channels)).await
        } else {
            manager
                .set_max_concurrent_streams(op["setMaxConcurrentStreams"].as_f64().unwrap())
                .await
        };
        results.push(result_to_json(&result));
        if *io.writes.borrow() > writes_before {
            records.push(WriteRecord {
                input: before,
                output: io.read(),
                ranges: manager.last_replaced_ranges(),
            });
        }
    }

    let updates: Vec<Value> = manager
        .target
        .updates
        .lock()
        .map(|guard| guard.iter().map(config_to_json).collect())
        .unwrap_or_default();
    let actual = json!({
        "results": results,
        "updates": updates,
        "writes": *io.writes.borrow(),
    });
    assert!(
        json_eq(&actual, &expected),
        "golden 案例不符：{name}\n實際：{actual}\n預期：{expected}"
    );

    // 有寫檔時：輸出解析後的值與 expected.yml 相同（用移植的 parse_yaml）。
    // 檔案處於語法錯誤狀態的案例（如 19 的 replaceFile 尾聲）兩邊都解析失敗，
    // 以相同的錯誤訊息視為相等。
    if !records.is_empty() {
        let expected_source = std::fs::read_to_string(format!("{base}.expected.yml"))
            .unwrap_or_else(|error| panic!("讀不到 {base}.expected.yml：{error}"));
        match (parse_yaml(&expected_source), parse_yaml(&io.read())) {
            (Ok(expected_value), Ok(actual_value)) => {
                assert_eq!(
                    actual_value, expected_value,
                    "golden 案例的輸出值不符：{name}"
                );
            }
            (Err(expected_error), Err(actual_error)) => {
                assert_eq!(
                    actual_error.message(),
                    expected_error.message(),
                    "golden 案例的輸出錯誤不符：{name}"
                );
            }
            (expected, actual) => panic!(
                "golden 案例的輸出可解析性不符：{name}\n實際：{actual:?}\n預期：{expected:?}"
            ),
        }
    }

    // 每筆寫檔：與取代範圍沒有交集的輸入行依原順序逐字出現在輸出裡。
    for record in &records {
        assert!(
            surviving_lines_appear_in_order(&record.input, &record.output, &record.ranges),
            "子序列檢查失敗：{name}\n輸入：{:?}\n輸出：{:?}",
            record.input,
            record.output
        );
    }
}

macro_rules! golden_writeback_case {
    ($test_name:ident, $case_name:literal, $doc:literal) => {
        #[doc = $doc]
        #[tokio::test]
        async fn $test_name() {
            run_golden_writeback_case($case_name).await;
        }
    };
}

golden_writeback_case!(
    golden_01_basic,
    "01-basic",
    "golden 01：block sequence 原地取代。"
);
golden_writeback_case!(golden_02_flow, "02-flow", "golden 02：flow sequence 取代。");
golden_writeback_case!(
    golden_03_inner_comments,
    "03-inner-comments",
    "golden 03：鍵行與序列內註解保留。"
);
golden_writeback_case!(
    golden_04_missing_max,
    "04-missing-max",
    "golden 04：鍵不存在時加在檔尾。"
);
golden_writeback_case!(
    golden_05_quoting,
    "05-quoting",
    "golden 05：會被解析成非字串的名稱用雙引號、大小寫去重。"
);
golden_writeback_case!(golden_06_crlf, "06-crlf", "golden 06：CRLF 換行沿用。");
golden_writeback_case!(
    golden_07_doc_markers,
    "07-doc-markers",
    "golden 07：`---` 與文件尾註解。"
);
golden_writeback_case!(
    golden_08_anchor,
    "08-anchor",
    "golden 08：被取代節點的 anchor 被範圍外 alias 引用。"
);
golden_writeback_case!(
    golden_09_indent4_long,
    "09-indent4-long",
    "golden 09：4 格縮排與其他內容不動。"
);
golden_writeback_case!(
    golden_10_syntax_error,
    "10-syntax-error",
    "golden 10：語法錯誤不寫檔。"
);
golden_writeback_case!(
    golden_11_root_seq,
    "11-root-seq",
    "golden 11：根節點是序列不寫檔。"
);
golden_writeback_case!(
    golden_12_duplicate_key,
    "12-duplicate-key",
    "golden 12：重複的鍵不寫檔。"
);
golden_writeback_case!(
    golden_13_blank_lines,
    "13-blank-lines",
    "golden 13：序列前後的空行保留。"
);
golden_writeback_case!(
    golden_14_anchor_elsewhere,
    "14-anchor-elsewhere",
    "golden 14：範圍外的 anchor／alias 不受影響。"
);
golden_writeback_case!(
    golden_15_max_anchor,
    "15-max-anchor",
    "golden 15：標量取代保留 anchor 與 tag。"
);
golden_writeback_case!(
    golden_16_nested_keys,
    "16-nested-keys",
    "golden 16：只動根層的鍵。"
);
golden_writeback_case!(
    golden_17_last_line_no_newline,
    "17-last-line-no-newline",
    "golden 17：檔尾沒有換行時補上一個。"
);
golden_writeback_case!(
    golden_18_validation,
    "18-validation",
    "golden 18：欄位驗證的順序與訊息。"
);
golden_writeback_case!(
    golden_19_external_edit,
    "19-external-edit",
    "golden 19：外部改過檔案後的每次 persist 都重新讀檔。"
);
golden_writeback_case!(
    golden_20_empty,
    "20-empty",
    "golden 20：空檔的根節點不是物件。"
);
golden_writeback_case!(
    golden_21_comment_only,
    "21-comment-only",
    "golden 21：只有註解的檔案。"
);
golden_writeback_case!(
    golden_22_flow_trailing_comment,
    "22-flow-trailing-comment",
    "golden 22：flow sequence 與鍵行註解。"
);

// ---------------------------------------------------------------------------
// 舊版單元測試移植（test/unit/runtime-config-manager.test.ts，4 個）
// ---------------------------------------------------------------------------

/// 在暫存目錄建立真實設定檔（對應舊版 `createConfig`）；回傳 (目錄, 檔案路徑)，
/// 目錄由呼叫端持有以存活到測試結束。
async fn create_config(source: &str) -> (tempfile::TempDir, String) {
    let directory = tempfile::Builder::new()
        .prefix("runtime-config-test-")
        .tempdir()
        .unwrap_or_else(|error| panic!("建暫存目錄失敗：{error}"));
    let path = directory.path().join("config.yml");
    tokio::fs::write(&path, source)
        .await
        .unwrap_or_else(|error| panic!("寫暫存設定檔失敗：{error}"));
    let path_string = path.to_string_lossy().to_string();
    (directory, path_string)
}

// 原測試：更新 channels 並保留其他 YAML 設定與註解
#[tokio::test]
async fn updates_channels_and_keeps_other_yaml_settings_and_comments() {
    let (_dir, path) = create_config(
        "# keep this comment\nchannels:\n  - first\n  - second\nmax_concurrent_streams: 2\nlog_level: debug\ntwitch_api:\n  client_id: ${TWITCH_CLIENT_ID}\n",
    )
    .await;
    let updates = Arc::new(Mutex::new(Vec::new()));
    let manager = YamlRuntimeConfigManager::new(
        path.clone(),
        RuntimeWatchConfig {
            channels: vec!["first".to_string(), "second".to_string()],
            max_concurrent_streams: 2.0,
        },
        RecordingTarget {
            updates: updates.clone(),
        },
        TokioConfigFileIo,
    );

    let result = manager
        .set_channels(&["second".to_string(), "third".to_string()])
        .await
        .unwrap_or_else(|error| panic!("setChannels 應成功：{error:?}"));
    let source = tokio::fs::read_to_string(&path)
        .await
        .unwrap_or_else(|error| panic!("讀回設定檔失敗：{error}"));

    assert_eq!(
        result,
        RuntimeWatchConfig {
            channels: vec!["second".to_string(), "third".to_string()],
            max_concurrent_streams: 2.0,
        }
    );
    let recorded = updates
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0], result);
    assert!(
        source.contains("# keep this comment"),
        "註解應保留：{source:?}"
    );
    assert!(
        source.contains("log_level: debug"),
        "其他設定應保留：{source:?}"
    );
    assert!(
        source.contains(r"client_id: ${TWITCH_CLIENT_ID}"),
        "其他設定應保留：{source:?}"
    );
    assert!(source.contains("  - second"), "既有頻道應保留：{source:?}");
    assert!(source.contains("  - third"), "新頻道應寫入：{source:?}");
}

// 原測試：頻道減少時自動降低最大同時觀看
#[tokio::test]
async fn downgrades_max_concurrent_streams_when_channels_shrink() {
    let (_dir, path) =
        create_config("channels: [first, second]\nmax_concurrent_streams: 2\n").await;
    let manager = YamlRuntimeConfigManager::new(
        path.clone(),
        RuntimeWatchConfig {
            channels: vec!["first".to_string(), "second".to_string()],
            max_concurrent_streams: 2.0,
        },
        RecordingTarget {
            updates: Arc::new(Mutex::new(Vec::new())),
        },
        TokioConfigFileIo,
    );

    let result = manager
        .set_channels(&["first".to_string()])
        .await
        .unwrap_or_else(|error| panic!("setChannels 應成功：{error:?}"));

    assert_eq!(
        result,
        RuntimeWatchConfig {
            channels: vec!["first".to_string()],
            max_concurrent_streams: 1.0,
        }
    );
    let source = tokio::fs::read_to_string(&path)
        .await
        .unwrap_or_else(|error| panic!("讀回設定檔失敗：{error}"));
    assert!(
        source.contains("max_concurrent_streams: 1"),
        "上限應自動降為 1：{source:?}"
    );
}

// 原測試：拒絕空頻道、非法名稱與超過頻道數的上限
#[tokio::test]
async fn rejects_empty_channels_invalid_names_and_too_large_limit() {
    let (_dir, path) = create_config("channels: [first]\n").await;
    let manager = YamlRuntimeConfigManager::new(
        path,
        RuntimeWatchConfig {
            channels: vec!["first".to_string()],
            max_concurrent_streams: 1.0,
        },
        RecordingTarget {
            updates: Arc::new(Mutex::new(Vec::new())),
        },
        TokioConfigFileIo,
    );

    for (name, outcome) in [
        ("空頻道", manager.set_channels(&[]).await.err()),
        (
            "非法名稱",
            manager.set_channels(&["bad-name".to_string()]).await.err(),
        ),
        (
            "超過頻道數的上限",
            manager.set_max_concurrent_streams(2.0).await.err(),
        ),
    ] {
        let Some(error) = outcome else {
            panic!("應拒絕（{name}）");
        };
        assert_eq!(error.name(), "ConfigValidationError", "{name}：{error:?}");
    }
}

// 原測試：大小寫不敏感去除重複頻道
#[tokio::test]
async fn deduplicates_channels_case_insensitively() {
    let (_dir, path) = create_config("channels: [first]\n").await;
    let manager = YamlRuntimeConfigManager::new(
        path,
        RuntimeWatchConfig {
            channels: vec!["first".to_string()],
            max_concurrent_streams: 1.0,
        },
        RecordingTarget {
            updates: Arc::new(Mutex::new(Vec::new())),
        },
        TokioConfigFileIo,
    );

    let result = manager
        .set_channels(&[
            "First".to_string(),
            "first".to_string(),
            "Second".to_string(),
        ])
        .await
        .unwrap_or_else(|error| panic!("setChannels 應成功：{error:?}"));

    assert_eq!(
        result,
        RuntimeWatchConfig {
            channels: vec!["First".to_string(), "Second".to_string()],
            max_concurrent_streams: 1.0,
        }
    );
}

// ---------------------------------------------------------------------------
// 取代規則的單元測試（預期內容以舊版實作驗證過；與計畫／舊版的差異見註解）
// ---------------------------------------------------------------------------

/// 斷言寫回內容逐字相等，並回傳取代範圍。
fn assert_written(source: &str, expected: &str) {
    let (content, ranges) = build_persisted_content(source, &test_config())
        .unwrap_or_else(|error| panic!("應寫回成功：{error:?}"));
    assert_eq!(content, expected, "寫回內容不符（輸入 {source:?}）");
    // 取代範圍都在原檔範圍內。
    for range in ranges {
        assert!(range.end <= source.len() || range.start == range.end);
    }
}

/// 測試用設定：channels 為 `["b"]`、上限 2。
fn test_config() -> RuntimeWatchConfig {
    RuntimeWatchConfig {
        channels: vec!["b".to_string()],
        max_concurrent_streams: 2.0,
    }
}

/// 斷言寫回失敗的 name 與訊息。
fn assert_write_error(source: &str, name: &str, message: &str) {
    let error = build_persisted_content(source, &test_config())
        .expect_err(&format!("應寫回失敗：{source:?}"));
    assert_eq!(
        error.name(),
        name,
        "錯誤類別不符（輸入 {source:?}）：{error:?}"
    );
    assert_eq!(error.message(), message);
}

/// block sequence：取代第一個 `-` 到最後一個項目內容結尾，沿用縮排。
#[test]
fn block_sequence_replaced_in_place() {
    assert_written(
        "channels:\n  - a\nmax_concurrent_streams: 1\n",
        "channels:\n  - b\nmax_concurrent_streams: 2\n",
    );
    assert_written(
        "channels:\n  - a\n  - c\nmax_concurrent_streams: 1\n",
        "channels:\n  - b\nmax_concurrent_streams: 2\n",
    );
    assert_written(
        "channels:\n    - a\nlog: 1\n",
        "channels:\n    - b\nlog: 1\nmax_concurrent_streams: 2\n",
    );
}

/// flow sequence：取代 `[` 到 `]`，輸出 `[a, b]`；範圍外的行尾註解保留
///（舊版會改成 block sequence 並丟掉註解，解析後的值相同）。
#[test]
fn flow_sequence_replaced_in_place() {
    assert_written(
        "channels: [a] # c\nmax_concurrent_streams: 1\n",
        "channels: [b] # c\nmax_concurrent_streams: 2\n",
    );
    assert_written(
        "channels: [\n  a,\n  c,\n]\nlog: 1\n",
        "channels: [b]\nlog: 1\nmax_concurrent_streams: 2\n",
    );
}

/// 其他值（標量、映射、alias、空值）：換掉整個節點（連同 `:` 之後的 anchor／tag）。
/// 預期內容以舊版實作驗證過。
#[test]
fn non_sequence_channels_replaced_as_whole_node() {
    assert_written(
        "channels: 5\nlog: 1\n",
        "channels:\n  - b\nlog: 1\nmax_concurrent_streams: 2\n",
    );
    assert_written(
        "channels:\n  1: 2\nlog_level: x\n",
        "channels:\n  - b\nlog_level: x\nmax_concurrent_streams: 2\n",
    );
    assert_written(
        "x: &ch 1\nchannels: *ch\nlog: 1\n",
        "x: &ch 1\nchannels:\n  - b\nlog: 1\nmax_concurrent_streams: 2\n",
    );
    assert_written(
        "channels:\nlog: 1\n",
        "channels:\n  - b\nlog: 1\nmax_concurrent_streams: 2\n",
    );
    // 帶 anchor 的空值：範圍含換行，新文字補回一個（舊版會移除 anchor，解析後的值相同）。
    assert_written(
        "channels: &x\nlog: 1\n",
        "channels:\n  - b\nlog: 1\nmax_concurrent_streams: 2\n",
    );
}

/// `max_concurrent_streams` 原本是標量：只換標量本身的文字，保留 anchor 與 tag。
/// 預期內容以舊版實作驗證過（案例 15 與 node 實測）。
#[test]
fn scalar_max_keeps_anchor_and_tag() {
    assert_written(
        "channels: [a]\nmax_concurrent_streams: &m 1\ncopy: *m\n",
        "channels: [b]\nmax_concurrent_streams: &m 2\ncopy: *m\n",
    );
    assert_written(
        "channels: [a, b]\nmax_concurrent_streams: !!int 1\n",
        "channels: [b]\nmax_concurrent_streams: !!int 2\n",
    );
    // 引號標量：引號在範圍內，輸出 plain 的數字（照舊版）。
    assert_written(
        "channels: [a, b]\nmax_concurrent_streams: \"1\"\n",
        "channels: [b]\nmax_concurrent_streams: 2\n",
    );
}

/// `max_concurrent_streams` 是隱式空值：在 `:` 之後插入，前置空白照需要補。
/// 預期內容以舊版實作驗證過。
#[test]
fn empty_max_gets_value_inserted() {
    assert_written(
        "channels: [a, b]\nmax_concurrent_streams:\nx: 1\n",
        "channels: [b]\nmax_concurrent_streams: 2\nx: 1\n",
    );
    // 帶 anchor 的空值：插在 anchor 之後（照舊版 `max: &m 2`）。
    assert_written(
        "channels: [a, b]\nmax_concurrent_streams: &m\nx: 1\n",
        "channels: [b]\nmax_concurrent_streams: &m 2\nx: 1\n",
    );
    // 帶 tag 的空值：插在 tag 之後（照舊版 `!!int 2`）。
    assert_written(
        "channels: [a, b]\nmax_concurrent_streams: !!int\nx: 1\n",
        "channels: [b]\nmax_concurrent_streams: !!int 2\nx: 1\n",
    );
    // 鍵行註解：插在註解之前並留一個空白（舊版把註解搬到值之後，解析後的值相同）。
    assert_written(
        "channels: [a, b]\nmax_concurrent_streams: # c\nx: 1\n",
        "channels: [b]\nmax_concurrent_streams: 2 # c\nx: 1\n",
    );
}

/// `max_concurrent_streams` 是集合或 alias：換掉整個節點。預期內容以舊版實作驗證過。
#[test]
fn non_scalar_max_replaced_as_whole_node() {
    assert_written(
        "channels: [a, b]\nmax_concurrent_streams: {a: 1}\n",
        "channels: [b]\nmax_concurrent_streams: 2\n",
    );
    assert_written(
        "channels: [a, b]\nmax_concurrent_streams:\n  - 1\nlog: 1\n",
        "channels: [b]\nmax_concurrent_streams: 2\nlog: 1\n",
    );
    assert_written(
        "x: &m 1\nmax_concurrent_streams: *m\n",
        "x: &m 1\nmax_concurrent_streams: 2\nchannels:\n  - b\n",
    );
}

/// 鍵不存在時加在檔尾，順序先 channels 後 max；檔尾沒有換行時補一個。
/// 預期內容以舊版實作驗證過。
#[test]
fn missing_keys_appended_at_end() {
    // 只有 max 缺。
    assert_written(
        "channels:\n  - a\nlog_level: info\n",
        "channels:\n  - b\nlog_level: info\nmax_concurrent_streams: 2\n",
    );
    // channels 缺、max 在。
    assert_written(
        "x: &m 1\nmax_concurrent_streams: *m\n",
        "x: &m 1\nmax_concurrent_streams: 2\nchannels:\n  - b\n",
    );
    // 兩者都缺。
    assert_written(
        "log_level: info\n",
        "log_level: info\nchannels:\n  - b\nmax_concurrent_streams: 2\n",
    );
    // 檔尾沒有換行：先補換行再加。
    assert_written(
        "channels: [a]",
        "channels: [b]\nmax_concurrent_streams: 2\n",
    );
    // 檔尾沒有換行且兩者都在：輸出以換行結尾（照舊版 String(document)）。
    assert_written(
        "channels:\n  - a\nmax_concurrent_streams: 1",
        "channels:\n  - b\nmax_concurrent_streams: 2\n",
    );
}

/// 換行風格沿用原檔（CRLF）；檔尾沒有換行時補 CRLF。
#[test]
fn crlf_newline_style_is_preserved() {
    assert_written(
        "# c\r\nchannels:\r\n  - a\r\nmax_concurrent_streams: 1\r\nlog: 1\r\n",
        "# c\r\nchannels:\r\n  - b\r\nmax_concurrent_streams: 2\r\nlog: 1\r\n",
    );
    assert_written(
        "channels: [a]\r\nmax_concurrent_streams: 1\r\n",
        "channels: [b]\r\nmax_concurrent_streams: 2\r\n",
    );
}

/// 頻道名稱的 quoting：會被 core schema 解析成非字串的用雙引號（golden 05 的節選，
/// 預期內容與 golden 相同）。
#[test]
fn channel_names_quoted_only_when_ambiguous() {
    let config = RuntimeWatchConfig {
        channels: [
            "123", "true", "null", "1e3", "0x1F", "1_000", "Yes", "on", "007", "0o17", "Inf", "_x",
        ]
        .iter()
        .map(|name| name.to_string())
        .collect(),
        max_concurrent_streams: 1.0,
    };
    let (content, _) =
        build_persisted_content("channels:\n  - a\nmax_concurrent_streams: 1\n", &config)
            .unwrap_or_else(|error| panic!("應寫回成功：{error:?}"));
    assert_eq!(
        content,
        "channels:\n  - \"123\"\n  - \"true\"\n  - \"null\"\n  - \"1e3\"\n  - \"0x1F\"\n  - 1_000\n  - Yes\n  - on\n  - \"007\"\n  - \"0o17\"\n  - Inf\n  - _x\nmax_concurrent_streams: 1\n"
    );
}

/// 被取代範圍內的 anchor 被範圍外 alias 引用：寫回失敗（golden 08 與項目 anchor
/// 的情況，預期以舊版實作驗證過）。
#[test]
fn dropped_anchor_referenced_outside_fails_write() {
    assert_write_error(
        "channels: &ch\n  - a\nmirror: *ch\nmax_concurrent_streams: 1\n",
        "Error",
        "Unresolved alias (the anchor must be set before the alias): ch",
    );
    assert_write_error(
        "channels:\n  - &x a\nmirror: *x\nmax_concurrent_streams: 1\n",
        "Error",
        "Unresolved alias (the anchor must be set before the alias): x",
    );
    // anchor 在範圍外：可以寫回。
    assert_written(
        "defaults: &d\n  x: 1\nother: *d\nchannels:\n  - a\nmax_concurrent_streams: 1\n",
        "defaults: &d\n  x: 1\nother: *d\nchannels:\n  - b\nmax_concurrent_streams: 2\n",
    );
}

/// 解析錯誤（語法、重複的鍵）與根節點不是 map 的情況。
#[test]
fn invalid_documents_fail_write() {
    assert_write_error(
        "channels: [a\n",
        "ConfigParseError",
        "設定檔格式錯誤：無法更新含有語法錯誤的設定檔",
    );
    assert_write_error(
        "channels: [a]\nchannels: [b]\n",
        "ConfigParseError",
        "設定檔格式錯誤：無法更新含有語法錯誤的設定檔",
    );
    assert_write_error(
        "- a\n",
        "ConfigParseError",
        "設定檔格式錯誤：設定檔根節點必須是物件",
    );
    assert_write_error(
        "",
        "ConfigParseError",
        "設定檔格式錯誤：設定檔根節點必須是物件",
    );
    assert_write_error(
        "# only a comment\n",
        "ConfigParseError",
        "設定檔格式錯誤：設定檔根節點必須是物件",
    );
}

/// UTF-8 BOM 保留在輸出（X3）。
#[test]
fn bom_is_preserved() {
    assert_written(
        "\u{feff}channels: [a]\nmax_concurrent_streams: 1\n",
        "\u{feff}channels: [b]\nmax_concurrent_streams: 2\n",
    );
}

/// indentless sequence（`key:\n- item`）：沿用 `-` 的縮排。
#[test]
fn indentless_sequence_replaced_in_place() {
    assert_written(
        "channels:\n- a\n- c\nmax_concurrent_streams: 1\n",
        "channels:\n- b\nmax_concurrent_streams: 2\n",
    );
}

/// `block_seq_dash_start`：一般 block sequence 在 `-` 上、indentless 在 `-` 之前。
#[test]
fn dash_start_detection() {
    for (source, expected) in [
        ("channels:\n  - a\n", 12usize),
        ("channels:\n- a\n", 10usize),
    ] {
        let composed = compose_with_ranges(source).unwrap_or_else(|()| panic!("應可解析"));
        let root = composed.root.unwrap_or_else(|| panic!("應有根節點"));
        let entries = match &composed.nodes[root].kind {
            RangeNodeKind::Map { entries, .. } => entries.clone(),
            _ => panic!("根節點應是 map"),
        };
        let value = entries[0].1;
        assert_eq!(
            block_seq_dash_start(source, &composed.nodes[value]),
            Some(expected)
        );
    }
}

/// 欄位驗證的 trim：照 JS `trim()` 的字元集合（`channels[0]` 的索引照原順序）。
#[test]
fn channel_validation_trims_like_javascript() {
    let validated =
        super::validate_channels(&[" A ".to_string(), "a".to_string(), "B_1".to_string()])
            .unwrap_or_else(|error| panic!("應驗證成功：{error:?}"));
    assert_eq!(validated, vec!["A".to_string(), "B_1".to_string()]);
}
