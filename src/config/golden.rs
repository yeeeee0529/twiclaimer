//! golden 比對的共用輔助（測試專用）：golden 目錄與 JSON 比較規則，
//! 供 `yaml/tests.rs` 與 `loader/tests.rs` 共用。
use serde_json::Value;

/// golden 案例目錄。
pub(crate) const GOLDEN_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/golden/config");

/// 比較 golden 的 JSON：物件比對鍵的順序與值、數字先轉 f64（NaN 視為相等）。
pub(crate) fn json_eq(actual: &Value, expected: &Value) -> bool {
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
