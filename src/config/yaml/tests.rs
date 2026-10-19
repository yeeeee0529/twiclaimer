//! YAML 載入的測試：golden 案例逐例比對（`testdata/golden/config/*.load.yml`，
//! 預期值由舊版實作產生，D11），外加 core schema 與 alias 規則的單元測試。
use super::{JsValue, ResolvedScalar, parse_yaml, resolve_plain_scalar, resolve_scalar};
use crate::config::golden::{GOLDEN_DIR, json_eq};
use saphyr_parser::{ScalarStyle, Tag};

/// 測試輔助：字串值。
fn s(value: &str) -> JsValue {
    JsValue::String(value.to_string())
}

/// 測試輔助：物件（插入順序保留）。
fn obj(entries: Vec<(&str, JsValue)>) -> JsValue {
    JsValue::Object(
        entries
            .into_iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect(),
    )
}

/// 測試輔助：陣列。
fn arr(items: Vec<JsValue>) -> JsValue {
    JsValue::Array(items)
}

/// 斷言解析結果等於期望值；錯誤時輸出兩者的 Debug 內容。
fn assert_value(source: &str, expected: JsValue) {
    match parse_yaml(source) {
        Ok(value) => assert_eq!(value, expected, "解析結果不符：{source:?}"),
        Err(error) => panic!("應成功解析 {source:?}，得到 {error:?}"),
    }
}

/// 斷言解析失敗且訊息為 `設定檔格式錯誤：<message>`（不含行號）。
fn assert_parse_error(source: &str, message: &str) {
    let error = parse_yaml(source).expect_err(&format!("應解析失敗：{source:?}"));
    assert_eq!(error.name(), "ConfigParseError");
    assert_eq!(error.message(), format!("設定檔格式錯誤：{message}"));
}

// ---------------------------------------------------------------------------
// golden 案例（一案例一測試）
// ---------------------------------------------------------------------------

/// 讀 golden 案例並比對（`{"value": …}` 或 `{"error": {"name", "message"}}`）。
fn run_golden_load_case(name: &str) {
    let base = format!("{GOLDEN_DIR}/{name}");
    let source = std::fs::read_to_string(format!("{base}.load.yml"))
        .unwrap_or_else(|error| panic!("讀不到 {base}.load.yml：{error}"));
    let expected: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(format!("{base}.load.expected.json"))
            .unwrap_or_else(|error| panic!("讀不到 {base}.load.expected.json：{error}")),
    )
    .unwrap();
    let actual = match parse_yaml(&source) {
        Ok(value) => serde_json::json!({ "value": js_value_to_json(&value) }),
        Err(error) => serde_json::json!({
            "error": { "name": error.name(), "message": error.message() },
        }),
    };
    assert!(
        json_eq(&actual, &expected),
        "golden 案例不符：{name}\n實際：{actual}\n預期：{expected}"
    );
}

/// 把 [`JsValue`] 轉成 golden 的 JSON 表示（非有限數字以 `{"$number": …}` 編碼，
/// 照 `gen-config-golden.mjs` 的 encode）。
fn js_value_to_json(value: &JsValue) -> serde_json::Value {
    match value {
        JsValue::Null => serde_json::Value::Null,
        JsValue::Bool(boolean) => serde_json::Value::Bool(*boolean),
        JsValue::Number(number) => {
            if number.is_nan() {
                serde_json::json!({ "$number": "NaN" })
            } else if *number == f64::INFINITY {
                serde_json::json!({ "$number": "Infinity" })
            } else if *number == f64::NEG_INFINITY {
                serde_json::json!({ "$number": "-Infinity" })
            } else {
                serde_json::Number::from_f64(*number)
                    .map(serde_json::Value::Number)
                    .unwrap_or(serde_json::Value::Null)
            }
        }
        JsValue::String(text) => serde_json::Value::String(text.clone()),
        JsValue::Array(items) => {
            serde_json::Value::Array(items.iter().map(js_value_to_json).collect())
        }
        JsValue::Object(entries) => {
            let mut map = serde_json::Map::new();
            for (key, value) in entries {
                map.insert(key.clone(), js_value_to_json(value));
            }
            serde_json::Value::Object(map)
        }
    }
}

/// golden l01：重複的鍵（`uniqueKeys`）是語法錯誤。
#[test]
fn golden_l01_duplicate_key_is_invalid() {
    run_golden_load_case("l01-dup");
}

/// golden l02：多份文件是語法錯誤。
#[test]
fn golden_l02_multiple_documents_are_invalid() {
    run_golden_load_case("l02-multidoc");
}

/// golden l03：101 個 alias（計數 1 + 101 > 100）是語法錯誤。
#[test]
fn golden_l03_alias_bomb_is_invalid() {
    run_golden_load_case("l03-alias-bomb");
}

/// golden l03b：99 個 alias（計數 1 + 99 = 100 ≤ 100）可載入。
#[test]
fn golden_l03b_alias_99_is_allowed() {
    run_golden_load_case("l03b-alias-99");
}

/// golden l03c：100 個 alias（計數 1 + 100 = 101 > 100）是語法錯誤。
#[test]
fn golden_l03c_alias_100_is_invalid() {
    run_golden_load_case("l03c-alias-100");
}

/// golden l04：alias 指向尚未建立完成的祖先是循環 alias。
#[test]
fn golden_l04_cyclic_alias_is_rejected() {
    run_golden_load_case("l04-cycle");
}

/// golden l05：`<<` 是普通的鍵（YAML 1.2 無 merge key）。
#[test]
fn golden_l05_merge_key_is_plain_key() {
    run_golden_load_case("l05-merge");
}

/// golden l06：core schema 的標量判定（`yes`／`on`／`1_000` 是字串、`0x1F`、
/// `0o17`、`.inf`、`.nan`、`+12`、`010`、超過 2^53 的整數失精度等）。
#[test]
fn golden_l06_core_schema_scalars() {
    run_golden_load_case("l06-scalars");
}

/// golden l07：空檔載入為 null。
#[test]
fn golden_l07_empty_file_is_null() {
    run_golden_load_case("l07-empty");
}

/// golden l08：語法錯誤（未收尾的 flow sequence）。
#[test]
fn golden_l08_syntax_error_is_invalid() {
    run_golden_load_case("l08-syntax");
}

/// golden l09：tag 處理（`!!str`、`!!int`、`!custom`）。
#[test]
fn golden_l09_tags() {
    run_golden_load_case("l09-tags");
}

/// golden l10：block 標量（literal／folded）與雙引號跳脫。
#[test]
fn golden_l10_block_scalars() {
    run_golden_load_case("l10-block-scalars");
}

/// golden l11：UTF-8 BOM 先去掉。
#[test]
fn golden_l11_bom_is_stripped() {
    run_golden_load_case("l11-bom");
}

// ---------------------------------------------------------------------------
// 單元測試（以 node 對 yaml 套件驗證過的行為）
// ---------------------------------------------------------------------------

/// core schema 的 plain 標量解析表（§7.1 的精確規則）。
#[test]
fn plain_scalar_resolution_matches_core_schema() {
    let cases: Vec<(&str, ResolvedScalar)> = vec![
        ("~", ResolvedScalar::Null),
        ("null", ResolvedScalar::Null),
        ("Null", ResolvedScalar::Null),
        ("NULL", ResolvedScalar::Null),
        ("", ResolvedScalar::Null),
        ("true", ResolvedScalar::Bool(true)),
        ("True", ResolvedScalar::Bool(true)),
        ("TRUE", ResolvedScalar::Bool(true)),
        ("false", ResolvedScalar::Bool(false)),
        ("False", ResolvedScalar::Bool(false)),
        ("FALSE", ResolvedScalar::Bool(false)),
        // `yes`、`on` 是字串（YAML 1.2 core schema）。
        ("yes", ResolvedScalar::Str("yes".to_string())),
        ("on", ResolvedScalar::Str("on".to_string())),
        ("off", ResolvedScalar::Str("off".to_string())),
        ("no", ResolvedScalar::Str("no".to_string())),
        // 十進位：`010` → 10、`+12` → 12。
        ("010", ResolvedScalar::Num(10.0)),
        ("+12", ResolvedScalar::Num(12.0)),
        ("-12", ResolvedScalar::Num(-12.0)),
        ("00", ResolvedScalar::Num(0.0)),
        // `1_000` 是字串（core schema 不吃底線）。
        ("1_000", ResolvedScalar::Str("1_000".to_string())),
        // 8／16 進位；`0X` 大寫與帶正負號的 16 進位是字串。
        ("0o17", ResolvedScalar::Num(15.0)),
        ("0o18", ResolvedScalar::Str("0o18".to_string())),
        ("0x1F", ResolvedScalar::Num(31.0)),
        ("0X1F", ResolvedScalar::Str("0X1F".to_string())),
        ("+0x1F", ResolvedScalar::Str("+0x1F".to_string())),
        ("0b101", ResolvedScalar::Str("0b101".to_string())),
        // 大整數照 JS 失去精度（字串轉 f64 的正確捨入）。
        (
            "123456789012345678",
            ResolvedScalar::Num(123456789012345680.0),
        ),
        (
            "0xFFFFFFFFFFFFFFFFFF",
            ResolvedScalar::Num(4.722366482869645e21),
        ),
        // float。
        (".inf", ResolvedScalar::Num(f64::INFINITY)),
        (".Inf", ResolvedScalar::Num(f64::INFINITY)),
        ("+.INF", ResolvedScalar::Num(f64::INFINITY)),
        ("-.inf", ResolvedScalar::Num(f64::NEG_INFINITY)),
        ("1e3", ResolvedScalar::Num(1000.0)),
        ("1E3", ResolvedScalar::Num(1000.0)),
        ("1e+3", ResolvedScalar::Num(1000.0)),
        ("1e-3", ResolvedScalar::Num(0.001)),
        (".5", ResolvedScalar::Num(0.5)),
        ("-.5", ResolvedScalar::Num(-0.5)),
        ("+.5", ResolvedScalar::Num(0.5)),
        ("0.5", ResolvedScalar::Num(0.5)),
        ("5.", ResolvedScalar::Num(5.0)),
        ("1e21", ResolvedScalar::Num(1e21)),
        ("1:2", ResolvedScalar::Str("1:2".to_string())),
    ];
    for (source, expected) in cases {
        assert_eq!(resolve_plain_scalar(source), expected, "輸入：{source:?}");
    }
    // NaN 與 -0 無法用 PartialEq 比較，另行斷言。
    assert!(matches!(
        resolve_plain_scalar(".nan"),
        ResolvedScalar::Num(value) if value.is_nan()
    ));
    assert!(matches!(
        resolve_plain_scalar("-0"),
        ResolvedScalar::Num(value) if value == 0.0 && value.is_sign_negative()
    ));
}

/// 引號與 block 標量一律字串；不解析內容（core schema 不套用）。
#[test]
fn quoted_and_block_scalars_are_strings() {
    let tag: Option<Tag> = None;
    for style in [
        ScalarStyle::SingleQuoted,
        ScalarStyle::DoubleQuoted,
        ScalarStyle::Literal,
        ScalarStyle::Folded,
    ] {
        for source in ["true", "123", "null", "", ".inf"] {
            assert_eq!(
                resolve_scalar(source, style, tag.as_ref()),
                ResolvedScalar::Str(source.to_string()),
                "樣式 {style:?} 輸入 {source:?}"
            );
        }
    }
}

/// 有 tag 時依該型別的 test 解析；不符則落回字串（照 yaml 套件）。
#[test]
fn tagged_scalars_follow_their_type_with_string_fallback() {
    let yaml_tag = |suffix: &str| {
        Some(Tag {
            handle: "tag:yaml.org,2002:".to_string(),
            suffix: suffix.to_string(),
        })
    };
    // `!!str` 一律字串。
    assert_eq!(
        resolve_scalar("123", ScalarStyle::Plain, yaml_tag("str").as_ref()),
        ResolvedScalar::Str("123".to_string())
    );
    // `!!int`：十進位、8／16 進位都可；不符落回字串。
    assert_eq!(
        resolve_scalar("7", ScalarStyle::Plain, yaml_tag("int").as_ref()),
        ResolvedScalar::Num(7.0)
    );
    assert_eq!(
        resolve_scalar("0x1F", ScalarStyle::Plain, yaml_tag("int").as_ref()),
        ResolvedScalar::Num(31.0)
    );
    assert_eq!(
        resolve_scalar("010", ScalarStyle::Plain, yaml_tag("int").as_ref()),
        ResolvedScalar::Num(10.0)
    );
    assert_eq!(
        resolve_scalar("abc", ScalarStyle::Plain, yaml_tag("int").as_ref()),
        ResolvedScalar::Str("abc".to_string())
    );
    assert_eq!(
        resolve_scalar(".5", ScalarStyle::Plain, yaml_tag("int").as_ref()),
        ResolvedScalar::Str(".5".to_string())
    );
    // `!!float`：NaN／±Inf、指數、一般小數；整數形狀（`3`）落回字串。
    assert_eq!(
        resolve_scalar(".5", ScalarStyle::Plain, yaml_tag("float").as_ref()),
        ResolvedScalar::Num(0.5)
    );
    assert_eq!(
        resolve_scalar("1e3", ScalarStyle::Plain, yaml_tag("float").as_ref()),
        ResolvedScalar::Num(1000.0)
    );
    assert_eq!(
        resolve_scalar(".inf", ScalarStyle::Plain, yaml_tag("float").as_ref()),
        ResolvedScalar::Num(f64::INFINITY)
    );
    assert_eq!(
        resolve_scalar("3", ScalarStyle::Plain, yaml_tag("float").as_ref()),
        ResolvedScalar::Str("3".to_string())
    );
    // `!!bool`：只有 bool test 符合才解析。
    assert_eq!(
        resolve_scalar("TRUE", ScalarStyle::Plain, yaml_tag("bool").as_ref()),
        ResolvedScalar::Bool(true)
    );
    assert_eq!(
        resolve_scalar("yes", ScalarStyle::Plain, yaml_tag("bool").as_ref()),
        ResolvedScalar::Str("yes".to_string())
    );
    // `!!null`：test（`~`／`null`／空）符合才是 null。
    assert_eq!(
        resolve_scalar("~", ScalarStyle::Plain, yaml_tag("null").as_ref()),
        ResolvedScalar::Null
    );
    assert_eq!(
        resolve_scalar("", ScalarStyle::Plain, yaml_tag("null").as_ref()),
        ResolvedScalar::Null
    );
    assert_eq!(
        resolve_scalar("x", ScalarStyle::Plain, yaml_tag("null").as_ref()),
        ResolvedScalar::Str("x".to_string())
    );
}

/// 其他 tag（`!custom`、`!`、verbatim、core 的非標量 tag）一律是字串。
#[test]
fn other_tags_are_treated_as_strings() {
    assert_value("a: !custom 123\n", obj(vec![("a", s("123"))]));
    assert_value("a: ! 123\n", obj(vec![("a", s("123"))]));
    assert_value(
        "a: !<tag:yaml.org,2002:int> 5\n",
        obj(vec![("a", JsValue::Number(5.0))]),
    );
    assert_value("a: !!seq 1\n", obj(vec![("a", s("1"))]));
    assert_value("a: !!map 1\n", obj(vec![("a", s("1"))]));
}

/// 重複的標量鍵（含引號與解析後同值的情況）在組合階段就報錯。
#[test]
fn duplicate_scalar_keys_are_parse_errors() {
    assert_parse_error("a: 1\na: 2\n", "YAML 語法無效");
    // 引號鍵與 plain 鍵解析後同值：`"a"` 與 `a`。
    assert_parse_error("a: 1\n\"a\": 2\n", "YAML 語法無效");
    // `1.0` 與 `1` 解析後都是數字 1。
    assert_parse_error("a: 1.0\na: 1\n", "YAML 語法無效");
    // flow map 也檢查。
    assert_parse_error("m: {a: 1, a: 2}\n", "YAML 語法無效");
}

/// 不是重複鍵、或字串化後同鍵的覆寫保留原位置（照 `addPairToJSMap`）。
#[test]
fn stringified_equal_keys_overwrite_in_place() {
    // 數字鍵與字串鍵解析後型別不同（JS `1 === "1"` 為 false）：組合階段不報錯，
    // 字串化後同為 "1"，覆寫成一個鍵。
    assert_value("? 1\n: x\n? \"1\"\n: y\n", obj(vec![("1", s("y"))]));
    // `.nan` 鍵：NaN 不等於 NaN，不算是重複；字串化後同為 "NaN"，覆寫。
    assert_value(".nan: x\n.nan: y\n", obj(vec![("NaN", s("y"))]));
    // alias 鍵解析成與前一鍵相同的字串：覆寫而非報錯。
    assert_value(
        "? &a x\n: 1\n? *a\n: 2\n",
        obj(vec![("x", JsValue::Number(2.0))]),
    );
}

/// map 鍵的 JS `String()` 字串化規則（null 為空字串、`-0` 為 "0"、`1e21` 為 "1e+21"）。
#[test]
fn key_stringification_follows_js_string() {
    let value = JsValue::Number(1.0);
    assert_value("? ~\n: 1\n", obj(vec![("", value.clone())]));
    assert_value("? -0\n: 1\n", obj(vec![("0", value.clone())]));
    assert_value("? 1e21\n: 1\n", obj(vec![("1e+21", value.clone())]));
    assert_value("? .inf\n: 1\n", obj(vec![("Infinity", value.clone())]));
    assert_value("? .nan\n: 1\n", obj(vec![("NaN", value.clone())]));
    assert_value("? true\n: 1\n", obj(vec![("true", value.clone())]));
    assert_value("? 0x1F\n: 1\n", obj(vec![("31", value.clone())]));
    // 有 tag 的鍵依解析後的值字串化。
    assert_value("? !!int 7\n: 1\n", obj(vec![("7", value)]));
}

/// 有 anchor 的空標量解析為 null（`x: &a`），alias 解析結果也是 null。
#[test]
fn anchored_empty_scalar_is_null() {
    assert_value(
        "x: &a\ny: *a\n",
        obj(vec![("x", JsValue::Null), ("y", JsValue::Null)]),
    );
}

/// 沒有巢狀 alias 的空集合倍數為 0：計數 × 0 永遠不超過上限（照 Alias.js）。
#[test]
fn aliases_of_empty_collection_are_unlimited() {
    let mut source = String::from("x: &a []\n");
    let mut entries: Vec<(String, JsValue)> = vec![("x".to_string(), arr(vec![]))];
    for index in 0..150 {
        source.push_str(&format!("k{index}: *a\n"));
        entries.push((format!("k{index}"), arr(vec![])));
    }
    assert_value(&source, JsValue::Object(entries));
}

/// 巢狀 alias 的倍數相乘：a1..a5 內部的解析都在限內，最外層 2 × 81 > 100 就失敗。
#[test]
fn nested_alias_multiplier_compounds() {
    let nested = "a1: &a1 [1]\na2: &a2 [*a1, *a1]\na3: &a3 [*a2, *a2]\na4: &a4 [*a3, *a3]\na5: &a5 [*a4, *a4]\n";
    // 沒有最外層 alias：5 層的檢查（54、81）都在限內。
    assert!(parse_yaml(nested).is_ok());
    // `k: *a5`：count = 2 × 81 = 162 > 100。
    assert_parse_error(&format!("{nested}k: *a5\n"), "YAML 語法無效");
}

/// 自我參照的陣列（alias 指向正在轉換中的祖先）回報循環 alias。
#[test]
fn self_referential_array_reports_cycle() {
    assert_parse_error("a: &x [*x]\n", "YAML 不可包含循環 alias");
    assert_parse_error("a: &x\n  b: *x\n", "YAML 不可包含循環 alias");
    // 雙層循環也要能偵測。
    assert_parse_error("a: &x\n  b: &y\n    c: *x\n", "YAML 不可包含循環 alias");
}

/// alias 指向不存在的 anchor：saphyr 在掃描階段就報語法錯誤（yaml 套件在
/// toJS 階段丟 Unresolved alias，訊息同為「YAML 語法無效」）。
#[test]
fn unresolved_alias_is_syntax_error() {
    assert_parse_error("b: *x\n", "YAML 語法無效");
}

/// 空文件（空檔、只有註解）載入為 null。
#[test]
fn comment_only_file_is_null() {
    assert_value("", JsValue::Null);
    assert_value("# 只有註解\n", JsValue::Null);
    assert_value("---\n", JsValue::Null);
    // 只有 `...`：saphyr 忽略（yaml 套件會報錯，見 doc/decisions.md）。
    assert_value("...\n", JsValue::Null);
}

/// 文件根節點可以是標量或序列。
#[test]
fn root_scalar_and_sequence() {
    assert_value("--- 42\n", JsValue::Number(42.0));
    assert_value("--- true\n", JsValue::Bool(true));
    assert_value(
        "- 1\n- 2\n",
        arr(vec![JsValue::Number(1.0), JsValue::Number(2.0)]),
    );
    assert_value(
        "--- [1, 2]\n",
        arr(vec![JsValue::Number(1.0), JsValue::Number(2.0)]),
    );
}

/// 檔尾再有 `---`（空文件）視為多份文件；`...` 之後再有內容也是多份文件。
#[test]
fn trailing_and_post_end_documents_are_invalid() {
    assert_parse_error("a: 1\n---\n", "YAML 語法無效");
    assert_parse_error("a: 1\n...\nb: 2\n", "YAML 語法無效");
}

/// `ConfigParseError` 的 Display 輸出完整訊息。
#[test]
fn parse_error_display_includes_prefix() {
    let error = crate::config::errors::ConfigParseError::new("YAML 語法無效");
    assert_eq!(error.to_string(), "設定檔格式錯誤：YAML 語法無效");
}
