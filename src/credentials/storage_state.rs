//! 登入狀態檔的解析：`StorageState` 與 `parseStorageState` 的移植。
//!
//! 舊版把兩者放在 `credentials/CredentialValidator.ts` 內，§4.2 對應拆成
//! 獨立檔案。JSON 以 serde_json 解析（JS number ↔ f64；`preserve_order` 保留
//! 鍵序），只驗證頂層形狀與 `cookies`／`origins` 的存在與型別，元素內容不驗證
//! （與舊版相同）。
#![allow(dead_code)]

use super::errors::{CredentialValidationError, StorageStateFormatError, StorageStateParseError};

/// 對應舊版 `StorageState`：`validate` 用得到的形狀（`cookies`／`origins`
/// 可缺省，存在時是元素任意的陣列）。
#[derive(Debug, PartialEq)]
pub(crate) struct StorageState {
    /// 對應 `cookies`；`None` 表示屬性不存在。
    cookies: Option<Vec<serde_json::Value>>,
    /// 對應 `origins`；`None` 表示屬性不存在。
    origins: Option<Vec<serde_json::Value>>,
}

impl StorageState {
    /// 對應 `(storageState.cookies?.length ?? 0) > 0`。
    pub(crate) fn has_cookies(&self) -> bool {
        self.cookies
            .as_ref()
            .is_some_and(|cookies| !cookies.is_empty())
    }
}

/// 對應舊版 `parseStorageState`：JSON 解析與頂層形狀檢查。
///
/// 只會回傳 [`CredentialValidationError::Parse`] 與
/// [`CredentialValidationError::Format`] 兩種變體（對應舊版函式拋出的兩種類別）。
pub(crate) fn parse_storage_state(
    source: &str,
    storage_state_path: &str,
) -> Result<StorageState, CredentialValidationError> {
    let parsed: serde_json::Value = serde_json::from_str(source).map_err(|_| {
        CredentialValidationError::Parse(StorageStateParseError::new(storage_state_path))
    })?;

    // 舊版 `typeof parsed !== 'object' || parsed === null || Array.isArray(parsed)`：
    // serde_json 的頂層不是 Object 的情況（null、陣列、字串、數字、布林）都屬此分支。
    let serde_json::Value::Object(entries) = parsed else {
        return Err(CredentialValidationError::Format(
            StorageStateFormatError::new(storage_state_path, "JSON 頂層必須是物件"),
        ));
    };

    // 舊版：cookies 檢查在前、origins 在後（錯誤的判定順序照舊）。
    // JSON 無法表達 `undefined`，屬性存在即對應舊版的 `!== undefined`。
    for (key, detail) in [
        ("cookies", "cookies 若存在必須是陣列"),
        ("origins", "origins 若存在必須是陣列"),
    ] {
        if let Some(value) = entries.get(key)
            && !value.is_array()
        {
            return Err(CredentialValidationError::Format(
                StorageStateFormatError::new(storage_state_path, detail),
            ));
        }
    }

    Ok(StorageState {
        cookies: array_field(&entries, "cookies"),
        origins: array_field(&entries, "origins"),
    })
}

/// 取出已驗證為陣列的屬性（對應舊版解析後的欄位存取）。
fn array_field(
    entries: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Option<Vec<serde_json::Value>> {
    match entries.get(key) {
        Some(serde_json::Value::Array(items)) => Some(items.clone()),
        _ => None,
    }
}
